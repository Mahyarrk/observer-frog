"""Catalog: valid data, instantiation to probe kinds, and port sanity."""

import tempfile
import unittest
from pathlib import Path

from statushub import catalog
from statushub.config import ConfigError, Service
from tests.helpers import entry as get_entry

class CatalogDataTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.entries = catalog.load()

    def test_catalog_loads_and_is_not_trivial(self):
        self.assertGreaterEqual(len(self.entries), 30)

    def test_every_entry_has_a_unique_id_and_a_valid_probe(self):
        ids = [e.id for e in self.entries]
        self.assertEqual(len(ids), len(set(ids)))
        for e in self.entries:
            self.assertIn(e.probe, ("http", "tcp", "process", "file_age", "file_json"), e.id)

    def test_http_and_tcp_entries_have_default_ports(self):
        for e in self.entries:
            if e.probe in ("http", "tcp"):
                self.assertGreater(e.default_port, 0, f"{e.id} has no default_port")

    def test_http_entries_point_at_a_verify_path(self):
        for e in self.entries:
            if e.probe == "http":
                self.assertTrue(e.verify_path.startswith("/"), f"{e.id} verify_path must start with /")

    def test_most_http_entries_carry_a_body_signature(self):
        """A signature is what stops a coincidental listener being read as this service."""
        http = [e for e in self.entries if e.probe == "http"]
        with_sig = [e for e in http if e.has_signature()]
        # Auth-gated health endpoints legitimately return an empty body, so a signature cannot be
        # universal; the rule is "signature where the body is public", and the ratio guards regression.
        self.assertGreaterEqual(len(with_sig) / len(http), 0.5,
                                f"only {len(with_sig)}/{len(http)} HTTP entries have a signature")

    def test_process_entries_have_a_match_pattern(self):
        for e in self.entries:
            if e.probe == "process":
                self.assertTrue(e.process_match, f"{e.id} needs process_match")

    def test_ids_are_lowercase_and_stable_looking(self):
        for e in self.entries:
            self.assertEqual(e.id, e.id.lower())
            self.assertNotIn(" ", e.id)

    def test_groups_are_capitalised_display_strings(self):
        for e in self.entries:
            self.assertTrue(e.group[0].isupper(), f"{e.id}: group {e.group!r} not capitalised")


class InstantiateTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.entries = catalog.load()

    def test_http_entry_uses_default_port_and_signature(self):
        e = get_entry("syncthing")
        svc = catalog.instantiate(e)
        self.assertEqual(svc.probe, "http")
        self.assertEqual(svc.target, "http://127.0.0.1:8384/rest/noauth/health")
        self.assertEqual(svc.contains, "OK")
        self.assertEqual(svc.ports, "8384")

    def test_port_override_wins(self):
        e = get_entry("9router")
        svc = catalog.instantiate(e, port=20127)
        self.assertEqual(svc.target, "http://127.0.0.1:20127/")
        self.assertEqual(svc.ports, "20127")

    def test_remote_host_and_scheme(self):
        e = get_entry("grafana")
        svc = catalog.instantiate(e, host="192.0.2.10", port=3001, scheme="https")
        self.assertEqual(svc.target, "https://192.0.2.10:3001/api/health")

    def test_instance_slug_avoids_collision(self):
        e = get_entry("sonarr")
        a = catalog.instantiate(e)
        b = catalog.instantiate(e, port=8990, instance="2")
        self.assertNotEqual(a.id, b.id)
        self.assertEqual(b.id, "sonarr-2")

    def test_tcp_entry_builds_host_port(self):
        e = get_entry("postgres")
        svc = catalog.instantiate(e, host="127.0.0.1", port=5433)
        self.assertEqual((svc.probe, svc.target), ("tcp", "127.0.0.1:5433"))

    def test_process_entry_uses_match_pattern(self):
        e = get_entry("docker")
        svc = catalog.instantiate(e)
        self.assertEqual((svc.probe, svc.target), ("process", "dockerd"))

    def test_rendered_toml_round_trips_through_the_config_loader(self):
        import tomllib
        e = get_entry("syncthing")
        text = catalog.to_toml(catalog.instantiate(e))
        parsed = tomllib.loads(text)
        svc = parsed["service"][0]
        self.assertEqual(svc["id"], "syncthing")
        self.assertEqual(svc["probe"], "http")
        self.assertIn("contains", svc)

    def test_catalog_covers_the_common_self_hosted_stack(self):
        """The catalog exists to spare people writing probes for everyday software."""
        ids = {e.id for e in self.entries}
        for expected in ("nginx", "syncthing", "docker", "postgres", "grafana",
                         "uptime-kuma", "vaultwarden", "home-assistant"):
            self.assertIn(expected, ids, f"catalog cannot express {expected}")

    def test_instantiated_services_are_valid_config_services(self):
        for e in self.entries:
            if e.probe == "process" or e.probe in ("http", "tcp"):
                svc = catalog.instantiate(e)
                self.assertIsInstance(svc, Service)
                self.assertTrue(svc.id and svc.target)


class BadCatalogTests(unittest.TestCase):
    def _load(self, text):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        p = Path(tmp.name) / "c.toml"
        p.write_text(text)
        return catalog.load(p)

    def test_unknown_probe_rejected(self):
        with self.assertRaises(ConfigError):
            self._load('[[catalog]]\nid = "x"\nprobe = "magic"\n')

    def test_missing_port_for_http_rejected(self):
        with self.assertRaises(ConfigError):
            self._load('[[catalog]]\nid = "x"\nprobe = "http"\n')

    def test_duplicate_id_rejected(self):
        with self.assertRaises(ConfigError):
            self._load('[[catalog]]\nid = "x"\nprobe = "tcp"\ndefault_port = 1\n'
                       '[[catalog]]\nid = "x"\nprobe = "tcp"\ndefault_port = 2\n')


if __name__ == "__main__":
    unittest.main()
