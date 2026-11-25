//! The state machine: transitions, incidents, rollups, notifications.
//!
//! Every rule here exists because it already went wrong once (see docs/DESIGN.md):
//!   I1  down only after DOWN_CONFIRM_POLLS consecutive failures; recovery is immediate
//!   I2  a service absent from an authoritative payload is "no longer reporting" after 4 absences
//!   I3  an empty payload is a broken poll, not N simultaneous deaths — absence pass skipped
//!   I4  first sighting seeds state silently
//!   I5  ignored services still alert on transitions but are excluded from uptime/MTTR
//!   I6  flags are read at serve time, never baked in here

use crate::db;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub const DOWN_CONFIRM_POLLS: i64 = 4;
pub const ABSENCE_CONFIRM_POLLS: i64 = 4;
pub const VANISHED_DETAIL: &str = "no longer reporting";

/// States that mean "the thing is answering". `unavailable` is neither healthy nor an outage.
pub fn is_healthy(state: &str) -> bool {
    state == "up" || state == "degraded"
}

pub fn is_neutral(state: &str) -> bool {
    state == "unavailable"
}

#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub node_id: String,
    pub service_id: String,
    pub name: String,
    pub group: String,
    pub from_state: String,
    pub to_state: String,
    pub detail: String,
    pub ts: i64,
    pub ignored: bool,
    pub first_sighting: bool,
}

impl Change {
    pub fn is_recovery(&self) -> bool {
        self.from_state == "down" && self.to_state != "down"
    }

    pub fn is_outage(&self) -> bool {
        self.to_state == "down" && self.from_state != "down" && self.from_state != "new"
    }
}

/// One probe result entering the machine.
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub service_id: String,
    pub name: String,
    pub group: String,
    pub status: String,
    pub detail: String,
    pub latency_ms: i64,
    pub ports: String,
    pub address: String,
}

#[derive(Debug, Clone, Default)]
pub struct StoredState {
    pub state: String,
    pub fails: i64,
    pub since_ts: i64,
    pub name: String,
    pub group: String,
    pub last_detail: String,
    pub ports: String,
    pub address: String,
}

pub fn load_states(path: &Path, node_id: &str) -> rusqlite::Result<HashMap<String, StoredState>> {
    let conn = db::connect(path)?;
    let mut stmt = conn.prepare(
        "SELECT service_id, state, fails, since_ts, name, grp, last_detail, ports, addr FROM service_state WHERE node_id = ?",
    )?;
    let mut out = HashMap::new();
    let rows = stmt.query_map((node_id,), |r| {
        Ok((
            r.get::<_, String>(0)?,
            StoredState {
                state: r.get(1)?,
                fails: r.get(2)?,
                since_ts: r.get(3)?,
                name: r.get(4)?,
                group: r.get(5)?,
                last_detail: r.get(6)?,
                ports: r.get(7)?,
                address: r.get(8)?,
            },
        ))
    })?;
    for row in rows {
        let (sid, st) = row?;
        out.insert(sid, st);
    }
    Ok(out)
}

fn save_state(conn: &Connection, node_id: &str, service_id: &str, st: &StoredState) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO service_state (node_id, service_id, state, fails, since_ts, name, grp, last_detail, ports, addr) \
         VALUES (?,?,?,?,?,?,?,?,?,?) \
         ON CONFLICT(node_id, service_id) DO UPDATE SET \
         state=excluded.state, fails=excluded.fails, since_ts=excluded.since_ts, \
         name=excluded.name, grp=excluded.grp, last_detail=excluded.last_detail, \
         ports=excluded.ports, addr=excluded.addr",
        (node_id, service_id, &st.state, st.fails, st.since_ts, &st.name, &st.group, &st.last_detail, &st.ports, &st.address),
    )?;
    Ok(())
}

fn open_incident(conn: &Connection, node_id: &str, row: &Row, ts: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO incidents (node_id, service_id, name, grp, started_ts, ended_ts, last_detail) \
         VALUES (?,?,?,?,?,NULL,?)",
        (node_id, &row.service_id, &row.name, &row.group, ts, &row.detail),
    )?;
    Ok(())
}

fn close_incident(conn: &Connection, node_id: &str, service_id: &str, ts: i64, detail: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE incidents SET ended_ts = ?, last_detail = ? \
         WHERE node_id = ? AND service_id = ? AND ended_ts IS NULL",
        (ts, detail, node_id, service_id),
    )?;
    Ok(())
}

