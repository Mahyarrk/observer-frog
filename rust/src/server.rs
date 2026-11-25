//! The HTTP server: /api/status, /api/history, /api/poll and the write endpoints.
//!
//! Bounds, per DESIGN.md: loopback by default, no wildcard CORS on writes, a poll lock so a
//! phone refreshing cannot stack forced polls, and flags read from SQLite at serve time (I6).

use crate::config::Config;
use crate::db;
use crate::metrics;
use crate::poller;
use crate::probes::clip;
use crate::state;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MIN_FORCED_POLL_S: f64 = 2.0;
pub const HISTORY_POINTS: usize = 7;
pub const MAX_BODY: usize = 8192;

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Holds the shared runtime pieces; each request handler reads from here.
pub struct App {
    pub cfg: Config,
    pub started_ts: i64,
    snapshot: Mutex<Value>,
    poll_lock: Mutex<bool>,
    device_cache: Mutex<(Value, f64)>,
    pub last_poll_error: Mutex<Option<String>>,
    pub waves: Arc<std::sync::atomic::AtomicU64>,
    pub dropped: Arc<std::sync::atomic::AtomicU64>,
    pub errors: Arc<std::sync::atomic::AtomicU64>,
    pub last_wave_error: Arc<Mutex<Option<String>>>,
    pub notifier: Mutex<Option<crate::notify::Notifier>>,
    pub poll_interval: u64,
}

impl App {
    pub fn new(cfg: Config, poll_interval: u64) -> Self {
        Self {
            cfg,
            started_ts: now(),
            snapshot: Mutex::new(json!({"timestamp": 0, "services": [], "devices": []})),
            poll_lock: Mutex::new(false),
            device_cache: Mutex::new((Value::Null, 0.0)),
            last_poll_error: Mutex::new(None),
            waves: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            errors: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            last_wave_error: Arc::new(Mutex::new(None)),
            notifier: Mutex::new(None),
            poll_interval,
        }
    }

    pub fn set_snapshot(&self, rows: Vec<Value>, ts: i64) {
        let snap = json!({
            "timestamp": ts,
            "services": rows,
            "devices": [self.device_card()],
            "confirm_polls": state::DOWN_CONFIRM_POLLS,
            "node_ts": ts,
        });
        *self.snapshot.lock().unwrap() = snap;
    }

    /// Compile the payload. Flags and history are read now, never baked in (I6).
    pub fn snapshot(&self, with_history: bool) -> Value {
        let mut snap = {
            let guard = self.snapshot.lock().unwrap();
            guard.clone()
        };
        if let Some(obj) = snap.as_object_mut() {
            obj.insert(
                "ignored".into(),
                json!(db::ignored_ids(&self.cfg.db_path, &self.cfg.node_id).unwrap_or_default()),
            );
            if with_history {
                obj.insert("history".into(), json!(self.history_map(HISTORY_POINTS)));
                let last_err = self.last_wave_error.lock().unwrap().clone();
                obj.insert(
                    "self_monitoring".into(),
                    json!({
                        "waves": self.waves.load(std::sync::atomic::Ordering::Relaxed),
                        "dropped": self.dropped.load(std::sync::atomic::Ordering::Relaxed),
                        "errors": self.errors.load(std::sync::atomic::Ordering::Relaxed),
                        "last_error": last_err,
                        "uptime_s": now() - self.started_ts,
                    }),
                );
            }
        }
        snap
    }

