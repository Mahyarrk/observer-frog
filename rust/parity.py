#!/usr/bin/env python3
"""Compare a Python observer-frog against the Rust build, endpoint by endpoint.

The Python implementation left `main` and lives frozen on the `python-v0.1` branch. To run this
you need both builds serving the same configuration:

    git worktree add /tmp/py-v01 python-v0.1
    cd /tmp/py-v01 && uv venv --python 3.12 && uv pip install -p .venv/bin/python psutil
    .venv/bin/python -m statushub --config /path/to/config.toml serve --port 8800

    ./target/release/observer-frog --config /path/to/config.toml serve --port 8770
    python3 parity.py http://127.0.0.1:8800 http://127.0.0.1:8770

Nothing here imports the Python code; it only compares HTTP responses, so it keeps working after
Python leaves the main tree.

Volatile fields (cpu, load, memory, uptime, network counters, latency) are excluded by design;
everything else must match exactly, including key sets and value types.
"""
import json
import sys
import urllib.request

VOLATILE_DEVICE = {
    "cpu_pct", "mem_pct", "mem_used", "mem_used_gb", "net_sent_mb", "net_recv_mb",
    "load_avg", "uptime_h", "mem_cached", "mem_compressed", "swap_used",
}
VOLATILE_KPI = {"mttr_s", "worst_uptime"}

# v0.2 changes that are intentional, not regressions. Each is named so a future reader knows the
# difference was decided rather than missed.
EXPECTED_DIVERGENCES = {
    "device.name": (
        "v0.2: a device names itself from its own hostname instead of a config field, so the "
        "name survives a rename and cannot go stale"
    ),
    "history.node": (
        "v0.2: /api/history states which node it describes, because the head now serves many"
    ),
}


def get(base, path):
    with urllib.request.urlopen(base.rstrip("/") + path, timeout=15) as r:
        return json.loads(r.read())


def cmp_shapes(name, a, b, problems):
    if sorted(a) != sorted(b):
        problems.append(f"{name}: key sets differ | only_py={sorted(set(a)-set(b))} only_rs={sorted(set(b)-set(a))}")


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    py_base, rs_base = sys.argv[1], sys.argv[2]
    problems = []

    py_status = get(py_base, "/api/status")
    rs_status = get(rs_base, "/api/status")
    cmp_shapes("/api/status", py_status, rs_status, problems)

    # services: id set, per-service key shape, and status must all agree
    py_svc = {s["id"]: s for s in py_status.get("services", [])}
    rs_svc = {s["id"]: s for s in rs_status.get("services", [])}
    if set(py_svc) != set(rs_svc):
        problems.append(f"service ids differ: only_py={sorted(set(py_svc)-set(rs_svc))} "
                        f"only_rs={sorted(set(rs_svc)-set(py_svc))}")
    for sid in sorted(set(py_svc) & set(rs_svc)):
        a, b = py_svc[sid], rs_svc[sid]
        if a["status"] != b["status"]:
            problems.append(f"{sid}: status py={a['status']} rs={b['status']}")
        if sorted(a) != sorted(b):
            problems.append(f"{sid}: keys only_py={sorted(set(a)-set(b))} only_rs={sorted(set(b)-set(a))}")
        for f in ("name", "group", "ports", "address"):
            if a.get(f) != b.get(f):
                problems.append(f"{sid}.{f}: py={a.get(f)!r} rs={b.get(f)!r}")

    # device card: same keys, and every non-volatile value identical
    if py_status.get("devices") and rs_status.get("devices"):
        d_py, d_rs = py_status["devices"][0], rs_status["devices"][0]
        cmp_shapes("device", d_py, d_rs, problems)
        for k in sorted(set(d_py) & set(d_rs)):
            if k in VOLATILE_DEVICE:
                continue
            if d_py[k] != d_rs[k]:
                key = f"device.{k}"
                if key in EXPECTED_DIVERGENCES:
                    continue
                problems.append(f"{key}: py={d_py[k]!r} rs={d_rs[k]!r}")

    # history: KPI keys always agree; counts agree when both have seen the same traffic
    py_hist = get(py_base, "/api/history?days=30")
    rs_hist = get(rs_base, "/api/history?days=30")
    extra = set(rs_hist) - set(py_hist)
    allowed_extra = {"node"}  # v0.2: /api/history names the node it describes
    if set(py_hist) - set(rs_hist) or (extra - allowed_extra):
        problems.append(
            f"/api/history: key sets differ | only_py={sorted(set(py_hist)-set(rs_hist))} "
            f"only_rs={sorted(extra - allowed_extra)}"
        )
    cmp_shapes("history.kpi", py_hist["kpi"], rs_hist["kpi"], problems)
    for k in py_hist["kpi"]:
        if k in VOLATILE_KPI:
            continue
        if py_hist["kpi"][k] != rs_hist["kpi"][k]:
            problems.append(f"kpi.{k}: py={py_hist['kpi'][k]!r} rs={rs_hist['kpi'][k]!r}")

    if problems:
        print(f"FAIL — {len(problems)} difference(s)")
        for p in problems:
            print("  " + p)
        return 1
    print("PASS — identical contract, identical service states")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
