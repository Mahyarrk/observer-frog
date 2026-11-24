"""Probe engine: five kinds, one result triple, honest `unavailable`.

Result is always `(state, detail, latency_ms)` where state is one of
`up` / `degraded` / `down` / `unavailable`. `unavailable` means "this machine cannot answer
this question" (missing binary, no permission, unsupported OS) and must never be rendered as
`down`. `error` means the probe itself blew up, which is a bug, not an outage.
"""

from __future__ import annotations

import json
import os
import socket
import ssl
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

STATE_UP = "up"
STATE_DEGRADED = "degraded"
STATE_DOWN = "down"
STATE_UNAVAILABLE = "unavailable"

DETAIL_MAX = 120
USER_AGENT = "observer-frog/0.1"
_INSECURE = ssl.create_default_context()
_INSECURE.check_hostname = False
_INSECURE.verify_mode = ssl.CERT_NONE

# HTTP status codes that mean "the service answered and is alive as far as the probe cares".
# 401 is here because auth-gated endpoints (v2rayA, Syncthing) are healthy when they say 401.
ALIVE_CODES = frozenset({200, 301, 302, 307, 401})


def clip(text: str) -> str:
    """Cap detail strings before they reach storage or a notification."""
    text = " ".join(str(text).split())
    return text if len(text) <= DETAIL_MAX else text[: DETAIL_MAX - 1] + "…"


def _result(state: str, detail: str, ms: int) -> tuple[str, str, int]:
    return state, clip(detail), max(0, int(ms))


# ---------------------------------------------------------------- http

def probe_http(url: str, expect_status: int = 0, contains: str = "", timeout: float = 2.0,
               headers: dict | None = None) -> tuple[str, str, int]:
    t0 = time.time()
    hdrs = {"User-Agent": USER_AGENT}
    hdrs.update(headers or {})
    ctx = _INSECURE if url.startswith("https://") else None
    try:
        req = urllib.request.Request(url, headers=hdrs)
        with urllib.request.urlopen(req, timeout=timeout, context=ctx) as resp:
            code = resp.getcode()
            ms = int((time.time() - t0) * 1000)
            if expect_status and code != expect_status:
                return _result(STATE_DEGRADED, f"HTTP {code} (wanted {expect_status})", ms)
            if contains and contains not in resp.read(200_000).decode("utf-8", "replace"):
                return _result(STATE_DEGRADED, f"HTTP {code} (signature missing)", ms)
            return _result(STATE_UP, f"HTTP {code}", ms)
    except urllib.error.HTTPError as e:
        ms = int((time.time() - t0) * 1000)
        if expect_status and e.code == expect_status:
            return _result(STATE_UP, f"HTTP {e.code}", ms)
        if e.code in ALIVE_CODES:
            return _result(STATE_UP, f"HTTP {e.code}" + (" (Auth)" if e.code == 401 else ""), ms)
        return _result(STATE_DEGRADED, f"HTTP {e.code}", ms)
    except Exception as e:
        return _result(STATE_DOWN, f"{type(e).__name__}: {e}", int((time.time() - t0) * 1000))


# ---------------------------------------------------------------- tcp

def _split_hostport(target: str) -> tuple[str, int]:
    """Accept `host:port`, `[::1]:port` and `tcp://host:port`."""
    t = target.split("://", 1)[-1].rstrip("/")
    if t.startswith("["):
        host, _, rest = t[1:].partition("]")
        return host, int(rest.lstrip(":"))
    host, _, port = t.rpartition(":")
    if not host:
        raise ValueError(f"target {target!r} needs host:port")
    return host, int(port)


def probe_tcp(target: str, timeout: float = 1.5) -> tuple[str, str, int]:
    t0 = time.time()
    try:
        host, port = _split_hostport(target)
    except ValueError as e:
        return _result(STATE_UNAVAILABLE, str(e), 0)
    try:
        with socket.create_connection((host, port), timeout=timeout):
            return _result(STATE_UP, f"TCP {host}:{port}", int((time.time() - t0) * 1000))
    except (socket.timeout, TimeoutError):
        return _result(STATE_DOWN, f"timeout after {timeout:g}s", int((time.time() - t0) * 1000))
    except ConnectionRefusedError:
        return _result(STATE_DOWN, "connection refused", int((time.time() - t0) * 1000))
    except OSError as e:
        return _result(STATE_DOWN, f"{type(e).__name__}: {e}", int((time.time() - t0) * 1000))


# ---------------------------------------------------------------- process

