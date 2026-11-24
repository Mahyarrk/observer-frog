"""The HTTP surface: routes, payload shape, write endpoints, forced-poll guard, I9."""

import json
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
from pathlib import Path

from statushub import db, server, state
from statushub.config import Service, load
from statushub.server import App, build_history, make_handler


class _Base(unittest.TestCase):
    """Boots a real server on an ephemeral port — the only way to trust routing."""

    services: tuple = ()

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        tmp = Path(self.tmp.name)
        self.cfg_path = tmp / "config.toml"
        self.cfg_path.write_text(f'db_path = "{tmp / "s.db"}"\n[node]\nid = "local"\nname = "T"\n')
        self.cfg = load(self.cfg_path)
        db.init(self.cfg.db_path)
        self.app = App(self.cfg, started_ts=int(time.time()))
        self.httpd = server.serve(self.cfg, self.app)
        # serve() binds to the configured address; rebind to an ephemeral port for isolation
        self.httpd.server_close()
        from http.server import ThreadingHTTPServer
        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(self.app))
        self.port = self.httpd.server_address[1]
        self.base = f"http://127.0.0.1:{self.port}"
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def tearDown(self):
        self.httpd.shutdown()
        self.httpd.server_close()
        self.tmp.cleanup()

    def get(self, path, method="GET", body=None):
        req = urllib.request.Request(self.base + path, method=method)
        data = None
        if body is not None:
            data = json.dumps(body).encode()
            req.add_header("Content-Type", "application/json")
        with urllib.request.urlopen(req, data=data, timeout=5) as r:
            return r.status, json.loads(r.read().decode()), dict(r.headers)

    def raw(self, path):
        with urllib.request.urlopen(self.base + path, timeout=5) as r:
            return r.status, r.read(), dict(r.headers)


class RouteTests(_Base):
    def test_status_returns_the_documented_keys(self):
        self.app.set_snapshot([], 1_800_000_000)
        status, body, _ = self.get("/api/status")
        self.assertEqual(status, 200)
        for key in ("timestamp", "services", "devices", "history", "ignored",
                    "intentional", "confirm_polls", "node_ts"):
            self.assertIn(key, body, f"payload lost {key}")

    def test_status_is_not_cached(self):
        self.app.set_snapshot([], 1)
        _, _, headers = self.get("/api/status")
        self.assertIn("no-store", headers.get("Cache-Control", ""))

    def test_health_is_cheap_and_plain(self):
        status, body, _ = self.get("/api/health")
        self.assertEqual(status, 200)
        self.assertTrue(body["ok"])

    def test_unknown_path_is_404_json(self):
        with self.assertRaises(urllib.error.HTTPError) as ctx:
            self.get("/api/nope")
        self.assertEqual(ctx.exception.code, 404)

    def test_index_is_served_as_html(self):
        status, body, headers = self.raw("/")
        self.assertEqual(status, 200)
        self.assertIn("text/html", headers["Content-Type"])
        self.assertGreater(len(body), 1000)

    def test_service_worker_is_served_with_javascript_type(self):
        status, body, headers = self.raw("/sw.js")
        self.assertEqual(status, 200)
        self.assertIn("javascript", headers["Content-Type"])

    def test_last_sighting_flags_are_read_at_serve_time(self):
        """I6: a flag written while the snapshot is unchanged must appear immediately."""
        self.app.set_snapshot([{"id": "web", "name": "Web", "group": "T", "status": "up",
                                "detail": "", "latency_ms": 1, "ports": ""}], 1)
        _, before, _ = self.get("/api/status")
        self.assertEqual(before["ignored"], [])
        with db.connect(self.cfg.db_path) as conn:
            conn.execute("INSERT INTO ignored VALUES ('local','web')")
        _, after, _ = self.get("/api/status")
        self.assertEqual(after["ignored"], ["web"])

    def test_bad_status_code_is_a_degraded_render_not_a_crash(self):
        self.app.set_snapshot([{"id": "x", "name": "X", "group": "T", "status": "unavailable",
                                "detail": "no permission", "latency_ms": 0, "ports": ""}], 1)
        _, body, _ = self.get("/api/status")
        self.assertEqual(body["services"][0]["status"], "unavailable")


