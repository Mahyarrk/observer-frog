"""Poller discipline: I12 (one snapshot per wave), bounded concurrency, no stacked waves."""

import threading
import time
import unittest
from unittest import mock

from statushub import poller, probes
from statushub.config import Service


def svc(i, probe="http", target="http://127.0.0.1:1/", **kw):
    return Service(id=f"s{i}", probe=probe, target=target, **kw)


class SnapshotTests(unittest.TestCase):
    def test_single_snapshot_serves_many_process_services(self):
        """I12: N process services must not cost N process scans."""
        calls = {"n": 0}
        real_init = poller.Snapshot._reload

        def counting(self):
            calls["n"] += 1
            real_init(self)

        services = [svc(i, "process", "python") for i in range(8)]
        with mock.patch.object(poller.Snapshot, "_reload", counting):
            rows = poller.sample(services)
        self.assertEqual(calls["n"], 1, "more than one process snapshot in a single wave")
        self.assertEqual(len(rows), 8)
        self.assertTrue(all(r["status"] == "up" for r in rows))

    def test_match_finds_needle_and_reports_ram(self):
        snap = poller.Snapshot()
        snap.rows = [("/usr/bin/python3 -m froggy", "/usr/bin/python3 -m froggy", 42.5)]
        found, cmd, ram = snap.match("froggy")
        self.assertTrue(found)
        self.assertEqual(ram, 42.5)

    def test_match_misses_absent_needle(self):
        snap = poller.Snapshot()
        snap.rows = [("nginx", "nginx", 3.0)]
        self.assertEqual(snap.match("nginx")[0], True)
        self.assertEqual(snap.match("caddy")[0], False)


class SampleTests(unittest.TestCase):
    def test_rows_preserve_config_order(self):
        services = [svc(3, "tcp", "127.0.0.1:1"), svc(1, "tcp", "127.0.0.1:1"),
                    svc(2, "tcp", "127.0.0.1:1")]
        rows = poller.sample(services)
        self.assertEqual([r["id"] for r in rows], ["s3", "s1", "s2"])

    def test_every_row_carries_the_triple_plus_display_fields(self):
        row = poller.sample([svc(1, "tcp", "127.0.0.1:1")])[0]
        for field in ("id", "name", "group", "status", "detail", "latency_ms", "ports"):
            self.assertIn(field, row)
        self.assertIn(row["status"], ("up", "degraded", "down", "unavailable"))

    def test_empty_service_list_is_not_an_error(self):
        self.assertEqual(poller.sample([]), [])

    def test_one_slow_probe_does_not_block_the_others(self):
        """A hung target must not consume the whole wave: the rest still report."""
        def slow_run(service):
            if service.id == "s_slow":
                time.sleep(0.5)
            return ("up", "ok", 1)

        services = [svc(0, "tcp", "x")] + [svc(i, "tcp", "y") for i in range(1, 6)]
        services[0] = Service(id="s_slow", probe="tcp", target="x")
        start = time.time()
        with mock.patch.object(probes, "run", slow_run):
            rows = poller.sample(services)
        elapsed = time.time() - start
        self.assertEqual(len(rows), 6)
        self.assertLess(elapsed, 1.0, "wave serialised instead of running concurrently")

    def test_probe_bug_is_surfaced_as_unavailable_not_crash(self):
        with mock.patch.object(probes, "probe_tcp", side_effect=RuntimeError("kaboom")):
            row = poller.sample([svc(1, "tcp", "127.0.0.1:1")])[0]
        self.assertEqual(row["status"], "unavailable")
        self.assertIn("probe error", row["detail"])
        self.assertNotIn("kaboom", row["detail"])


class PollerLoopTests(unittest.TestCase):
    def test_delivers_waves_and_stops_cleanly(self):
        seen = []
        stop_at = 2
        done = threading.Event()

        class Cfg:
            services = [svc(1, "tcp", "127.0.0.1:1")]
            poll_interval = 0.05

        def on_wave(rows, ts):
            seen.append(rows)
            if len(seen) >= stop_at:
                done.set()

        p = poller.Poller(lambda: Cfg(), on_wave, interval=0.05)
        p.start()
        self.assertTrue(done.wait(5), "poller never delivered a wave")
        p.stop()
        p.join(timeout=2)
        self.assertFalse(p.is_alive())
        self.assertEqual(p.waves, len(seen))
        self.assertIsNone(p.last_error)

    def test_failing_wave_does_not_kill_the_loop(self):
        calls = {"n": 0}

        def on_wave(rows, ts):
            calls["n"] += 1
            if calls["n"] == 1:
                raise RuntimeError("first wave explodes")

        class Cfg:
            services = []
            poll_interval = 0.05

        p = poller.Poller(lambda: Cfg(), on_wave, interval=0.05)
        p.start()
        deadline = time.time() + 3
        while calls["n"] < 3 and time.time() < deadline:
            time.sleep(0.02)
        p.stop()
        p.join(timeout=2)
        self.assertGreaterEqual(calls["n"], 3)
        self.assertGreaterEqual(p.errors, 1)

    def test_overrunning_wave_is_dropped_and_counted(self):
        class Cfg:
            services = []
            poll_interval = 0.05

        def on_wave(rows, ts):
            time.sleep(0.08)   # overruns the 0.04 budget

        p = poller.Poller(lambda: Cfg(), on_wave, interval=0.05)
        p.start()
        time.sleep(0.3)
        p.stop()
        p.join(timeout=2)
        self.assertGreater(p.dropped, 0)


if __name__ == "__main__":
    unittest.main()
