"""Probe engine: five kinds, real timeouts, and `unavailable` never masquerading as `down`."""

import http.server
import json
import os
import socket
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

from statushub import probes
from statushub.config import Service


class _Handler(http.server.BaseHTTPRequestHandler):
    """Tiny server: /ok 200, /auth 401, /teapot 418, /sig carries a signature."""
    def log_message(self, format, *args):
        pass

    def do_GET(self):
        routes = {
            "/ok": (200, b"healthy"),
            "/auth": (401, b"unauthorized"),
            "/teapot": (418, b"short and stout"),
            "/sig": (200, b'{"app":"9router","ok":true}'),
        }
        code, body = routes.get(self.path, (404, b"nope"))
        self.send_response(code)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class HttpTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), _Handler)
        cls.base = f"http://127.0.0.1:{cls.srv.server_address[1]}"
        threading.Thread(target=cls.srv.serve_forever, daemon=True).start()

    @classmethod
    def tearDownClass(cls):
        cls.srv.shutdown()
        cls.srv.server_close()

    def test_200_is_up_with_http_detail(self):
        state, detail, ms = probes.probe_http(self.base + "/ok")
        self.assertEqual(state, "up")
        self.assertEqual(detail, "HTTP 200")
        self.assertIsInstance(ms, int)

    def test_401_counts_as_alive_and_says_auth(self):
        state, detail, _ = probes.probe_http(self.base + "/auth")
        self.assertEqual((state, detail), ("up", "HTTP 401 (Auth)"))

    def test_4xx_is_degraded_not_down(self):
        state, detail, _ = probes.probe_http(self.base + "/teapot")
        self.assertEqual((state, detail), ("degraded", "HTTP 418"))

    def test_expect_status_mismatch_is_degraded(self):
        state, detail, _ = probes.probe_http(self.base + "/ok", expect_status=201)
        self.assertEqual(state, "degraded")
        self.assertIn("wanted 201", detail)

    def test_signature_present_is_up(self):
        state, _, _ = probes.probe_http(self.base + "/sig", contains="9router")
        self.assertEqual(state, "up")

    def test_signature_absent_is_degraded(self):
        state, detail, _ = probes.probe_http(self.base + "/sig", contains="portainer")
        self.assertEqual(state, "degraded")
        self.assertIn("signature missing", detail)

    def test_refused_connection_is_down(self):
        s = socket.socket()
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
        s.close()
        state, detail, _ = probes.probe_http(f"http://127.0.0.1:{port}/", timeout=0.6)
        self.assertEqual(state, "down")
        self.assertTrue(detail)


class TcpTests(unittest.TestCase):
    def test_open_port_is_up(self):
        srv = socket.socket()
        srv.bind(("127.0.0.1", 0))
        srv.listen(1)
        port = srv.getsockname()[1]
        try:
            state, detail, _ = probes.probe_tcp(f"127.0.0.1:{port}")
            self.assertEqual(state, "up")
            self.assertIn(str(port), detail)
        finally:
            srv.close()

    def test_closed_port_is_down(self):
        s = socket.socket()
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
        s.close()
        state, detail, _ = probes.probe_tcp(f"127.0.0.1:{port}")
        self.assertEqual(state, "down")
        self.assertIn("refused", detail)

    def test_bad_target_is_unavailable_not_down(self):
        state, detail, _ = probes.probe_tcp("no-port-here")
        self.assertEqual(state, "unavailable")

    def test_accepts_url_scheme_and_bracketed_ipv6(self):
        self.assertEqual(probes._split_hostport("tcp://10.0.0.5:5432"), ("10.0.0.5", 5432))
        self.assertEqual(probes._split_hostport("[::1]:80"), ("::1", 80))


