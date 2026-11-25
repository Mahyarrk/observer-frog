//! SQLite storage: schema v2, composite (node_id, service_id) keys, WAL.
//!
//! Every table carries node_id from the first commit so push nodes need no migration later.
//! The schema string here is byte-identical to statushub/db.py so an existing database opens
//! with no migration at all.

use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const SCHEMA_VERSION: i64 = 9;
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
    timestamp  INTEGER NOT NULL,          -- head arrival: buckets and incidents use this
    status     TEXT    NOT NULL,
    detail     TEXT    NOT NULL,
    latency_ms INTEGER NOT NULL,
    observed_at INTEGER NOT NULL DEFAULT 0 -- tail's own clock: the idempotency key
);
CREATE INDEX IF NOT EXISTS idx_checks_node_time ON service_checks (node_id, service_id, timestamp);

CREATE TABLE IF NOT EXISTS service_state (
    ports TEXT NOT NULL DEFAULT '',
    addr  TEXT NOT NULL DEFAULT '', 
    node_id     TEXT    NOT NULL,
    service_id  TEXT    NOT NULL,
    state       TEXT    NOT NULL DEFAULT 'up',
    fails       INTEGER NOT NULL DEFAULT 0,
    since_ts    INTEGER NOT NULL DEFAULT 0,
    name        TEXT    NOT NULL DEFAULT '',
    grp         TEXT    NOT NULL DEFAULT '',
    last_detail TEXT    NOT NULL DEFAULT '',
    ram_mb      REAL    NOT NULL DEFAULT 0,
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

-- A tail asks to join; the head approves. Storing the request separately from `nodes` means an
-- unapproved machine is not a known node, so it cannot report until someone says yes.
CREATE TABLE IF NOT EXISTS pair_requests (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    node_id       TEXT    NOT NULL,
    name          TEXT    NOT NULL DEFAULT '',
    token_sha256  TEXT    NOT NULL DEFAULT '',
    address       TEXT    NOT NULL DEFAULT '',
    interval_s    INTEGER NOT NULL DEFAULT 30,
    requested_ts  INTEGER NOT NULL DEFAULT 0,
    status        TEXT    NOT NULL DEFAULT 'pending'
);

CREATE TABLE IF NOT EXISTS ignored (
    node_id    TEXT NOT NULL,
    service_id TEXT NOT NULL,
    PRIMARY KEY (node_id, service_id)
);

-- v3: a tail registers itself, so the head can name it, know how often it reports, and decide
-- when its silence means trouble. Without this there is nowhere to keep a node's interval.
CREATE TABLE IF NOT EXISTS nodes (
    node_id       TEXT    PRIMARY KEY,  -- every node this process has heard from
    name          TEXT    NOT NULL DEFAULT '',
    interval_s    INTEGER NOT NULL DEFAULT 30,
    token_sha256  TEXT    NOT NULL DEFAULT '',
    first_seen_ts INTEGER NOT NULL DEFAULT 0,
    last_seen_ts  INTEGER NOT NULL DEFAULT 0,
    reported_name TEXT    NOT NULL DEFAULT '',
    display_name  TEXT    NOT NULL DEFAULT '',
    -- The device card the node last reported: cpu, memory, disk, uptime, thermal. Stored as
    -- JSON because the shape belongs to the probe side and the head only renders it.
    metrics_json  TEXT    NOT NULL DEFAULT ''
);
"#;

/// Forget every node except this machine.
///
/// Forget every other machine completely, enrolment included.
///
/// A node only aggregates others while it acts as head. Once it stops, everything it knew about
/// them is dead weight: the phone's node picker would still offer machines this box no longer
/// tracks, and their services would sit in the dashboard as permanently unavailable. Called
/// automatically when the role switches away from head, so nothing is ever cleared by hand.
///
/// The enrolment goes too, deliberately. A machine that later wants back in must `enrol` again,
/// which is the point: dropping the roster is a real unpairing, not a pause. Keeping token digests
/// for machines this node no longer watches would leave standing credentials for absent hosts.
pub fn forget_other_nodes(db_path: &Path, keep_node_id: &str) -> Result<usize, String> {
    let conn = connect(db_path).map_err(|e| e.to_string())?;
    let mut removed = 0;
    for sql in [
        "DELETE FROM service_state WHERE node_id <> ?1",
        "DELETE FROM service_checks WHERE node_id <> ?1",
        "DELETE FROM hourly_rollup WHERE node_id <> ?1",
        "DELETE FROM incidents WHERE node_id <> ?1",
        "DELETE FROM ignored WHERE node_id <> ?1",
        "DELETE FROM nodes WHERE node_id <> ?1",
    ] {
        if let Ok(mut stmt) = conn.prepare(sql) {
            if let Ok(n) = stmt.execute([keep_node_id]) {
                removed += n;
            }
        }
    }
    Ok(removed)
}

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
            conn.execute(
                "CREATE UNIQUE INDEX IF NOT EXISTS ux_checks ON service_checks (node_id, service_id, observed_at)",
                [],
            )?;
            // One token, one machine. Empty means "not enrolled yet" and is exempt, so several
            // nodes can wait for a token without colliding.
            conn.execute(
                "CREATE UNIQUE INDEX IF NOT EXISTS ux_nodes_token ON nodes (token_sha256) \
                 WHERE token_sha256 <> ''",
                [],
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
            if version < 3 {
                // Add observed_at if the table predates it. Existing rows keep 0, which would
                // collide under the new unique index, so they are backfilled from timestamp:
                // for a local head the two are the same observation anyway.
                let has_col: bool = conn
                    .query_row(
                        "SELECT 1 FROM pragma_table_info('service_checks') WHERE name='observed_at'",
                        [],
                        |_| Ok(true),
                    )
                    .unwrap_or(false);
                if !has_col {
                    conn.execute(
                        "ALTER TABLE service_checks ADD COLUMN observed_at INTEGER NOT NULL DEFAULT 0",
                        [],
                    )?;
                }
                conn.execute(
                    "UPDATE service_checks SET observed_at = timestamp WHERE observed_at = 0",
                    [],
                )?;
                // v3 makes a check idempotent: the unique index below turns a retried report into
                // a no-op. Existing databases carry duplicate rows from the old unconstrained
                // INSERT (two writers, one second), so they are collapsed first or the index
                // cannot be built.
                conn.execute(
                    "DELETE FROM service_checks WHERE rowid NOT IN (\
                     SELECT MIN(rowid) FROM service_checks GROUP BY node_id, service_id, timestamp)",
                    [],
                )?;
                version = 3;
            }
            if version < 4 {
                // Two new columns: the name the device reports about itself, and an optional
                // name the operator prefers. Keeping them apart means clearing an override
                // restores what the machine actually calls itself.
                for (col, ddl) in [
                    ("reported_name", "ALTER TABLE nodes ADD COLUMN reported_name TEXT NOT NULL DEFAULT ''"),
                    ("display_name", "ALTER TABLE nodes ADD COLUMN display_name TEXT NOT NULL DEFAULT ''"),
                ] {
                    let has: bool = conn
                        .query_row(
                            "SELECT 1 FROM pragma_table_info('nodes') WHERE name = ?",
                            (col,),
                            |_| Ok(true),
                        )
                        .unwrap_or(false);
                    if !has {
                        conn.execute(ddl, [])?;
                    }
                }
                conn.execute("UPDATE nodes SET reported_name = name WHERE reported_name = ''", [])?;
                version = 4;
            }
            if version < 5 {
                // v5 makes a token unique to one machine. The index itself is created below, after
                // this block, so the migration only has to clear any pre-existing duplicate. An
                // operator who reused a token has to re-enrol one node; there is no safe guess.
                conn.execute(
                    "UPDATE nodes SET token_sha256 = '' WHERE token_sha256 <> '' AND rowid NOT IN (\
                     SELECT MIN(rowid) FROM nodes WHERE token_sha256 <> '' GROUP BY token_sha256)",
                    [],
                )?;
                version = 5;
            }
            if version < 6 {
                let has: bool = conn
                    .query_row(
                        "SELECT 1 FROM pragma_table_info('nodes') WHERE name='metrics_json'",
                        [],
                        |_| Ok(true),
                    )
                    .unwrap_or(false);
                if !has {
                    conn.execute(
                        "ALTER TABLE nodes ADD COLUMN metrics_json TEXT NOT NULL DEFAULT ''",
                        [],
                    )?;
                }
                version = 6;
            }
            if version < 7 {
                // Where a service listens is a fact the tail knows and the head does not, so it
                // has to be stored rather than recomputed.
                for col in ["ports", "addr"] {
                    let has: bool = conn
                        .query_row(
                            &format!("SELECT 1 FROM pragma_table_info('service_state') WHERE name='{col}'"),
                            [],
                            |_| Ok(true),
                        )
                        .unwrap_or(false);
                    if !has {
                        conn.execute(
                            &format!("ALTER TABLE service_state ADD COLUMN {col} TEXT NOT NULL DEFAULT ''"),
                            [],
                        )?;
                    }
                }
                version = 7;
            }
            if version < 8 {
                // Memory per service. Added after the fact, so an existing database gains the
                // column here rather than being rebuilt — the history in it is the point.
                let has: bool = conn
                    .query_row(
                        "SELECT 1 FROM pragma_table_info('service_state') WHERE name='ram_mb'",
                        [],
                        |_| Ok(true),
                    )
                    .unwrap_or(false);
                if !has {
                    conn.execute(
                        "ALTER TABLE service_state ADD COLUMN ram_mb REAL NOT NULL DEFAULT 0",
                        [],
                    )?;
                }
                version = 8;
            }
            if version < 9 {
                // Pairing handshake. A tail asks to join; the head shows the request with approve
                // and reject, because a token arriving over the network is not the same as an
                // operator deciding to trust that machine. The token digest is stored here on
                // approval — during the request itself it is not yet a credential.
                conn.execute(
                    "CREATE TABLE IF NOT EXISTS pair_requests (
                        id            INTEGER PRIMARY KEY AUTOINCREMENT,
                        node_id       TEXT    NOT NULL,
                        name          TEXT    NOT NULL DEFAULT '',
                        token_sha256  TEXT    NOT NULL DEFAULT '',
                        address       TEXT    NOT NULL DEFAULT '',
                        interval_s    INTEGER NOT NULL DEFAULT 30,
                        requested_ts  INTEGER NOT NULL DEFAULT 0,
                        status        TEXT    NOT NULL DEFAULT 'pending'
                     )",
                    [],
                )?;
                conn.execute(
                    "CREATE UNIQUE INDEX IF NOT EXISTS ux_pair_pending ON pair_requests (node_id) \
                     WHERE status = 'pending'",
                    [],
                )?;
                version = 9;
            }
            conn.execute(
                "UPDATE schema_version SET version = ?, applied_ts = ?",
                (version, now()),
            )?;
            conn.execute(
                "CREATE UNIQUE INDEX IF NOT EXISTS ux_checks ON service_checks (node_id, service_id, observed_at)",
                [],
            )?;
            // One token, one machine. Empty means "not enrolled yet" and is exempt, so several
            // nodes can wait for a token without colliding.
            conn.execute(
                "CREATE UNIQUE INDEX IF NOT EXISTS ux_nodes_token ON nodes (token_sha256) \
                 WHERE token_sha256 <> ''",
                [],
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
    fn forgetting_other_nodes_unpairs_completely_but_keeps_this_machine() {
        let dir = std::env::temp_dir().join(format!("of-forget-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("forget.db");
        let _ = std::fs::remove_file(&db);
        init(&db).unwrap();
        let conn = connect(&db).unwrap();
        conn.execute("INSERT INTO nodes (node_id, token_sha256, last_seen_ts) VALUES ('me', 'aaa', 111)", []).unwrap();
        conn.execute("INSERT INTO nodes (node_id, token_sha256, last_seen_ts) VALUES ('other', 'bbb', 222)", []).unwrap();
        conn.execute("INSERT INTO service_state (node_id, service_id) VALUES ('other', 'ssh')", []).unwrap();
        drop(conn);

        forget_other_nodes(&db, "me").unwrap();

        let conn = connect(&db).unwrap();
        let seen: i64 = conn.query_row("SELECT last_seen_ts FROM nodes WHERE node_id='me'", [], |r| r.get(0)).unwrap();
        assert_eq!(seen, 111, "this machine's own data must survive");
        // A hard unpairing: the row goes, so its token digest goes with it and the machine must
        // enrol again. Leaving the digest behind would keep standing credentials for a host this
        // node no longer watches.
        let others: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes WHERE node_id='other'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(others, 0, "the other node must be forgotten entirely, token included");
        let states: i64 = conn
            .query_row("SELECT COUNT(*) FROM service_state WHERE node_id='other'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(states, 0, "its observations go with it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_column_survives_the_whole_ladder() {
        // The bug this exists for: the migration blocks were ordered 5, 7, 6, so a v5 database ran
        // the v7 step, recorded version 7, and then skipped the v6 step for ever — a database that
        // claimed to be current while missing a column the code writes to. Build a v5 database and
        // climb, then assert on every column the schema promises.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("v5.db");
        {
            // Build a v5 database directly: the v4 columns are there, the v6 and v7 ones are not.
            let conn = connect(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_version (version INTEGER PRIMARY KEY, applied_ts INTEGER NOT NULL DEFAULT 0);
                 CREATE TABLE nodes (
                    node_id TEXT PRIMARY KEY, name TEXT NOT NULL DEFAULT '',
                    interval_s INTEGER NOT NULL DEFAULT 30, token_sha256 TEXT NOT NULL DEFAULT '',
                    first_seen_ts INTEGER NOT NULL DEFAULT 0, last_seen_ts INTEGER NOT NULL DEFAULT 0,
                    reported_name TEXT NOT NULL DEFAULT '', display_name TEXT NOT NULL DEFAULT '');
                 CREATE TABLE service_state (
                    node_id TEXT NOT NULL, service_id TEXT NOT NULL, name TEXT NOT NULL DEFAULT '',
                    \"group\" TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT '',
                    detail TEXT NOT NULL DEFAULT '', latency_ms INTEGER NOT NULL DEFAULT 0,
                    timestamp INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (node_id, service_id));
                 CREATE TABLE service_checks (
                    node_id TEXT NOT NULL, service_id TEXT NOT NULL, status TEXT NOT NULL DEFAULT '',
                    timestamp INTEGER NOT NULL DEFAULT 0, observed_at INTEGER NOT NULL DEFAULT 0);
                 CREATE TABLE ignored (node_id TEXT NOT NULL, service_id TEXT NOT NULL,
                    PRIMARY KEY (node_id, service_id));
                 INSERT INTO schema_version (version, applied_ts) VALUES (5, 0);",
            )
            .unwrap();
        }
        let v = init(&db).unwrap();
        assert_eq!(v, SCHEMA_VERSION, "the ladder must land on the current version");
        let conn = connect(&db).unwrap();
        for (t, c) in [
            ("nodes", "reported_name"),
            ("nodes", "display_name"),
            ("nodes", "metrics_json"),
            ("service_state", "ports"),
            ("service_state", "addr"),
        ] {
            let has: bool = conn
                .query_row(
                    &format!("SELECT 1 FROM pragma_table_info('{t}') WHERE name='{c}'"),
                    [],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            assert!(has, "version {v} on disk but {t}.{c} was never added");
        }
    }

    #[test]
    fn the_migration_ladder_ascends() {
        // A ladder that visits versions out of order silently skips steps; assert the source does.
        let src = include_str!("db.rs");
        let mut seen: Vec<i64> = Vec::new();
        for part in src.split("if version < ").skip(1) {
            if let Some(n) = part.split(|c: char| !c.is_ascii_digit()).next() {
                if let Ok(v) = n.parse::<i64>() {
                    if !seen.contains(&v) {
                        seen.push(v);
                    }
                }
            }
        }
        let mut ascending = seen.clone();
        ascending.sort_unstable();
        assert_eq!(seen, ascending, "migration steps are out of order: {seen:?}");
    }

    #[test]
    fn init_is_idempotent_and_reports_the_current_version() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("status.db");
        assert_eq!(init(&db).unwrap(), SCHEMA_VERSION);
        assert_eq!(init(&db).unwrap(), SCHEMA_VERSION);
        assert_eq!(version(&db).unwrap(), SCHEMA_VERSION);
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
        assert_eq!(init(&db).unwrap(), SCHEMA_VERSION);
        let ignored = ignored_ids(&db, "local").unwrap();
        assert_eq!(ignored, vec!["docker".to_string(), "lmstudio".to_string()]);
        let conn = connect(&db).unwrap();
        let still: bool = conn
            .query_row("SELECT 1 FROM sqlite_master WHERE name='intentional'", [], |_| Ok(true))
            .unwrap_or(false);
        assert!(!still, "intentional table survived the migration");
    }

    #[test]
    fn two_nodes_cannot_share_a_token() {
        // One token, one machine: revoking a laptop must revoke exactly one thing.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("tok.db");
        init(&db).unwrap();
        let conn = connect(&db).unwrap();
        conn.execute(
            "INSERT INTO nodes (node_id, token_sha256) VALUES ('a', 'deadbeef')",
            [],
        )
        .unwrap();
        let clash = conn.execute(
            "INSERT INTO nodes (node_id, token_sha256) VALUES ('b', 'deadbeef')",
            [],
        );
        assert!(clash.is_err(), "the same token must not serve two nodes");
    }

    #[test]
    fn nodes_without_a_token_yet_do_not_collide() {
        // An enrolled-later node has an empty token; several must be able to wait.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("empty.db");
        init(&db).unwrap();
        let conn = connect(&db).unwrap();
        conn.execute("INSERT INTO nodes (node_id) VALUES ('a')", []).unwrap();
        conn.execute("INSERT INTO nodes (node_id) VALUES ('b')", []).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2);
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
    fn v2_database_upgrades_to_v3_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("v2.db");
        // Build a v2 database the way the old code left it: no nodes table, no unique index.
        {
            let conn = connect(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_version (version INTEGER NOT NULL, applied_ts INTEGER NOT NULL);
                 INSERT INTO schema_version VALUES (2, 0);
                 CREATE TABLE service_checks (node_id TEXT NOT NULL, service_id TEXT NOT NULL,
                    timestamp INTEGER NOT NULL, status TEXT NOT NULL, detail TEXT NOT NULL,
                    latency_ms INTEGER NOT NULL);
                 CREATE TABLE incidents (id INTEGER PRIMARY KEY AUTOINCREMENT, node_id TEXT NOT NULL,
                    service_id TEXT NOT NULL, name TEXT NOT NULL, grp TEXT NOT NULL,
                    started_ts INTEGER NOT NULL, ended_ts INTEGER, last_detail TEXT NOT NULL DEFAULT '');
                 INSERT INTO service_checks VALUES ('local','a',100,'up','HTTP 200',5);
                 INSERT INTO service_checks VALUES ('local','a',200,'down','refused',3);
                 INSERT INTO incidents (node_id,service_id,name,grp,started_ts) VALUES ('local','a','A','G',150);",
            )
            .unwrap();
        }
        assert_eq!(init(&db).unwrap(), SCHEMA_VERSION);
        let conn = connect(&db).unwrap();
        let checks: i64 = conn.query_row("SELECT COUNT(*) FROM service_checks", [], |r| r.get(0)).unwrap();
        let incs: i64 = conn.query_row("SELECT COUNT(*) FROM incidents", [], |r| r.get(0)).unwrap();
        assert_eq!(checks, 2, "migration lost check rows");
        assert_eq!(incs, 1, "migration lost incidents");
        let nodes: bool = conn
            .query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name='nodes'", [], |_| Ok(true))
            .unwrap_or(false);
        assert!(nodes, "the upgrade must create the nodes table");
        // v4 adds the two name columns; a v2 database must gain them too.
        let has_reported: bool = conn
            .query_row("SELECT 1 FROM pragma_table_info('nodes') WHERE name='reported_name'", [], |_| Ok(true))
            .unwrap_or(false);
        assert!(has_reported, "the upgrade must add reported_name");
    }

    #[test]
    fn v2_duplicate_checks_are_collapsed_before_the_index_is_built() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("dupes.db");
        {
            let conn = connect(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_version (version INTEGER NOT NULL, applied_ts INTEGER NOT NULL);
                 INSERT INTO schema_version VALUES (2, 0);
                 CREATE TABLE service_checks (node_id TEXT NOT NULL, service_id TEXT NOT NULL,
                    timestamp INTEGER NOT NULL, status TEXT NOT NULL, detail TEXT NOT NULL,
                    latency_ms INTEGER NOT NULL);
                 INSERT INTO service_checks VALUES ('local','a',100,'up','x',1);
                 INSERT INTO service_checks VALUES ('local','a',100,'up','x',1);
                 INSERT INTO service_checks VALUES ('local','a',100,'up','x',1);",
            )
            .unwrap();
        }
        // This is the real bug: two writers landed in the same second, and the old schema had no
        // constraint to stop them. The upgrade must not fail.
        assert_eq!(init(&db).unwrap(), SCHEMA_VERSION);
        let conn = connect(&db).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM service_checks", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "duplicates survived the migration");
    }

    #[test]
    fn a_duplicate_check_write_is_a_noop_at_v3() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("idem.db");
        init(&db).unwrap();
        let conn = connect(&db).unwrap();
        // Column order is (node_id, service_id, timestamp, status, detail, latency_ms, observed_at).
        let sql = "INSERT OR IGNORE INTO service_checks VALUES ('local','a',100,'up','x',1,100)";
        conn.execute(sql, []).unwrap();
        conn.execute(sql, []).unwrap();
        conn.execute(sql, []).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM service_checks", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "a retried report must not add rows");
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
                "INSERT INTO service_checks (node_id, service_id, timestamp, status, detail, latency_ms, observed_at) \
                 VALUES ('local','a',?, 'up','',0,?)",
                (t - RAW_RETENTION_S - 10, t - RAW_RETENTION_S - 10),
            )
            .unwrap();
            conn.execute(
                "INSERT INTO service_checks (node_id, service_id, timestamp, status, detail, latency_ms, observed_at) \
                 VALUES ('local','a',?, 'up','',0,?)",
                (t, t),
            )
            .unwrap();
        }
        let (checks, _) = prune(&db, Some(t)).unwrap();
        assert_eq!(checks, 1);
        let conn = connect(&db).unwrap();
        let left: i64 = conn.query_row("SELECT COUNT(*) FROM service_checks", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 1);
    }
}
