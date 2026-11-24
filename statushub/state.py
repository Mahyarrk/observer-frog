"""The state machine: transitions, incidents, rollups, notifications.

Every rule here exists because it already went wrong once (see docs/DESIGN.md):
  I1  down only after DOWN_CONFIRM_POLLS consecutive failures; recovery is immediate
  I2  a service absent from an authoritative payload is "no longer reporting" after 4 absences
  I3  an empty payload is a broken poll, not N simultaneous deaths — absence pass skipped
  I4  first sighting seeds state silently
  I5  intentional services alert on transitions but are excluded from uptime/MTTR
  I6  flags are read at serve time, never baked in here
"""

from __future__ import annotations

import logging
import time
from dataclasses import dataclass, field

from . import db

log = logging.getLogger("observer-frog.state")

DOWN_CONFIRM_POLLS = 4
ABSENCE_CONFIRM_POLLS = 4
VANISHED_DETAIL = "no longer reporting"

# States that mean "the thing is answering". `unavailable` is neither healthy nor an outage.
HEALTHY = frozenset({"up", "degraded"})
PROBLEM = frozenset({"down"})
NEUTRAL = frozenset({"unavailable"})


@dataclass
class Change:
    """A state transition, in the shape a notifier wants."""
    node_id: str
    service_id: str
    name: str
    group: str
    from_state: str
    to_state: str
    detail: str
    ts: int
    intentional: bool = False
    first_sighting: bool = False

    @property
    def is_recovery(self) -> bool:
        return self.from_state == "down" and self.to_state != "down"

    @property
    def is_outage(self) -> bool:
        return self.to_state == "down" and self.from_state not in ("down", "new")


@dataclass
class Row:
    """One probe result entering the machine."""
    service_id: str
    name: str
    group: str
    status: str
    detail: str
    latency_ms: int = 0


@dataclass
class StoredState:
    state: str = "up"
    fails: int = 0
    since_ts: int = 0
    name: str = ""
    group: str = ""
    last_detail: str = ""


def load_states(path, node_id: str) -> dict[str, StoredState]:
    states: dict[str, StoredState] = {}
    with db.connect(path) as conn:
        for r in conn.execute(
            "SELECT service_id, state, fails, since_ts, name, grp, last_detail "
            "FROM service_state WHERE node_id = ?", (node_id,)
        ):
            states[r[0]] = StoredState(r[1], r[2], r[3], r[4], r[5], r[6])
    return states


def _save_state(conn, node_id: str, row: Row, state: StoredState) -> None:
    conn.execute(
        "INSERT INTO service_state (node_id, service_id, state, fails, since_ts, name, grp, last_detail) "
        "VALUES (?,?,?,?,?,?,?,?) "
        "ON CONFLICT(node_id, service_id) DO UPDATE SET "
        "state=excluded.state, fails=excluded.fails, since_ts=excluded.since_ts, "
        "name=excluded.name, grp=excluded.grp, last_detail=excluded.last_detail",
        (node_id, row.service_id, state.state, state.fails, state.since_ts,
         state.name, state.group, state.last_detail),
    )


def _open_incident(conn, node_id: str, row: Row, ts: int) -> None:
    conn.execute(
        "INSERT INTO incidents (node_id, service_id, name, grp, started_ts, ended_ts, last_detail) "
        "VALUES (?,?,?,?,?,NULL,?)",
        (node_id, row.service_id, row.name, row.group, ts, row.detail),
    )


def _close_incident(conn, node_id: str, service_id: str, ts: int, detail: str) -> None:
    conn.execute(
        "UPDATE incidents SET ended_ts = ?, last_detail = ? "
        "WHERE node_id = ? AND service_id = ? AND ended_ts IS NULL",
        (ts, detail, node_id, service_id),
    )


def _record_check(conn, node_id: str, row: Row, ts: int) -> None:
    conn.execute(
        "INSERT INTO service_checks (node_id, service_id, timestamp, status, detail, latency_ms) "
        "VALUES (?,?,?,?,?,?)",
        (node_id, row.service_id, ts, row.status, row.detail, row.latency_ms),
    )


