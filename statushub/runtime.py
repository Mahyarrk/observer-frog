"""Runtime wiring: poller thread + notifier + HTTP server, with self-monitoring last (I9)."""

from __future__ import annotations

import logging
import threading
import time

from . import db, notify, state
from .config import Config, Watcher
from .poller import Poller
from .server import App, serve


def build(cfg: Config, watcher: Watcher | None = None) -> App:
    """Assemble the runtime without binding anything. Returned App is safe to test."""
    db.init(cfg.db_path)
    topics = {"ntfy": cfg.sinks[0].topic if cfg.sinks else ""}
    notifier = notify.Notifier(sinks=cfg.sinks, topics=topics,
                               dashboard_url=f"http://{cfg.bind}:{cfg.port}/")
    app = App(cfg, poller=None, notifier=notifier)

    def on_wave(rows: list[dict], ts: int) -> None:
        changes = state.apply_wave(
            cfg.db_path, cfg.node_id,
            [state.Row(r["id"], r["name"], r["group"], r["status"], r["detail"], r["latency_ms"])
             for r in rows],
            ts,
        )
        app.set_snapshot(rows, ts)
        if changes:
            notifier.submit(changes)
            notifier.flush()

    cfg_getter = watcher.refresh if watcher else (lambda: cfg)
    poller = Poller(cfg_getter, on_wave)
    app.poller = poller
    return app


def run(cfg: Config, tls_cert: str | None = None, tls_key: str | None = None,
        on_ready=None) -> None:
    """Start serving. The HTTP thread is bound first, then the poller (I9)."""
    watcher = Watcher(cfg.path) if cfg.path else None
    app = build(cfg, watcher)
    httpd = serve(cfg, app, tls_cert=tls_cert, tls_key=tls_key)

    scheme = "https" if tls_cert and tls_key else "http"
    url = f"{scheme}://{cfg.bind}:{cfg.port}/"
    banner = [
        f"observer-frog serving {url}",
        f"  node {cfg.node_id} ({cfg.node_name})",
        f"  {len(cfg.services)} service(s), poll every {cfg.poll_interval}s",
        f"  database {cfg.db_path}",
    ]
    print("\n".join(banner), flush=True)
    if on_ready:
        on_ready(url, app)

    # Self-monitoring begins only now that the socket is bound (invariant I9).
    app.poller.start()
    threads = [threading.Thread(target=httpd.serve_forever, daemon=True, name="http")]
    for t in threads:
        t.start()
    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        print("shutting down", flush=True)
    finally:
        app.poller.stop()
        httpd.shutdown()
        httpd.server_close()


def setup_logging(verbose: bool = False) -> None:
    logging.basicConfig(
        level=logging.DEBUG if verbose else logging.INFO,
        format="%(asctime)s %(levelname)-7s %(name)s: %(message)s",
        datefmt="%H:%M:%S",
    )
