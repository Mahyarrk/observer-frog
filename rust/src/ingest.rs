//! Ingest: a tail posts what it observed, the head stamps arrival time and applies it.
//!
//! Design points that matter:
//!   - The head's arrival time is the truth. A tail's `observed_at` is an ordering hint only, so a
//!     machine with a bad clock cannot write into an hour bucket that has already been pruned.
//!   - One bearer token per node, stored as a SHA-256 digest. A token identifies a node; a report
//!     whose payload `node.id` disagrees with the token's node is rejected, so node A cannot write
//!     as node B.
//!   - A report is idempotent at the storage layer (unique index on node/service/timestamp), so a
//!     retry over a flaky link changes nothing.
//!   - An empty `services` list is not the same as an absent one. Absent is a malformed report;
//!     present-but-empty is a legitimate node-only heartbeat and must never wipe state.

use crate::db;
use crate::server::{App, REPORT_MAX_BODY};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;

/// Stable digest of a token, for storage and comparison.
pub fn token_digest(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time comparison, so a wrong token does not leak its length or prefix by timing.
fn digest_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// The node a token belongs to, if any. Tokens come from the environment, never the config file.
pub fn node_for_token(db_path: &Path, token: &str) -> Option<String> {
    if token.is_empty() {
        return None;
    }
    let want = token_digest(token);
    let conn = db::connect(db_path).ok()?;
    let mut stmt = conn.prepare("SELECT node_id, token_sha256 FROM nodes").ok()?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .ok()?;
    for row in rows.flatten() {
        let (node_id, stored) = row;
        if !stored.is_empty() && digest_eq(&stored, &want) {
            return Some(node_id);
        }
    }
    None
}

#[derive(Debug, Clone)]
pub struct ReportService {
    pub id: String,
    pub name: String,
    pub group: String,
    pub status: String,
    pub detail: String,
    pub latency_ms: i64,
    /// Where the service actually listens, as the tail discovered or was told. The head cannot
    /// work this out for another machine, so it must be carried rather than guessed.
    pub ports: String,
    pub address: String,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub node_id: String,
    pub node_name: String,
    pub interval_s: i64,
    pub observed_at: i64,
    pub services: Option<Vec<ReportService>>,
    /// The device card the tail measured: cpu, memory, disk, uptime, thermal. Opaque here;
    /// the head stores and renders it rather than re-deriving it.
    pub metrics: Option<serde_json::Value>,
}

/// Parse a report body. Returns Err with a client-facing message on a malformed payload.
pub fn parse(body: &[u8]) -> Result<Report, String> {
    let doc: Value = serde_json::from_slice(body).map_err(|e| format!("invalid JSON: {e}"))?;

    let node = doc.get("node").ok_or("missing node")?;
    let node_id = node
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if node_id.is_empty() {
        return Err("missing node.id".into());
    }
    let node_name = node.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let interval_s = node.get("interval").and_then(|v| v.as_i64()).unwrap_or(0);

    // `services` absent is a malformed report; present-and-empty is a heartbeat.
    let services = match doc.get("services") {
        None => return Err("missing services (send [] for a node-only heartbeat)".into()),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                let sid = item.get("id").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                if sid.is_empty() {
                    return Err(format!("services[{i}] has no id"));
                }
                let status = item.get("status").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if !matches!(status.as_str(), "up" | "degraded" | "down" | "unavailable") {
                    return Err(format!(
                        "services[{i}] status {status:?} must be one of up, degraded, down, unavailable"
                    ));
                }
                out.push(ReportService {
                    id: sid,
                    name: item.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    group: item.get("group").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    status,
                    detail: item.get("detail").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    latency_ms: item.get("latency_ms").and_then(|v| v.as_i64()).unwrap_or(0),
                    ports: item.get("ports").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    address: item.get("address").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                });
            }
            // Two rows for one id in a single report would fight over state; reject rather than
            // silently pick a winner.
            let mut seen = HashSet::new();
            for s in &out {
                if !seen.insert(s.id.clone()) {
                    return Err(format!("duplicate service id {:?} in one report", s.id));
                }
            }
            Some(out)
        }
        Some(_) => return Err("services must be an array".into()),
    };

    Ok(Report {
        node_id,
        node_name,
        interval_s,
        observed_at: doc.get("observed_at").and_then(|v| v.as_i64()).unwrap_or(0),
        services,
        metrics: doc.get("metrics").filter(|v| v.is_object()).cloned(),
    })
}