/// Record one check.
///
/// `ts` is the head's arrival stamp (what buckets and incidents use); `observed_at` is the tail's
/// own clock and is the idempotency key. A retried report carries the same `observed_at`, so the
/// unique index turns the retry into a no-op; two genuinely different observations a second apart
/// carry different values and both land. With the arrival stamp as the key, every retry would
/// duplicate.
fn record_check(
    conn: &Connection,
    node_id: &str,
    row: &Row,
    ts: i64,
    observed_at: i64,
) -> rusqlite::Result<bool> {
    let changed = conn.execute(
        "INSERT OR IGNORE INTO service_checks \
           (node_id, service_id, timestamp, status, detail, latency_ms, observed_at) \
         VALUES (?,?,?,?,?,?,?)",
        (node_id, &row.service_id, ts, &row.status, &row.detail, row.latency_ms, observed_at),
    )?;
    Ok(changed > 0)
}

fn record_rollup(conn: &Connection, node_id: &str, row: &Row, ts: i64) -> rusqlite::Result<()> {
    let hour = ts - (ts % 3600);
    let bucket = match row.status.as_str() {
        "up" => "up_n",
        "degraded" => "deg_n",
        "down" => "down_n",
        // `unavailable` is not a health sample; counting it would fake downtime.
        _ => return Ok(()),
    };
    let (up, deg, down) = match bucket {
        "up_n" => (1, 0, 0),
        "deg_n" => (0, 1, 0),
        _ => (0, 0, 1),
    };
    let sql = format!(
        "INSERT INTO hourly_rollup (node_id, service_id, hour_ts, up_n, deg_n, down_n, lat_sum, lat_n) \
         VALUES (?,?,?,?,?,?,?,1) \
         ON CONFLICT(node_id, service_id, hour_ts) DO UPDATE SET \
         {bucket} = {bucket} + 1, lat_sum = lat_sum + excluded.lat_sum, lat_n = lat_n + 1"
    );
    conn.execute(
        &sql,
        (node_id, &row.service_id, hour, up, deg, down, row.latency_ms.max(0)),
    )?;
    Ok(())
}

/// I4 — a first sighting seeds silently, whatever state it arrives in.
fn seed_new(conn: &Connection, node_id: &str, row: &Row, ts: i64, observed_at: i64, ignored: &HashSet<String>) -> rusqlite::Result<Change> {
    let st = StoredState {
        state: row.status.clone(),
        fails: 0,
        since_ts: ts,
        name: row.name.clone(),
        group: row.group.clone(),
        last_detail: row.detail.clone(),
        ports: row.ports.clone(),
        address: row.address.clone(),
    };
    save_state(conn, node_id, &row.service_id, &st)?;
    let fresh = record_check(conn, node_id, row, ts, observed_at)?;
    if fresh {
        record_rollup(conn, node_id, row, ts)?;
    }
    if row.status == "down" {
        open_incident(conn, node_id, row, ts)?;
    }
    Ok(Change {
        node_id: node_id.to_string(),
        service_id: row.service_id.clone(),
        name: row.name.clone(),
        group: row.group.clone(),
        from_state: "new".into(),
        to_state: row.status.clone(),
        detail: row.detail.clone(),
        ts,
        ignored: ignored.contains(&row.service_id),
        first_sighting: true,
    })
}