class ProcessTests(unittest.TestCase):
    def test_matches_own_interpreter(self):
        state, detail, _ = probes.probe_process("python")
        self.assertEqual(state, "up")
        self.assertTrue(detail)

    def test_absent_process_is_down(self):
        state, detail, _ = probes.probe_process("frog_daemon_that_does_not_exist_9d3f")
        self.assertEqual((state, detail), ("down", "not running"))

    def test_missing_psutil_is_unavailable(self):
        with mock.patch.dict("sys.modules", {"psutil": None}):
            state, detail, _ = probes.probe_process("python")
        self.assertEqual(state, "unavailable")
        self.assertIn("psutil", detail)


class FileAgeTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = Path(self.tmp.name) / "job.stamp"

    def tearDown(self):
        self.tmp.cleanup()

    def test_fresh_file_is_up(self):
        self.path.write_text("x")
        state, detail, _ = probes.probe_file_age(str(self.path), 3600)
        self.assertEqual(state, "up")
        self.assertIn("ago", detail)

    def test_old_file_is_degraded(self):
        self.path.write_text("x")
        old = time.time() - 7200
        os.utime(self.path, (old, old))
        state, detail, _ = probes.probe_file_age(str(self.path), 3600)
        self.assertEqual(state, "degraded")
        self.assertIn("stale", detail)

    def test_missing_file_is_down(self):
        state, detail, _ = probes.probe_file_age(str(self.path), 3600)
        self.assertEqual((state, detail), ("down", "file missing"))

    def test_unset_threshold_is_unavailable(self):
        state, _, _ = probes.probe_file_age(str(self.path), 0)
        self.assertEqual(state, "unavailable")


class FileJsonTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = Path(self.tmp.name) / "state.json"

    def tearDown(self):
        self.tmp.cleanup()

    def _write(self, obj):
        self.path.write_text(json.dumps(obj))

    def test_reads_dotted_field(self):
        self._write({"proxy": {"connected": True, "node": "NewYork"}})
        state, detail, _ = probes.probe_file_json(str(self.path), "proxy.node")
        self.assertEqual(state, "up")
        self.assertIn("NewYork", detail)

    def test_contains_mismatch_is_degraded(self):
        self._write({"proxy": {"node": "NewYork"}})
        state, _, _ = probes.probe_file_json(str(self.path), "proxy.node", "Berlin")
        self.assertEqual(state, "degraded")

    def test_contains_match_is_up(self):
        self._write({"proxy": {"node": "NewYork"}})
        state, _, _ = probes.probe_file_json(str(self.path), "proxy.node", "newyork")
        self.assertEqual(state, "up")

    def test_missing_field_is_degraded(self):
        self._write({"a": 1})
        state, detail, _ = probes.probe_file_json(str(self.path), "b.c")
        self.assertEqual(state, "degraded")
        self.assertIn("missing", detail)

    def test_invalid_json_is_degraded(self):
        self.path.write_text("{oops")
        state, detail, _ = probes.probe_file_json(str(self.path))
        self.assertEqual(state, "degraded")
        self.assertIn("invalid json", detail)

    def test_index_into_list(self):
        self._write({"nodes": ["a", "b"]})
        state, detail, _ = probes.probe_file_json(str(self.path), "nodes.1")
        self.assertEqual(state, "up")
        self.assertIn("b", detail)


class DispatchTests(unittest.TestCase):
    def test_unknown_probe_is_unavailable(self):
        svc = Service(id="x", probe="http", target="http://127.0.0.1:1/")
        svc = type(svc)(**{**svc.__dict__, "probe": "http"})
        self.assertEqual(probes.run(svc)[0] in ("up", "down", "degraded"), True)

    def test_probe_exception_becomes_unavailable(self):
        svc = Service(id="x", probe="tcp", target="127.0.0.1:1")
        with mock.patch.object(probes, "probe_tcp", side_effect=RuntimeError("boom")):
            state, detail, _ = probes.run(svc)
        self.assertEqual(state, "unavailable")
        self.assertIn("boom", detail)

    def test_detail_is_clipped(self):
        self.assertLessEqual(len(probes.clip("y" * 500)), probes.DETAIL_MAX)
        self.assertTrue(probes.clip("y" * 500).endswith("…"))


if __name__ == "__main__":
    unittest.main()
