"""Config: zero-config boot, validation, and last-good retention on a broken edit."""

import os
import tempfile
import unittest
from pathlib import Path

from statushub.config import ConfigError, Watcher, load

MINIMAL = """
[node]
id = "local"
name = "Test Node"

[serve]
enabled = true
bind = "127.0.0.1"
port = 8768

[[service]]
id = "router"
probe = "http"
target = "http://127.0.0.1:20128/"
expect_status = 200

[[service]]
id = "docker"
probe = "process"
target = "dockerd"

[[sink]]
type = "ntfy"
url = "http://127.0.0.1:2586"
topic = "statushub"
token_env = "NTFY_TOKEN"
"""


class ConfigTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = Path(self.tmp.name) / "config.toml"

    def tearDown(self):
        self.tmp.cleanup()

    def _write(self, text, mtime=None):
        self.path.write_text(text, encoding="utf-8")
        if mtime is not None:
            os.utime(self.path, (mtime, mtime))

    def test_absent_file_yields_defaults(self):
        cfg = load(self.path)
        self.assertIsNone(cfg.path)
        self.assertEqual(cfg.node_id, "local")
        self.assertEqual(cfg.port, 8770)
        self.assertTrue(cfg.serve)
        self.assertEqual(cfg.poll_interval, 30)
        self.assertEqual(cfg.services, ())
        self.assertTrue(str(cfg.db_path).endswith("status.db"))

    def test_loads_services_and_sinks(self):
        self._write(MINIMAL)
        cfg = load(self.path)
        self.assertEqual(cfg.node_name, "Test Node")
        self.assertEqual(cfg.port, 8768)
        self.assertEqual([s.id for s in cfg.services], ["router", "docker"])
        self.assertEqual(cfg.services[0].expect_status, 200)
        self.assertEqual(cfg.services[0].name, "router")   # falls back to id
        self.assertEqual(len(cfg.sinks), 1)
        self.assertEqual(cfg.sinks[0].token_env, "NTFY_TOKEN")

    def test_duplicate_service_id_rejected(self):
        self._write(MINIMAL + '\n[[service]]\nid = "docker"\nprobe = "tcp"\ntarget = "127.0.0.1:2375"\n')
        with self.assertRaises(ConfigError):
            load(self.path)

    def test_unknown_probe_rejected(self):
        self._write('[service]\nid = "x"\nprobe = "magic"\ntarget = "y"\n')
        with self.assertRaises(ConfigError):
            load(self.path)

    def test_service_without_target_rejected(self):
        self._write('[service]\nid = "x"\nprobe = "http"\n')
        with self.assertRaises(ConfigError):
            load(self.path)

    def test_malformed_toml_reports_file(self):
        self._write("[[service]\nid = ")
        with self.assertRaises(ConfigError) as ctx:
            load(self.path)
        self.assertIn(str(self.path), str(ctx.exception))

    def test_watcher_reloads_on_mtime_change(self):
        self._write(MINIMAL, mtime=1_700_000_000)
        w = Watcher(self.path)
        self.assertEqual(len(w.config.services), 2)
        edited = MINIMAL.replace('id = "docker"', 'id = "docker2"')
        self._write(edited, mtime=1_700_000_100)
        self.assertEqual([s.id for s in w.refresh().services], ["router", "docker2"])
        self.assertIsNone(w.error)

    def test_watcher_keeps_last_good_config_on_broken_edit(self):
        self._write(MINIMAL, mtime=1_700_000_000)
        w = Watcher(self.path)
        self._write('[service]\nid = "x"\nprobe = "magic"\ntarget = "y"\n', mtime=1_700_000_200)
        cfg = w.refresh()
        self.assertEqual(len(cfg.services), 2)     # still the good one
        self.assertIsNotNone(w.error)


if __name__ == "__main__":
    unittest.main()