/// Apply one observation to an existing service. Returns a Change only on a real transition.
fn advance(
    conn: &Connection,
    node_id: &str,
    row: &Row,
    prev: &mut StoredState,
    ts: i64,
    observed_at: i64,
    ignored: &HashSet<String>,
) -> rusqlite::Result<Option<Change>> {
    prev.name = row.name.clone();
    prev.group = row.group.clone();
    prev.last_detail = row.detail.clone();
    // A duplicate observation (a retried report) must not advance the state machine or the
    // rollup either, or the retry would inflate uptime while the raw table looked correct.
    if !record_check(conn, node_id, row, ts, observed_at)? {
        return Ok(None);
    }
    record_rollup(conn, node_id, row, ts)?;
    let flagged = ignored.contains(&row.service_id);

    if row.status != "down" {
        prev.fails = 0;
        if prev.state == "down" {
            // I1 — recovery is immediate
            prev.state = row.status.clone();
            prev.since_ts = ts;
            close_incident(conn, node_id, &row.service_id, ts, &row.detail)?;
            save_state(conn, node_id, &row.service_id, prev)?;
            return Ok(Some(Change {
                node_id: node_id.to_string(),
                service_id: row.service_id.clone(),
                name: row.name.clone(),
                group: row.group.clone(),
                from_state: "down".into(),
                to_state: row.status.clone(),
                detail: row.detail.clone(),
                ts,
                ignored: flagged,
                first_sighting: false,
            }));
        }
        if is_neutral(&prev.state) && is_healthy(&row.status) {
            // A node that was silent and is reporting again. This is worth one message: the
            // operator was told it went quiet, so they need to be told it came back.
            prev.state = row.status.clone();
            prev.since_ts = ts;
            save_state(conn, node_id, &row.service_id, prev)?;
            return Ok(Some(Change {
                node_id: node_id.to_string(),
                service_id: row.service_id.clone(),
                name: row.name.clone(),
                group: row.group.clone(),
                from_state: "unavailable".into(),
                to_state: row.status.clone(),
                detail: row.detail.clone(),
                ts,
                ignored: flagged,
                first_sighting: false,
            }));
        }
        save_state(conn, node_id, &row.service_id, prev)?;
        return Ok(None);
    }

    prev.fails += 1;
    if prev.state != "down" && prev.fails >= DOWN_CONFIRM_POLLS {
        // I1 — confirm before down
        prev.state = "down".into();
        prev.since_ts = ts;
        open_incident(conn, node_id, row, ts)?;
        save_state(conn, node_id, &row.service_id, prev)?;
        return Ok(Some(Change {
            node_id: node_id.to_string(),
            service_id: row.service_id.clone(),
            name: row.name.clone(),
            group: row.group.clone(),
            from_state: "up".into(),
            to_state: "down".into(),
            detail: row.detail.clone(),
            ts,
            ignored: flagged,
            first_sighting: false,
        }));
    }
    save_state(conn, node_id, &row.service_id, prev)?;
    Ok(None)
}

/// I2 — vanished is not healthy. Reaching this point guarantees a non-empty payload (I3).
fn sweep_absent(
    conn: &Connection,
    node_id: &str,
    states: &HashMap<String, StoredState>,
    seen: &HashSet<String>,
    ts: i64,
    ignored: &HashSet<String>,
) -> rusqlite::Result<Vec<Change>> {
    let mut changes = Vec::new();
    for (sid, prev) in states {
        if seen.contains(sid) || prev.state == "down" {
            continue;
        }
        let ghost = Row {
            service_id: sid.clone(),
            name: if prev.name.is_empty() { sid.clone() } else { prev.name.clone() },
            group: prev.group.clone(),
            status: "down".into(),
            detail: VANISHED_DETAIL.into(),
            latency_ms: 0,
            ports: prev.ports.clone(),
            address: prev.address.clone(),
        };
        let mut ghost_prev = StoredState {
            state: prev.state.clone(),
            fails: prev.fails + 1,
            since_ts: prev.since_ts,
            name: prev.name.clone(),
            group: prev.group.clone(),
            last_detail: VANISHED_DETAIL.into(),
            ports: prev.ports.clone(),
            address: prev.address.clone(),
        };
        if ghost_prev.fails >= DOWN_CONFIRM_POLLS.min(ABSENCE_CONFIRM_POLLS) {
            ghost_prev.state = "down".into();
            ghost_prev.since_ts = ts;
            open_incident(conn, node_id, &ghost, ts)?;
            changes.push(Change {
                node_id: node_id.to_string(),
                service_id: sid.clone(),
                name: ghost.name.clone(),
                group: ghost.group.clone(),
                from_state: prev.state.clone(),
                to_state: "down".into(),
                detail: VANISHED_DETAIL.into(),
                ts,
                ignored: ignored.contains(sid),
                first_sighting: false,
            });
        }
        save_state(conn, node_id, sid, &ghost_prev)?;
    }
    Ok(changes)
}

/// Feed one poll wave through the machine. Returns the transitions worth notifying about.
///
/// An empty `rows` is a broken poll (I3): nothing is marked down and nothing is persisted.
pub fn apply_wave(path: &Path, node_id: &str, rows: &[Row], ts: Option<i64>) -> rusqlite::Result<Vec<Change>> {
    apply_wave_observed(path, node_id, rows, ts, None)
}