class WriteEndpointTests(_Base):
    def test_ignore_round_trip(self):
        status, body, _ = self.post("/api/ignore", {"id": "web", "ignored": True})
        self.assertEqual(status, 200)
        self.assertEqual(body["ignored"], ["web"])
        status, body, _ = self.post("/api/ignore", {"id": "web", "ignored": False})
        self.assertEqual(body["ignored"], [])

    def test_intentional_round_trip(self):
        _, body, _ = self.post("/api/intentional", {"id": "docker", "intentional": True})
        self.assertEqual(body["intentional"], ["docker"])

    def test_missing_id_is_400(self):
        with self.assertRaises(urllib.error.HTTPError) as ctx:
            self.post("/api/ignore", {"ignored": True})
        self.assertEqual(ctx.exception.code, 400)

    def test_write_endpoints_carry_no_wildcard_cors(self):
        _, _, headers = self.post("/api/ignore", {"id": "web", "ignored": True})
        self.assertNotEqual(headers.get("Access-Control-Allow-Origin"), "*")

    def post(self, path, body):
        req = urllib.request.Request(self.base + path, method="POST",
                                    data=json.dumps(body).encode(),
                                    headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=5) as r:
            return r.status, json.loads(r.read().decode()), dict(r.headers)


class ForcedPollTests(_Base):
    def test_poll_with_no_services_reports_a_payload_not_an_error(self):
        self.app.cfg = load(self.cfg_path)
        status, body, _ = self.get("/api/poll")
        self.assertEqual(status, 200)
        self.assertIn("services", body)

    def test_a_second_concurrent_poll_is_refused(self):
        """A phone holding refresh must not stack forced polls."""
        from unittest import mock
        from statushub import poller as poller_mod

        release = threading.Event()
        entered = threading.Event()

        def slow_sample(services):
            entered.set()
            release.wait(3)
            return []

        with mock.patch.object(poller_mod, "sample", slow_sample):
            results = {}

            def first():
                results["first"] = self.get("/api/poll")

            def second():
                entered.wait(2)
                try:
                    results["second"] = self.get("/api/poll")
                except urllib.error.HTTPError as e:
                    results["second"] = (e.code, {}, {})

            t1 = threading.Thread(target=first)
            t1.start()
            entered.wait(2)
            t2 = threading.Thread(target=second)
            t2.start()
            t2.join(2)
            release.set()
            t1.join(4)

        self.assertEqual(results["second"][0], 409, "concurrent forced poll was not refused")
        self.assertEqual(results["first"][0], 200)


class HistoryTests(_Base):
    def test_history_shape(self):
        self.app.set_snapshot([], 1)
        status, body, _ = self.get("/api/history?days=7")
        self.assertEqual(status, 200)
        for key in ("generated", "window_days", "incidents", "uptime", "kpi"):
            self.assertIn(key, body)
        self.assertEqual(body["window_days"], 7)

    def test_days_is_clamped(self):
        _, body, _ = self.get("/api/history?days=99999")
        self.assertEqual(body["window_days"], 365)
        _, body, _ = self.get("/api/history?days=-5")
        self.assertEqual(body["window_days"], 1)

    def test_non_numeric_days_falls_back_to_30(self):
        _, body, _ = self.get("/api/history?days=banana")
        self.assertEqual(body["window_days"], 30)

    def test_kpi_excludes_intentional_services(self):
        now = int(time.time())
        state.apply_wave(self.cfg.db_path, "local",
                         [state.Row("web", "Web", "T", "up", "", 1),
                          state.Row("docker", "Docker", "T", "up", "", 1)], ts=now)
        with db.connect(self.cfg.db_path) as conn:
            conn.execute("INSERT INTO intentional VALUES ('local','docker')")
        _, body, _ = self.get("/api/history?days=7")
        self.assertNotIn("docker", body["uptime"])
        self.assertIn("docker", body["kpi"]["excluded_intentional"])


class SelfMonitoringOrderTests(unittest.TestCase):
    def test_metrics_are_available_before_the_poller_starts(self):
        """I9: self-monitoring must not be counted before the socket exists."""
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        cfg_path = Path(tmp.name) / "c.toml"
        cfg_path.write_text(f'db_path = "{Path(tmp.name) / "s.db"}"')
        cfg = load(cfg_path)
        db.init(cfg.db_path)
        app = App(cfg)
        self.assertEqual(app.poller, None)
        self.assertIn("cpu_pct", app.device_card())
        self.assertIn("self_monitoring", app.snapshot())
        self.assertEqual(app.snapshot()["self_monitoring"]["waves"], 0)


class SnapshotImmutabilityTests(unittest.TestCase):
    def test_callers_cannot_mutate_the_stored_snapshot(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        cfg_path = Path(tmp.name) / "c.toml"
        cfg_path.write_text(f'db_path = "{Path(tmp.name) / "s.db"}"')
        cfg = load(cfg_path)
        db.init(cfg.db_path)
        app = App(cfg)
        app.set_snapshot([{"id": "a", "status": "up"}], 1)
        snap = app.snapshot()
        snap["services"][0]["status"] = "tampered"
        self.assertEqual(app.snapshot()["services"][0]["status"], "up")


if __name__ == "__main__":
    unittest.main()
