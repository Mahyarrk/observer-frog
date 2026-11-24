"""Schema migrations: v1 → v2 folds `intentional` into `ignored` without losing the operator's marks."""

import sqlite3
import tempfile
import unittest
from pathlib import Path

from statushub import db

V1_SCHEMA = """
CREATE TABLE schema_version (version INTEGER NOT NULL, applied_ts INTEGER NOT NULL);
CREATE TABLE service_checks (node_id TEXT NOT NULL, service_id TEXT NOT NULL,
    timestamp INTEGER NOT NULL, status TEXT NOT NULL, detail TEXT NOT NULL,
    latency_ms INTEGER NOT NULL);
CREATE TABLE service_state (node_id TEXT NOT NULL, service_id TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'up', fails INTEGER NOT NULL DEFAULT 0,
    since_ts INTEGER NOT NULL DEFAULT 0, name TEXT NOT NULL DEFAULT '',
    grp TEXT NOT NULL DEFAULT '', last_detail TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (node_id, service_id));
CREATE TABLE incidents (id INTEGER PRIMARY KEY AUTOINCREMENT, node_id TEXT NOT NULL,
    service_id TEXT NOT NULL, name TEXT NOT NULL, grp TEXT NOT NULL,
    started_ts INTEGER NOT NULL, ended_ts INTEGER, last_detail TEXT NOT NULL DEFAULT '');
CREATE TABLE hourly_rollup (node_id TEXT NOT NULL, service_id TEXT NOT NULL,
    hour_ts INTEGER NOT NULL, up_n INTEGER NOT NULL DEFAULT 0, deg_n INTEGER NOT NULL DEFAULT 0,
    down_n INTEGER NOT NULL DEFAULT 0, lat_sum INTEGER NOT NULL DEFAULT 0,
    lat_n INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (node_id, service_id, hour_ts));
CREATE TABLE ignored (node_id TEXT NOT NULL, service_id TEXT NOT NULL,
    PRIMARY KEY (node_id, service_id));
CREATE TABLE intentional (node_id TEXT NOT NULL, service_id TEXT NOT NULL,
    PRIMARY KEY (node_id, service_id));
"""


class MigrationTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = Path(self.tmp.name) / "v1.db"

    def tearDown(self):
        self.tmp.cleanup()

    def _make_v1(self, ignored=(), intentional=()):
        conn = sqlite3.connect(self.path)
        conn.executescript(V1_SCHEMA)
        conn.execute("INSERT INTO schema_version VALUES (1, 1700000000)")
        for sid in ignored:
            conn.execute("INSERT INTO ignored VALUES ('local', ?)", (sid,))
        for sid in intentional:
            conn.execute("INSERT INTO intentional VALUES ('local', ?)", (sid,))
        conn.commit()
        conn.close()

    def test_v1_database_upgrades_to_current(self):
        self._make_v1()
        self.assertEqual(db.init(self.path), db.SCHEMA_VERSION)
        self.assertEqual(db.version(self.path), 2)

    def test_intentional_rows_become_ignored(self):
        """The whole point: a service you marked meant-to-be-off must stay out of uptime."""
        self._make_v1(intentional=("docker", "lmstudio"))
        db.init(self.path)
        self.assertEqual(db.ignored_ids(self.path, "local"), ["docker", "lmstudio"])

    def test_existing_ignored_rows_are_kept_and_not_duplicated(self):
        self._make_v1(ignored=("tailscale",), intentional=("tailscale", "docker"))
        db.init(self.path)
        self.assertEqual(db.ignored_ids(self.path, "local"), ["docker", "tailscale"])

    def test_intentional_table_is_gone_after_upgrade(self):
        self._make_v1(intentional=("docker",))
        db.init(self.path)
        conn = sqlite3.connect(self.path)
        names = {r[0] for r in conn.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        conn.close()
        self.assertNotIn("intentional", names)
        for table in db.TABLES:
            self.assertIn(table, names)

    def test_upgrade_preserves_history(self):
        self._make_v1()
        conn = sqlite3.connect(self.path)
        conn.execute("INSERT INTO service_checks VALUES ('local','web',1700000000,'up','HTTP 200',5)")
        conn.execute("INSERT INTO hourly_rollup VALUES ('local','web',1700000000,3,0,1,40,4)")
        conn.commit()
        conn.close()
        db.init(self.path)
        conn = sqlite3.connect(self.path)
        self.assertEqual(conn.execute("SELECT count(*) FROM service_checks").fetchone()[0], 1)
        self.assertEqual(conn.execute("SELECT up_n FROM hourly_rollup").fetchone()[0], 3)
        conn.close()

    def test_migration_is_idempotent(self):
        self._make_v1(intentional=("docker",))
        db.init(self.path)
        db.init(self.path)   # a second run must not error or duplicate
        self.assertEqual(db.ignored_ids(self.path, "local"), ["docker"])

    def test_fresh_database_is_born_at_current_version_without_migration(self):
        self.assertEqual(db.init(self.path), db.SCHEMA_VERSION)
        conn = sqlite3.connect(self.path)
        names = {r[0] for r in conn.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        conn.close()
        self.assertNotIn("intentional", names)


if __name__ == "__main__":
    unittest.main()