/// As `apply_wave`, but with the tail's own observation stamp used as the idempotency key.
pub fn apply_wave_observed(
    path: &Path,
    node_id: &str,
    rows: &[Row],
    ts: Option<i64>,
    observed_at: Option<i64>,
) -> rusqlite::Result<Vec<Change>> {
    let ts = ts.unwrap_or_else(db::now);
    // A local head observes at the moment it records, so absent a tail stamp the two coincide.
    let observed_at = observed_at.unwrap_or(ts);

    if rows.is_empty() {
        log_empty_payload();
        return Ok(vec![]);
    }

    let ignored: HashSet<String> = db::ignored_ids(path, node_id)?.into_iter().collect();
    let states = load_states(path, node_id)?;
    let mut changes = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let conn = db::connect(path)?;
    let mut states = states;
    for row in rows {
        seen.insert(row.service_id.clone());
        match states.get_mut(&row.service_id) {
            None => changes.push(seed_new(&conn, node_id, row, ts, observed_at, &ignored)?),
            Some(prev) => {
                if let Some(c) = advance(&conn, node_id, row, prev, ts, observed_at, &ignored)? {
                    changes.push(c);
                }
            }
        }
    }
    changes.extend(sweep_absent(&conn, node_id, &states, &seen, ts, &ignored)?);
    Ok(changes)
}

fn log_empty_payload() {
    // Kept as a function so the invariant is greppable in one place.
    eprintln!("empty payload: skipping wave entirely (invariant I3)");
}

pub struct UptimeRow {
    pub service_id: String,
    pub name: String,
    pub group: String,
    pub uptime_pct: f64,
    pub checks: i64,
    pub down_checks: i64,
}

