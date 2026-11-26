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
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MIN_FORCED_POLL_S: f64 = 2.0;
pub const HISTORY_POINTS: usize = 7;
pub const MAX_BODY: usize = 8192;      // /api/ignore: a small control message
pub const REPORT_MAX_BODY: usize = 65536;  // /api/report: a full node report
/// Hard cap on what the reader will pull off the socket, so a hostile Content-Length cannot
/// make the process allocate. Slightly above the largest legal body so the route can answer 413.
const READ_CAP: usize = REPORT_MAX_BODY + 1;

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
    /// Reports this node sent upstream, and the last reason one failed. Zero counters on a pure
    /// head, which is how `status` distinguishes "not configured to report" from "reporting badly".
    pub reports_ok: Arc<std::sync::atomic::AtomicU64>,
    pub reports_failed: Arc<std::sync::atomic::AtomicU64>,
    pub last_report_error: Arc<Mutex<Option<String>>>,
    /// Serialises password checks. bcrypt at cost 12 is 376 ms on the head, so concurrent logins
    /// would starve the poller on a two-core box.
    pub login_lock: Mutex<()>,
    pub login_limiter: LoginLimiter,
    /// Whether the dashboard listener terminated TLS. Read to decide the cookie's `Secure` flag:
    /// setting it unconditionally would lock out every plain-HTTP install. Interior mutability
    /// because `listen()` learns the transport after `App` exists.
    pub tls_enabled: std::sync::atomic::AtomicBool,
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
            reports_ok: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            reports_failed: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            last_report_error: Arc::new(Mutex::new(None)),
            login_lock: Mutex::new(()),
            login_limiter: LoginLimiter::new(),
            tls_enabled: std::sync::atomic::AtomicBool::new(false),
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

    /// Compile the payload for one node. Flags and history are read now, never baked in (I6).
    ///
    /// `None` means the node this process runs on. A named node is assembled from the database
    /// rather than the in-memory snapshot, because only the local probe loop keeps one.
    pub fn snapshot_for(&self, node_id: Option<&str>, with_history: bool) -> Value {
        let target = node_id.unwrap_or(&self.cfg.node_id);
        let mut snap = if target == self.cfg.node_id {
            let guard = self.snapshot.lock().unwrap();
            guard.clone()
        } else {
            // A remote node has no live snapshot here; its last known state comes from SQLite.
            self.remote_snapshot(target)
        };
        if let Some(obj) = snap.as_object_mut() {
            obj.insert(
                "ignored".into(),
                json!(db::ignored_ids(&self.cfg.db_path, target).unwrap_or_default()),
            );
            if with_history {
                obj.insert("history".into(), json!(self.history_map_for(target, HISTORY_POINTS)));
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

    /// The local node's snapshot, the shape v0.1 clients expect.
    pub fn snapshot(&self, with_history: bool) -> Value {
        self.snapshot_for(None, with_history)
    }

    /// Assemble a remote node's view from what it last reported.
    ///
    /// The head keeps no live poll of a remote node, so this is the database's opinion: the last
    /// known state per service, and the last time the node was heard from.
    fn remote_snapshot(&self, node_id: &str) -> Value {
        let states = state::load_states(&self.cfg.db_path, node_id).unwrap_or_default();
        let mut services: Vec<Value> = states
            .iter()
            .map(|(sid, st)| {
                json!({
                    "id": sid,
                    "name": if st.name.is_empty() { sid.clone() } else { st.name.clone() },
                    "group": st.group,
                    "status": st.state,
                    "detail": st.last_detail,
                    "latency_ms": 0,
                    // The tail measured this on its own machine; the head only relays it, and must
                    // never re-resolve a remote port locally — both machines bind 8770. Omitted
                    // when unknown so the UI renders "—" rather than a confident "0 MB".
                    "ram_mb": if st.ram_mb > 0.0 { json!(st.ram_mb) } else { Value::Null },
                    "ports": st.ports,
                    "address": st.address,
                })
            })
            .collect();
        services.sort_by(|a, b| {
            a["group"].as_str().unwrap_or("").cmp(b["group"].as_str().unwrap_or(""))
                .then_with(|| a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or("")))
        });

        let (name, last_seen, interval) = node_meta(&self.cfg.db_path, node_id);
        // The device card the node last measured, not a placeholder: cpu, memory, disk, uptime,
        // thermal all come from the machine that can actually read them.
        let mut card = stored_metrics(&self.cfg.db_path, node_id).unwrap_or_else(|| {
            json!({
                "id": format!("dev_{node_id}"),
                "kind": "remote",
                "detail": format!("reports every {interval}s"),
            })
        });
        if let Some(obj) = card.as_object_mut() {
            // The head is authoritative for the name and for whether the node is reachable.
            obj.insert("id".into(), json!(format!("dev_{node_id}")));
            obj.insert(
                "name".into(),
                json!(if name.is_empty() { node_id.to_string() } else { name }),
            );
            obj.insert("kind".into(), json!("remote"));
            obj.insert(
                "status".into(),
                json!(if last_seen > 0 { "up" } else { "unknown" }),
            );
        }
        json!({
            "timestamp": last_seen,
            "node_ts": last_seen,
            "node": node_id,
            "services": services,
            "devices": [card],
            "confirm_polls": state::DOWN_CONFIRM_POLLS,
        })
    }

    /// Per-service sparkline: 1 for healthy, 0 otherwise, oldest first.
    pub fn history_map_for(&self, node_id: &str, points: usize) -> HashMap<String, Vec<u8>> {
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
        let rows = stmt.query_map((node_id, t - bucket * points as i64), |r| {
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
            cache.0 = metrics::device_card(&self.cfg.node_id);
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
                    ram_mb: r["ram_mb"].as_f64().unwrap_or(0.0),
                    ports: r["ports"].as_str().unwrap_or("").to_string(),
                    address: r["address"].as_str().unwrap_or("").to_string(),
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

/// The local node's history, the shape v0.1 clients expect.
pub fn build_history(app: &App, days: i64) -> Value {
    build_history_for(app, &app.cfg.node_id.clone(), days)
}

/// The device card a node last reported, or None when it never sent one.
fn stored_metrics(path: &std::path::Path, node_id: &str) -> Option<Value> {
    let conn = db::connect(path).ok()?;
    let text: String = conn
        .query_row(
            "SELECT metrics_json FROM nodes WHERE node_id = ?",
            (node_id,),
            |r| r.get(0),
        )
        .ok()?;
    if text.trim().is_empty() {
        return None;
    }
    serde_json::from_str(&text).ok()
}

/// A node's display name, last report time and interval. Zeros when the node is unknown.
fn node_meta(path: &std::path::Path, node_id: &str) -> (String, i64, i64) {
    let Ok(conn) = db::connect(path) else { return (String::new(), 0, 30) };
    // The device names itself. A display override wins when the operator set one, but the
    // reported name is what applies by default and is never lost.
    conn.query_row(
        "SELECT CASE WHEN display_name <> '' THEN display_name \
                     WHEN reported_name <> '' THEN reported_name \
                     ELSE name END, \
                last_seen_ts, interval_s FROM nodes WHERE node_id = ?",
        (node_id,),
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap_or((String::new(), 0, 30))
}

/// Every node the head knows about, for the selector.
/// Ports already watched by services of this catalog id.
///
/// Used to tell a duplicate from a second instance: the same service on the same port is the
/// mistake, the same service on another port is something the operator may legitimately want.
fn text_block_ports(path: &std::path::Path, catalog_id: &str) -> Vec<String> {
    let Ok(cfg) = crate::config::load(Some(path)) else { return Vec::new() };
    let prefix = format!("{catalog_id}-");
    cfg.services
        .iter()
        .filter(|s| s.id == catalog_id || s.id.starts_with(&prefix))
        .map(|s| crate::config::port_of(&s.target))
        .filter(|p| !p.is_empty())
        .collect()
}

pub fn nodes_list(app: &App) -> Value {
    let now = now();
    let mut out: Vec<Value> = Vec::new();
    // The head itself is always present, even before any tail enrols.
    out.push(json!({
        "id": app.cfg.node_id,
        "name": crate::config::hostname(),
        "interval_s": app.cfg.poll_interval,
        "last_seen_ts": now,
        "local": true,
        "silent": false,
    }));
    if let Ok(conn) = db::connect(&app.cfg.db_path) {
        if let Ok(mut stmt) = conn.prepare(
            // A row with no `last_seen_ts` is a machine this node has stopped hearing from, either
            // because it went quiet long ago or because the role changed away from head and its
            // observations were cleared. Either way it is not part of the roster any more.
            "SELECT node_id, name, interval_s, last_seen_ts, reported_name, display_name \
             FROM nodes WHERE last_seen_ts > 0 ORDER BY node_id",
        ) {
            if let Ok(rows) = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            }) {
                for row in rows.flatten() {
                    let (id, name, interval, last_seen, reported, display) = row;
                    let effective = if !display.is_empty() {
                        display.clone()
                    } else if !reported.is_empty() {
                        reported.clone()
                    } else {
                        name.clone()
                    };
                    if id == app.cfg.node_id {
                        continue; // already added as local
                    }
                    let silent = last_seen > 0 && now - last_seen > interval.max(1) * crate::silence::SILENCE_INTERVALS;
                    out.push(json!({
                        "id": id,
                        "name": if effective.is_empty() { id.clone() } else { effective },
                        "reported_name": reported,
                        "display_name": display,
                        "interval_s": interval,
                        "last_seen_ts": last_seen,
                        "local": false,
                        "silent": silent,
                    }));
                }
            }
        }
    }
    json!({"nodes": out, "generated": now})
}

/// The `/api/history` contract the UI reads: incidents list, uptime rows, and KPIs.
pub fn build_history_for(app: &App, node_id: &str, days: i64) -> Value {
    let t = now();
    let incidents = state::recent_incidents(&app.cfg.db_path, node_id, days, 100).unwrap_or_default();
    let rows = state::uptime_rows(&app.cfg.db_path, node_id, days).unwrap_or_default();

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
        "uptime_excluded": db::ignored_ids(&app.cfg.db_path, node_id).map(|v| v.len()).unwrap_or(0),
    });

    json!({
        "generated": t,
        "node": node_id,
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

/// Which listener accepted this connection.
///
/// Both ports run one router over one `App`. Without this the plain-HTTP report port answers
/// admin routes, because nothing in the request says which port it arrived on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ListenerRole {
    /// The dashboard: browsers, the phone, the operator.
    Dashboard,
    /// Plain HTTP for tail reports. Deliberately certificate-free, therefore deliberately narrow.
    Reports,
}

/// Everything about the connection that is not in the request line.
#[derive(Clone, Copy, Debug)]
pub struct ConnMeta {
    pub role: ListenerRole,
    pub peer: Option<std::net::SocketAddr>,
}

impl ConnMeta {
    pub fn new(role: ListenerRole, peer: Option<std::net::SocketAddr>) -> Self {
        Self { role, peer }
    }

    /// Local means a request that demonstrably came from this machine's own loopback.
    ///
    /// Read from the socket, never from a header: `X-Forwarded-For` is a value the client writes.
    pub fn is_loopback(&self) -> bool {
        self.peer.map(|p| p.ip().is_loopback()).unwrap_or(false)
    }
}

/// The only routes a report-port connection may reach.
///
/// Everything else answers 404 rather than 403: a 403 confirms that a route exists on this machine,
/// which is information a LAN scanner should not get for free.
///
/// This list is the whole policy for that listener. It is short on purpose: a tail speaks to one
/// endpoint, and health exists so a monitor can see the port is alive.
pub const REPORTS_PORT_ROUTES: &[&str] = &["/api/report", "/api/health"];

pub fn reports_port_allows(path: &str) -> bool {
    REPORTS_PORT_ROUTES.contains(&path)
}

/// Routes an anonymous visitor may reach on the dashboard port.
///
/// Three things justify an entry here: the shell has to load before anyone can log in, a tail has to
/// be able to ask to join before it has an account, and logout has to clear a cookie even when the
/// session is already dead. Everything else needs a session.
pub const PUBLIC_ROUTES: &[&str] = &[
    "/api/health",
    "/api/auth/status",
    "/api/auth/login",
    "/api/auth/logout",
    "/api/pair",
];

pub fn is_public(normalized: &str) -> bool {
    PUBLIC_ROUTES.contains(&normalized)
}

/// The routes that accept a machine's bearer token rather than a person's session.
pub const MACHINE_ROUTES: &[&str] = &["/api/report"];

/// Every route the dashboard serves. Kept here so the admin surface is a list a reader can see,
/// rather than something discovered by scrolling a thousand-line match.
///
/// A route missing from this list is not a security hole on its own, but it is a route nobody
/// classified, so the test below fails when the two drift apart.
pub const DASHBOARD_ROUTES: &[&str] = &[
    "/api/status",
    "/api/nodes",
    "/api/poll",
    "/api/history",
    "/api/health",
    "/api/node/name",
    "/api/report",
    "/api/settings",
    "/api/autostart",
    "/api/discover/services",
    "/api/catalog",
    "/api/services",
    "/api/services/add",
    "/api/services/remove",
    "/api/heads",
    "/api/pair",
    "/api/pair/ask",
    "/api/auth/status",
    "/api/auth/login",
    "/api/auth/logout",
    "/api/auth/password",
    "/api/auth/logout-all",
    "/api/auth/tls",
    "/api/pair/decide",
    "/api/pair/remove",
    "/api/ignore",
];

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
    /// Declared Content-Length. Zero when absent. Lets a route answer 413 instead of parsing a
    /// body the reader refused to pull.
    declared_len: usize,
}

fn handle<S: Read + Write>(mut stream: S, app: Arc<App>, conn: ConnMeta) {
    // Parse the request inside one borrow of the stream, then write the response after it drops.
    let req = {
        let mut reader = BufReader::new(&mut stream);
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

        let length: usize = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        // Over READ_CAP we deliberately do not read: the route answers 413 from declared_len.
        let mut body = Vec::new();
        if length > 0 && length <= READ_CAP {
            body = vec![0u8; length];
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

        Request { method, path, query, headers, body, declared_len: length }
    };

    let mut cookie: Option<String> = None;
    let (code, ctype, payload, cors) = route(&req, &app, conn, &mut cookie);
    let head_only = req.method == "HEAD";
    let mut out = Vec::new();
    out.extend_from_slice(format!("HTTP/1.1 {code} {}\r\n", reason(code)).as_bytes());
    out.extend_from_slice(format!("Content-Type: {ctype}\r\n").as_bytes());
    if let Some(c) = cookie {
        out.extend_from_slice(format!("Set-Cookie: {c}\r\n").as_bytes());
    }
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

/// Whether this request carries a valid session.
///
/// A node with no password has nothing to verify a session against, so it is not authenticated.
/// The setup route exists for exactly that window and is gated on the token file instead.
fn session_valid(app: &Arc<App>, headers: &std::collections::HashMap<String, String>, _conn: ConnMeta) -> bool {
    let Some(token) = crate::auth::cookie_from(headers) else {
        return false;
    };
    crate::auth::verify(&app.cfg.db_path, &token).is_some()
}

/// The key a rate limiter counts against. The socket address, never a header: `X-Forwarded-For` is
/// a value the client writes, so trusting it would make the limit opt-out.
fn peer_key(conn: ConnMeta) -> String {
    conn.peer
        .map(|p| p.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Five failures in fifteen minutes, per peer. In memory, so a restart clears it: persisting
/// lockouts is a reliable way to lock yourself out during an outage.
pub struct LoginLimiter {
    inner: std::sync::Mutex<std::collections::HashMap<String, (u32, i64)>>,
}

pub const LOGIN_MAX_FAILURES: u32 = 5;
pub const LOGIN_WINDOW_S: i64 = 900;

impl LoginLimiter {
    pub fn new() -> Self {
        Self { inner: std::sync::Mutex::new(std::collections::HashMap::new()) }
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// Seconds to wait, when this peer has spent its attempts.
    pub fn retry_after(&self, peer: &str) -> Option<i64> {
        let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let (count, first) = map.get(peer)?;
        if *count < LOGIN_MAX_FAILURES {
            return None;
        }
        let elapsed = Self::now() - *first;
        if elapsed >= LOGIN_WINDOW_S {
            return None; // the window has passed; the next attempt may go ahead
        }
        Some(LOGIN_WINDOW_S - elapsed)
    }

    pub fn record_failure(&self, peer: &str) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Self::now();
        match map.get_mut(peer) {
            Some((count, first)) if now - *first < LOGIN_WINDOW_S => {
                *count += 1;
            }
            // A new window, or the first failure from this peer.
            _ => {
                map.insert(peer.to_string(), (1, now));
            }
        }
    }

    pub fn clear(&self, peer: &str) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).remove(peer);
    }
}

impl Default for LoginLimiter {
    fn default() -> Self {
        Self::new()
    }
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

/// The cookie a route wants written, if any. Passed in rather than returned so the four-tuple every
/// existing arm produces stays untouched: only login and logout set it.
fn route(
    req: &Request,
    app: &Arc<App>,
    conn: ConnMeta,
    cookie: &mut Option<String>,
) -> (u16, String, Vec<u8>, bool) {
    let json_ct = "application/json; charset=utf-8".to_string();
    // Strip the reverse-proxy prefix, not every occurrence of the word. `replace("/status", "")`
    // turned `/api/status` into `/api/`, so a path check on the result was comparing something
    // that no longer named the route — which is how the auth gate first let `/api/status` through.
    let normalized = req
        .path
        .strip_prefix("/status")
        .filter(|rest| rest.starts_with('/'))
        .unwrap_or(&req.path)
        .to_string();

    // A report-port connection may not reach the admin surface. Checked on both spellings of the
    // path, because every route also answers under `/status/...` for the reverse-proxy layout, and
    // a check on one spelling would be a way around the other.
    if conn.role == ListenerRole::Reports
        && !(reports_port_allows(&req.path) || reports_port_allows(&normalized))
    {
        // 404, not 403: this listener should not confirm which routes exist.
        return (404, json_ct, json_bytes(&json!({"error": "not found"})), false);
    }

    // The person-versus-machine gate. An operator route needs a session; a machine route needs a
    // bearer token and is left to the report handler, which already checks it.
    //
    // Only API paths are gated. Static assets and the shell have to load before anyone can present
    // a cookie, so blocking them would make the login page itself unreachable.
    let is_api = normalized.starts_with("/api/");
    if is_api
        && !is_public(&normalized)
        && !MACHINE_ROUTES.contains(&normalized.as_str())
    {
        if !session_valid(app, &req.headers, conn) {
            return (401, json_ct, json_bytes(&json!({
                "ok": false,
                "error": "not signed in",
                "needs_login": true,
            })), false);
        }
        // Signed in with the default password is not the same as being set up. Everything except
        // changing the password and signing out stays shut, so the default cannot be used as a
        // way to read the dashboard.
        let allowed_while_default = matches!(
            normalized.as_str(),
            "/api/auth/password" | "/api/auth/logout" | "/api/auth/status"
        );
        if crate::auth::password_must_change(&app.cfg.db_path) && !allowed_while_default {
            return (403, json_ct, json_bytes(&json!({
                "ok": false,
                "error": "the default password has to be changed before anything else",
                "must_change": true,
            })), false);
        }
    }

    match (req.method.as_str(), req.path.as_str()) {
        ("GET" | "HEAD", p) if p == "/api/status" || p == "/status/api/status" => {
            let node = req.query.get("node").map(|s| s.as_str()).filter(|s| !s.is_empty());
            (200, json_ct, json_bytes(&app.snapshot_for(node, true)), true)
        }
        ("GET" | "HEAD", p) if p == "/api/nodes" || p == "/status/api/nodes" => {
            (200, json_ct, json_bytes(&nodes_list(app)), true)
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
            let node = req.query.get("node").cloned().unwrap_or_else(|| app.cfg.node_id.clone());
            (200, json_ct, json_bytes(&build_history_for(app, &node, days)), true)
        }
        (m, p) if (m == "GET" || m == "HEAD") && (p == "/api/auth/status" || p == "/status/api/auth/status") => {
            // Booleans only. Naming the operator, or hinting how close a password was, would be a
            // free oracle for anyone on the LAN.
            let has = crate::auth::password_is_set(&app.cfg.db_path);
            let authed = session_valid(app, &req.headers, conn);
            let must_change = crate::auth::password_must_change(&app.cfg.db_path);
            (200, json_ct, json_bytes(&json!({
                "ok": true,
                // Always required: a fresh install has the default password, so there is never a
                // window where the dashboard is reachable without signing in.
                "required": true,
                "authenticated": authed,
                "has_password": has,
                // Signed in with the default: the app stays shut until it is replaced.
                "must_change": must_change && authed,
                // Not the password itself: just whether this machine still has the default, so the
                // login screen can mention it once and stop.
                "first_run": must_change,
            })), true)
        }
        ("POST", p) if p == "/api/auth/login" || p == "/status/api/auth/login" => {
            if req.body.len() > MAX_BODY {
                return (413, json_ct, json_bytes(&json!({"ok": false, "error": "body too large"})), false);
            }
            // Serialised: bcrypt at cost 12 takes 376 ms on the head, so five concurrent attempts
            // would starve the poller on a two-core box. One at a time caps it at one core.
            let _guard = app.login_lock.lock().unwrap_or_else(|e| e.into_inner());

            let peer = peer_key(conn);
            if let Some(wait) = app.login_limiter.retry_after(&peer) {
                return (429, json_ct, json_bytes(&json!({
                    "ok": false,
                    "error": format!("too many attempts; try again in {wait} seconds"),
                    "retry_after": wait,
                })), false);
            }

            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let password = body.get("password").and_then(|v| v.as_str()).unwrap_or("");
            if crate::auth::verify_password(&app.cfg.db_path, password) {
                app.login_limiter.clear(&peer);
                match crate::auth::mint(&app.cfg.db_path) {
                    Some(token) => {
                        let exp = crate::auth::verify(&app.cfg.db_path, &token).unwrap_or(0);
                        let secure = app.tls_enabled.load(std::sync::atomic::Ordering::Relaxed);
                        *cookie = Some(crate::auth::set_cookie(&token, exp - crate::auth::now_s(), secure));
                        (200, json_ct, json_bytes(&json!({"ok": true})), false)
                    }
                    None => (500, json_ct, json_bytes(&json!({"ok": false, "error": "cannot mint a session"})), false),
                }
            } else {
                app.login_limiter.record_failure(&peer);
                // Deliberately the same message whether the password is wrong or absent: whether a
                // password exists is already answered by /api/auth/status, and nothing else should be.
                (401, json_ct, json_bytes(&json!({"ok": false, "error": "wrong password"})), false)
            }
        }
        ("POST", p) if p == "/api/auth/password" || p == "/status/api/auth/password" => {
            // Set, change, or remove. Requires a session, which the gate above has already checked.
            if req.body.len() > MAX_BODY {
                return (413, json_ct, json_bytes(&json!({"ok": false, "error": "body too large"})), false);
            }
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let action = body.get("action").and_then(|v| v.as_str()).unwrap_or("change");
            let current = body.get("current").and_then(|v| v.as_str()).unwrap_or("");
            let failure = |msg: &str| {
                (403, json_ct.clone(), json_bytes(&json!({"ok": false, "error": msg})), false)
            };

            // Removing needs the current password too: without it, anyone holding a phone left
            // unlocked could open the port to the whole LAN.
            if crate::auth::password_is_set(&app.cfg.db_path)
                && !crate::auth::verify_password(&app.cfg.db_path, current)
            {
                return failure("the current password is not right");
            }

            match action {
                "remove" => match crate::auth::remove_password(&app.cfg.db_path) {
                    Ok(()) => {
                        *cookie = Some(crate::auth::clear_cookie());
                        (200, json_ct, json_bytes(&json!({
                            "ok": true, "removed": true,
                            "warning": "the dashboard is now open to anyone who can reach this port",
                        })), false)
                    }
                    Err(e) => (500, json_ct, json_bytes(&json!({"ok": false, "error": e})), false),
                },
                _ => {
                    let next = body.get("password").and_then(|v| v.as_str()).unwrap_or("");
                    match crate::auth::set_password(&app.cfg.db_path, next) {
                        Ok(()) => {
                            // set_password rotated the secret, so re-issue to this device: the whole
                            // point is that other devices are logged out and this one is not.
                            match crate::auth::mint(&app.cfg.db_path) {
                                Some(token) => {
                                    let exp = crate::auth::verify(&app.cfg.db_path, &token).unwrap_or(0);
                                    let secure = app.tls_enabled.load(std::sync::atomic::Ordering::Relaxed);
                                    *cookie = Some(crate::auth::set_cookie(
                                        &token,
                                        exp - crate::auth::now_s(),
                                        secure,
                                    ));
                                }
                                None => {}
                            }
                            (200, json_ct, json_bytes(&json!({"ok": true})), false)
                        }
                        Err(e) => (400, json_ct, json_bytes(&json!({"ok": false, "error": e})), false),
                    }
                }
            }
        }
        ("POST", p) if p == "/api/auth/logout-all" || p == "/status/api/auth/logout-all" => {
            // One button, one purpose: a lost phone. Rotating the secret ends every session,
            // including this one, so a fresh cookie is issued here.
            if let Err(e) = crate::auth::rotate_secret(&app.cfg.db_path) {
                return (500, json_ct, json_bytes(&json!({"ok": false, "error": e})), false);
            }
            match crate::auth::mint(&app.cfg.db_path) {
                Some(token) => {
                    let exp = crate::auth::verify(&app.cfg.db_path, &token).unwrap_or(0);
                    let secure = app.tls_enabled.load(std::sync::atomic::Ordering::Relaxed);
                    *cookie = Some(crate::auth::set_cookie(&token, exp - crate::auth::now_s(), secure));
                    (200, json_ct, json_bytes(&json!({"ok": true, "detail": "every other device is signed out"})), false)
                }
                None => (500, json_ct, json_bytes(&json!({"ok": false, "error": "cannot mint a session"})), false),
            }
        }
        ("POST", p) if p == "/api/auth/logout" || p == "/status/api/auth/logout" => {
            *cookie = Some(crate::auth::clear_cookie());
            (200, json_ct, json_bytes(&json!({"ok": true})), false)
        }
        ("GET" | "HEAD", p) if p == "/api/health" || p == "/status/api/health" => {
            (200, json_ct, json_bytes(&json!({"ok": true, "uptime_s": now() - app.started_ts})), false)
        }
        ("POST", _) if normalized == "/api/node/name" => {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let node = body.get("node").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            if node.is_empty() {
                return (400, json_ct, json_bytes(&json!({"ok": false, "error": "missing node"})), false);
            }
            // An empty display name clears the override, restoring the name the device reports.
            let display = body.get("display_name").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            let res = (|| -> rusqlite::Result<()> {
                let conn = db::connect(&app.cfg.db_path)?;
                conn.execute(
                    "UPDATE nodes SET display_name = ? WHERE node_id = ?",
                    (&display, &node),
                )?;
                Ok(())
            })();
            match res {
                Ok(()) => {
                    let (effective, _, _) = node_meta(&app.cfg.db_path, &node);
                    (200, json_ct, json_bytes(&json!({
                        "ok": true, "node": node, "name": effective, "display_name": display,
                    })), false)
                }
                Err(e) => (500, json_ct, json_bytes(&json!({"ok": false, "error": e.to_string()})), false),
            }
        }
        ("POST", _) if normalized == "/api/report" => {
            let (code, payload) = crate::ingest::handle_report(app, &req.headers, &req.body, req.declared_len);
            (code, json_ct, json_bytes(&payload), false)
        }
        // ---- admin GUI (v0.3, no auth by design; loopback by default) ----
        ("GET" | "HEAD", _) if normalized == "/api/settings" => {
            // Read the file, not the boot-time config: the GUI's whole job is to change this file,
            // so reporting the values the process started with would show a switch that never
            // moves. The runtime itself picks the change up through its mtime watcher.
            let cfg = crate::config::load(Some(&crate::settings::target(&app.cfg)))
                .unwrap_or_else(|_| app.cfg.clone());
            let role = match (cfg.serve, cfg.report_to.is_some()) {
                (true, true) => "head + tail",
                (true, false) => "head",
                (false, true) => "tail",
                (false, false) => "idle",
            };
            let autostart = crate::install::is_installed(&cfg);
            (200, json_ct, json_bytes(&json!({
                "ok": true,
                "role": role,
                "serve": cfg.serve,
                // A tail with nowhere to send its data is a legitimate configuration, and the page
                // says so in red rather than the process dying over it.
                "head_paired": cfg.report_to.is_some(),
                "paused": cfg.serve && cfg.report_to.is_some(),
                "token_ok": cfg.token().is_ok(),
                "bind": cfg.bind,
                "port": cfg.port,
                "report_port": cfg.report_port,
                "report_to": cfg.report_to,
                "poll_interval": cfg.poll_interval,
                "node_id": cfg.node_id,
                "config_path": crate::settings::target(&cfg).display().to_string(),
                "db_path": cfg.db_path.display().to_string(),
                "autostart": autostart,
                "autostart_supported": cfg!(target_os = "macos") || cfg!(target_os = "linux"),
                "services": cfg.services.len(),
            })), false)
        }
        ("POST", _) if normalized == "/api/settings" => {
            if req.body.len() > MAX_BODY {
                return (413, json_ct, json_bytes(&json!({"ok": false, "error": "body too large"})), false);
            }
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let path = crate::settings::target(&app.cfg);
            match crate::settings::apply(&path, &body) {
                Ok(changed) => {
                    // No restart and no unit rewrite: acting as head is read by the sweeper each
                    // tick and reporting is read each wave, so the change is live within seconds.
                    (200, json_ct, json_bytes(&json!({
                        "ok": true, "changed": changed, "path": path.display().to_string(),
                    })), false)
                }
                Err(e) => (500, json_ct, json_bytes(&json!({"ok": false, "error": e})), false),
            }
        }
        ("POST", _) if normalized == "/api/auth/tls" => {
            // Import a certificate pair. Validated here, in the service's own context, because a
            // path that parses from your shell may be unreadable to the unit that will open it.
            if req.body.len() > MAX_BODY {
                return (413, json_ct, json_bytes(&json!({"ok": false, "error": "body too large"})), false);
            }
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let cert = body.get("cert").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            let key = body.get("key").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            match crate::server::check_tls_pair(&cert, &key) {
                Ok(()) => {
                    let path = crate::settings::target(&app.cfg);
                    match crate::settings::apply(&path, &json!({ "tls_cert": cert, "tls_key": key })) {
                        Ok(_) => (200, json_ct, json_bytes(&json!({
                            "ok": true,
                            "detail": "certificate accepted. Restart this machine's service to use it.",
                            "restart_required": true,
                        })), false),
                        Err(e) => (500, json_ct, json_bytes(&json!({"ok": false, "error": e})), false),
                    }
                }
                Err(e) => (400, json_ct, json_bytes(&json!({"ok": false, "error": e})), false),
            }
        }
        ("GET" | "HEAD", _) if normalized == "/api/autostart" => {
            // Read the file, not `app.cfg`: the role can change underneath a running process
            // (installing rewrites the unit), and a head answering about a tail's autostart is
            // how the switch appeared to reset itself after every refresh.
            let cfg = crate::config::load(Some(&crate::settings::target(&app.cfg)))
                .unwrap_or_else(|_| app.cfg.clone());
            (200, json_ct, json_bytes(&json!({
                "ok": true,
                "enabled": crate::install::is_installed(&cfg),
                "supported": cfg!(target_os = "macos") || cfg!(target_os = "linux"),
            })), false)
        }
        ("POST", _) if normalized == "/api/autostart" => {
            if req.body.len() > MAX_BODY {
                return (413, json_ct, json_bytes(&json!({"ok": false, "error": "body too large"})), false);
            }
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let on = body.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
            let cfg = crate::config::load(Some(&crate::settings::target(&app.cfg)))
                .unwrap_or_else(|_| app.cfg.clone());
            match crate::install::set_enabled(&cfg, on) {
                Ok(msg) => (200, json_ct, json_bytes(&json!({
                    "ok": true, "enabled": on, "detail": msg,
                })), false),
                Err(e) => (400, json_ct, json_bytes(&json!({"ok": false, "error": e})), false),
            }
        }
        ("GET" | "HEAD", _) if normalized == "/api/discover/services" => {
            let entries = crate::catalog::load(None).unwrap_or_default();
            let cfg = crate::config::load(Some(&crate::settings::target(&app.cfg)))
                .unwrap_or_else(|_| app.cfg.clone());
            let monitored: Vec<String> = cfg.services.iter().map(|s| s.id.clone()).collect();
            let cands = crate::discover::candidates(&entries, "127.0.0.1", &monitored);
            let bucket = |kind: &str| -> Vec<Value> {
                cands
                    .iter()
                    .filter(|c| c.state == kind)
                    .map(|c| json!({
                        "id": c.id, "name": c.name, "group": c.group,
                        "probe": c.probe, "default_port": c.default_port,
                        "expected_off": c.expected_off, "detail": c.detail,
                    }))
                    .collect()
            };
            (200, json_ct, json_bytes(&json!({
                "ok": true,
                "available": bucket(crate::discover::CAND_AVAILABLE),
                // Deliberately not reported: "not running" was every catalog entry that is not
                // live, which reads as a list of what you have installed and is wrong — it is the
                // catalog, not this machine. Only what answered is offered.
                "not_running": Vec::<String>::new(),
                "port_taken": bucket(crate::discover::CAND_PORT_TAKEN),
                "already": bucket(crate::discover::CAND_ALREADY),
            })), false)
        }
        ("GET" | "HEAD", _) if normalized == "/api/catalog" => {
            let entries = crate::catalog::load(None).unwrap_or_default();
            let out: Vec<Value> = entries
                .iter()
                .map(|e| json!({
                    // The picker shows the friendly name with the id beside it; without `name`
                    // every row read as a bare lowercase id.
                    "id": e.id, "name": e.name, "group": e.group, "probe": e.probe,
                    "default_port": e.default_port,
                }))
                .collect();
            (200, json_ct, json_bytes(&json!({"ok": true, "count": out.len(), "entries": out})), false)
        }
        ("GET" | "HEAD", _) if normalized == "/api/services" => {
            // The services this node is monitoring, straight from the file.
            let path = crate::settings::target(&app.cfg);
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let cfg = crate::config::load(Some(&path)).unwrap_or_else(|_| app.cfg.clone());
            let ids = crate::settings::service_ids_in(&text);
            let out: Vec<Value> = cfg
                .services
                .iter()
                .map(|s| json!({
                    "id": s.id, "name": s.name, "group": s.group,
                    "probe": s.probe, "target": s.target, "ports": s.ports,
                }))
                .collect();
            (200, json_ct, json_bytes(&json!({"ok": true, "count": out.len(), "services": out, "ids": ids})), false)
        }
        ("POST", _) if normalized == "/api/services/add" => {
            if req.body.len() > MAX_BODY {
                return (413, json_ct, json_bytes(&json!({"ok": false, "error": "body too large"})), false);
            }
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let path = crate::settings::target(&app.cfg);
            // Two ways in: a catalog id with a port override, or a ready-made block.
            let result = if let Some(block) = body.get("block").and_then(|v| v.as_str()) {
                let id = body.get("id").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                crate::settings::add_service(&path, block, &id)
            } else {
                let cid = body.get("catalog_id").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                let port = body.get("port").and_then(|v| v.as_u64()).map(|p| p as u16);
                let instance = body.get("instance").and_then(|v| v.as_str()).unwrap_or("");
                match crate::catalog::load(None) {
                    Err(e) => Err(format!("cannot load the catalog: {e}")),
                    Ok(entries) => match crate::catalog::by_id(&entries, &cid) {
                        None => Err(format!("no catalog entry {cid:?}")),
                        Some(entry) => match crate::catalog::instantiate(entry, "127.0.0.1", port, instance) {
                            Err(e) => Err(e.to_string()),
                            Ok(mut svc) => {
                                // A second copy of the same service is legitimate as long as it is on
                                // a different port. Only a repeat on the very same port is the
                                // mistake, so compare ports and suffix the id when they differ —
                                // otherwise the operator had no way to watch two instances.
                                let wanted = svc.ports.clone();
                                let already: Vec<String> = text_block_ports(&path, &entry.id);
                                if already.iter().any(|p| *p == wanted) {
                                    return (
                                        400,
                                        json_ct,
                                        json_bytes(&json!({
                                            "ok": false,
                                            "error": format!(
                                                "{} is already watched on port {}; give this instance another port",
                                                entry.name, wanted
                                            ),
                                        })),
                                        false,
                                    );
                                }
                                if !already.is_empty() {
                                    svc.id = format!("{}-{}", entry.id, wanted);
                                }
                                // The button says "Verify and add", so verify: probe the candidate
                                // now and refuse it if it does not answer as itself. Without this
                                // the block was written for a port nobody was listening on, and the
                                // dashboard showed a service that had never existed.
                                let (state, detail, _) = crate::probes::run(&svc);
                                if state != crate::probes::STATE_UP {
                                    return (
                                        400,
                                        json_ct,
                                        json_bytes(&json!({
                                            "ok": false,
                                            "error": format!(
                                                "nothing answered at {}: {detail}",
                                                svc.target
                                            ),
                                        })),
                                        false,
                                    );
                                }
                                let block = crate::catalog::to_toml(&svc);
                                crate::settings::add_service(&path, &block, &svc.id)
                            }
                        },
                    },
                }
            };
            match result {
                Ok(()) => (200, json_ct, json_bytes(&json!({"ok": true})), false),
                Err(e) => (400, json_ct, json_bytes(&json!({"ok": false, "error": e})), false),
            }
        }
        ("POST", _) if normalized == "/api/services/remove" => {
            if req.body.len() > MAX_BODY {
                return (413, json_ct, json_bytes(&json!({"ok": false, "error": "body too large"})), false);
            }
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let id = body.get("id").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            match crate::settings::remove_service(&crate::settings::target(&app.cfg), &id) {
                Ok(()) => (200, json_ct, json_bytes(&json!({"ok": true, "removed": id})), false),
                Err(e) => (400, json_ct, json_bytes(&json!({"ok": false, "error": e})), false),
            }
        }
        ("GET" | "HEAD", _) if normalized == "/api/heads" => {
            // Pairing lives here rather than in a CLI flow: the settings page lists what answered.
            let me = crate::config::load(Some(&crate::settings::target(&app.cfg)))
                .map(|c| c.node_name)
                .unwrap_or_else(|_| app.cfg.node_name.clone());
            // An instance never offers itself as a head to pair with.
            // shortcut: matched by node name, so two machines sharing a name would hide each other;
            // upgrade to a per-install uuid in the advertisement if that ever happens.
            let found = crate::lan::find_heads(std::time::Duration::from_millis(1500));
            // Only a node that advertises a report port will accept reports, so only those are
            // heads worth pairing with. Every machine answers discovery with its dashboard port,
            // which is how a tail stays findable — listing it here offered a head that cannot
            // collect, and pairing with it wrote an address nothing was listening on.
            let heads: Vec<Value> = found
                .iter()
                .filter(|h| h.name != me && h.report_port.is_some())
                .map(|h| h.to_json())
                .collect();
            (200, json_ct, json_bytes(&json!({
                "ok": true, "found": heads.len(), "heads": heads,
            })), false)
        }
        ("POST", _) if normalized == "/api/pair/ask" => {
            // Pairing from the scanning side, in one call: write the head, then ask it. The GUI
            // used to write the address and stop, leaving the request to a report loop that asks
            // once per process lifetime — so a second Pair press after a rejection did nothing.
            if req.body.len() > MAX_BODY {
                return (413, json_ct, json_bytes(&json!({"ok": false, "error": "body too large"})), false);
            }
            let b: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let head = b.get("url").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            if head.is_empty() {
                return (400, json_ct, json_bytes(&json!({"ok": false, "error": "a head url is required"})), false);
            }
            if !head.starts_with("http://") && !head.starts_with("https://") {
                return (400, json_ct, json_bytes(&json!({"ok": false, "error": "the head url must start with http:// or https://"})), false);
            }
            let path = crate::settings::target(&app.cfg);
            if let Err(e) = crate::settings::apply(&path, &json!({ "report_to": head })) {
                return (500, json_ct, json_bytes(&json!({"ok": false, "error": e})), false);
            }
            let live = crate::config::load(Some(&path)).unwrap_or_else(|_| app.cfg.clone());
            let token = match live.token() {
                Ok(t) => t,
                Err(e) => return (400, json_ct, json_bytes(&json!({"ok": false, "error": e})), false),
            };
            match crate::pair::ask_head(&head, &live.node_id, &crate::config::hostname(), &token, live.poll_interval as i64) {
                Ok(detail) => (200, json_ct, json_bytes(&json!({
                    "ok": true, "paired_with": head, "detail": detail,
                })), false),
                Err(e) => (502, json_ct, json_bytes(&json!({"ok": false, "error": e})), false),
            }
        }
        ("POST", _) if normalized == "/api/pair" => {
            // A tail asks to join. Deliberately unauthenticated: the token it proposes is not a
            // credential yet, and requiring a secret to request one would be circular. Nothing
            // happens until someone on this machine approves it.
            if req.body.len() > MAX_BODY {
                return (413, json_ct, json_bytes(&json!({"ok": false, "error": "body too large"})), false);
            }
            let b: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let node_id = b.get("node_id").and_then(|v| v.as_str()).unwrap_or("");
            let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let token = b.get("token").and_then(|v| v.as_str()).unwrap_or("");
            let interval = b.get("interval_s").and_then(|v| v.as_i64()).unwrap_or(30);
            if node_id.trim().is_empty() || token.trim().is_empty() {
                return (400, json_ct, json_bytes(&json!({"ok": false, "error": "node_id and token are required"})), false);
            }
            let from = req
                .headers
                .get("x-forwarded-for")
                .cloned()
                .unwrap_or_else(|| "lan".to_string());
            match crate::pair::request(&app.cfg.db_path, node_id, name, token, &from, interval) {
                Ok(_) => (200, json_ct, json_bytes(&json!({
                    "ok": true,
                    "pending": true,
                    "detail": "waiting for the head to approve this machine",
                })), false),
                Err(e) => (500, json_ct, json_bytes(&json!({"ok": false, "error": e})), false),
            }
        }
        ("GET" | "HEAD", _) if normalized == "/api/pair" => {
            // Reading a pairing request only makes sense on a machine acting as head: a tail has
            // nothing to approve, and saying so beats an empty list that looks like no requests.
            // Read from disk, not from boot: the role is a switch the operator flips while this
            // process keeps running, and a head gate that only knows what it started as would
            // refuse to list requests minutes after being made a head.
            let live = crate::config::load(Some(&crate::settings::target(&app.cfg)))
                .unwrap_or_else(|_| app.cfg.clone());
            if !live.serve {
                return (200, json_ct, json_bytes(&json!({
                    "ok": true, "requests": [], "note": "this node is not acting as head",
                })), false);
            }
            // Both lists in one call: the page shows pending requests above the machines already
            // paired, and a second round trip would let the two disagree.
            let rows = crate::pair::pending(&app.cfg.db_path);
            let known = crate::pair::paired(&app.cfg.db_path);
            (200, json_ct, json_bytes(&json!({
                "ok": true, "requests": rows, "paired": known,
            })), false)
        }
        ("POST", _) if normalized == "/api/pair/decide" => {
            if req.body.len() > MAX_BODY {
                return (413, json_ct, json_bytes(&json!({"ok": false, "error": "body too large"})), false);
            }
            let b: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let id = b.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
            let approve = b.get("approve").and_then(|v| v.as_bool()).unwrap_or(false);
            let res = if approve {
                crate::pair::approve(&app.cfg.db_path, id)
            } else {
                crate::pair::reject(&app.cfg.db_path, id)
            };
            match res {
                Ok(msg) => (200, json_ct, json_bytes(&json!({"ok": true, "detail": msg})), false),
                Err(e) => (400, json_ct, json_bytes(&json!({"ok": false, "error": e})), false),
            }
        }
        ("POST", _) if normalized == "/api/pair/remove" => {
            if req.body.len() > MAX_BODY {
                return (413, json_ct, json_bytes(&json!({"ok": false, "error": "body too large"})), false);
            }
            let b: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let node_id = b.get("node_id").and_then(|v| v.as_str()).unwrap_or("");
            match crate::pair::unpair(&app.cfg.db_path, node_id) {
                Ok(msg) => (200, json_ct, json_bytes(&json!({"ok": true, "detail": msg})), false),
                Err(e) => (400, json_ct, json_bytes(&json!({"ok": false, "error": e})), false),
            }
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
            // The flag is per (node, service). Ignoring a service on one node must not silence
            // the same service id on another.
            let node = body
                .get("node")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| app.cfg.node_id.clone());
            let res = (|| -> rusqlite::Result<()> {
                let conn = db::connect(&app.cfg.db_path)?;
                if want {
                    conn.execute("INSERT OR IGNORE INTO ignored VALUES (?,?)", (&node, &sid))?;
                } else {
                    conn.execute(
                        "DELETE FROM ignored WHERE node_id = ? AND service_id = ?",
                        (&node, &sid),
                    )?;
                }
                Ok(())
            })();
            if let Err(e) = res {
                return (500, json_ct, json_bytes(&json!({"ok": false, "error": clip(&e.to_string())})), false);
            }
            let ignored = db::ignored_ids(&app.cfg.db_path, &node).unwrap_or_default();
            // Both shapes: the full list under both keys. The Python build returns the LIST in
            // `ignored` (not the boolean), and the frontend reads it as a list — returning a
            // bool here silently stops the UI from learning the server's truth.
            (
                200,
                json_ct,
                json_bytes(&json!({
                    "ok": true, "id": sid, "node": node,
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
///
/// With a cert and key every accepted connection is wrapped in TLS before the HTTP parser sees
/// it, so the request handler stays unaware of the transport.
pub fn serve(app: Arc<App>, bind: &str, port: u16, tls: Option<(PathBuf, PathBuf)>) -> std::io::Result<u16> {
    listen(app, bind, port, tls, ListenerRole::Dashboard)
}

/// Bind one listener and serve it. Call it twice to have TLS and plain HTTP at the same time.
///
/// Both listeners share the `App`, so state, flags and the poll lock are single instances: a
/// report that arrives over plain HTTP is immediately visible to a browser reading over TLS.
/// This is deliberate because a tail on a LAN cannot reasonably be asked to manage certificates,
/// while a phone installing a PWA cannot reasonably be asked to trust a plain-HTTP origin.
pub fn listen(
    app: Arc<App>,
    bind: &str,
    port: u16,
    tls: Option<(PathBuf, PathBuf)>,
    role: ListenerRole,
) -> std::io::Result<u16> {
    let listener = TcpListener::bind((bind, port))?;
    let actual = listener.local_addr()?.port();

    let config = match tls {
        Some((cert, key)) => Some(Arc::new(tls_config(&cert, &key)?)),
        None => None,
    };

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            // The peer address comes from the socket, captured before TLS wraps it and the HTTP
            // parser takes the stream. Anything read from a header later is client-supplied.
            let peer = stream.peer_addr().ok();
            let meta = ConnMeta::new(role, peer);
            let app = Arc::clone(&app);
            match config.clone() {
                None => {
                    std::thread::spawn(move || handle(stream, app, meta));
                }
                Some(cfg) => {
                    std::thread::spawn(move || {
                        let Ok(conn) = rustls::ServerConnection::new(cfg) else { return };
                        let mut tls = rustls::StreamOwned::new(conn, stream);
                        handle(&mut tls, app, meta);
                    });
                }
            }
        }
    });
    Ok(actual)
}

/// Validate an imported certificate pair without starting a listener.
///
/// Reuses `tls_config`, so the check is the same code that will open the files at boot. A pair that
/// passes here cannot fail later for a reason this did not see, apart from the files being moved.
pub fn check_tls_pair(cert: &str, key: &str) -> Result<(), String> {
    if cert.is_empty() || key.is_empty() {
        return Err("both a certificate and a key are required".to_string());
    }
    let cert_path = PathBuf::from(cert);
    let key_path = PathBuf::from(key);
    if !cert_path.is_file() {
        return Err(format!("no readable file at {cert}"));
    }
    if !key_path.is_file() {
        return Err(format!("no readable file at {key}"));
    }
    // `with_single_cert` refuses a key that does not match the certificate, so this one call
    // catches a mismatched pair as well as an unparseable file.
    tls_config(&cert_path, &key_path).map_err(|e| {
        let text = e.to_string();
        // Name the likely causes instead of appending a guess to every failure. An X509v1
        // certificate is common from a bare `openssl req -x509` and rustls will not accept it,
        // which previously read as a key mismatch and sent the operator looking in the wrong place.
        if text.contains("UnsupportedCertVersion") {
            "this certificate is X509 version 1. rustls needs version 3: regenerate with              `-addext subjectAltName=DNS:your.host` (any extension forces v3)."
                .to_string()
        } else if text.contains("key") || text.contains("Key") {
            format!("{text}. The key may not match this certificate.")
        } else {
            text
        }
    })?;
    Ok(())
}

/// Build a rustls server config from a PEM certificate chain and key.
fn tls_config(cert_path: &PathBuf, key_path: &PathBuf) -> std::io::Result<rustls::ServerConfig> {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });

    let certs: Vec<_> = rustls_pemfile::certs(&mut std::io::BufReader::new(
        std::fs::File::open(cert_path).map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, format!("{}: {e}", cert_path.display())))?,
    ))
    .collect::<Result<_, _>>()
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(
        std::fs::File::open(key_path).map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, format!("{}: {e}", key_path.display())))?,
    ))
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
    .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no private key in file"))?;

    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loopback() -> ConnMeta {
        ConnMeta::new(ListenerRole::Dashboard, Some("127.0.0.1:50000".parse().unwrap()))
    }

    #[test]
    fn the_reports_port_admits_only_its_own_routes() {
        // A tail speaks to exactly one endpoint, so anything else on that port is a mistake or a
        // probe. Both spellings are checked because every route also answers under /status/...
        for route in REPORTS_PORT_ROUTES {
            assert!(reports_port_allows(route), "{route} must be reachable from the reports port");
        }
        for route in ["/api/settings", "/api/services/add", "/api/pair/decide", "/api/status"] {
            assert!(!reports_port_allows(route), "{route} must not be reachable from the reports port");
        }
    }

    #[test]
    fn the_admin_surface_is_enumerated_not_discovered() {
        // Every dashboard route is listed, and the two lists do not overlap by accident: /api/report
        // and /api/health are the only routes both listeners answer, because a tail reports and a
        // monitor checks.
        let shared: Vec<&str> = REPORTS_PORT_ROUTES
            .iter()
            .copied()
            .filter(|r| DASHBOARD_ROUTES.contains(r))
            .collect();
        assert_eq!(shared, vec!["/api/report", "/api/health"]);
        assert_eq!(DASHBOARD_ROUTES.len(), 26, "a route was added; classify it deliberately");

        // Auth routes are on the dashboard and split into public and session-required. Setup is
        // public but refuses once a password exists; status and login are public; the rest need a
        // session, which the gate enforces because they are absent from PUBLIC_ROUTES.
        for r in ["/api/auth/status", "/api/auth/login", "/api/auth/logout"] {
            assert!(is_public(r), "{r} must be reachable without a session");
        }
        for r in ["/api/auth/password", "/api/auth/logout-all"] {
            assert!(!is_public(r), "{r} must require a session");
        }
    }

    #[test]
    fn loopback_is_read_from_the_socket() {
        assert!(loopback().is_loopback());
        let remote = ConnMeta::new(ListenerRole::Dashboard, Some("192.0.2.10:5000".parse().unwrap()));
        assert!(!remote.is_loopback());
        // No peer at all is not loopback. An unknown address must never be treated as local.
        assert!(!ConnMeta::new(ListenerRole::Dashboard, None).is_loopback());
    }
}