/// Record that a node reported. Creates the row on first sight.
pub fn touch_node(
    db_path: &Path,
    node_id: &str,
    name: &str,
    interval_s: i64,
    token_digest: &str,
    at: i64,
) -> rusqlite::Result<()> {
    touch_node_with_metrics(db_path, node_id, name, interval_s, token_digest, at, None)
}

/// As `touch_node`, but also stores the device card the node reported.
pub fn touch_node_with_metrics(
    db_path: &Path,
    node_id: &str,
    name: &str,
    interval_s: i64,
    token_digest: &str,
    at: i64,
    metrics: Option<&serde_json::Value>,
) -> rusqlite::Result<()> {
    let conn = db::connect(db_path)?;
    // `name` mirrors reported_name for anything still reading the v3 column; reported_name is
    // the device's own word and is never overwritten by a display override.
    let metrics_json = metrics.map(|m| m.to_string()).unwrap_or_default();
    conn.execute(
        "INSERT INTO nodes (node_id, name, reported_name, interval_s, token_sha256, first_seen_ts, last_seen_ts, metrics_json) \
         VALUES (?,?,?,?,?,?,?,?) \
         ON CONFLICT(node_id) DO UPDATE SET \
           name = CASE WHEN excluded.name <> '' THEN excluded.name ELSE nodes.name END, \
           reported_name = CASE WHEN excluded.reported_name <> '' THEN excluded.reported_name ELSE nodes.reported_name END, \
           interval_s = CASE WHEN excluded.interval_s > 0 THEN excluded.interval_s ELSE nodes.interval_s END, \
           token_sha256 = CASE WHEN excluded.token_sha256 <> '' THEN excluded.token_sha256 ELSE nodes.token_sha256 END, \
           last_seen_ts = excluded.last_seen_ts, \
           metrics_json = CASE WHEN excluded.metrics_json <> '' THEN excluded.metrics_json ELSE nodes.metrics_json END",
        (node_id, name, name, interval_s.max(0), token_digest, at, at, metrics_json),
    )?;
    Ok(())
}

/// Apply a report. Returns the number of service states submitted.
///
/// `head_now` is the arrival stamp and is what goes into the database. Writing the tail's
/// `observed_at` instead would let a slow clock land checks in a pruned hour bucket.
pub fn apply(app: &App, report: &Report, head_now: i64) -> rusqlite::Result<usize> {
    let rows: Vec<crate::state::Row> = match &report.services {
        Some(list) => list
            .iter()
            .map(|s| crate::state::Row {
                service_id: s.id.clone(),
                name: if s.name.is_empty() { s.id.clone() } else { s.name.clone() },
                group: s.group.clone(),
                status: s.status.clone(),
                detail: s.detail.clone(),
                latency_ms: s.latency_ms,
                ports: s.ports.clone(),
                address: s.address.clone(),
            })
            .collect(),
        None => Vec::new(),
    };
    let count = rows.len();
    // An empty list still means "this node is alive with nothing to say"; apply_wave refuses it
    // (I3) and the node row alone carries the liveness, so call it only when there is data.
    if !rows.is_empty() {
        // head_now stamps arrival (bucketing); report.observed_at is the idempotency key, so a
        // retried report cannot duplicate rows.
        let observed = if report.observed_at > 0 { Some(report.observed_at) } else { None };
        crate::state::apply_wave_observed(
            &app.cfg.db_path,
            &report.node_id,
            &rows,
            Some(head_now),
            observed,
        )?;
    }
    Ok(count)
}

