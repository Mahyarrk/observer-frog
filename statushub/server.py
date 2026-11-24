"""The HTTP server: /api/status, /api/history, /api/poll and the write endpoints.

Bounds, per DESIGN.md: loopback by default, no wildcard CORS on writes, `POLL_LOCK` so a
phone refreshing cannot stack forced polls, and flags read from SQLite at serve time (I6).
"""

from __future__ import annotations

import json
import logging
import ssl
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from . import db, metrics, state
from .config import Config
from .probes import clip

log = logging.getLogger("observer-frog.http")

ROOT = Path(__file__).resolve().parent.parent
STATIC_FILES = {
    "/": ("index.html", "text/html; charset=utf-8"),
    "/index.html": ("index.html", "text/html; charset=utf-8"),
    "/sw.js": ("sw.js", "application/javascript; charset=utf-8"),
    "/manifest.json": ("manifest.json", "application/manifest+json; charset=utf-8"),
    "/icons/icon-32.png": ("icons/icon-32.png", "image/png"),
    # iOS probes these root paths itself when adding to the home screen
    "/apple-touch-icon.png": ("icons/icon-180.png", "image/png"),
    "/apple-touch-icon-precomposed.png": ("icons/icon-180.png", "image/png"),
    "/favicon.ico": ("icons/icon-32.png", "image/png"),
    "/icons/icon-180.png": ("icons/icon-180.png", "image/png"),
    "/icons/icon-192.png": ("icons/icon-192.png", "image/png"),
    "/icons/icon-512.png": ("icons/icon-512.png", "image/png"),
}
MIN_FORCED_POLL_S = 2.0      # a phone holding refresh must not hammer every monitored target
HISTORY_POINTS = 7


