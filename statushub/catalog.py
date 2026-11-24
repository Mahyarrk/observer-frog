"""The service catalog: shipped data that compiles to probe kinds.

An entry is a *template*, not a service. `instantiate()` turns one catalog row plus a user's
host/port into a `config.Service`. Ports are defaults, never identities — an HTTP entry carries a
verify signature so a coincidental listener on the default port is not mistaken for the service.
"""

from __future__ import annotations

import tomllib
from dataclasses import dataclass
from pathlib import Path

from .config import PROBES, ConfigError, Service

CATALOG_PATH = Path(__file__).resolve().parent.parent / "catalog" / "services.toml"


@dataclass(frozen=True)
class Entry:
    id: str
    name: str
    group: str
    probe: str
    default_port: int = 0
    verify_path: str = ""
    verify_contains: str = ""
    process_match: str = ""
    notes: str = ""

    @property
    def is_http(self) -> bool:
        return self.probe == "http"

    def has_signature(self) -> bool:
        """An HTTP entry without a body signature can only be trusted by status code."""
        return self.is_http and bool(self.verify_contains)


def load(path: str | Path | None = None) -> list[Entry]:
    p = Path(path) if path else CATALOG_PATH
    try:
        raw = tomllib.loads(p.read_text(encoding="utf-8"))
    except FileNotFoundError:
        raise ConfigError(f"catalog not found at {p}") from None
    except tomllib.TOMLDecodeError as e:
        raise ConfigError(f"{p}: {e}") from None

    rows = raw.get("catalog") or []
    if isinstance(rows, dict):
        rows = [rows]
    entries: list[Entry] = []
    seen: set[str] = set()
    for i, r in enumerate(rows):
        cid = str(r.get("id") or "").strip()
        if not cid:
            raise ConfigError(f"{p}: catalog entry #{i + 1} has no id")
        if cid in seen:
            raise ConfigError(f"{p}: duplicate catalog id {cid!r}")
        seen.add(cid)
        probe = str(r.get("probe") or "").strip()
        if probe not in PROBES:
            raise ConfigError(f"{p}: {cid}: probe {probe!r} is not one of {', '.join(PROBES)}")
        if probe in ("http", "tcp") and not int(r.get("default_port") or 0):
            raise ConfigError(f"{p}: {cid}: probe {probe} needs a default_port")
        entries.append(
            Entry(
                id=cid,
                name=str(r.get("name") or cid),
                group=str(r.get("group") or "Services"),
                probe=probe,
                default_port=int(r.get("default_port") or 0),
                verify_path=str(r.get("verify_path") or ""),
                verify_contains=str(r.get("verify_contains") or ""),
                process_match=str(r.get("process_match") or ""),
                notes=str(r.get("notes") or ""),
            )
        )
    return sorted(entries, key=lambda e: (e.group, e.name.lower()))


def by_id(entries: list[Entry], cid: str) -> Entry | None:
    for e in entries:
        if e.id == cid:
            return e
    return None


def instantiate(entry: Entry, host: str = "127.0.0.1", port: int | None = None,
                instance: str = "", scheme: str = "http") -> Service:
    """Build a runnable Service from a catalog entry.

    `instance` disambiguates two of the same thing, giving `sonarr-2` rather than a collision.
    """
    sid = f"{entry.id}-{instance}" if instance else entry.id
    if entry.probe == "http":
        p = port if port is not None else entry.default_port
        target = f"{scheme}://{host}:{p}{entry.verify_path}"
        return Service(
            id=sid, probe="http", target=target, name=entry.name, group=entry.group,
            contains=entry.verify_contains, ports=str(p),
        )
    if entry.probe == "tcp":
        p = port if port is not None else entry.default_port
        return Service(id=sid, probe="tcp", target=f"{host}:{p}", name=entry.name,
                       group=entry.group, ports=str(p))
    if entry.probe == "process":
        return Service(id=sid, probe="process", target=entry.process_match or entry.id,
                       name=entry.name, group=entry.group)
    raise ConfigError(f"{entry.id}: probe {entry.probe!r} cannot be instantiated from a catalog entry")


def to_toml(service: Service) -> str:
    """Render a Service as the `[[service]]` block a user would paste into config.toml."""
    lines = ["[[service]]"]
    lines.append(f'id = "{service.id}"')
    lines.append(f'name = "{service.name}"')
    lines.append(f'group = "{service.group}"')
    lines.append(f'probe = "{service.probe}"')
    lines.append(f'target = "{service.target}"')
    if service.ports:
        lines.append(f'ports = "{service.ports}"')
    if service.contains:
        lines.append(f'contains = "{service.contains}"')
    if service.max_age_s:
        lines.append(f"max_age_s = {service.max_age_s}")
    if service.field:
        lines.append(f'field = "{service.field}"')
    return "\n".join(lines)