    /// Per-service sparkline: 1 for healthy, 0 otherwise, oldest first.
    pub fn history_map(&self, points: usize) -> HashMap<String, Vec<u8>> {
        let mut out: HashMap<String, Vec<u8>> = HashMap::new();
        if points == 0 {
            return out;
        }
        let t = now();
        let bucket = (60i64).max(points as i64 * self.poll_interval as i64);
        let conn = match db::connect(&self.cfg.db_path) {
            Ok(c) => c,
            Err(_) => return out,
        };
        let mut stmt = match conn.prepare(
            "SELECT service_id, timestamp, status FROM service_checks \
             WHERE node_id = ? AND timestamp >= ? ORDER BY timestamp",
        ) {
            Ok(s) => s,
            Err(_) => return out,
        };
        let rows = stmt.query_map((&self.cfg.node_id, t - bucket * points as i64), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        });
        if let Ok(rows) = rows {
            for row in rows.flatten() {
                let (sid, ts, st) = row;
                let vec = out.entry(sid).or_insert_with(|| vec![0u8; points]);
                let idx = (((t - ts) / bucket).clamp(0, points as i64 - 1)) as usize;
                if st == "up" || st == "degraded" {
                    vec[points - 1 - idx] = 1;
                }
            }
        }
        out
    }

    pub fn device_card(&self) -> Value {
        let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let mut cache = self.device_cache.lock().unwrap();
        if cache.0.is_null() || t - cache.1 > 5.0 {
            cache.0 = metrics::device_card(&self.cfg.node_id, Some(&self.cfg.node_name));
            cache.1 = t;
        }
        cache.0.clone()
    }

    /// Run one wave on demand. A second concurrent caller gets 409.
    pub fn force_poll(&self) -> (bool, Value) {
        {
            let mut lock = self.poll_lock.lock().unwrap();
            if *lock {
                return (false, json!({"error": "a poll is already running"}));
            }
            *lock = true;
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let rows = poller::sample(&self.cfg.services, None);
            let ts = now();
            let state_rows: Vec<state::Row> = rows
                .iter()
                .map(|r| state::Row {
                    service_id: r["id"].as_str().unwrap_or("").to_string(),
                    name: r["name"].as_str().unwrap_or("").to_string(),
                    group: r["group"].as_str().unwrap_or("").to_string(),
                    status: r["status"].as_str().unwrap_or("down").to_string(),
                    detail: r["detail"].as_str().unwrap_or("").to_string(),
                    latency_ms: r["latency_ms"].as_i64().unwrap_or(0),
                })
                .collect();
            let changes = state::apply_wave(&self.cfg.db_path, &self.cfg.node_id, &state_rows, Some(ts))
                .unwrap_or_default();
            if let Some(n) = self.notifier.lock().unwrap().as_mut() {
                if !changes.is_empty() {
                    n.submit(&changes);
                }
            }
            self.set_snapshot(rows, ts);
            self.snapshot(true)
        }));
        *self.poll_lock.lock().unwrap() = false;
        match result {
            Ok(v) => (true, v),
            Err(_) => {
                let msg = "forced poll failed".to_string();
                *self.last_poll_error.lock().unwrap() = Some(msg.clone());
                (true, json!({"error": msg}))
            }
        }
    }
}

/// The `/api/history` contract the UI reads: incidents list, uptime rows, and KPIs.
pub fn build_history(app: &App, days: i64) -> Value {
    let t = now();
    let incidents = state::recent_incidents(&app.cfg.db_path, &app.cfg.node_id, days, 100).unwrap_or_default();
    let rows = state::uptime_rows(&app.cfg.db_path, &app.cfg.node_id, days).unwrap_or_default();

    let unacknowledged: Vec<&state::Incident> = incidents.iter().filter(|i| !i.ignored).collect();
    let closed_real: Vec<&&state::Incident> = unacknowledged.iter().filter(|i| !i.ongoing).collect();

    let mut affected = std::collections::HashSet::new();
    for i in &unacknowledged {
        affected.insert(i.service_id.clone());
    }
    let mttr_total: i64 = closed_real.iter().map(|i| i.duration_s).sum();
    let mttr = if closed_real.is_empty() {
        Value::Null
    } else {
        json!(mttr_total / closed_real.len() as i64)
    };

    let incidents_json: Vec<Value> = incidents
        .iter()
        .map(|i| {
            json!({
                "id": i.id,
                "service_id": i.service_id,
                "name": i.name,
                "group": i.group,
                "started_ts": i.started_ts,
                "ended_ts": i.ended_ts,
                "ongoing": i.ongoing,
                "duration_s": i.duration_s,
                "detail": i.detail,
                "ignored": i.ignored,
            })
        })
        .collect();

    let uptime_json: Vec<Value> = rows
        .iter()
        .map(|u| {
            json!({
                "service_id": u.service_id,
                "name": u.name,
                "group": u.group,
                "uptime_pct": u.uptime_pct,
                "checks": u.checks,
                "down_checks": u.down_checks,
            })
        })
        .collect();

    let kpi = json!({
        "incidents_30d": incidents.len(),
        "incidents_real": unacknowledged.len(),
        "incidents_ignored": incidents.len() - unacknowledged.len(),
        "ongoing": incidents.iter().filter(|i| i.ongoing).count(),
        "ongoing_real": unacknowledged.iter().filter(|i| i.ongoing).count(),
        "ongoing_ignored": incidents.iter().filter(|i| i.ongoing && i.ignored).count(),
        "services_affected": affected.len(),
        "mttr_s": mttr,
        "mttr_n": closed_real.len(),
        "worst_uptime": rows.first().map(|r| json!(r.uptime_pct)).unwrap_or(Value::Null),
        "worst_service": rows.first().map(|r| json!(r.name)).unwrap_or(Value::Null),
        "uptime_services": rows.len(),
        "uptime_excluded": db::ignored_ids(&app.cfg.db_path, &app.cfg.node_id).map(|v| v.len()).unwrap_or(0),
    });

    json!({
        "generated": t,
        "window_days": days,
        "incidents": incidents_json,
        "uptime": uptime_json,
        "kpi": kpi,
    })
}

