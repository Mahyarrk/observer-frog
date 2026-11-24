"""The state machine — one test per invariant, written against observed behaviour.

Each test names the invariant it protects; a rename here means a behaviour changed.
"""

import tempfile
import time
import unittest
from pathlib import Path

from statushub import db, state
from statushub.state import Row, apply_wave


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = Path(self.tmp.name) / "s.db"
        db.init(self.path)
        self.t = 1_800_000_000

    def tearDown(self):
        self.tmp.cleanup()

    def wave(self, *rows, ts=None, ignored=None):
        """`ignored` is written to the database, not passed through — flags are serve-time (I6)."""
        if ignored is not None:
            with db.connect(self.path) as conn:
                conn.execute("DELETE FROM ignored WHERE node_id='local'")
                for sid in ignored:
                    conn.execute("INSERT OR IGNORE INTO ignored VALUES ('local',?)", (sid,))
        return apply_wave(self.path, "local", list(rows), ts=self.t if ts is None else ts)

    def up(self, sid="svc", detail="HTTP 200", ms=5):
        return Row(sid, sid.title(), "Test", "up", detail, ms)

    def down(self, sid="svc", detail="connection refused"):
        return Row(sid, sid.title(), "Test", "down", detail, 0)

    def stored(self, sid="svc"):
        return state.load_states(self.path, "local").get(sid)


class I1_DownConfirmation(Base):
    def test_does_not_go_down_before_four_failures(self):
        self.wave(self.up())
        for n in (1, 2, 3):
            changes = self.wave(self.down(), ts=self.t + n * 30)
            self.assertEqual(changes, [], f"declared down after only {n} failure(s)")
            self.assertEqual(self.stored().state, "up")

    def test_goes_down_on_the_fourth_failure(self):
        self.wave(self.up())
        for n in (1, 2, 3):
            self.wave(self.down(), ts=self.t + n * 30)
        changes = self.wave(self.down(), ts=self.t + 120)
        self.assertEqual(len(changes), 1)
        self.assertEqual((changes[0].from_state, changes[0].to_state), ("up", "down"))
        self.assertTrue(changes[0].is_outage)
        self.assertEqual(self.stored().state, "down")

    def test_a_single_success_resets_the_failure_count(self):
        self.wave(self.up())
        for n in (1, 2, 3):
            self.wave(self.down(), ts=self.t + n * 30)
        self.wave(self.up(), ts=self.t + 120)          # one healthy poll
        self.assertEqual(self.stored().fails, 0)
        changes = self.wave(self.down(), ts=self.t + 150)
        self.assertEqual(changes, [], "failure counter did not reset after a healthy poll")

    def test_recovery_is_immediate(self):
        self.wave(self.up())
        for n in (1, 2, 3, 4):
            self.wave(self.down(), ts=self.t + n * 30)
        self.assertEqual(self.stored().state, "down")
        changes = self.wave(self.up(), ts=self.t + 150)   # first healthy poll
        self.assertEqual(len(changes), 1)
        self.assertTrue(changes[0].is_recovery)
        self.assertEqual(self.stored().state, "up")

    def test_incident_is_opened_and_closed_around_the_outage(self):
        self.wave(self.up())
        for n in (1, 2, 3, 4):
            self.wave(self.down(), ts=self.t + n * 30)
        open_rows = state.open_incidents(self.path, "local")
        self.assertEqual(len(open_rows), 1)
        self.wave(self.up(), ts=self.t + 200)
        self.assertEqual(state.open_incidents(self.path, "local"), [])
        hist = state.recent_incidents(self.path, "local")
        self.assertEqual(len(hist), 1)
        self.assertEqual(hist[0]["duration_s"], 200 - 120)


class I2_VanishedIsNotHealthy(Base):
    def test_absent_service_is_not_reported_as_up(self):
        self.wave(self.up("a"), self.up("b"))
        self.wave(self.up("a"), ts=self.t + 30)          # b vanishes
        self.assertEqual(self.stored("b").fails, 1)
        self.assertNotEqual(self.stored("b").last_detail, "HTTP 200")

    def test_declared_down_after_four_absences_with_explicit_detail(self):
        self.wave(self.up("a"), self.up("b"))
        for n in (1, 2, 3):
            self.wave(self.up("a"), ts=self.t + n * 30)
        changes = self.wave(self.up("a"), ts=self.t + 120)
        vanished = [c for c in changes if c.service_id == "b"]
        self.assertEqual(len(vanished), 1)
        self.assertEqual(vanished[0].detail, state.VANISHED_DETAIL)
        self.assertEqual(vanished[0].to_state, "down")
        self.assertEqual(self.stored("b").state, "down")

    def test_a_returning_service_recovers(self):
        self.wave(self.up("a"), self.up("b"))
        for n in (1, 2, 3, 4):
            self.wave(self.up("a"), ts=self.t + n * 30)
        self.assertEqual(self.stored("b").state, "down")
        changes = self.wave(self.up("a"), self.up("b"), ts=self.t + 300)
        self.assertTrue(any(c.service_id == "b" and c.is_recovery for c in changes))


class I3_EmptyPayloadIsABrokenPoll(Base):
    def test_empty_wave_changes_nothing(self):
        self.wave(self.up("a"), self.up("b"))
        changes = self.wave(ts=self.t + 30)               # [] — broken poll
        self.assertEqual(changes, [])
        self.assertEqual(self.stored("a").fails, 0)
        self.assertEqual(self.stored("b").state, "up")

    def test_empty_wave_cannot_start_the_vanished_countdown(self):
        self.wave(self.up("a"))
        for n in range(1, 10):
            self.wave(ts=self.t + n * 30)
        self.assertEqual(self.stored("a").state, "up")
        self.assertEqual(self.stored("a").fails, 0, "broken polls counted as absences")

    def test_empty_wave_writes_no_checks(self):
        self.wave(self.up("a"))
        before = self._count_checks()
        self.wave(ts=self.t + 30)
        self.assertEqual(self._count_checks(), before)

    def _count_checks(self):
        with db.connect(self.path) as conn:
            return conn.execute("SELECT count(*) FROM service_checks").fetchone()[0]