def _record_rollup(conn, node_id: str, row: Row, ts: int) -> None:
    hour = ts - (ts % 3600)
    bucket = {"up": "up_n", "degraded": "deg_n", "down": "down_n"}.get(row.status)
    if bucket is None:
        return   # `unavailable` is not a health sample; counting it would fake downtime
    conn.execute(
        "INSERT INTO hourly_rollup (node_id, service_id, hour_ts, up_n, deg_n, down_n, lat_sum, lat_n) "
        f"VALUES (?,?,?,?,?,?,?,1) "
        f"ON CONFLICT(node_id, service_id, hour_ts) DO UPDATE SET "
        f"{bucket} = {bucket} + 1, lat_sum = lat_sum + excluded.lat_sum, lat_n = lat_n + 1",
        (node_id, row.service_id, hour,
         1 if bucket == "up_n" else 0,
         1 if bucket == "deg_n" else 0,
         1 if bucket == "down_n" else 0,
         max(0, row.latency_ms)),
    )


def apply_wave(path, node_id: str, rows: list[Row], ts: int | None = None) -> list[Change]:
    """Feed one poll wave through the machine. Returns the transitions worth notifying about.

    An empty `rows` is a broken poll (I3): nothing is marked down and nothing is persisted.
    Intentional flags are read from the database here, exactly as the serve path reads them (I6),
    so a flag toggled in the UI takes effect on the next wave without any in-memory state.
    """
    ts = int(time.time()) if ts is None else int(ts)

    if not rows:
        log.warning("empty payload: skipping wave entirely (invariant I3)")
        return []

    intentional = set(db.flags(path, node_id)[1])
    states = load_states(path, node_id)
    changes: list[Change] = []
    seen: set[str] = set()

    with db.connect(path) as conn:
        for row in rows:
            seen.add(row.service_id)
            prev = states.get(row.service_id)

            # I4 — first sighting seeds silently, whatever it is
            if prev is None:
                st = StoredState(state=row.status, fails=0, since_ts=ts, name=row.name,
                                 group=row.group, last_detail=row.detail)
                _save_state(conn, node_id, row, st)
                _record_check(conn, node_id, row, ts)
                _record_rollup(conn, node_id, row, ts)
                if row.status == "down":
                    _open_incident(conn, node_id, row, ts)
                changes.append(Change(node_id, row.service_id, row.name, row.group, "new",
                                      row.status, row.detail, ts,
                                      row.service_id in intentional, first_sighting=True))
                continue

            prev.state, prev.name, prev.group = prev.state, row.name, row.group
            prev.last_detail = row.detail
            _record_check(conn, node_id, row, ts)
            _record_rollup(conn, node_id, row, ts)

            if row.status == "down":
                prev.fails += 1
                # I1 — confirm before declaring down
                if prev.state != "down" and prev.fails >= DOWN_CONFIRM_POLLS:
                    prev.state = "down"
                    prev.since_ts = ts
                    _open_incident(conn, node_id, row, ts)
                    changes.append(Change(node_id, row.service_id, row.name, row.group,
                                          "up", "down", row.detail, ts,
                                          row.service_id in intentional))
            else:
                # I1 — recovery is immediate
                if prev.state == "down":
                    prev.state = row.status
                    prev.since_ts = ts
                    _close_incident(conn, node_id, row.service_id, ts, row.detail)
                    changes.append(Change(node_id, row.service_id, row.name, row.group,
                                          "down", row.status, row.detail, ts,
                                          row.service_id in intentional))
                elif prev.state in NEUTRAL and row.status in HEALTHY:
                    prev.state = row.status
                    prev.since_ts = ts
                prev.fails = 0

            _save_state(conn, node_id, row, prev)

        # I2 — vanished is not healthy, but only when the payload had something in it (I3)
        for sid, prev in states.items():
            if sid in seen or prev.state == "down":
                continue
            absent = min(DOWN_CONFIRM_POLLS, ABSENCE_CONFIRM_POLLS)
            ghost = Row(service_id=sid, name=prev.name or sid, group=prev.group,
                        status="down", detail=VANISHED_DETAIL)
            ghost_prev = StoredState(state=prev.state, fails=prev.fails + 1, since_ts=prev.since_ts,
                                     name=prev.name, group=prev.group, last_detail=VANISHED_DETAIL)
            if ghost_prev.fails >= absent:
                ghost_prev.state = "down"
                ghost_prev.since_ts = ts
                _open_incident(conn, node_id, ghost, ts)
                changes.append(Change(node_id, sid, ghost.name, ghost.group, prev.state, "down",
                                      VANISHED_DETAIL, ts, sid in intentional))
            _save_state(conn, node_id, ghost, ghost_prev)

    return changes


