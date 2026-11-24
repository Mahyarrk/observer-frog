"""Notifications: header form (I7), no proxy (I8), coalescing, rate limit, quiet first sightings."""

import http.server
import os
import threading
import unittest
from unittest import mock

from statushub import notify
from statushub.config import Sink
from statushub.state import Change


def change(sid="web", frm="up", to="down", first=False, ignored=False):
    return Change("local", sid, sid.title(), "Test", frm, to, "connection refused", 1_800_000_000,
                  ignored=ignored, first_sighting=first)


class CapturingServer:
    """Records the raw request so the header form can be asserted exactly."""
    def __init__(self):
        self.requests = []
        outer = self

        class H(http.server.BaseHTTPRequestHandler):
            def log_message(self, format, *args):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(length).decode("utf-8")
                outer.requests.append({"path": self.path, "headers": dict(self.headers), "body": body})
                self.send_response(200)
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"ok")

        self.srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.base = f"http://127.0.0.1:{self.srv.server_address[1]}"
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()

    def close(self):
        self.srv.shutdown()
        self.srv.server_close()


class NtfyHeaderFormTests(unittest.TestCase):
    def setUp(self):
        self.srv = CapturingServer()
        self.addCleanup(self.srv.close)
        self.sink = Sink(type="ntfy", url=self.srv.base, topic="statushub")

    def test_publishes_with_headers_and_plain_body(self):
        """I7: header form, not JSON — a JSON body is stored verbatim by this ntfy build."""
        event = notify.Event("Web is down", "connection refused", notify.PRIORITY_OUTAGE,
                             ("rotating_light", "web"), click="https://hub/status/")
        ok, info = notify.publish_ntfy(self.sink, "statushub", event)
        self.assertTrue(ok, info)
        req = self.srv.requests[-1]
        self.assertEqual(req["path"], "/statushub")
        self.assertEqual(req["body"], "connection refused")
        self.assertEqual(req["headers"]["X-Title"], "Web is down")
        self.assertEqual(req["headers"]["X-Priority"], "4")
        self.assertEqual(req["headers"]["X-Tags"], "rotating_light,web")
        self.assertEqual(req["headers"]["X-Click"], "https://hub/status/")
        self.assertIn("text/plain", req["headers"]["Content-Type"])

    def test_body_is_not_json(self):
        event = notify.Event("t", "plain text body", 2, ())
        notify.publish_ntfy(self.sink, "statushub", event)
        body = self.srv.requests[-1]["body"]
        self.assertFalse(body.strip().startswith("{"), "JSON body leaked into ntfy")

    def test_token_is_read_from_environment_not_config(self):
        os.environ["OF_TEST_TOKEN"] = "secret-abc"
        self.addCleanup(os.environ.pop, "OF_TEST_TOKEN", None)
        sink = Sink(type="ntfy", url=self.srv.base, topic="t", token_env="OF_TEST_TOKEN")
        notify.publish_ntfy(sink, "t", notify.Event("t", "b"))
        self.assertEqual(self.srv.requests[-1]["headers"]["Authorization"], "Bearer secret-abc")

    def test_missing_token_env_sends_no_header(self):
        sink = Sink(type="ntfy", url=self.srv.base, topic="t", token_env="OF_ABSENT_TOKEN")
        notify.publish_ntfy(sink, "t", notify.Event("t", "b"))
        self.assertNotIn("Authorization", self.srv.requests[-1]["headers"])

    def test_no_topic_is_a_reported_failure_not_a_crash(self):
        ok, info = notify.publish_ntfy(Sink(type="ntfy", url=self.srv.base), "", notify.Event("t", "b"))
        self.assertFalse(ok)
        self.assertIn("topic", info)

    def test_unreachable_server_returns_false_and_a_reason(self):
        sink = Sink(type="ntfy", url="http://127.0.0.1:1", topic="t")
        ok, info = notify.publish_ntfy(sink, "t", notify.Event("t", "b"))
        self.assertFalse(ok)
        self.assertTrue(info)


class ProxyBypassTests(unittest.TestCase):
    def test_default_sink_bypasses_the_proxy(self):
        """I8: the monitored proxy must not sit between the alerter and the phone."""
        seen = {}

        def fake_build(*handlers):
            seen["handlers"] = handlers
            return mock.MagicMock()

        with mock.patch.object(urllib_request, "build_opener", fake_build):
            notify._opener(use_proxy=False)
        self.assertEqual(len(seen["handlers"]), 1)
        self.assertIsInstance(seen["handlers"][0], urllib_request.ProxyHandler)

    def test_sink_can_opt_into_the_proxy(self):
        seen = {}

        def fake_build(*handlers):
            seen["handlers"] = handlers
            return mock.MagicMock()

        with mock.patch.object(urllib_request, "build_opener", fake_build):
            notify._opener(use_proxy=True)
        self.assertEqual(seen["handlers"], ())


import urllib.request as urllib_request  # noqa: E402  (used by the class above)


