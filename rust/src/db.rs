//! SQLite storage: schema v2, composite (node_id, service_id) keys, WAL.
//!
//! Every table carries node_id from the first commit so push nodes need no migration later.
//! The schema string here is byte-identical to statushub/db.py so an existing database opens
//! with no migration at all.

use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const SCHEMA_VERSION: i64 = 2;
pub const RAW_RETENTION_S: i64 = 86_400; // 24 h of per-check rows
pub const ROLLUP_RETENTION_S: i64 = 2_592_000; // 30 days of hourly counters

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (
    version    INTEGER NOT NULL,
    applied_ts INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS service_checks (
    node_id    TEXT    NOT NULL,
    service_id TEXT    NOT NULL,
    timestamp  INTEGER NOT NULL,
    status     TEXT    NOT NULL,
    detail     TEXT    NOT NULL,
    latency_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_checks_node_time ON service_checks (node_id, service_id, timestamp);

CREATE TABLE IF NOT EXISTS service_state (
    node_id     TEXT    NOT NULL,
    service_id  TEXT    NOT NULL,
    state       TEXT    NOT NULL DEFAULT 'up',
    fails       INTEGER NOT NULL DEFAULT 0,
    since_ts    INTEGER NOT NULL DEFAULT 0,
    name        TEXT    NOT NULL DEFAULT '',
    grp         TEXT    NOT NULL DEFAULT '',
    last_detail TEXT    NOT NULL DEFAULT '',
    PRIMARY KEY (node_id, service_id)
);

CREATE TABLE IF NOT EXISTS incidents (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    node_id     TEXT    NOT NULL,
    service_id  TEXT    NOT NULL,
    name        TEXT    NOT NULL,
    grp         TEXT    NOT NULL,
    started_ts  INTEGER NOT NULL,
    ended_ts    INTEGER,
    last_detail TEXT    NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_inc_started ON incidents (started_ts);
CREATE INDEX IF NOT EXISTS idx_inc_open ON incidents (node_id, service_id, ended_ts);

CREATE TABLE IF NOT EXISTS hourly_rollup (
    node_id    TEXT    NOT NULL,
    service_id TEXT    NOT NULL,
    hour_ts    INTEGER NOT NULL,
    up_n       INTEGER NOT NULL DEFAULT 0,
    deg_n      INTEGER NOT NULL DEFAULT 0,
    down_n     INTEGER NOT NULL DEFAULT 0,
    lat_sum    INTEGER NOT NULL DEFAULT 0,
    lat_n      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (node_id, service_id, hour_ts)
);

CREATE TABLE IF NOT EXISTS ignored (
    node_id    TEXT NOT NULL,
    service_id TEXT NOT NULL,
    PRIMARY KEY (node_id, service_id)
);
"#;

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// One short-lived connection per transaction, matching the Python `connect()` contract.
pub fn connect(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_URI,
    )?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    Ok(conn)
}

/// Create or upgrade the schema, returning the version now on disk.
pub fn init(path: &Path) -> rusqlite::Result<i64> {
    let conn = connect(path)?;
    conn.execute_batch(SCHEMA)?;
    let current: Option<i64> = conn
        .query_row("SELECT version FROM schema_version LIMIT 1", [], |r| r.get(0))
        .ok();
    match current {
        None => {
            conn.execute(
                "INSERT INTO schema_version (version, applied_ts) VALUES (?, ?)",
                (SCHEMA_VERSION, now()),
            )?;
            Ok(SCHEMA_VERSION)
        }
        Some(v) if v < SCHEMA_VERSION => {
            let mut version = v;
            if version < 2 {
                // v2 dropped `intentional` in favour of `ignored`. Carry the operator's intent
                // across rather than silently un-ignoring services they marked as meant-to-be-off.
                let has: bool = conn
                    .query_row(
                        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='intentional'",
                        [],
                        |_| Ok(true),
                    )
                    .unwrap_or(false);
                if has {
                    conn.execute(
                        "INSERT OR IGNORE INTO ignored (node_id, service_id) SELECT node_id, service_id FROM intentional",
                        [],
                    )?;
                    conn.execute("DROP TABLE intentional", [])?;
                }
                version = 2;
            }
            conn.execute(
                "UPDATE schema_version SET version = ?, applied_ts = ?",
                (version, now()),
            )?;
            Ok(version)
        }
        Some(v) => Ok(v),
    }
}

pub fn version(path: &Path) -> rusqlite::Result<i64> {
    let conn = connect(path)?;
    let v: Option<i64> = conn
        .query_row("SELECT version FROM schema_version LIMIT 1", [], |r| r.get(0))
        .ok();
    Ok(v.unwrap_or(0))
}

/// Drop expired raw checks and rollups; returns (checks_deleted, rollups_deleted).
pub fn prune(path: &Path, at: Option<i64>) -> rusqlite::Result<(usize, usize)> {
    let at = at.unwrap_or_else(now);
    let conn = connect(path)?;
    let checks = conn.execute("DELETE FROM service_checks WHERE timestamp < ?", (at - RAW_RETENTION_S,))?;
    let rollups = conn.execute("DELETE FROM hourly_rollup WHERE hour_ts < ?", (at - ROLLUP_RETENTION_S,))?;
    Ok((checks, rollups))
}

/// Read the ignored set at serve time — never baked into a snapshot (invariant I6).
pub fn ignored_ids(path: &Path, node_id: &str) -> rusqlite::Result<Vec<String>> {
    let conn = connect(path)?;
    let mut stmt =
        conn.prepare("SELECT service_id FROM ignored WHERE node_id = ? ORDER BY service_id")?;
    let rows = stmt.query_map((node_id,), |r| r.get::<_, String>(0))?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_is_idempotent_and_reports_v2() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("status.db");
        assert_eq!(init(&db).unwrap(), 2);
        assert_eq!(init(&db).unwrap(), 2);
        assert_eq!(version(&db).unwrap(), 2);
    }

    #[test]
    fn v1_intentional_rows_move_into_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("old.db");
        {
            let conn = connect(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_version (version INTEGER NOT NULL, applied_ts INTEGER NOT NULL);
                 INSERT INTO schema_version VALUES (1, 0);
                 CREATE TABLE intentional (node_id TEXT NOT NULL, service_id TEXT NOT NULL,
                                           PRIMARY KEY (node_id, service_id));
                 INSERT INTO intentional VALUES ('local','docker'),('local','lmstudio');",
            )
            .unwrap();
        }
        assert_eq!(init(&db).unwrap(), 2);
        let ignored = ignored_ids(&db, "local").unwrap();
        assert_eq!(ignored, vec!["docker".to_string(), "lmstudio".to_string()]);
        let conn = connect(&db).unwrap();
        let still: bool = conn
            .query_row("SELECT 1 FROM sqlite_master WHERE name='intentional'", [], |_| Ok(true))
            .unwrap_or(false);
        assert!(!still, "intentional table survived the migration");
    }

    #[test]
    fn ignored_ids_are_ordered_and_node_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("status.db");
        init(&db).unwrap();
        let conn = connect(&db).unwrap();
        conn.execute("INSERT INTO ignored VALUES ('local','zebra')", []).unwrap();
        conn.execute("INSERT INTO ignored VALUES ('local','alpha')", []).unwrap();
        conn.execute("INSERT INTO ignored VALUES ('remote','beta')", []).unwrap();
        assert_eq!(ignored_ids(&db, "local").unwrap(), vec!["alpha".to_string(), "zebra".to_string()]);
        assert_eq!(ignored_ids(&db, "remote").unwrap(), vec!["beta".to_string()]);
    }

    #[test]
    fn prune_drops_only_expired_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("status.db");
        init(&db).unwrap();
        let t = 1_800_000_000i64;
        {
            let conn = connect(&db).unwrap();
            conn.execute(
                "INSERT INTO service_checks VALUES ('local','a',?, 'up','',0)",
                (t - RAW_RETENTION_S - 10,),
            )
            .unwrap();
            conn.execute("INSERT INTO service_checks VALUES ('local','a',?, 'up','',0)", (t,)).unwrap();
        }
        let (checks, _) = prune(&db, Some(t)).unwrap();
        assert_eq!(checks, 1);
        let conn = connect(&db).unwrap();
        let left: i64 = conn.query_row("SELECT COUNT(*) FROM service_checks", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 1);
    }
}
