"""Shared test helper: fetch a catalog entry that must exist."""

from statushub import catalog


def entry(cid: str) -> catalog.Entry:
    e = catalog.by_id(catalog.load(), cid)
    if e is None:
        raise AssertionError(f"catalog entry {cid!r} missing")
    return e
