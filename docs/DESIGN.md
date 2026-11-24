# Design

Authoritative technical contract for observer-frog. If code and this document disagree, one of them
is a bug — decide which, then fix it here in the same commit.

Build plan and work-package gates live outside this document (project roadmap, v0.1).

## Locked decisions

1. **Python ≥ 3.11, one dependency (`psutil`).** TOML config via stdlib `tomllib`.
2. **One binary, two independent flags, no "mode".** `serve = true` binds and serves;
   `report_to = "url"` pushes. Both on = a node that is its own dashboard and reports upward.
   Both off = refuse to start. v0.1 sets `serve` only.
3. **Native only, never a container.** A container sees its own cgroup, not the host's memory;
   needs `--pid=host` for host processes and `--net=host` for real sockets.
4. **Push over pull** (v0.2+). The head stamps arrival time; every report carries node identity and
   metrics, so a payload is never empty. No sequence numbers, no dedupe, no store-and-forward.
5. **Services are data, not code.** A shipped catalog entry compiles to one of five probe kinds.
6. **Ports are not identities.** HTTP entries carry a `verify` signature (path + body substring,
   header, or expected status). Discovery may only propose entries whose signature passes.
7. **Exposure model is Uptime Kuma's.** Loopback by default, `--bind 0.0.0.0` opt-in, one bearer
   token per node on ingest, 64 KB body cap, no wildcard CORS on writes, TLS only via
   `--tls-cert`/`--tls-key`. No CA, no cert generation in-product.
8. **SQLite:** WAL mode, `busy_timeout`, one short-lived write connection per transaction,
   `schema_version` table from the first commit, 24 h raw + hourly rollups kept.
9. **Poller discipline:** fixed-rate ticks. A wave that overruns its interval is dropped and logged,
   never stacked.
10. **Every probe result is `(state, detail, latency_ms)`.** Unsupported or unprivileged →
    `unavailable`, never `down`.
11. **Detail strings capped at ~120 chars** before storage or notification.
12. **Config hot reload on `stat` mtime.** Adding a service never requires a restart.

## Payload contract

Measured from a running instance, not invented. Epochs are absolute seconds (`int`).

`GET /api/status`

| key | type | meaning |
|-----|------|---------|
| `timestamp` | int | snapshot epoch (head clock) |
| `services` | list | service rows, see below |
| `devices` | list | machine cards |
| `history` | dict | `id -> [0\|1, ...]`, 7 points, sparkline |
| `ignored` | list[str] | silenced ids |
| `intentional` | list[str] | deliberately-off ids |
| `confirm_polls` | int | consecutive failures before `down` (4) |
| `node_ts` | int | remote node's own report epoch (v0.2) |

`services[]`: `id` (str, stable), `name`, `group`, `status` (`up`/`down`, unknown values are
displayable), `detail` (free text), `latency_ms` (int), `ports` (display string, may be empty or
`"20170/20171"`), `category` (node/grouping label), and optionally `ram_mb` (number),
`children` (list of `{name, status, detail}` — advisory display rows that never affect the parent's
state). A `control` action id existed in the original and is **dropped**: it was an SSH command
channel, and v0.1 has no control paths.

`devices[]`: `id`, `name`, `kind`, `status`, `detail`, `cpu_pct`, `mem_used` (pre-formatted string,
e.g. `"9.6/16 GB"`), `mem_pct`, `uptime_h`, `temp_c` (null where unreadable), plus optional
`thermal`, `mem_cached`, `mem_compressed`, `swap_used`, `disk_used`. Pre-formatted strings are kept
for renderer compatibility; raw numbers are added alongside.

**Dropped field: `control`.** The original payload carried a `control` action id on one service
(Terraria), which the UI renders as a start/stop button that POSTs to `/api/action`. That endpoint
was an SSH command channel. v0.1 has no control paths at all — the server never emits `control`, so
the UI's own conditional (`if (svc.control)`) renders nothing. The frontend code stays as it is; the
capability is absent by omission, which is cheaper and safer than deleting UI that a future version
may want back with proper auth.

`GET /api/history?days=N` → `{generated, window_days, incidents[], uptime{}, kpi{}}`;
`incidents[]`: `{id, service_id, name, group, started_ts, ended_ts|null, ongoing, duration_s, detail, intentional}`.

## Invariants

Numbers follow the project handoff so cross-references stay valid: 10 and 11 are frontend, 13–16 are
frontend invariants added during the design of this build. Each has a test named after it.

**Backend**

- **I1** Down only after `DOWN_CONFIRM_POLLS = 4` consecutive failures; recovery immediate on the
  first healthy poll.
- **I2** Vanished ≠ healthy: a service absent from an authoritative payload is marked
  `no longer reporting` after 4 absences.
- **I3** An empty payload is a broken poll, not N simultaneous deaths; the absence pass is skipped.
- **I4** First sighting seeds state silently.
- **I5** `intentional` services still alert on transitions but are excluded from uptime, MTTR,
  worst-uptime and `incidents_30d`.
- **I6** Ignore/intentional flags are read at serve time, never baked into the snapshot.
- **I7** ntfy publishes with headers (`X-Title`, `X-Priority`, `X-Tags`) and a plain-text body.
- **I8** No notifier goes through the monitored proxy; per-sink `use_proxy` overrides this once a
  sink genuinely needs one.
- **I9** Self-monitoring starts after the socket is bound.
- **I12** One process snapshot per wave, mapped per service; never block the poller on file parsing;
  cache file metadata by `st_mtime`.

**Frontend**

- **F10** Service worker: network-first shell, **network-only `/api/*`**, `caches.match()` always
  awaited (un-awaited resolves to a truthy Promise and paints a black screen).
- **F11** Paint from `localStorage` before any network call, so an offline launch is instant.
- **F13** No CDN fonts — offline PWA. System stack or a self-hosted woff2 committed to the repo.
- **F14** `100dvh`, `env(safe-area-inset-*)`, tap targets ≥ 44 px, no hover-only affordances.
- **F15** Dark by default; `prefers-reduced-motion` respected.
- **F16** Cache name is versioned; old caches deleted on activate; the precache list is updated in
  the same commit as any file split.

## Parity

The refactor is verified against a golden baseline captured from the original installation, held
outside this repository (it contains host-specific topology). Scope: locally-probed services only;
remote-node services are a declared gap restored when push nodes land.

## Layout

```
observer-frog/
  index.html        single-file UI (CSS + JS inline, data-URI manifest today)
  sw.js             service worker
  statushub/        python package (server, probes, state, catalog, notifier)
  catalog/          shipped service definitions (data, not code)
  docs/DESIGN.md    this file
  tests/            invariant suite
```