class App:
    """Holds the shared runtime pieces; the request handler reads from here."""

    def __init__(self, cfg: Config, poller=None, notifier=None, started_ts: int | None = None):
        self.cfg = cfg
        self.poller = poller
        self.notifier = notifier
        self.started_ts = started_ts or int(time.time())
        self._snapshot: dict = {"timestamp": 0, "services": [], "devices": []}
        self._lock = threading.Lock()
        self._poll_lock = threading.Lock()
        self._device_cache: dict = {}
        self._device_ts = 0.0
        self.last_poll_error: str | None = None

    # -- snapshot ---------------------------------------------------------
    def set_snapshot(self, rows: list[dict], ts: int) -> None:
        snap = {
            "timestamp": int(ts),
            "services": rows,
            "devices": [self.device_card()],
            "confirm_polls": state.DOWN_CONFIRM_POLLS,
            "node_ts": int(ts),
        }
        with self._lock:
            self._snapshot = snap

    def snapshot(self, with_history: bool = True) -> dict:
        """Compile the payload. Flags and history are read now, never baked into the snapshot (I6)."""
        with self._lock:
            snap = dict(self._snapshot)
            snap["services"] = [dict(r) for r in snap.get("services", [])]
            snap["devices"] = [dict(d) for d in snap.get("devices", [])]

        snap["ignored"] = db.ignored_ids(self.cfg.db_path, self.cfg.node_id)
        if with_history:
            snap["history"] = self.history_map()
            snap["self_monitoring"] = {
                "waves": getattr(self.poller, "waves", 0),
                "dropped": getattr(self.poller, "dropped", 0),
                "errors": getattr(self.poller, "errors", 0),
                "last_error": getattr(self.poller, "last_error", None),
                "uptime_s": int(time.time()) - self.started_ts,
            }
        return snap

    def history_map(self, points: int = HISTORY_POINTS) -> dict[str, list[int]]:
        """Per-service sparkline: 1 for healthy, 0 otherwise, oldest first."""
        out: dict[str, list[int]] = {}
        if points <= 0:
            return out
        now = int(time.time())
        bucket = max(60, (points * self.cfg.poll_interval))
        with db.connect(self.cfg.db_path) as conn:
            for sid, ts, st in conn.execute(
                "SELECT service_id, timestamp, status FROM service_checks "
                "WHERE node_id = ? AND timestamp >= ? ORDER BY timestamp",
                (self.cfg.node_id, now - bucket * points),
            ):
                out.setdefault(sid, [0] * points)
                idx = min(points - 1, (now - ts) // bucket)
                if st in ("up", "degraded"):
                    out[sid][points - 1 - idx] = 1
        for sid in out:
            out[sid] = out[sid]
        return out

    # -- devices ----------------------------------------------------------
    def device_card(self, max_age_s: float = 5.0) -> dict:
        now = time.time()
        if now - self._device_ts > max_age_s or not self._device_cache:
            self._device_cache = metrics.device_card(self.cfg.node_id, self.cfg.node_name)
            self._device_ts = now
        return self._device_cache

    # -- polling ----------------------------------------------------------
    def force_poll(self) -> tuple[bool, dict]:
        """Run one wave on demand. Returns (ran, payload). A second caller gets 409."""
        if not self._poll_lock.acquire(blocking=False):
            return False, {"error": "a poll is already running"}
        try:
            from . import poller as poller_mod
            rows = poller_mod.sample(self.cfg.services)
            ts = int(time.time())
            changes = state.apply_wave(self.cfg.db_path, self.cfg.node_id,
                                       [state.Row(r["id"], r["name"], r["group"], r["status"],
                                                  r["detail"], r["latency_ms"]) for r in rows], ts)
            if self.notifier and changes:
                self.notifier.submit(changes)
            self.set_snapshot(rows, ts)
            return True, self.snapshot()
        except Exception as e:
            self.last_poll_error = f"{type(e).__name__}: {e}"
            log.exception("forced poll failed")
            return True, {"error": self.last_poll_error}
        finally:
            self._poll_lock.release()


def _json_bytes(payload) -> bytes:
    return json.dumps(payload, separators=(",", ":"), ensure_ascii=False).encode("utf-8")


def make_handler(app: App):
    class Handler(BaseHTTPRequestHandler):
        server_version = "observer-frog/0.1"
        sys_version = ""

        def log_message(self, format, *args):
            log.debug("%s %s", self.address_string(), format % args)

        # -- helpers ------------------------------------------------------
        def _send(self, code: int, body: bytes, ctype: str, cors: bool = False) -> None:
            self.send_response(code)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-cache, no-store, must-revalidate")
            if cors:
                self.send_header("Access-Control-Allow-Origin", "*")
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(body)

        def _json(self, code: int, payload, cors: bool = False) -> None:
            self._send(code, _json_bytes(payload), "application/json; charset=utf-8", cors=cors)

        def _read_json(self, limit: int = 8192) -> dict:
            length = int(self.headers.get("Content-Length") or 0)
            if length > limit:
                raise ValueError(f"body too large ({length} bytes)")
            raw = self.rfile.read(length) if length else b"{}"
            return json.loads(raw.decode("utf-8") or "{}")

        def _path(self) -> str:
            return urllib.parse.urlparse(self.path).path.rstrip("/") or "/"

        def _query(self) -> dict[str, str]:
            return dict(urllib.parse.parse_qsl(urllib.parse.urlparse(self.path).query))

        # -- routes -------------------------------------------------------
        def do_HEAD(self):
            self._route(head=True)

        def do_GET(self):
            self._route()

        def _route(self, head: bool = False) -> None:
            path = self._path()
            if path in ("/api/status", "/status/api/status"):
                self._json(200, app.snapshot(), cors=True)
                return
            if path in ("/api/poll", "/status/api/poll"):
                ok, payload = app.force_poll()
                self._json(200 if ok else 409, payload, cors=False)
                return
            if path in ("/api/history", "/status/api/history"):
                days = 30
                try:
                    days = max(1, min(365, int(self._query().get("days", "30"))))
                except ValueError:
                    pass
                self._json(200, build_history(app, days), cors=True)
                return
            if path in ("/api/health", "/status/api/health"):
                self._json(200, {"ok": True, "uptime_s": int(time.time()) - app.started_ts})
                return
            if path == "/status" or path == "/":
                path = "/"
            self._static(path)

        def _static(self, path: str) -> None:
            entry = STATIC_FILES.get(path)
            if entry is None:
                self._json(404, {"error": "not found"})
                return
            name, ctype = entry
            file_path = ROOT / name
            try:
                body = file_path.read_bytes()
            except OSError:
                self._send(500, f"missing {name}".encode(), "text/plain; charset=utf-8")
                return
            self._send(200, body, ctype)

        def do_POST(self):
            path = self._path()
            routes = {
                "/api/ignore": self._set_flag("ignored", "ignored"),
            }
            handler = routes.get(path) or routes.get(path.replace("/status", "", 1))
            if handler is None:
                self._json(404, {"ok": False, "error": "not found"})
                return
            handler()

        def _set_flag(self, table: str, field: str):
            def handle() -> None:
                try:
                    body = self._read_json()
                except Exception as e:
                    self._json(400, {"ok": False, "error": "bad request: " + clip(str(e))})
                    return
                sid = str(body.get("id") or "").strip()
                if not sid:
                    self._json(400, {"ok": False, "error": "missing id"})
                    return
                want = bool(body.get(field))
                try:
                    with db.connect(app.cfg.db_path) as conn:
                        if want:
                            conn.execute(f"INSERT OR IGNORE INTO {table} VALUES (?,?)",
                                         (app.cfg.node_id, sid))
                        else:
                            conn.execute(f"DELETE FROM {table} WHERE node_id = ? AND service_id = ?",
                                         (app.cfg.node_id, sid))
                except Exception as e:
                    self._json(500, {"ok": False, "error": clip(str(e))})
                    return
                ignored = db.ignored_ids(app.cfg.db_path, app.cfg.node_id)
                # Both shapes: the flag as a boolean for this id, and the full list the UI reads
                # back. The list was missing once, so the client kept its optimistic guess and
                # never learned the server's truth.
                self._json(200, {"ok": True, "id": sid, field: want,
                                 "ignored": ignored, "ignored_ids": ignored})
            return handle

    return Handler


def build_history(app: App, days: int = 30) -> dict:
    """The `/api/history` contract the UI reads: incidents list, uptime rows, and KPIs.

    KPI names match what the frontend reads (`ongoing_real`/`ongoing_ignored`); when they were
    absent the counts silently rendered as zero.

    One flag, one rule: `ignored` means "I know about this". It drives the red/amber split
    (red = not ignored, amber = ignored) AND keeps the service out of uptime/MTTR, so the number
    you see in History always describes services you actually care about.
    """
    now = int(time.time())
    incidents = state.recent_incidents(app.cfg.db_path, app.cfg.node_id, days=days)
    rows = state.uptime_rows(app.cfg.db_path, app.cfg.node_id, days=days)

    unacknowledged = [i for i in incidents if not i["ignored"]]
    closed_real = [i for i in unacknowledged if not i["ongoing"]]

    kpi = {
        "incidents_30d": len(incidents),
        "incidents_real": len(unacknowledged),
        "incidents_ignored": len(incidents) - len(unacknowledged),
        "ongoing": sum(1 for i in incidents if i["ongoing"]),
        "ongoing_real": sum(1 for i in unacknowledged if i["ongoing"]),
        "ongoing_ignored": sum(1 for i in incidents if i["ongoing"] and i["ignored"]),
        "services_affected": len({i["service_id"] for i in unacknowledged}),
        # MTTR only means something with a sample; report n so the UI can say "no recoveries yet"
        "mttr_s": int(sum(i["duration_s"] for i in closed_real) / len(closed_real)) if closed_real else None,
        "mttr_n": len(closed_real),
        # worst uptime excludes ignored services because uptime_rows() drops them (I5)
        "worst_uptime": rows[0]["uptime_pct"] if rows else None,
        "worst_service": rows[0]["name"] if rows else None,
        "uptime_services": len(rows),
        # ignored services are absent from the uptime table by design (I5); say how many
        "uptime_excluded": len(db.ignored_ids(app.cfg.db_path, app.cfg.node_id)),
    }
    return {"generated": now, "window_days": days, "incidents": incidents,
            "uptime": rows, "kpi": kpi}


def serve(cfg: Config, app: App | None = None, tls_cert: str | None = None,
          tls_key: str | None = None) -> ThreadingHTTPServer:
    """Bind, then start self-monitoring — never the other way round (invariant I9)."""
    app = app or App(cfg)
    httpd = ThreadingHTTPServer((cfg.bind, cfg.port), make_handler(app))
    if tls_cert and tls_key:
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.load_cert_chain(tls_cert, tls_key)
        httpd.socket = ctx.wrap_socket(httpd.socket, server_side=True)
    return httpd
