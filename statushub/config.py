"""Configuration: TOML on disk, validated into frozen dataclasses, reloaded on mtime.

No third-party imports. A broken edit never takes the poller down: the last good config is kept.
"""

from __future__ import annotations

import os
import socket
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path

PROBES = ("http", "tcp", "process", "file_age", "file_json")
SINK_TYPES = ("ntfy",)
DEFAULT_PORT = 8770          # distinct from the legacy 8765 so both can run side by side
DEFAULT_POLL_INTERVAL = 30


class ConfigError(Exception):
    """Invalid configuration. The message names the file and the offending field."""


@dataclass(frozen=True)
class Service:
    id: str
    probe: str
    target: str
    name: str = ""
    group: str = "Services"
    category: str = ""
    expect_status: int = 0      # http: 0 means "any 2xx"
    contains: str = ""          # http: required body substring; file_json: required value
    field: str = ""             # file_json: dotted key to read
    max_age_s: int = 0          # file_age: staleness threshold
    timeout: float = 2.0
    ports: str = ""
    children: tuple[dict, ...] = ()


@dataclass(frozen=True)
class Sink:
    type: str
    url: str = ""
    topic: str = ""
    token_env: str = ""
    use_proxy: bool = False


@dataclass(frozen=True)
class Config:
    path: Path | None
    node_id: str
    node_name: str
    serve: bool
    bind: str
    port: int
    poll_interval: int
    db_path: Path
    services: tuple[Service, ...] = ()
    sinks: tuple[Sink, ...] = ()

    def service_ids(self) -> set[str]:
        return {s.id for s in self.services}


def config_home() -> Path:
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support" / "observer-frog"
    if os.name == "nt":
        base = os.environ.get("APPDATA") or (Path.home() / "AppData" / "Roaming")
        return Path(base) / "observer-frog"
    base = os.environ.get("XDG_CONFIG_HOME") or (Path.home() / ".config")
    return Path(base) / "observer-frog"


def state_home() -> Path:
    if sys.platform == "darwin":
        return config_home()
    if os.name == "nt":
        base = os.environ.get("LOCALAPPDATA") or (Path.home() / "AppData" / "Local")
        return Path(base) / "observer-frog"
    base = os.environ.get("XDG_STATE_HOME") or (Path.home() / ".local" / "state")
    return Path(base) / "observer-frog"


def default_path() -> Path:
    return config_home() / "config.toml"


def default_db_path() -> Path:
    return state_home() / "status.db"


def _tables(value, field: str, where: str) -> list[dict]:
    """Accept both `[[service]]` (array of tables) and a single `[service]` table."""
    if value is None:
        return []
    if isinstance(value, dict):
        return [value]
    if isinstance(value, list) and all(isinstance(v, dict) for v in value):
        return value
    raise ConfigError(f"{where}: {field} must be a table or an array of tables")


def _as_service(raw: dict, where: str) -> Service:
    sid = str(raw.get("id") or "").strip()
    if not sid:
        raise ConfigError(f"{where}: service without an id")
    probe = str(raw.get("probe") or "").strip()
    if probe not in PROBES:
        raise ConfigError(f"{where}: service '{sid}' has probe {probe!r}; expected one of {', '.join(PROBES)}")
    target = str(raw.get("target") or "").strip()
    if not target:
        raise ConfigError(f"{where}: service '{sid}' has no target")
    return Service(
        id=sid,
        probe=probe,
        target=target,
        name=str(raw.get("name") or sid),
        group=str(raw.get("group") or "Services"),
        category=str(raw.get("category") or ""),
        expect_status=int(raw.get("expect_status") or 0),
        contains=str(raw.get("contains") or ""),
        field=str(raw.get("field") or ""),
        max_age_s=int(raw.get("max_age_s") or 0),
        timeout=float(raw.get("timeout") or 2.0),
        ports=str(raw.get("ports") or ""),
        children=tuple(raw.get("children") or ()),
    )


def _as_sink(raw: dict, where: str) -> Sink:
    kind = str(raw.get("type") or "").strip()
    if kind not in SINK_TYPES:
        raise ConfigError(f"{where}: unknown sink type {kind!r}; expected one of {', '.join(SINK_TYPES)}")
    return Sink(
        type=kind,
        url=str(raw.get("url") or ""),
        topic=str(raw.get("topic") or ""),
        token_env=str(raw.get("token_env") or ""),
        use_proxy=bool(raw.get("use_proxy", False)),
    )


def load(path: str | os.PathLike | None = None) -> Config:
    """Read the config, or return defaults when the file does not exist yet (zero-config boot)."""
    p = Path(path) if path else default_path()
    where = str(p)
    raw: dict = {}
    if p.is_file():
        try:
            raw = tomllib.loads(p.read_text(encoding="utf-8"))
        except tomllib.TOMLDecodeError as e:
            raise ConfigError(f"{where}: {e}") from None

    node = raw.get("node") or {}
    serve_cfg = raw.get("serve") or {}
    services = tuple(_as_service(s, where) for s in _tables(raw.get("service"), "service", where))
    ids = [s.id for s in services]
    dupes = {i for i in ids if ids.count(i) > 1}
    if dupes:
        raise ConfigError(f"{where}: duplicate service id(s): {', '.join(sorted(dupes))}")
    sinks = tuple(_as_sink(s, where) for s in _tables(raw.get("sink"), "sink", where))

    return Config(
        path=p if p.is_file() else None,
        node_id=str(node.get("id") or "local"),
        node_name=str(node.get("name") or socket.gethostname()),
        serve=bool(serve_cfg.get("enabled", True)),
        bind=str(serve_cfg.get("bind") or "127.0.0.1"),
        port=int(serve_cfg.get("port") or DEFAULT_PORT),
        poll_interval=int(raw.get("poll_interval") or DEFAULT_POLL_INTERVAL),
        db_path=Path(raw.get("db_path") or default_db_path()),
        services=services,
        sinks=sinks,
    )


class Watcher:
    """Holds the current config and swaps it when the file's mtime changes.

    A file that fails to parse keeps the previous config in place (shortcut: mtime only, so a
    same-second edit is missed; upgrade to content hashing if that ever bites).
    """

    def __init__(self, path: str | os.PathLike | None = None):
        self.config = load(path)
        self._mtime = self._stat()
        self.error: str | None = None

    def _stat(self) -> float | None:
        p = self.config.path
        try:
            return p.stat().st_mtime if p else None
        except OSError:
            return None

    def refresh(self) -> Config:
        mtime = self._stat()
        if mtime != self._mtime:
            self._mtime = mtime
            try:
                self.config = load(self.config.path or default_path())
                self.error = None
            except ConfigError as e:
                self.error = str(e)
        return self.config
