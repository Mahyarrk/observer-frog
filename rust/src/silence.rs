//! Silence detection: a node that stops reporting is not a healthy node.
//!
//! In pull mode the absence of a response was self-evident. In push mode it is not: a dead tail
//! sends nothing at all, so there is no report to fail on. This module turns "last_seen_ts is old"
//! into a state change.
//!
//! Two rules that matter more than the code:
//!
//!   1. Silence goes through `mark_node_silent`, never through `apply_wave`. `apply_wave` refuses
//!      an empty row set on purpose (invariant I3: an empty payload is a broken poll, not N
//!      simultaneous deaths). A silent node produces exactly that empty input, so routing it
//!      through `apply_wave` would be silently swallowed and the node would read healthy forever.
//!      That is the exact failure the old dashboard had, with sixteen cards frozen green.
//!
//!   2. Silence reads `unavailable`, never `down`. A laptop whose wifi dropped has not had
//!      seventeen services fail. Reporting `down` would turn the whole board red and page once per
//!      service. `unavailable` is already the state for "this machine cannot answer", and the
//!      uptime maths already excludes it, so a silent node cannot fake downtime either.

use crate::db;
use crate::state::{self, Change, Row};
use rusqlite::Connection;
use std::collections::HashSet;
use std::path::Path;

/// A node is silent after this many of its own intervals without a report.
pub const SILENCE_INTERVALS: i64 = 3;
/// Detail text for a swept service.
pub const SILENT_DETAIL: &str = "node not reporting";

pub struct SilentNode {
    pub node_id: String,
    pub name: String,
    pub interval_s: i64,
    pub last_seen_ts: i64,
}