fn static_file(path: &str) -> Option<(&'static str, &'static str)> {
    // (relative path, content type). The assets are embedded so the binary is self-contained.
    let entry = match path {
        "/" | "/index.html" => ("index.html", "text/html; charset=utf-8"),
        "/sw.js" => ("sw.js", "application/javascript; charset=utf-8"),
        "/manifest.json" => ("manifest.json", "application/manifest+json; charset=utf-8"),
        "/icons/icon-32.png" | "/favicon.ico" => ("icons/icon-32.png", "image/png"),
        "/apple-touch-icon.png" | "/apple-touch-icon-precomposed.png" | "/icons/icon-180.png" => {
            ("icons/icon-180.png", "image/png")
        }
        "/icons/icon-192.png" => ("icons/icon-192.png", "image/png"),
        "/icons/icon-512.png" => ("icons/icon-512.png", "image/png"),
        _ => return None,
    };
    Some(entry)
}

fn embedded(name: &str) -> Option<&'static [u8]> {
    Some(match name {
        "index.html" => include_bytes!("../../index.html").as_slice(),
        "sw.js" => include_bytes!("../../sw.js").as_slice(),
        "manifest.json" => include_bytes!("../../manifest.json").as_slice(),
        "icons/icon-32.png" => include_bytes!("../../icons/icon-32.png").as_slice(),
        "icons/icon-180.png" => include_bytes!("../../icons/icon-180.png").as_slice(),
        "icons/icon-192.png" => include_bytes!("../../icons/icon-192.png").as_slice(),
        "icons/icon-512.png" => include_bytes!("../../icons/icon-512.png").as_slice(),
        _ => return None,
    })
}

fn json_bytes(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec())
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    body: Vec<u8>,
}

fn handle(mut stream: TcpStream, app: Arc<App>) {
    let peer_ok = stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).is_ok();
    if !peer_ok {
        return;
    }
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });

    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let raw_path = parts.next().unwrap_or("/").to_string();

    let mut headers: HashMap<String, String> = HashMap::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = h.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }

    let length: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0u8; length.min(MAX_BODY * 4)];
    if length > 0 && length <= body.len() {
        let _ = reader.read_exact(&mut body);
    }

    let (path_part, query_part) = match raw_path.split_once('?') {
        Some((p, q)) => (p, q),
        None => (raw_path.as_str(), ""),
    };
    let path = {
        let trimmed = path_part.trim_end_matches('/');
        if trimmed.is_empty() { "/".to_string() } else { trimmed.to_string() }
    };
    let query: HashMap<String, String> = query_part
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (k.to_string(), urldecode(v)),
            None => (kv.to_string(), String::new()),
        })
        .collect();

    let req = Request { method: method.clone(), path, query, body };
    let (code, ctype, payload, cors) = route(&req, &app);
    let head_only = method == "HEAD";
    let mut out = Vec::new();
    out.extend_from_slice(format!("HTTP/1.1 {code} {}\r\n", reason(code)).as_bytes());
    out.extend_from_slice(format!("Content-Type: {ctype}\r\n").as_bytes());
    out.extend_from_slice(format!("Content-Length: {}\r\n", payload.len()).as_bytes());
    out.extend_from_slice(b"Cache-Control: no-cache, no-store, must-revalidate\r\n");
    if cors {
        out.extend_from_slice(b"Access-Control-Allow-Origin: *\r\n");
    }
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    if !head_only {
        out.extend_from_slice(&payload);
    }
    let _ = stream.write_all(&out);
    let _ = stream.flush();
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        _ => "OK",
    }
}

