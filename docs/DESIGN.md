# Design

The technical contract for observer-frog. If the code and this document disagree, one of them is a
bug. Decide which, then fix it here in the same commit.

## Locked decisions

1. **Rust, one crate, no runtime.** A static binary per architecture. It began in Python; that
   original is frozen on the `python-v0.1` branch and is no longer part of this tree.
2. **One binary, two independent settings, no "mode".** `[serve] enabled` binds and aggregates;
   `report_to` pushes. Both on is a node that keeps its own dashboard and reports upward. Both off
   is idle, not broken: it still probes this machine and still serves its own view.
3. **Native only, never a container.** A container sees its own cgroup, not the host's memory, and
   needs `--pid=host` and `--net=host` to see real sockets and processes.
4. **Push over pull.** The head stamps arrival time; every report carries node identity and
   metrics, so a payload is never empty. No sequence numbers, no store-and-forward.
5. **Services are data, not code.** A catalog entry compiles to one of five probe kinds.
6. **Ports are not identities.** Every `http` catalog entry carries a `verify_contains` fragment,
   and `parse()` rejects an entry without one. macOS AirPlay holds port 5000, so "something
   answered" must never be enough.
7. **Exposure model is Uptime Kuma's.** Loopback by default, `--bind 0.0.0.0` opt-in, one bearer
   token per node on ingest, 64 KB body cap, no wildcard CORS on writes. TLS terminates from a file
   the operator supplies. The product issues nothing and renews nothing.
8. **SQLite:** WAL mode, `busy_timeout`, one short-lived write connection per transaction,
   `schema_version` from the first commit, 24 h raw plus hourly rollups kept for 30 days.
9. **Poller discipline.** Fixed-rate ticks, one process snapshot per wave. A wave that overruns its
   interval is dropped and logged, never stacked.
10. **Every probe result is `(state, detail, latency_ms)`.** Unsupported or unprivileged reads as
    `unavailable`, never `down`.
11. **Detail strings are capped at ~120 chars** before storage or notification.
12. **One flag, not two.** `ignored` means "I know about this": it silences alerting, removes the
    service from uptime and MTTR, and is the amber half of the History counters. An earlier
    `intentional` flag said the same thing in different words and was folded into `ignored` by the
    v1 to v2 migration, which copies the rows before dropping the table.
13. **Config hot reload on `stat` mtime.** Adding a service never requires a restart.
14. **Role is a setting, not an install profile.** A node that boots as a tail may become a head
    while it runs, and back again, without a restart and without a second unit file.
15. **One unit name per machine.** `dev.observerfrog` on macOS, `observer-frog.service` on Linux.
    The role is never part of the service identity, because a name that changes with a checkbox is
    a name a restart cannot find. Legacy `-head` and `-tail` units are retired on install.
16. **Two listeners, one state.** The dashboard port and the report port can serve at the same time
    from the same process and the same database.

## Discovery and pairing

LAN discovery is UDP 8790: a broadcast query, one answer per node carrying its dashboard URL and,
only when it is acting as head, its report port. A node always advertises its dashboard port, so
every machine is findable. The report port is what says whether it will take reports.

The advertisement carries the configured bind address verbatim, including `0.0.0.0`, because that
is the honest description of what the node listens on. It is not an address anyone can dial, so the
**receiver** substitutes the sender's address from the packet header. The advertisement states the
port; the packet states the address. That keeps the substitution correct per path: the LAN address
over the LAN, the tunnel address over WireGuard, with no configuration and no guessing about which
interface matters.

An instance never lists itself as a head to pair with. Results are keyed on the responder's address
rather than its URL, because every node advertises the same wildcard URL.

Pairing is an approval flow, not a token handoff. A tail proposes a token; the head stores only its
digest and shows the request with a tick and a cross. Nothing is trusted until a human clicks, which
is what makes the routes safely unauthenticated: the proposed token is not a credential yet.

- Rejecting leaves the machine unable to report and does not disturb an existing enrolment.
- Removing is a hard delete of the enrolment row, token digest included. Re-pairing needs a fresh
  `enrol`, and a removed machine does not re-ask on its own, because removal is a decision.
- When a node stops acting as head it drops every other machine: roster, history, enrolment. That
  is a real unpairing, not a pause.

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
| `confirm_polls` | int | consecutive failures before `down` (4) |
| `node_ts` | int | a remote node's own report epoch |