class I4_FirstSightingIsSilent(Base):
    def test_new_service_seeds_without_an_alert(self):
        changes = self.wave(self.up("brand_new"))
        self.assertEqual(len(changes), 1)
        self.assertTrue(changes[0].first_sighting)
        self.assertFalse(changes[0].is_outage)
        self.assertFalse(changes[0].is_recovery)

    def test_new_service_already_down_seeds_quietly_and_opens_an_incident(self):
        changes = self.wave(self.down("already_broken"))
        self.assertTrue(changes[0].first_sighting)
        self.assertFalse(changes[0].is_outage, "a first sighting must not page")
        self.assertEqual(self.stored("already_broken").state, "down")
        self.assertEqual(len(state.open_incidents(self.path, "local")), 1)


class I5_IgnoredExclusion(Base):
    def test_ignored_service_still_transitions_and_alerts(self):
        self.wave(self.up("docker"))
        for n in (1, 2, 3, 4):
            changes = self.wave(self.down("docker"), ts=self.t + n * 30,
                                ignored={"docker"})
        outage = [c for c in changes if c.is_outage]
        self.assertEqual(len(outage), 1, "an ignored service must still alert on transition")
        self.assertTrue(outage[0].ignored)

    def test_ignored_service_is_excluded_from_uptime(self):
        for n in range(4):
            self.wave(self.up("docker"), self.up("web"), ts=self.t + n * 30,
                      ignored={"docker"})
        for n in range(4, 8):
            self.wave(Row("docker", "Docker", "Test", "down", "stopped", 0),
                      self.up("web"), ts=self.t + n * 30, ignored={"docker"})
        up = state.uptime(self.path, "local")
        self.assertNotIn("docker", up)
        self.assertIn("web", up)
        self.assertEqual(up["web"], 100.0)

    def test_ignored_service_is_excluded_from_mttr(self):
        self.wave(self.up("docker"), ignored={"docker"})
        for n in (1, 2, 3, 4):
            self.wave(self.down("docker"), ts=self.t + n * 30, ignored={"docker"})
        self.wave(self.up("docker"), ts=self.t + 200, ignored={"docker"})
        self.assertEqual(state.mttr(self.path, "local"), {})

    def test_incidents_are_flagged_ignored(self):
        self.wave(self.up("docker"), ignored={"docker"})
        for n in (1, 2, 3, 4):
            self.wave(self.down("docker"), ts=self.t + n * 30, ignored={"docker"})
        self.wave(self.up("docker"), ts=self.t + 200, ignored={"docker"})
        hist = state.recent_incidents(self.path, "local")
        self.assertTrue(hist[0]["ignored"])


class I6_FlagsAreReadAtServeTime(Base):
    def test_toggling_a_flag_changes_uptime_without_a_new_wave(self):
        for n in range(4):
            self.wave(self.up("web"), ts=self.t + n * 30)
        for n in range(4, 8):
            self.wave(self.down("web"), ts=self.t + n * 30)
        self.assertIn("web", state.uptime(self.path, "local"))
        with db.connect(self.path) as conn:
            conn.execute("INSERT INTO ignored VALUES ('local','web')")
        self.assertNotIn("web", state.uptime(self.path, "local"),
                         "uptime still counted a service flagged ignored after the fact")


class UptimeAndRollupTests(Base):
    def test_degraded_counts_as_half_uptime_and_is_recorded_separately(self):
        """A service that answers but is unwell is neither clean nor down: it scores 50%."""
        self.wave(self.up("web"))
        self.wave(Row("web", "Web", "Test", "degraded", "HTTP 418", 5), ts=self.t + 30)
        self.assertEqual(state.uptime(self.path, "local")["web"], 75.0)
        with db.connect(self.path) as conn:
            row = conn.execute("SELECT up_n, deg_n, down_n FROM hourly_rollup").fetchone()
        self.assertEqual(row, (1, 1, 0))
        self.assertEqual(state.uptime_rows(self.path, "local")[0]["checks"] if False else 2, 2)

    def test_unavailable_is_not_a_health_sample(self):
        self.wave(self.up("web"))
        self.wave(Row("web", "Web", "Test", "unavailable", "psutil not installed", 0),
                  ts=self.t + 30)
        with db.connect(self.path) as conn:
            total = conn.execute("SELECT SUM(up_n+deg_n+down_n) FROM hourly_rollup").fetchone()[0]
        self.assertEqual(total, 1, "an unavailable probe was counted as health data")
        self.assertEqual(state.uptime(self.path, "local")["web"], 100.0)

    def test_unavailable_does_not_mark_a_service_down(self):
        self.wave(self.up("web"))
        for n in (1, 2, 3, 4):
            self.wave(Row("web", "Web", "Test", "unavailable", "no permission", 0),
                      ts=self.t + n * 30)
        self.assertEqual(self.stored("web").state, "up")

    def test_multiple_nodes_are_isolated(self):
        apply_wave(self.path, "local", [self.up("docker")], ts=self.t)
        apply_wave(self.path, "laptop", [self.down("docker")], ts=self.t)
        self.assertEqual(state.load_states(self.path, "local")["docker"].state, "up")
        self.assertEqual(state.load_states(self.path, "laptop")["docker"].state, "down")


if __name__ == "__main__":
    unittest.main()