def uptime(path, node_id: str, days: int = 30) -> dict[str, float]:
    """Percentage of healthy samples per service, excluding intentional ones (I5)."""
    since = int(time.time()) - days * 86400
    skip = set(db.flags(path, node_id)[1])
    with db.connect(path) as conn:
        rows = conn.execute(
            "SELECT service_id, SUM(up_n), SUM(deg_n), SUM(down_n) FROM hourly_rollup "
            "WHERE node_id = ? AND hour_ts >= ? GROUP BY service_id", (node_id, since),
        ).fetchall()
    out: dict[str, float] = {}
    for sid, up_n, deg_n, down_n in rows:
        if sid in skip:
            continue
        total = (up_n or 0) + (deg_n or 0) + (down_n or 0)
        out[sid] = round(100.0 * ((up_n or 0) + (deg_n or 0)) / total, 2) if total else 0.0
    return out


def mttr(path, node_id: str, days: int = 30) -> dict[str, float]:
    """Mean seconds to recover per service, intentional services excluded (I5)."""
    since = int(time.time()) - days * 86400
    skip = set(db.flags(path, node_id)[1])
    with db.connect(path) as conn:
        rows = conn.execute(
            "SELECT service_id, AVG(ended_ts - started_ts), COUNT(*) FROM incidents "
            "WHERE node_id = ? AND started_ts >= ? AND ended_ts IS NOT NULL "
            "GROUP BY service_id", (node_id, since),
        ).fetchall()
    return {sid: round(avg, 1) for sid, avg, _ in rows if sid not in skip}


def open_incidents(path, node_id: str) -> list[dict]:
    with db.connect(path) as conn:
        rows = conn.execute(
            "SELECT id, service_id, name, grp, started_ts, last_detail FROM incidents "
            "WHERE node_id = ? AND ended_ts IS NULL ORDER BY started_ts DESC", (node_id,)
        ).fetchall()
    now = int(time.time())
    return [
        {"id": r[0], "service_id": r[1], "name": r[2], "group": r[3],
         "started_ts": r[4], "duration_s": now - r[4], "detail": r[5], "ongoing": True}
        for r in rows
    ]


def recent_incidents(path, node_id: str, days: int = 30, limit: int = 100) -> list[dict]:
    since = int(time.time()) - days * 86400
    skip = set(db.flags(path, node_id)[1])
    with db.connect(path) as conn:
        rows = conn.execute(
            "SELECT id, service_id, name, grp, started_ts, ended_ts, last_detail FROM incidents "
            "WHERE node_id = ? AND started_ts >= ? ORDER BY started_ts DESC LIMIT ?",
            (node_id, since, limit),
        ).fetchall()
    now = int(time.time())
    out = []
    for r in rows:
        ended = r[5]
        sid = r[1]
        out.append({
            "id": r[0], "service_id": sid, "name": r[2], "group": r[3],
            "started_ts": r[4], "ended_ts": ended, "ongoing": ended is None,
            "duration_s": (ended or now) - r[4], "detail": r[6],
            "intentional": sid in skip,
        })
    return out
