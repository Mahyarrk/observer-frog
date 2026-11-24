"""Notifications: event rendering, coalescing, rate limiting, and the ntfy sink.

Invariants:
  I7  ntfy publishes with headers (X-Title/X-Priority/X-Tags) and a plain-text body. A JSON
      body is stored verbatim as message text by this ntfy build.
  I8  no sink goes through the monitored proxy unless it opts in (`use_proxy`). The proxy is a
      monitored service; the alerter must not depend on the thing it reports on.
  Rate limit — a burst of simultaneous failures becomes one message, never N. The 29-message
      burst is the bug this exists to prevent.
"""

from __future__ import annotations

import logging
import urllib.error
import urllib.request
from dataclasses import dataclass, field

from .config import Sink
from .state import Change

log = logging.getLogger("observer-frog.notify")

PRIORITY_OUTAGE = 4      # urgent
PRIORITY_RECOVERY = 2    # default
RATE_WINDOW_S = 60
MAX_MESSAGES_PER_WINDOW = 6


@dataclass
class Event:
    """A notification, already rendered. Sinks never see the state machine's internals."""
    title: str
    message: str
    priority: int = PRIORITY_RECOVERY
    tags: tuple[str, ...] = ()
    click: str = ""

    def as_dict(self) -> dict:
        return {"title": self.title, "message": self.message,
                "priority": self.priority, "tags": list(self.tags)}


def fmt_duration(seconds: float) -> str:
    s = int(max(0, seconds))
    if s < 90:
        return f"{s}s"
    if s < 5400:
        return f"{s // 60}m"
    if s < 172800:
        return f"{s // 3600}h"
    return f"{s // 86400}d"


def events_from_changes(changes: list[Change], dashboard_url: str = "") -> list[Event]:
    """Turn state transitions into human messages. First sightings never page."""
    events: list[Event] = []
    for c in changes:
        if c.first_sighting:
            continue
        if c.is_outage:
            suffix = " (ignored)" if c.ignored else ""
            events.append(Event(
                title=f"{c.name} is down{suffix}",
                message=f"{c.detail or 'no detail'}\nnode {c.node_id} · {c.group}",
                priority=PRIORITY_OUTAGE,
                tags=("rotating_light", c.service_id),
                click=dashboard_url,
            ))
        elif c.is_recovery:
            events.append(Event(
                title=f"{c.name} recovered",
                message=f"{c.detail or 'healthy again'}\nnode {c.node_id} · {c.group}",
                priority=PRIORITY_RECOVERY,
                tags=("white_check_mark", c.service_id),
                click=dashboard_url,
            ))
    return events


# ---------------------------------------------------------------- coalescing

@dataclass
class Digest:
    """Buffers events for a short window so a mass outage is one page instead of thirty."""
    window_s: float = 15.0
    max_items: int = 12
    pending: list[Event] = field(default_factory=list)

    def add(self, event: Event) -> None:
        self.pending.append(event)

    def is_empty(self) -> bool:
        return not self.pending

    def drain(self) -> list[Event]:
        if not self.pending:
            return []
        items, self.pending = self.pending[: self.max_items], self.pending[self.max_items:]
        if len(items) == 1:
            return items
        outages = [e for e in items if e.priority >= PRIORITY_OUTAGE]
        recoveries = [e for e in items if e.priority < PRIORITY_OUTAGE]
        if not outages:
            names = ", ".join(e.title.replace(" recovered", "") for e in items)
            return [Event(f"{len(items)} services recovered", names, PRIORITY_RECOVERY,
                          ("white_check_mark",))]
        names = ", ".join(e.title.replace(" is down", "") for e in outages)
        detail = names if len(names) <= 220 else names[:217] + "…"
        message = f"{detail}\n\n{len(outages)} service(s) down"
        if recoveries:
            message += f", {len(recoveries)} recovered"
        return [Event(f"{len(outages)} services down", message, PRIORITY_OUTAGE,
                      ("rotating_light",))]


