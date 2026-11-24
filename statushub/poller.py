"""The poller: fixed-rate waves, bounded concurrency, one process snapshot per wave.

Invariants honoured here:
  I12 — a single process snapshot serves every `process` / `ram_mb` lookup in a wave.
  Discipline — a wave that overruns its interval is dropped and logged, never stacked.
"""

from __future__ import annotations

import logging
import threading
import time
from concurrent.futures import ThreadPoolExecutor

from . import probes

log = logging.getLogger("observer-frog.poller")

MAX_WORKERS = 12          # bounded: one hung LAN target must not consume the wave
WAVE_BUDGET_FRACTION = 0.8  # a wave must finish inside 80% of the interval to count


class Snapshot:
    """Start-of-wave process table, so N services cost one scan instead of N scans."""

    def __init__(self):
        self.rows: list[tuple[str, str, float]] = []   # (pid, cmdline_lower, rss_mb)
        self.ok = False
        self._reload()

    def _reload(self):
        try:
            import psutil
        except ImportError:
            return
        rows = []
        for proc in psutil.process_iter(["cmdline", "name", "memory_info"]):
            try:
                info = proc.info
                cmd = " ".join(info.get("cmdline") or []) or (info.get("name") or "")
                mem = info.get("memory_info")
                rss = round(mem.rss / (1024 * 1024), 1) if mem else 0.0
            except Exception:
                continue
            rows.append((cmd.lower(), cmd, rss))
        self.rows = rows
        self.ok = True

    def match(self, needle: str) -> tuple[bool, str, float]:
        """Return (found, cmdline, rss_mb) for the first process whose cmdline contains needle."""
        needle = needle.lower()
        for low, cmd, rss in self.rows:
            if needle in low:
                return True, cmd, rss
        return False, "", 0.0

    def size(self) -> int:
        return len(self.rows)


def sample(services, snapshot: Snapshot | None = None) -> list[dict]:
    """Probe every service concurrently and return payload rows in configuration order."""
    t0 = time.time()
    snap = snapshot if snapshot is not None else Snapshot()
    rows: list[dict] = [{}] * len(services)

    def one(i: int, svc) -> None:
        if svc.probe == "process":
            found, cmd, rss = snap.match(svc.target)
            state = probes.STATE_UP if found else probes.STATE_DOWN
            detail = cmd if found else "not running"
            triple = (state, probes.clip(detail), 0)
        else:
            triple = probes.run(svc)
        state, detail, ms = triple
        rows[i] = {
            "id": svc.id,
            "name": svc.name or svc.id,
            "group": svc.group,
            "status": state,
            "detail": detail,
            "latency_ms": ms,
            "ports": svc.ports,
            "address": svc.target,
        }
        ram = 0.0
        if svc.probe == "process":
            _, _, ram = snap.match(svc.target)
        if ram:
            rows[i]["ram_mb"] = ram

    if services:
        with ThreadPoolExecutor(max_workers=min(MAX_WORKERS, len(services))) as pool:
            futures = [pool.submit(one, i, s) for i, s in enumerate(services)]
            for f in futures:
                f.result()

    log.debug("wave: %d services in %.0f ms", len(services), (time.time() - t0) * 1000)
    return rows


class Poller(threading.Thread):
    """Owns the wave schedule. `on_wave(rows, ts)` is called from the poller thread."""

    def __init__(self, config_getter, on_wave, interval: float | None = None):
        super().__init__(daemon=True, name="poller")
        self._config_getter = config_getter
        self._on_wave = on_wave
        self._interval = interval
        self._halt = threading.Event()
        self.dropped = 0
        self.waves = 0
        self.errors = 0
        self.last_error: str | None = None

    def stop(self) -> None:
        self._halt.set()

    def run(self) -> None:
        while not self._halt.is_set():
            cfg = self._config_getter()
            interval = self._interval or cfg.poll_interval
            started = time.time()
            try:
                rows = sample(cfg.services)
                self.waves += 1
                self.last_error = None
                self._on_wave(rows, int(started))
            except Exception as e:  # a failed wave must not kill the loop
                self.errors += 1
                self.last_error = f"{type(e).__name__}: {e}"
                log.exception("wave failed")
            elapsed = time.time() - started
            budget = interval * WAVE_BUDGET_FRACTION
            if elapsed > budget:
                self.dropped += 1
                log.warning("wave took %.1fs (budget %.1fs): dropping this tick", elapsed, budget)
                sleep_for = max(0.0, interval - elapsed)
            else:
                sleep_for = interval - elapsed
            self._halt.wait(sleep_for)