/// Per-service uptime rows for the History tab, worst first. Ignored services excluded (I5).
pub fn uptime_rows(path: &Path, node_id: &str, days: i64) -> rusqlite::Result<Vec<UptimeRow>> {
    let since = db::now() - days * 86400;
    let skip: HashSet<String> = db::ignored_ids(path, node_id)?.into_iter().collect();
    let conn = db::connect(path)?;
    let mut stmt = conn.prepare(
        "SELECT r.service_id, SUM(r.up_n), SUM(r.deg_n), SUM(r.down_n), \
           (SELECT s.name FROM service_state s WHERE s.node_id = r.node_id AND s.service_id = r.service_id), \
           (SELECT s.grp  FROM service_state s WHERE s.node_id = r.node_id AND s.service_id = r.service_id) \
         FROM hourly_rollup r WHERE r.node_id = ? AND r.hour_ts >= ? GROUP BY r.service_id",
    )?;
    let rows = stmt.query_map((node_id, since), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<i64>>(1)?.unwrap_or(0),
            r.get::<_, Option<i64>>(2)?.unwrap_or(0),
            r.get::<_, Option<i64>>(3)?.unwrap_or(0),
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
        ))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (sid, up_n, deg_n, down_n, name, grp) = row?;
        if skip.contains(&sid) {
            continue;
        }
        let total = up_n + deg_n + down_n;
        if total == 0 {
            continue;
        }
        let pct = 100.0 * (up_n as f64 + 0.5 * deg_n as f64) / total as f64;
        out.push(UptimeRow {
            name: name.unwrap_or_else(|| sid.clone()),
            group: grp.unwrap_or_default(),
            uptime_pct: (pct * 100.0).round() / 100.0,
            checks: total,
            down_checks: down_n,
            service_id: sid,
        });
    }
    out.sort_by(|a, b| {
        a.uptime_pct
            .partial_cmp(&b.uptime_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

pub struct Incident {
    pub id: i64,
    pub service_id: String,
    pub name: String,
    pub group: String,
    pub started_ts: i64,
    pub ended_ts: Option<i64>,
    pub duration_s: i64,
    pub detail: String,
    pub ignored: bool,
    pub ongoing: bool,
}

/// Incident log for the History tab, with both flags resolved at read time (I6).
pub fn recent_incidents(path: &Path, node_id: &str, days: i64, limit: i64) -> rusqlite::Result<Vec<Incident>> {
    let since = db::now() - days * 86400;
    let ignored: HashSet<String> = db::ignored_ids(path, node_id)?.into_iter().collect();
    let conn = db::connect(path)?;
    let mut stmt = conn.prepare(
        "SELECT id, service_id, name, grp, started_ts, ended_ts, last_detail FROM incidents \
         WHERE node_id = ? AND (ended_ts IS NULL OR ended_ts >= ?) \
         ORDER BY started_ts DESC LIMIT ?",
    )?;
    let rows = stmt.query_map((node_id, since, limit), |r| {
        Ok(Incident {
            id: r.get(0)?,
            service_id: r.get(1)?,
            name: r.get(2)?,
            group: r.get(3)?,
            started_ts: r.get(4)?,
            ended_ts: r.get(5)?,
            duration_s: 0,
            detail: r.get(6)?,
            ignored: false,
            ongoing: false,
        })
    })?;
    let now = db::now();
    let mut out = Vec::new();
    for row in rows {
        let mut inc = row?;
        inc.ongoing = inc.ended_ts.is_none();
        inc.duration_s = inc.ended_ts.unwrap_or(now) - inc.started_ts;
        inc.ignored = ignored.contains(&inc.service_id);
        out.push(inc);
    }
    Ok(out)
}

/// Mean seconds to recover per service, ignored services excluded (I5).
pub fn mttr(path: &Path, node_id: &str, days: i64) -> rusqlite::Result<HashMap<String, f64>> {
    let since = db::now() - days * 86400;
    let skip: HashSet<String> = db::ignored_ids(path, node_id)?.into_iter().collect();
    let conn = db::connect(path)?;
    let mut stmt = conn.prepare(
        "SELECT service_id, AVG(ended_ts - started_ts) FROM incidents \
         WHERE node_id = ? AND started_ts >= ? AND ended_ts IS NOT NULL GROUP BY service_id",
    )?;
    let rows = stmt.query_map((node_id, since), |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))?;
    let mut out = HashMap::new();
    for row in rows {
        let (sid, avg) = row?;
        if !skip.contains(&sid) {
            out.insert(sid, (avg * 10.0).round() / 10.0);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn row(id: &str, status: &str) -> Row {
        Row {
            service_id: id.into(),
            name: id.to_uppercase(),
            group: "G".into(),
            status: status.into(),
            detail: format!("detail {status}"),
            latency_ms: 5,
            ..Default::default()
        }
    }

    fn setup() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("status.db");
        db::init(&p).unwrap();
        (dir, p)
    }

    #[test]
    fn i1_down_needs_four_consecutive_failures() {
        let (_d, p) = setup();
        apply_wave(&p, "local", &[row("a", "up")], Some(100)).unwrap();
        for n in 0..3 {
            let ch = apply_wave(&p, "local", &[row("a", "down")], Some(200 + n)).unwrap();
            assert!(ch.is_empty(), "fired down too early at failure {}", n + 1);
        }
        let ch = apply_wave(&p, "local", &[row("a", "down")], Some(300)).unwrap();
        assert_eq!(ch.len(), 1);
        assert_eq!(ch[0].to_state, "down");
        assert!(ch[0].is_outage());
    }

    #[test]
    fn i1_recovery_is_immediate() {
        let (_d, p) = setup();
        apply_wave(&p, "local", &[row("a", "up")], Some(100)).unwrap();
        for n in 0..4 {
            apply_wave(&p, "local", &[row("a", "down")], Some(200 + n)).unwrap();
        }
        let ch = apply_wave(&p, "local", &[row("a", "up")], Some(400)).unwrap();
        assert_eq!(ch.len(), 1);
        assert!(ch[0].is_recovery());
    }

    #[test]
    fn i3_empty_payload_writes_nothing() {
        let (_d, p) = setup();
        apply_wave(&p, "local", &[row("a", "up")], Some(100)).unwrap();
        let before = count(&p, "service_checks");
        for _ in 0..10 {
            let ch = apply_wave(&p, "local", &[], Some(200)).unwrap();
            assert!(ch.is_empty());
        }
        assert_eq!(count(&p, "service_checks"), before, "empty payload wrote rows");
        // and the service is still up, not swept to down
        let states = load_states(&p, "local").unwrap();
        assert_eq!(states["a"].state, "up");
    }

    #[test]
    fn i4_first_sighting_is_silent_and_does_not_page() {
        let (_d, p) = setup();
        let ch = apply_wave(&p, "local", &[row("a", "down")], Some(100)).unwrap();
        assert_eq!(ch.len(), 1);
        assert!(ch[0].first_sighting);
        assert!(!ch[0].is_outage(), "a first sighting must not read as an outage");
    }

    #[test]
    fn i2_absent_service_goes_down_after_four_absences() {
        let (_d, p) = setup();
        apply_wave(&p, "local", &[row("a", "up"), row("b", "up")], Some(100)).unwrap();
        for n in 0..3 {
            let ch = apply_wave(&p, "local", &[row("a", "up")], Some(200 + n)).unwrap();
            assert!(ch.is_empty(), "swept too early at absence {}", n + 1);
        }
        let ch = apply_wave(&p, "local", &[row("a", "up")], Some(300)).unwrap();
        assert_eq!(ch.len(), 1, "expected b to be swept");
        assert_eq!(ch[0].service_id, "b");
        assert_eq!(ch[0].detail, VANISHED_DETAIL);
    }

    #[test]
    fn i5_ignored_service_alerts_but_is_out_of_uptime() {
        let (_d, p) = setup();
        {
            let conn = db::connect(&p).unwrap();
            conn.execute("INSERT INTO ignored VALUES ('local','b')", []).unwrap();
        }
        let base = crate::db::now() - 7200;
        apply_wave(&p, "local", &[row("a", "up"), row("b", "up")], Some(base)).unwrap();
        for n in 0..4 {
            apply_wave(&p, "local", &[row("a", "up"), row("b", "down")], Some(base + 100 + n)).unwrap();
        }
        let rows = uptime_rows(&p, "local", 30).unwrap();
        assert_eq!(rows.len(), 1, "ignored service leaked into uptime");
        assert_eq!(rows[0].service_id, "a");
    }

    #[test]
    fn unavailable_is_not_counted_as_downtime() {
        let (_d, p) = setup();
        let base = crate::db::now() - 7200;
        apply_wave(&p, "local", &[row("a", "up")], Some(base)).unwrap();
        apply_wave(&p, "local", &[row("a", "unavailable")], Some(base + 100)).unwrap();
        let rows = uptime_rows(&p, "local", 30).unwrap();
        // one up sample counted, the unavailable one skipped entirely
        assert_eq!(rows[0].checks, 1);
        assert_eq!(rows[0].uptime_pct, 100.0);
    }

    #[test]
    fn rollup_is_hourly_and_upserted() {
        let (_d, p) = setup();
        // Inside the retention window: uptime_rows filters on hour_ts >= now - 30d.
        let base = crate::db::now() - 7200;
        apply_wave(&p, "local", &[row("a", "up")], Some(base)).unwrap();
        apply_wave(&p, "local", &[row("a", "up")], Some(base + 100)).unwrap();
        let rows = uptime_rows(&p, "local", 9_999).unwrap();
        assert_eq!(rows[0].checks, 2, "two samples in one hour must aggregate to 2");
    }

    #[test]
    fn degraded_counts_as_half() {
        let (_d, p) = setup();
        let base = crate::db::now() - 7200;
        apply_wave(&p, "local", &[row("a", "up")], Some(base)).unwrap();
        apply_wave(&p, "local", &[row("a", "degraded")], Some(base + 1)).unwrap();
        let rows = uptime_rows(&p, "local", 9_999).unwrap();
        assert_eq!(rows[0].checks, 2);
        assert_eq!(rows[0].uptime_pct, 75.0);
    }

    #[test]
    fn incidents_open_once_and_close_on_recovery() {
        let (_d, p) = setup();
        let base = crate::db::now() - 7200;
        apply_wave(&p, "local", &[row("a", "up")], Some(base)).unwrap();
        for n in 0..6 {
            apply_wave(&p, "local", &[row("a", "down")], Some(base + 100 + n)).unwrap();
        }
        let open = recent_incidents(&p, "local", 30, 100).unwrap();
        assert_eq!(open.len(), 1, "duplicate incidents were opened");
        assert!(open[0].ongoing);
        apply_wave(&p, "local", &[row("a", "up")], Some(base + 300)).unwrap();
        let closed = recent_incidents(&p, "local", 30, 100).unwrap();
        assert!(!closed[0].ongoing);
        assert_eq!(closed[0].ended_ts, Some(base + 300));
    }

    fn count(p: &std::path::Path, table: &str) -> i64 {
        let conn = db::connect(p).unwrap();
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }
}