/// The HTTP entry point for POST /api/report.
pub fn handle_report(app: &App, req_headers: &std::collections::HashMap<String, String>, body: &[u8], declared_len: usize) -> (u16, Value) {
    let json_ct = ();
    let _ = json_ct;
    if declared_len > REPORT_MAX_BODY {
        return (413, json!({"ok": false, "error": "report too large"}));
    }

    let token = req_headers
        .get("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.trim())
        .unwrap_or("");

    let node = match node_for_token(&app.cfg.db_path, token) {
        Some(n) => n,
        None => return (401, json!({"ok": false, "error": "unknown or missing token"})),
    };

    let report = match parse(body) {
        Ok(r) => r,
        Err(e) => return (400, json!({"ok": false, "error": e})),
    };

    // A token belongs to one node. Otherwise node A could write as node B.
    if report.node_id != node {
        return (
            403,
            json!({"ok": false, "error": format!("token belongs to {node:?}, report claims {:?}", report.node_id)}),
        );
    }

    let head_now = db::now();
    if let Err(e) = touch_node_with_metrics(
        &app.cfg.db_path,
        &report.node_id,
        &report.node_name,
        report.interval_s,
        &token_digest(token),
        head_now,
        report.metrics.as_ref(),
    ) {
        return (500, json!({"ok": false, "error": format!("cannot register node: {e}")}));
    }

    match apply(app, &report, head_now) {
        Ok(n) => (
            200,
            json!({
                "ok": true,
                "node": report.node_id,
                "received_at": head_now,
                "observed_at": report.observed_at,
                "services": n,
            }),
        ),
        Err(e) => (500, json!({"ok": false, "error": format!("cannot apply report: {e}")})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(v: Value) -> Vec<u8> {
        serde_json::to_vec(&v).unwrap()
    }

    #[test]
    fn a_minimal_report_parses() {
        let r = parse(&body(json!({
            "node": {"id": "laptop", "name": "Laptop", "interval": 60},
            "observed_at": 100,
            "services": [{"id": "nginx", "status": "up", "detail": "HTTP 200"}]
        })))
        .unwrap();
        assert_eq!(r.node_id, "laptop");
        assert_eq!(r.interval_s, 60);
        assert_eq!(r.services.unwrap()[0].id, "nginx");
    }

    #[test]
    fn absent_services_is_an_error_but_empty_is_a_heartbeat() {
        let absent = body(json!({"node": {"id": "a"}, "observed_at": 1}));
        assert!(parse(&absent).is_err(), "absent services must be rejected");

        let empty = body(json!({"node": {"id": "a"}, "observed_at": 1, "services": []}));
        let r = parse(&empty).unwrap();
        assert_eq!(r.services.unwrap().len(), 0);
    }

    #[test]
    fn metrics_ride_the_report_and_are_stored() {
        // A head cannot read a tail's cpu or memory; it must keep what the tail measured.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("m.db");
        crate::db::init(&db).unwrap();
        let metrics = serde_json::json!({"cpu_pct": 12.5, "mem_used": "3.1/4 GB", "uptime_h": 42.0});
        touch_node_with_metrics(&db, "laptop", "Laptop", 30, "tok", 1_000, Some(&metrics)).unwrap();
        let conn = crate::db::connect(&db).unwrap();
        let stored: String = conn
            .query_row("SELECT metrics_json FROM nodes WHERE node_id='laptop'", [], |r| r.get(0))
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(parsed["cpu_pct"], 12.5);
        assert_eq!(parsed["mem_used"], "3.1/4 GB");
    }

    #[test]
    fn a_later_report_without_metrics_keeps_the_old_ones() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("keep.db");
        crate::db::init(&db).unwrap();
        let metrics = serde_json::json!({"cpu_pct": 5.0});
        touch_node_with_metrics(&db, "laptop", "Laptop", 30, "tok", 1_000, Some(&metrics)).unwrap();
        touch_node_with_metrics(&db, "laptop", "Laptop", 30, "tok", 1_060, None).unwrap();
        let conn = crate::db::connect(&db).unwrap();
        let stored: String = conn
            .query_row("SELECT metrics_json FROM nodes WHERE node_id='laptop'", [], |r| r.get(0))
            .unwrap();
        assert!(stored.contains("cpu_pct"), "a heartbeat must not wipe the last known metrics");
    }

    #[test]
    fn an_unknown_status_is_rejected() {
        let b = body(json!({
            "node": {"id": "a"}, "services": [{"id": "x", "status": "healthy"}]
        }));
        assert!(parse(&b).is_err());
    }

    #[test]
    fn duplicate_ids_in_one_report_are_rejected() {
        let b = body(json!({
            "node": {"id": "a"},
            "services": [{"id": "x", "status": "up"}, {"id": "x", "status": "down"}]
        }));
        assert!(parse(&b).is_err());
    }

    #[test]
    fn missing_node_id_is_rejected() {
        let b = body(json!({"node": {"name": "nameless"}, "services": []}));
        assert!(parse(&b).is_err());
    }

    #[test]
    fn token_digest_is_stable_and_not_the_token() {
        let d = token_digest("hunter2");
        assert_eq!(d, token_digest("hunter2"));
        assert_ne!(d, token_digest("hunter3"));
        assert_eq!(d.len(), 64);
        assert!(!d.contains("hunter2"));
    }

    #[test]
    fn constant_time_compare_still_compares_correctly() {
        assert!(digest_eq("abc", "abc"));
        assert!(!digest_eq("abc", "abd"));
        assert!(!digest_eq("abc", "ab"));
    }
}
