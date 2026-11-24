"""CLI entry point: `python -m statushub [status|config|version]`.

`serve` arrives with the HTTP server; until then these two report real state, not placeholders.
"""

from __future__ import annotations

import argparse
import sys

from . import __version__, db
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


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="observer-frog", description="status dashboard for your machines")
    ap.add_argument("--config", help="path to config.toml")
    ap.add_argument("--version", action="version", version=f"observer-frog {__version__}")
    sub = ap.add_subparsers(dest="cmd")
    sub.add_parser("status", help="show resolved configuration and database state")
    sub.add_parser("config", help="print resolved settings")
    args = ap.parse_args(argv)

    try:
        cfg = load(args.config)
    except ConfigError as e:
        print(f"config error: {e}", file=sys.stderr)
        return 2

    if args.cmd == "config":
        return cmd_config(cfg)
    return cmd_status(cfg)


if __name__ == "__main__":
    raise SystemExit(main())
