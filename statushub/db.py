"""SQLite storage: schema v1, composite (node_id, service_id) keys, WAL.

Every table carries node_id from the first commit so push nodes need no migration later.
"""

from __future__ import annotations

import sqlite3
import time
from contextlib import contextmanager
from pathlib import Path

SCHEMA_VERSION = 1
RAW_RETENTION_S = 86_400          # 24 h of per-check rows
ROLLUP_RETENTION_S = 2_592_000    # 30 days of hourly counters

TABLES = (
    "service_checks",
    "service_state",
    "incidents",
    "hourly_rollup",
    "ignored",
    "intentional",
)

_SCHEMA = """
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

CREATE TABLE IF NOT EXISTS intentional (
    node_id    TEXT NOT NULL,
    service_id TEXT NOT NULL,
    PRIMARY KEY (node_id, service_id)
);
"""


@contextmanager
def connect(path: str | Path):
    """One short-lived connection per transaction; commits on success, rolls back on error."""
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(p, timeout=10.0)
    try:
        conn.execute("PRAGMA journal_mode=WAL")
        conn.execute("PRAGMA busy_timeout=5000")
        yield conn
        conn.commit()
    except BaseException:
        conn.rollback()
        raise
    finally:
        conn.close()


def init(path: str | Path) -> int:
    """Create the schema if absent and return the schema version on disk."""
    with connect(path) as conn:
        conn.executescript(_SCHEMA)
        row = conn.execute("SELECT version FROM schema_version LIMIT 1").fetchone()
        if row is None:
            conn.execute(
                "INSERT INTO schema_version (version, applied_ts) VALUES (?, ?)",
                (SCHEMA_VERSION, int(time.time())),
            )
            return SCHEMA_VERSION
        return int(row[0])


def version(path: str | Path) -> int:
    with connect(path) as conn:
        row = conn.execute("SELECT version FROM schema_version LIMIT 1").fetchone()
    return int(row[0]) if row else 0


def prune(path: str | Path, now: int | None = None) -> tuple[int, int]:
    """Drop expired raw checks and rollups; returns (checks_deleted, rollups_deleted)."""
    now = int(time.time()) if now is None else now
    with connect(path) as conn:
        checks = conn.execute(
            "DELETE FROM service_checks WHERE timestamp < ?", (now - RAW_RETENTION_S,)
        ).rowcount
        rollups = conn.execute(
            "DELETE FROM hourly_rollup WHERE hour_ts < ?", (now - ROLLUP_RETENTION_S,)
        ).rowcount
    return checks, rollups


def flags(path: str | Path, node_id: str) -> tuple[list[str], list[str]]:
    """Read ignored/intentional at serve time — never baked into a snapshot (invariant I6)."""
    with connect(path) as conn:
        ignored = [r[0] for r in conn.execute(
            "SELECT service_id FROM ignored WHERE node_id = ? ORDER BY service_id", (node_id,))]
        intentional = [r[0] for r in conn.execute(
            "SELECT service_id FROM intentional WHERE node_id = ? ORDER BY service_id", (node_id,))]
    return ignored, intentional