/// Every registered node whose last report is older than `SILENCE_INTERVALS` × its own interval.
pub fn silent_nodes(path: &Path, now: i64, exclude: &[&str]) -> rusqlite::Result<Vec<SilentNode>> {
    let conn = db::connect(path)?;
    let mut stmt = conn.prepare(
        "SELECT node_id, name, interval_s, last_seen_ts FROM nodes WHERE last_seen_ts > 0",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(SilentNode {
            node_id: r.get(0)?,
            name: r.get(1)?,
            interval_s: r.get(2)?,
            last_seen_ts: r.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        let n = row?;
        // A node the head probes itself is never "silent": its liveness is the poll loop.
        if exclude.contains(&n.node_id.as_str()) {
            continue;
        }
        let interval = n.interval_s.max(1);
        if now - n.last_seen_ts > interval * SILENCE_INTERVALS {
            out.push(n);
        }
    }
    Ok(out)
}

/// Mark every service on a silent node `unavailable`.
///
/// Bypasses `apply_wave` deliberately (see the module note). Returns the transitions, so the
/// caller can notify once per episode rather than once per service.
pub fn mark_node_silent(
    path: &Path,
    node_id: &str,
    interval_s: i64,
    now: i64,
) -> rusqlite::Result<Vec<Change>> {
    let ignored: HashSet<String> = db::ignored_ids(path, node_id)?.into_iter().collect();
    let states = state::load_states(path, node_id)?;
    let mut changes = Vec::new();
    let conn: Connection = db::connect(path)?;

    for (sid, prev) in &states {
        // Already silent, or already known down: nothing to announce.
        if prev.state == "unavailable" || prev.state == "down" {
            continue;
        }
        let row = Row {
            service_id: sid.clone(),
            name: if prev.name.is_empty() { sid.clone() } else { prev.name.clone() },
            group: prev.group.clone(),
            status: "unavailable".into(),
            detail: SILENT_DETAIL.into(),
            latency_ms: 0,
            ram_mb: prev.ram_mb,
            ports: prev.ports.clone(),
            address: prev.address.clone(),
        };
        let mut next = prev.clone();
        next.state = "unavailable".into();
        next.since_ts = now;
        next.last_detail = SILENT_DETAIL.into();

        // Write state directly; no check row and no rollup, because no observation happened.
        // Recording `unavailable` as a sample would be inventing data.
        conn.execute(
            "INSERT INTO service_state (node_id, service_id, state, fails, since_ts, name, grp, last_detail) \
             VALUES (?,?,?,?,?,?,?,?) \
             ON CONFLICT(node_id, service_id) DO UPDATE SET \
             state=excluded.state, fails=excluded.fails, since_ts=excluded.since_ts, \
             name=excluded.name, grp=excluded.grp, last_detail=excluded.last_detail",
            (node_id, sid, &next.state, next.fails, next.since_ts, &next.name, &next.group, &next.last_detail),
        )?;

        changes.push(Change {
            node_id: node_id.to_string(),
            service_id: sid.clone(),
            name: row.name,
            group: row.group,
            from_state: prev.state.clone(),
            to_state: "unavailable".into(),
            detail: SILENT_DETAIL.into(),
            ts: now,
            ignored: ignored.contains(sid),
            first_sighting: false,
        });
    }

    // An open incident for a service that has gone silent must not stay open forever claiming
    // downtime that may never have happened. Close it with the silence detail.
    if !changes.is_empty() {
        conn.execute(
            "UPDATE incidents SET ended_ts = ?, last_detail = ? \
             WHERE node_id = ? AND ended_ts IS NULL",
            (now, SILENT_DETAIL, node_id),
        )?;
    }

    let _ = interval_s;
    Ok(changes)
}

/// One sweep over every known node. Returns the nodes that went silent in this pass.
pub fn sweep(path: &Path, now: i64, head_node: &str) -> rusqlite::Result<Vec<(String, Vec<Change>)>> {
    let mut out = Vec::new();
    for node in silent_nodes(path, now, &[head_node])? {
        let changes = mark_node_silent(path, &node.node_id, node.interval_s, now)?;
        if !changes.is_empty() {
            out.push((node.node_id, changes));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest;
    use crate::state::Row;

    fn setup() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("status.db");
        db::init(&p).unwrap();
        (dir, p)
    }

    fn row(id: &str, status: &str) -> Row {
        Row {
            service_id: id.into(),
            name: id.to_uppercase(),
            group: "G".into(),
            status: status.into(),
            detail: "d".into(),
            latency_ms: 1,
            ..Default::default()
        }
    }

    #[test]
    fn a_node_reporting_on_time_is_never_silent() {
        let (_d, p) = setup();
        ingest::touch_node(&p, "laptop", "Laptop", 30, "", 1_000).unwrap();
        // 60s later, with a 30s interval: two intervals, inside the three-interval grace.
        assert!(silent_nodes(&p, 1_060, &["local"]).unwrap().is_empty());
    }

    #[test]
    fn a_node_crossing_three_intervals_is_silent() {
        let (_d, p) = setup();
        ingest::touch_node(&p, "laptop", "Laptop", 30, "", 1_000).unwrap();
        let nodes = silent_nodes(&p, 1_000 + 91, &["local"]).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].node_id, "laptop");
    }

    #[test]
    fn the_head_never_reports_itself_silent() {
        let (_d, p) = setup();
        ingest::touch_node(&p, "local", "Head", 30, "", 1_000).unwrap();
        assert!(silent_nodes(&p, 99_999, &["local"]).unwrap().is_empty());
    }

    #[test]
    fn silence_reads_unavailable_never_down() {
        let (_d, p) = setup();
        state::apply_wave(&p, "laptop", &[row("nginx", "up")], Some(1_000)).unwrap();
        ingest::touch_node(&p, "laptop", "Laptop", 30, "", 1_000).unwrap();
        let changes = mark_node_silent(&p, "laptop", 30, 1_100).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].to_state, "unavailable");
        assert_ne!(changes[0].to_state, "down");
        let states = state::load_states(&p, "laptop").unwrap();
        assert_eq!(states["nginx"].state, "unavailable");
    }

    #[test]
    fn silence_writes_no_check_rows() {
        // No observation happened, so inventing one would corrupt the record.
        let (_d, p) = setup();
        state::apply_wave(&p, "laptop", &[row("nginx", "up")], Some(1_000)).unwrap();
        let before: i64 = {
            let c = db::connect(&p).unwrap();
            c.query_row("SELECT COUNT(*) FROM service_checks", [], |r| r.get(0)).unwrap()
        };
        mark_node_silent(&p, "laptop", 30, 1_100).unwrap();
        let after: i64 = {
            let c = db::connect(&p).unwrap();
            c.query_row("SELECT COUNT(*) FROM service_checks", [], |r| r.get(0)).unwrap()
        };
        assert_eq!(before, after, "silence must not fabricate a check row");
    }

    #[test]
    fn a_second_sweep_does_not_re_announce() {
        let (_d, p) = setup();
        state::apply_wave(&p, "laptop", &[row("nginx", "up")], Some(1_000)).unwrap();
        let first = mark_node_silent(&p, "laptop", 30, 1_100).unwrap();
        assert_eq!(first.len(), 1);
        let second = mark_node_silent(&p, "laptop", 30, 1_200).unwrap();
        assert!(second.is_empty(), "a silent node must not page again every sweep");
    }

    #[test]
    fn recovery_is_immediate_when_the_node_reports_again() {
        let (_d, p) = setup();
        state::apply_wave(&p, "laptop", &[row("nginx", "up")], Some(1_000)).unwrap();
        mark_node_silent(&p, "laptop", 30, 1_100).unwrap();
        // The node comes back with a real observation.
        let changes = state::apply_wave(&p, "laptop", &[row("nginx", "up")], Some(1_200)).unwrap();
        assert_eq!(changes.len(), 1, "recovery should produce one transition");
        assert_eq!(changes[0].from_state, "unavailable");
        assert_eq!(changes[0].to_state, "up");
        let states = state::load_states(&p, "laptop").unwrap();
        assert_eq!(states["nginx"].state, "up");
    }

    #[test]
    fn a_down_service_on_a_silent_node_is_left_alone() {
        // It was already down; the node going silent adds no information about it.
        let (_d, p) = setup();
        for n in 0..4 {
            state::apply_wave(&p, "laptop", &[row("nginx", "down")], Some(1_000 + n)).unwrap();
        }
        let states = state::load_states(&p, "laptop").unwrap();
        assert_eq!(states["nginx"].state, "down");
        let changes = mark_node_silent(&p, "laptop", 30, 1_200).unwrap();
        assert!(changes.is_empty());
        let after = state::load_states(&p, "laptop").unwrap();
        assert_eq!(after["nginx"].state, "down", "silence must not overwrite a known outage");
    }

    #[test]
    fn sweep_returns_only_nodes_that_changed() {
        let (_d, p) = setup();
        state::apply_wave(&p, "laptop", &[row("nginx", "up")], Some(1_000)).unwrap();
        ingest::touch_node(&p, "laptop", "Laptop", 30, "", 1_000).unwrap();
        let swept = sweep(&p, 1_200, "local").unwrap();
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].0, "laptop");
        // and a second pass is quiet
        assert!(sweep(&p, 1_300, "local").unwrap().is_empty());
    }
}