class ChangeToEventTests(unittest.TestCase):
    def test_first_sighting_never_pages(self):
        self.assertEqual(notify.events_from_changes([change(first=True)]), [])

    def test_outage_is_urgent_with_two_tags(self):
        events = notify.events_from_changes([change()])
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0].priority, notify.PRIORITY_OUTAGE)
        self.assertIn("web", events[0].tags)
        self.assertIn("down", events[0].title)

    def test_recovery_is_normal_priority(self):
        events = notify.events_from_changes([change(frm="down", to="up")])
        self.assertEqual(events[0].priority, notify.PRIORITY_RECOVERY)
        self.assertIn("recovered", events[0].title)

    def test_ignored_is_marked_in_the_title(self):
        events = notify.events_from_changes([change(sid="docker", ignored=True)])
        self.assertIn("ignored", events[0].title)

    def test_click_points_at_the_dashboard(self):
        events = notify.events_from_changes([change()], dashboard_url="https://hub/status/")
        self.assertEqual(events[0].click, "https://hub/status/")


class DigestTests(unittest.TestCase):
    def test_single_event_passes_through_unchanged(self):
        d = notify.Digest()
        d.add(notify.Event("Web is down", "x", notify.PRIORITY_OUTAGE, ()))
        out = d.drain()
        self.assertEqual(len(out), 1)
        self.assertEqual(out[0].title, "Web is down")

    def test_mass_outage_becomes_one_message(self):
        """The 29-message burst must be structurally impossible."""
        d = notify.Digest()
        for i in range(12):
            d.add(notify.Event(f"Svc{i} is down", "x", notify.PRIORITY_OUTAGE, ()))
        out = d.drain()
        self.assertEqual(len(out), 1, f"expected one coalesced message, got {len(out)}")
        self.assertIn("Svc0", out[0].message)
        self.assertIn("12 service(s) down", out[0].message)

    def test_mixed_batch_reports_both_counts(self):
        d = notify.Digest()
        for i in range(3):
            d.add(notify.Event(f"A{i} is down", "x", notify.PRIORITY_OUTAGE, ()))
        for i in range(2):
            d.add(notify.Event(f"B{i} recovered", "x", notify.PRIORITY_RECOVERY, ()))
        out = d.drain()
        self.assertEqual(len(out), 1)
        self.assertIn("3 service(s) down", out[0].message)
        self.assertIn("2 recovered", out[0].message)

    def test_recoveries_only_are_one_low_priority_message(self):
        d = notify.Digest()
        for i in range(4):
            d.add(notify.Event(f"S{i} recovered", "x", notify.PRIORITY_RECOVERY, ()))
        out = d.drain()
        self.assertEqual(len(out), 1)
        self.assertEqual(out[0].priority, notify.PRIORITY_RECOVERY)
        self.assertIn("4 services recovered", out[0].title)

    def test_digest_is_bounded_per_flush(self):
        d = notify.Digest(max_items=3)
        for i in range(7):
            d.add(notify.Event(f"S{i} is down", "x", notify.PRIORITY_OUTAGE, ()))
        first = d.drain()
        self.assertEqual(len(d.pending), 4, "the digest did not retain the remainder")
        self.assertIn("3 service(s) down", first[0].message)

    def test_empty_digest_drains_to_nothing(self):
        self.assertEqual(notify.Digest().drain(), [])


class RateLimiterTests(unittest.TestCase):
    def test_allows_up_to_the_limit_then_suppresses(self):
        rl = notify.RateLimiter(window_s=60, limit=3)
        self.assertEqual([rl.allow(1000 + i) for i in range(3)], [True, True, True])
        self.assertFalse(rl.allow(1004))
        self.assertEqual(rl.suppressed, 1)

    def test_window_slides(self):
        rl = notify.RateLimiter(window_s=10, limit=2)
        rl.allow(1000)
        rl.allow(1001)
        self.assertFalse(rl.allow(1002))
        self.assertTrue(rl.allow(1015), "the limiter never forgot old sends")


class NotifierIntegrationTests(unittest.TestCase):
    def setUp(self):
        self.srv = CapturingServer()
        self.addCleanup(self.srv.close)

    def test_two_outages_in_one_flush_produce_one_request(self):
        sink = Sink(type="ntfy", url=self.srv.base, topic="statushub")
        n = notify.Notifier(sinks=(sink,), topics={"ntfy": "statushub"})
        n.submit([change("a"), change("b")])
        delivered = n.flush(now=1000)
        self.assertEqual(len(delivered), 1)
        self.assertEqual(len(self.srv.requests), 1)

    def test_rate_limited_flush_sends_nothing(self):
        sink = Sink(type="ntfy", url=self.srv.base, topic="statushub")
        n = notify.Notifier(sinks=(sink,), topics={"ntfy": "statushub"})
        n.limiter = notify.RateLimiter(window_s=60, limit=0)
        n.submit([change()])
        self.assertEqual(n.flush(now=1000), [])
        self.assertEqual(self.srv.requests, [])

    def test_no_sinks_means_nothing_delivered_but_no_crash(self):
        n = notify.Notifier(sinks=())
        n.submit([change()])
        self.assertEqual(n.flush(now=1000), [])

    def test_failed_sink_records_a_reason(self):
        sink = Sink(type="ntfy", url="http://127.0.0.1:1", topic="t")
        n = notify.Notifier(sinks=(sink,), topics={"ntfy": "t"})
        n.submit([change()])
        n.flush(now=1000)
        self.assertTrue(n.failures)


class FmtDurationTests(unittest.TestCase):
    def test_units(self):
        self.assertEqual(notify.fmt_duration(30), "30s")
        self.assertEqual(notify.fmt_duration(600), "10m")
        self.assertEqual(notify.fmt_duration(7200), "2h")
        self.assertEqual(notify.fmt_duration(200000), "2d")


if __name__ == "__main__":
    unittest.main()