fn route(req: &Request, app: &Arc<App>) -> (u16, String, Vec<u8>, bool) {
    let json_ct = "application/json; charset=utf-8".to_string();
    let normalized = req.path.replace("/status", "");

    match (req.method.as_str(), req.path.as_str()) {
        ("GET" | "HEAD", p) if p == "/api/status" || p == "/status/api/status" => {
            (200, json_ct, json_bytes(&app.snapshot(true)), true)
        }
        ("GET" | "HEAD", p) if p == "/api/poll" || p == "/status/api/poll" => {
            let (ok, payload) = app.force_poll();
            (if ok { 200 } else { 409 }, json_ct, json_bytes(&payload), false)
        }
        ("GET" | "HEAD", p) if p == "/api/history" || p == "/status/api/history" => {
            let days = req
                .query
                .get("days")
                .and_then(|v| v.parse::<i64>().ok())
                .map(|d| d.clamp(1, 365))
                .unwrap_or(30);
            (200, json_ct, json_bytes(&build_history(app, days)), true)
        }
        ("GET" | "HEAD", p) if p == "/api/health" || p == "/status/api/health" => {
            (200, json_ct, json_bytes(&json!({"ok": true, "uptime_s": now() - app.started_ts})), false)
        }
        ("POST", _) if normalized == "/api/ignore" => {
            if req.body.len() > MAX_BODY {
                return (413, json_ct, json_bytes(&json!({"ok": false, "error": "body too large"})), false);
            }
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let sid = body.get("id").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            if sid.is_empty() {
                return (400, json_ct, json_bytes(&json!({"ok": false, "error": "missing id"})), false);
            }
            let want = body.get("ignored").and_then(|v| v.as_bool()).unwrap_or(false);
            let res = (|| -> rusqlite::Result<()> {
                let conn = db::connect(&app.cfg.db_path)?;
                if want {
                    conn.execute(
                        "INSERT OR IGNORE INTO ignored VALUES (?,?)",
                        (&app.cfg.node_id, &sid),
                    )?;
                } else {
                    conn.execute(
                        "DELETE FROM ignored WHERE node_id = ? AND service_id = ?",
                        (&app.cfg.node_id, &sid),
                    )?;
                }
                Ok(())
            })();
            if let Err(e) = res {
                return (500, json_ct, json_bytes(&json!({"ok": false, "error": clip(&e.to_string())})), false);
            }
            let ignored = db::ignored_ids(&app.cfg.db_path, &app.cfg.node_id).unwrap_or_default();
            // Both shapes: the full list under both keys. The Python build returns the LIST in
            // `ignored` (not the boolean), and the frontend reads it as a list — returning a
            // bool here silently stops the UI from learning the server's truth.
            (
                200,
                json_ct,
                json_bytes(&json!({
                    "ok": true, "id": sid,
                    "ignored": ignored,
                    "ignored_ids": ignored,
                })),
                false,
            )
        }
        (method, p) if method == "GET" || method == "HEAD" => {
            let lookup = if p == "/status" { "/" } else { p };
            match static_file(lookup) {
                Some((name, ctype)) => match embedded(name) {
                    Some(bytes) => (200, ctype.to_string(), bytes.to_vec(), false),
                    None => (500, "text/plain; charset=utf-8".into(), format!("missing {name}").into_bytes(), false),
                },
                None => (404, json_ct, json_bytes(&json!({"error": "not found"})), false),
            }
        }
        _ => (404, json_ct, json_bytes(&json!({"ok": false, "error": "not found"})), false),
    }
}

/// Bind and serve. Returns the bound port (useful when port 0 was requested).
pub fn serve(app: Arc<App>, bind: &str, port: u16, tls: Option<(PathBuf, PathBuf)>) -> std::io::Result<u16> {
    let listener = TcpListener::bind((bind, port))?;
    let actual = listener.local_addr()?.port();
    let _ = tls; // TLS termination stays the reverse proxy's job in v0.1.
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    let app = Arc::clone(&app);
                    std::thread::spawn(move || handle(s, app));
                }
                Err(_) => break,
            }
        }
    });
    Ok(actual)
}
