"""Self-match protection: the probe's own command line must never satisfy a process probe.

This is the bug that makes every service read `up` forever, so it gets a test rather than a fix
buried in a helper.
"""

import os
import subprocess
import sys
import unittest

from statushub import poller, probes


class SelfMatchTests(unittest.TestCase):
    def test_own_pid_is_excluded(self):
        self.assertIn(os.getpid(), probes.self_exclusions())

    def test_parent_pid_is_excluded(self):
        self.assertIn(os.getppid(), probes.self_exclusions())

    def test_a_pattern_that_appears_only_in_our_own_argv_does_not_match(self):
        """The regression: `python -c "... unique_needle_xyz ..."` must not read as a service."""
        needle = "unique_needle_9f3a1c"
        # The needle appears nowhere except this test's own command line.
        state, detail, _ = probes.probe_process(needle)
        self.assertEqual(state, "down",
                         f"probe matched its own command line: {detail!r}")

    def test_snapshot_excludes_its_own_process(self):
        snap = poller.Snapshot()
        self.assertTrue(snap.excluded)
        self.assertIn(os.getpid(), snap.excluded)

    def test_snapshot_does_not_match_the_probe_invocation(self):
        snap = poller.Snapshot()
        found, cmd, _ = snap.match("unique_needle_9f3a1c")
        self.assertFalse(found, f"snapshot matched {cmd!r}")

    def test_a_real_process_is_still_found(self):
        """Exclusion must not blind the probe to everything."""
        state, detail, _ = probes.probe_process("python")
        self.assertEqual(state, "up", f"a running python was not found: {detail!r}")

    def test_excluding_the_parent_does_not_exclude_the_service(self):
        """A child of ours that represents a service is still visible — only ancestors are hidden."""
        proc = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(2)",
                                 "unique_child_service_marker"])
        try:
            snap = poller.Snapshot()
            found, cmd, _ = snap.match("unique_child_service_marker")
            self.assertTrue(found, "a genuine child process was wrongly excluded")
        finally:
            proc.kill()
            proc.wait()


if __name__ == "__main__":
    unittest.main()