`services[]`: `id` (str, stable), `name`, `group`, `status`, `detail` (free text), `latency_ms`
(int), `ports` (display string, may be empty or `"20170/20171"`), `category`, and optionally
`ram_mb` (number) and `children` (list of `{name, status, detail}`, advisory display rows that
never affect the parent's state).

`devices[]`: `id`, `name`, `kind`, `status`, `detail`, `cpu_pct`, `mem_used` (pre-formatted, e.g.
`"9.6/16 GB"`), `mem_pct`, `uptime_h`, `temp_c` (null where unreadable), plus optional `thermal`,
`mem_cached`, `mem_compressed`, `swap_used`, `disk_used`. Where raw numbers exist they sit alongside
the pre-formatted strings, which are kept for renderer compatibility.

**No `control` field.** The original payload carried one service with a `control` action id, which
the UI rendered as a start/stop button posting to `/api/action`. That endpoint was an SSH command
channel. It is gone, the server never emits `control`, and the UI's own `if (svc.control)` renders
nothing. Deleting the frontend branch would be cheaper than keeping it, but a future version may
want the shape back with proper auth.

`GET /api/history?days=N` returns `{generated, window_days, incidents[], uptime{}, kpi{}}`, where
`incidents[]` is `{id, service_id, name, group, started_ts, ended_ts|null, ongoing, duration_s,
detail, ignored}`.

## Invariants

Each has a test named after it. Numbers follow the original handoff so cross-references stay valid:
10 and 11 are frontend, 13 to 16 are frontend invariants added during this build.

**Backend**

- **I1** Down only after `DOWN_CONFIRM_POLLS = 4` consecutive failures; recovery is immediate on
  the first healthy poll.
- **I2** Vanished is not healthy: a service absent from an authoritative payload is marked
  `no longer reporting` after 4 absences.
- **I3** An empty payload is a broken poll, not N simultaneous deaths; the absence pass is skipped.
- **I4** First sighting seeds state silently.
- **I5** `ignored` services still alert on transitions but are excluded from uptime, MTTR,
  worst-uptime and `incidents_30d`.
- **I6** The ignored flag is read at serve time, never baked into the snapshot.
- **I7** ntfy publishes with headers (`X-Title`, `X-Priority`, `X-Tags`) and a plain-text body.
- **I8** No notifier goes through the monitored proxy; per-sink `use_proxy` overrides this once a
  sink genuinely needs one.
- **I9** Self-monitoring starts only after the socket is bound.
- **I12** One process snapshot per wave, mapped per service; never block the poller on file parsing;
  cache file metadata by `st_mtime`.

**Frontend**

- **F10** Service worker: network-first shell, **network-only `/api/*`**, `caches.match()` always
  awaited. An un-awaited call resolves to a truthy Promise and paints a black screen.
- **F11** Paint from `localStorage` before any network call, so an offline launch is instant.
- **F13** No CDN fonts. The PWA has to work offline, so: system stack, or a self-hosted woff2
  committed to the repo.
- **F14** `100dvh`, `env(safe-area-inset-*)`, tap targets of at least 44 px, no hover-only
  affordances.
- **F15** Dark by default; `prefers-reduced-motion` respected.
- **F16** The cache name is versioned and old caches are deleted on activate. The precache list is
  updated in the same commit as any file split.
- **F17** Settings is live, not loaded once. The poll refreshes it while it is the visible tab,
  because a PWA has no reload button and a pairing request that only appears after a manual refresh
  never appears at all. Live refresh must not overwrite a field with focus, must not rewrite a value
  that is already correct, and must not rebuild a list whose rows have not changed, since replacing
  a button under the operator's finger turns a tap into a no-op.

## Parity

The Rust port was verified against the frozen Python branch by a harness that lives **outside this
repository**, at `~/workspace/statushub-migration/`. It holds golden payloads containing
host-specific topology (LAN addresses, local paths, service inventory), which must never be
committed. The repository keeps only this contract and the shape tests built on synthetic data.

That harness compared both builds on the same configuration: every endpoint, every service state,
every response shape, byte-identical web assets, and a 0.55% pixel difference on rendered
screenshots (volatile CPU, memory and latency only).

**Epoch convention:** every timestamp in the payload and the database is an absolute epoch second,
never a local-naive string and never milliseconds. Durations subtract directly.

## Layout

```
observer-frog/
  index.html          single-file UI (CSS and JS inline, manifest as a data URI)
  sw.js               service worker
  manifest.json       PWA manifest
  rust/src/           one crate: server, poller, probes, state, db, catalog, discover, install
  catalog/            shipped service definitions (data, not code)
  docs/               this file, the guide, the roadmap
```