def probe_process(pattern: str) -> tuple[str, str, int]:
    """Match a cmdline substring. Best-effort by nature: a hung process still reads present."""
    try:
        import psutil
    except ImportError:
        return _result(STATE_UNAVAILABLE, "psutil not installed", 0)
    t0 = time.time()
    try:
        needle = pattern.lower()
        for proc in psutil.process_iter(["cmdline", "name"]):
            try:
                cmd = " ".join(proc.info.get("cmdline") or []) or (proc.info.get("name") or "")
            except (psutil.NoSuchProcess, psutil.AccessDenied):
                continue
            if needle in cmd.lower():
                ms = int((time.time() - t0) * 1000)
                return _result(STATE_UP, clip(cmd), ms)
    except Exception as e:
        return _result(STATE_UNAVAILABLE, f"{type(e).__name__}: {e}", 0)
    return _result(STATE_DOWN, "not running", int((time.time() - t0) * 1000))


# ---------------------------------------------------------------- file_age

def probe_file_age(path: str, max_age_s: int) -> tuple[str, str, int]:
    """Freshness of a file: the health signal for job-shaped services."""
    p = Path(os.path.expanduser(path))
    if max_age_s <= 0:
        return _result(STATE_UNAVAILABLE, "max_age_s not set", 0)
    try:
        age = time.time() - p.stat().st_mtime
    except FileNotFoundError:
        return _result(STATE_DOWN, "file missing", 0)
    except OSError as e:
        return _result(STATE_UNAVAILABLE, f"{type(e).__name__}: {e}", 0)
    if age <= max_age_s:
        return _result(STATE_UP, f"updated {_fmt_age(age)} ago", 0)
    return _result(STATE_DEGRADED, f"stale: {_fmt_age(age)} old", 0)


def _fmt_age(seconds: float) -> str:
    s = int(max(0, seconds))
    if s < 90:
        return f"{s}s"
    if s < 5400:
        return f"{s // 60}m"
    if s < 172800:
        return f"{s // 3600}h"
    return f"{s // 86400}d"


# ---------------------------------------------------------------- file_json

def _dig(obj, dotted: str):
    cur = obj
    for part in dotted.split("."):
        if isinstance(cur, list):
            cur = cur[int(part)]
        elif isinstance(cur, dict):
            cur = cur[part]
        else:
            raise KeyError(part)
    return cur


def probe_file_json(path: str, field: str = "", contains: str = "") -> tuple[str, str, int]:
    """Read a JSON file and optionally assert a field's value contains a string."""
    p = Path(os.path.expanduser(path))
    try:
        raw = p.read_text(encoding="utf-8", errors="replace")
    except FileNotFoundError:
        return _result(STATE_DOWN, "file missing", 0)
    except OSError as e:
        return _result(STATE_UNAVAILABLE, f"{type(e).__name__}: {e}", 0)
    try:
        data = json.loads(raw)
    except json.JSONDecodeError as e:
        return _result(STATE_DEGRADED, f"invalid json: {e.msg}", 0)
    if not field:
        return _result(STATE_UP, "json ok", 0)
    try:
        value = _dig(data, field)
    except (KeyError, IndexError, ValueError):
        return _result(STATE_DEGRADED, f"field {field} missing", 0)
    shown = clip(json.dumps(value) if not isinstance(value, str) else value)
    if contains and contains.lower() not in str(value).lower():
        return _result(STATE_DEGRADED, f"{field}={shown}", 0)
    return _result(STATE_UP, f"{field}={shown}", 0)


# ---------------------------------------------------------------- dispatch

def run(service, node_platform: str | None = None) -> tuple[str, str, int]:
    """Run one Service (see config.Service) and return its triple."""
    kind = service.probe
    try:
        if kind == "http":
            return probe_http(service.target, service.expect_status, service.contains, service.timeout)
        if kind == "tcp":
            return probe_tcp(service.target, service.timeout)
        if kind == "process":
            return probe_process(service.target)
        if kind == "file_age":
            return probe_file_age(service.target, service.max_age_s)
        if kind == "file_json":
            return probe_file_json(service.target, service.field, service.contains)
        return _result(STATE_UNAVAILABLE, f"unknown probe {kind!r}", 0)
    except Exception as e:  # a probe bug is never an outage
        return _result(STATE_UNAVAILABLE, f"probe error: {type(e).__name__}: {e}", 0)


# ---------------------------------------------------------------- platform capabilities

def platform_name() -> str:
    return {"darwin": "darwin", "win32": "windows"}.get(sys.platform, "linux")


def has_binary(name: str) -> bool:
    from shutil import which
    return which(name) is not None


def unsupported_reason(kind: str, target: str = "") -> str | None:
    """Return why a probe cannot run here, or None when it can."""
    if kind == "process":
        try:
            import psutil  # noqa: F401
        except ImportError:
            return "psutil not installed"
    if kind == "file_age" or kind == "file_json":
        if target.startswith("~") and not os.path.isdir(os.path.expanduser("~")):
            return "home directory not readable"
    return None