class RateLimiter:
    """Hard ceiling on messages per window, so a flapping service cannot silence your phone."""

    def __init__(self, window_s: float = RATE_WINDOW_S, limit: int = MAX_MESSAGES_PER_WINDOW):
        self.window_s = window_s
        self.limit = limit
        self._sent: list[float] = []
        self.suppressed = 0

    def allow(self, now: float) -> bool:
        cutoff = now - self.window_s
        self._sent = [t for t in self._sent if t >= cutoff]
        if len(self._sent) >= self.limit:
            self.suppressed += 1
            return False
        self._sent.append(now)
        return True


# ---------------------------------------------------------------- sinks

def _opener(use_proxy: bool):
    """I8: bypass the proxy unless the sink explicitly needs it (Telegram from a censored net)."""
    handlers = [] if use_proxy else [urllib.request.ProxyHandler({})]
    return urllib.request.build_opener(*handlers)


def sink_ntfy_base(sink: Sink) -> str:
    return (sink.url or "https://ntfy.sh").rstrip("/")


def publish_ntfy(sink: Sink, topic: str, event: Event, timeout: float = 6.0) -> tuple[bool, str]:
    """POST one event with headers and a plain-text body (I7)."""
    if not topic:
        return False, "no topic configured"
    token = ""
    if sink.token_env:
        import os
        token = os.environ.get(sink.token_env, "")
    headers = {
        "Content-Type": "text/plain; charset=utf-8",
        "X-Title": event.title.encode("ascii", "replace").decode("ascii")[:120],
        "X-Priority": str(int(event.priority)),
        "X-Tags": ",".join(event.tags),
        "User-Agent": "observer-frog/0.1",
    }
    if event.click:
        headers["X-Click"] = event.click
    if token:
        headers["Authorization"] = f"Bearer {token}"
    req = urllib.request.Request(
        f"{sink_ntfy_base(sink)}/{topic}",
        data=event.message.encode("utf-8")[:500],
        headers=headers,
        method="POST",
    )
    try:
        with _opener(sink.use_proxy).open(req, timeout=timeout) as resp:
            return True, str(resp.status)
    except urllib.error.HTTPError as e:
        return False, f"HTTP {e.code}"
    except Exception as e:
        return False, f"{type(e).__name__}: {e}"


class Notifier:
    """Owns the digest, the rate limit and the sink list."""

    def __init__(self, sinks: tuple[Sink, ...] = (), topics: dict[str, str] | None = None,
                 dashboard_url: str = ""):
        self.sinks = sinks
        self.topics = topics or {}
        self.dashboard_url = dashboard_url
        self.digest = Digest()
        self.limiter = RateLimiter()
        self.failures: list[str] = []

    def submit(self, changes: list[Change]) -> None:
        for e in events_from_changes(changes, self.dashboard_url):
            self.digest.add(e)

    def flush(self, now: float | None = None) -> list[Event]:
        """Send everything buffered. Returns the events actually delivered."""
        import time
        now = time.time() if now is None else now
        delivered: list[Event] = []
        for event in self.digest.drain():
            if not self.limiter.allow(now):
                log.warning("rate limit: dropping %r (suppressed=%d)", event.title,
                            self.limiter.suppressed)
                continue
            ok_any = self._deliver(event)
            if ok_any:
                delivered.append(event)
        return delivered

    def _deliver(self, event: Event) -> bool:
        sent = False
        for sink in self.sinks:
            if sink.type == "ntfy":
                topic = self.topics.get("ntfy") or sink.topic
                ok, info = publish_ntfy(sink, topic, event)
                if ok:
                    sent = True
                    log.info("ntfy %s: %s", sink_ntfy_base(sink), info)
                else:
                    self.failures.append(f"ntfy {sink_ntfy_base(sink)}: {info}")
                    log.warning("ntfy publish failed: %s", info)
            else:
                log.warning("unknown sink type %r skipped", sink.type)
        return sent
