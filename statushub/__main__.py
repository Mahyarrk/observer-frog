"""CLI entry point: `python -m statushub [status|config|catalog|add|version]`.

Until the HTTP server lands, `serve` is intentionally absent rather than faked.
"""

from __future__ import annotations

import argparse
import sys

from . import __version__, catalog, db
from .config import Config, ConfigError, load


def cmd_status(cfg: Config) -> int:
    ver = db.init(cfg.db_path)
    print(f"config      {cfg.path or '(defaults, no file)'}")
    print(f"database    {cfg.db_path}  (schema v{ver})")
    print(f"node        {cfg.node_id} / {cfg.node_name}")
    print(f"serve       {cfg.serve} on {cfg.bind}:{cfg.port}  poll {cfg.poll_interval}s")
    print(f"services    {len(cfg.services)}")
    for s in cfg.services:
        print(f"  {s.id:24s} {s.probe:9s} {s.target}")
    print(f"sinks       {len(cfg.sinks) or 'none'}")
    return 0


def cmd_config(cfg: Config) -> int:
    for f in ("path", "node_id", "node_name", "serve", "bind", "port", "poll_interval", "db_path"):
        print(f"{f:14s} {getattr(cfg, f)}")
    return 0


def cmd_catalog(entries, query: str | None) -> int:
    rows = entries
    if query:
        q = query.lower()
        rows = [e for e in rows if q in e.id or q in e.name.lower() or q in e.group.lower()]
    group = None
    for e in rows:
        if e.group != group:
            group = e.group
            print(f"\n{group}")
        port = f":{e.default_port}" if e.default_port else ""
        sig = "signed" if e.has_signature() else ""
        print(f"  {e.id:18s} {e.name:22s} {e.probe:8s}{port:6s} {sig}")
    print(f"\n{len(rows)} entr{'y' if len(rows) == 1 else 'ies'}")
    return 0


def cmd_add(cfg: Config, entries, cid: str, host: str, port: int | None, instance: str) -> int:
    """Print the config block for a catalog entry. Prints; never writes."""
    entry = catalog.by_id(entries, cid)
    if entry is None:
        print(f"no catalog entry {cid!r}; run `catalog` to list ids", file=sys.stderr)
        return 2
    if entry.probe in ("file_age", "file_json"):
        print(f"{cid} needs a path and threshold; add it by hand", file=sys.stderr)
        return 2
    svc = catalog.instantiate(entry, host=host, port=port, instance=instance)
    if svc.id in cfg.service_ids():
        print(f"service {svc.id!r} already exists in {cfg.path}", file=sys.stderr)
        return 1
    print(f"# add to {cfg.path or '(your config.toml)'}")
    print(catalog.to_toml(svc))
    return 0


def cmd_metrics(cfg: Config) -> int:
    """One-shot local metrics, for a quick check and for debugging the collector."""
    from . import metrics
    card = metrics.device_card(cfg.node_id, cfg.node_name)
    print(f"{card['name']}  ({card['kind']})  {card['detail']}")
    print(f"  cpu      {card.get('cpu_pct')}%   uptime {card.get('uptime_h')} h")
    print(f"  memory   {card.get('mem_used_str') or 'n/a'}  ({card.get('mem_pct')}%)")
    if card.get("swap_used") is not None:
        print(f"  swap     {card['swap_used']} MB")
    if card.get("disk_used"):
        print(f"  disk     {card['disk_used']}")
    if card.get("thermal"):
        print(f"  thermal  {card['thermal']}")
    if card.get("temp_c") is not None:
        print(f"  temp     {card['temp_c']} °C")
    return 0


def cmd_serve(cfg: Config, args) -> int:
    from . import runtime

    runtime.setup_logging(verbose=getattr(args, "verbose", False))
    bind = getattr(args, "bind", None)
    port = getattr(args, "port", None)
    if bind or port:
        from dataclasses import replace
        cfg = replace(cfg, bind=bind or cfg.bind, port=port or cfg.port)
    cert, key = getattr(args, "tls_cert", None), getattr(args, "tls_key", None)
    if bool(cert) != bool(key):
        print("--tls-cert and --tls-key must be given together", file=sys.stderr)
        return 2
    if cfg.bind not in ("127.0.0.1", "localhost", "::1"):
        print(f"binding {cfg.bind}: this is reachable from your network. "
              f"Put it behind your proxy or a VPN.", file=sys.stderr)
    db.init(cfg.db_path)
    runtime.run(cfg, tls_cert=cert, tls_key=key)
    return 0


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="observer-frog", description="status dashboard for your machines")
    ap.add_argument("--config", help="path to config.toml")
    ap.add_argument("--version", action="version", version=f"observer-frog {__version__}")
    sub = ap.add_subparsers(dest="cmd")
    sub.add_parser("status", help="show resolved configuration and database state")
    sub.add_parser("config", help="print resolved settings")
    p_cat = sub.add_parser("catalog", help="list services that ship with a ready-made definition")
    p_cat.add_argument("query", nargs="?", help="filter by id, name or group")
    p_add = sub.add_parser("add", help="print the config block for a catalog service")
    p_add.add_argument("id", help="catalog id, e.g. syncthing")
    p_add.add_argument("--host", default="127.0.0.1")
    p_add.add_argument("--port", type=int, default=None, help="override the default port")
    p_add.add_argument("--instance", default="", help="suffix for a second copy, e.g. 2")
    p_serve = sub.add_parser("serve", help="start the dashboard (the default when no command is given)")
    p_serve.add_argument("--bind", help="address to bind (default from config; loopback unless set)")
    p_serve.add_argument("--port", type=int, help="port to bind")
    p_serve.add_argument("--tls-cert", help="PEM certificate; requires --tls-key")
    p_serve.add_argument("--tls-key", help="PEM private key; requires --tls-cert")
    p_serve.add_argument("-v", "--verbose", action="store_true", help="debug logging")
    p_stat = sub.add_parser("metrics", help="print this machine's status line (no server)")
    args = ap.parse_args(argv)

    try:
        cfg = load(args.config)
        entries = catalog.load()
    except ConfigError as e:
        print(f"config error: {e}", file=sys.stderr)
        return 2

    if args.cmd == "config":
        return cmd_config(cfg)
    if args.cmd == "catalog":
        return cmd_catalog(entries, args.query)
    if args.cmd == "add":
        return cmd_add(cfg, entries, args.id, args.host, args.port, args.instance)
    if args.cmd == "metrics":
        return cmd_metrics(cfg)
    if args.cmd == "serve" or args.cmd is None:
        return cmd_serve(cfg, args)
    return cmd_status(cfg)


if __name__ == "__main__":
    raise SystemExit(main())
