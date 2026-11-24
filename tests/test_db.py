"""Schema v1 contracts: node dimension, composite keys, WAL, retention."""

import os
import sqlite3
import tempfile
import time
import unittest
from pathlib import Path

from statushub import db


class SchemaTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = Path(self.tmp.name) / "status.db"
        db.init(self.path)

    def tearDown(self):
        self.tmp.cleanup()

    def _cols(self, table):
        with db.connect(self.path) as conn:
            return [r[1] for r in conn.execute(f"PRAGMA table_info({table})")]

    def _pk(self, table):
        with db.connect(self.path) as conn:
            rows = conn.execute(f"PRAGMA table_info({table})").fetchall()
        return [r[1] for r in sorted((r for r in rows if r[5]), key=lambda r: r[5])]

    def test_fresh_db_is_current_version(self):
        self.assertEqual(db.version(self.path), 2)
        self.assertEqual(db.SCHEMA_VERSION, 2)

    def test_every_table_carries_node_id(self):
        for table in db.TABLES:
            self.assertIn("node_id", self._cols(table), f"{table} lost its node dimension")

    def test_composite_keys(self):
        self.assertEqual(self._pk("service_state"), ["node_id", "service_id"])
        self.assertEqual(self._pk("hourly_rollup"), ["node_id", "service_id", "hour_ts"])
        self.assertEqual(self._pk("ignored"), ["node_id", "service_id"])

    def test_wal_enabled(self):
        with db.connect(self.path) as conn:
            self.assertEqual(conn.execute("PRAGMA journal_mode").fetchone()[0].lower(), "wal")

    def test_two_nodes_can_share_a_service_id(self):
        with db.connect(self.path) as conn:
            for node in ("local", "laptop"):
                conn.execute(
                    "INSERT INTO service_state (node_id, service_id, state) VALUES (?,?,?)",
                    (node, "docker", "up"),
                )
        with db.connect(self.path) as conn:
            self.assertEqual(conn.execute("SELECT count(*) FROM service_state").fetchone()[0], 2)

    def test_prune_drops_expired_rows_only(self):
        now = int(time.time())
        with db.connect(self.path) as conn:
            old = now - db.RAW_RETENTION_S - 60
            conn.execute(
                "INSERT INTO service_checks VALUES ('local','x',?,'up','',1)", (old,)
            )
            conn.execute(
                "INSERT INTO service_checks VALUES ('local','x',?,'up','',1)", (now - 5,)
            )
        checks, _ = db.prune(self.path, now=now)
        self.assertEqual(checks, 1)
        with db.connect(self.path) as conn:
            self.assertEqual(conn.execute("SELECT count(*) FROM service_checks").fetchone()[0], 1)

    def test_flags_are_scoped_per_node(self):
        with db.connect(self.path) as conn:
            conn.execute("INSERT INTO ignored VALUES ('local','lmstudio')")
            conn.execute("INSERT INTO ignored VALUES ('laptop','terraria')")
        self.assertEqual(db.ignored_ids(self.path, "local"), ["lmstudio"])

    def test_init_is_idempotent(self):
        self.assertEqual(db.init(self.path), 2)
        with db.connect(self.path) as conn:
            self.assertEqual(conn.execute("SELECT count(*) FROM schema_version").fetchone()[0], 1)


if __name__ == "__main__":
    unittest.main()
