# CODE MAP

A working map of observer-frog: what each file is for, what the load-bearing functions do, and
how a sample gets from a probe to your phone. Written to be read before changing anything, not
to be read once.

Line numbers drift. Function names do not, so this refers to names.

---

# The shape of the thing

One binary. No modes. A node is whatever its two settings say it is at this instant:

```
serve = true        → this node runs a dashboard and holds a database
report_to = "..."   → this node probes itself and POSTs the result somewhere
```

Both, either, or neither. `config` prints the role it adds up to. Nothing in the code branches on
a "mode" — the two settings are read independently at the moment they matter, which is why
flipping a switch in Settings takes effect within seconds instead of at the next restart.

```
                    ┌──────────────────────────────────────┐
                    │  your browser / phone PWA            │
                    └───────────────┬──────────────────────┘
                                    │ https, session cookie
                    ┌───────────────▼──────────────────────┐
                    │  DASHBOARD listener  (:8770, TLS)    │
                    │  server::route() — 26 API routes     │
                    └───────────────┬──────────────────────┘
                                    │ one shared App
   ┌────────────────────────────────┼────────────────────────────────┐
   │                                │                                │
┌──▼──────────────┐   ┌─────────────▼────────────┐   ┌───────────────▼────┐
│ poller thread   │   │  App (snapshot, counters,│   │ REPORTS listener   │
│ sample()        │──▶│  poll lock, notifier)    │◀──│ (:8771, plain)     │
│ once per wave   │   │                          │   │ /api/report only   │
└──┬──────────────┘   └─────────────┬────────────┘   └────────▲───────────┘
   │ probes::run()                  │ state::apply_wave()      │
┌──▼──────────────┐   ┌─────────────▼────────────┐   ┌────────┴───────────┐
│ probes.rs       │   │ state.rs → SQLite        │   │ OTHER MACHINES     │
│ http/tcp/       │   │ + incidents + rollups    │   │ (tails) reporting  │
│ process/iface/  │   └─────────────┬────────────┘   └────────▲───────────┘
│ file_age/       │                 │ changes                 │
│ file_json       │   ┌─────────────▼────────────┐   ┌────────┴───────────┐
└─────────────────┘   │ notify.rs → sinks        │   │ tail.rs            │
                      │ ntfy | webhook           │   │ builds the report  │
                      └──────────────────────────┘   └────────────────────┘
```

---

# Files, in the order you should learn them

| File | Lines | One sentence |
|---|---|---|
| `main.rs` | 519 | CLI dispatch; every subcommand starts here |
| `config.rs` | 639 | TOML → validated structs; the `Watcher` that reloads on mtime |
| `probes.rs` | 573 | The five probe kinds; every "is it up" answer comes from `run()` |
| `poller.rs` | 382 | One wave: process snapshot, bounded concurrency, memory attribution |
| `state.rs` | 776 | The state machine — confirmations, incidents, rollups, transitions |
| `db.rs` | 745 | Schema v10, the migration ladder, retention |
| `server.rs` | 1947 | The HTTP server, all routes, auth gate, TLS upload |
| `runtime.rs` | 411 | Threads: poll wave, silence sweeper, advertisement |
| `notify.rs` | 645 | Event rendering, digest, rate limit, ntfy + webhook senders |
| `tail.rs` | 340 | Build a report, post it, classify the answer as `Outcome` |
| `ingest.rs` | 533 | Head side of a report: token → node, parse, apply |
| `silence.rs` | 283 | A quiet node becomes `unavailable`, never `down` |
| `auth.rs` | 540 | bcrypt password, HMAC session cookie, constant-time compare |
| `settings.rs` | 702 | Surgical TOML editing — every GUI write goes through here |
| `pair.rs` | 435 | The pairing handshake, both sides |
| `discover.rs` | 440 | Socket table → what is listening; catalog candidates |
| `catalog.rs` | 315 | The 23-entry shipped catalog, embedded at build time |
| `install.rs` | 563 | LaunchAgent / systemd user unit, one name for both roles |
| `lan.rs` | 261 | UDP 8790 discovery of heads |
| `metrics.rs` | 342 | The device card: cpu, memory, disk, thermal, uptime |
| `watchdog.rs` | 233 | The dead man's switch: three states, three signals |

`index.html` (4950) is the whole frontend — one file, no build step, no framework.
`sw.js` (58) is the service worker shell cache.

---

# The path a sample takes

This is the spine. Everything else is a detail hanging off it.

### 1. A wave begins — `runtime::run`, poller thread

```
loop {
    cfg_now = watcher.refresh()        // re-read the file if its mtime moved
    rows    = poller::sample(&cfg_now.services, None)
    changes = state::apply_wave(db, node_id, &rows, Some(ts))
    app.set_snapshot(rows)
    notifier.submit(&changes); notifier.flush()
    [tail] report_outcome(cfg, head, &rows, ts)
    sleep(interval - elapsed)
}
```

**One thread does both roles.** This is the design decision that removed a whole class of bug: a
head that also reports does not probe twice, because the wave it just ran *is* the report.

### 2. `poller::sample` — the wave

- Takes **one** process snapshot (`Snapshot::new()` → one `ps -Ao pid=,ppid=,rss=,args=`), so N
  services cost one scan, not N. That is invariant I12.
- Spawns `MAX_WORKERS = 12` scoped threads pulling indices off an atomic counter.
- Results are sorted back into **configuration order** so the dashboard does not reshuffle.
- For each service it produces `(state, detail, latency_ms)` plus a `ram_mb` when attributable.

The `process` branch is special: it answers from the snapshot, and **falls back to the interface**
when the process is not found. That fallback is the WireGuard fix — Linux has no `wg0` process,
only the interface, and this branch used to answer from the process table alone.

Memory attribution, in priority order:
1. `rss_for_port(port)` — resolve the PID holding the socket, sum it with its children. Exact.
2. `rss_for_unique(id)` — a process whose *executable name* matches this service's id, and only if
   exactly one matches. Ambiguity returns `None`, not a sum.
3. `None` → the UI renders `—`. A literal `0` would claim "uses no memory", which is a claim
   nobody measured.

`rss_for` (the summing variant) matches the executable name, never a substring of the command
line. A substring match once reported a 59 MB process as 888 MB because it swept up every shell
whose *arguments* mentioned the name.

### 3. `probes::run(&Service)` — one answer

The only place that answers "is this up". `poller` calls it, the GUI's verify-before-add calls it,
nothing else should.

| probe | says up when |
|---|---|
| `http` | status in `ALIVE_CODES` (200/301/302/307/**401**) and, if a signature is set, it appears in body **or** final URL **or** `Server` header |
| `tcp` | a TCP connection completes |
| `process` | a process's command line contains the needle — **or** the named interface exists |
| `file_age` | the file's mtime is within `max_age_s` |
| `file_json` | the file parses and the dotted `field` contains `contains` |

Every probe returns `(state, detail, latency_ms)` with `detail` clipped to 120 chars by `clip()`.

Four states, and the distinction is load-bearing:

- `up` — answered and proved itself
- `degraded` — answered, but not as expected (wrong status, missing signature, stale file)
- `down` — asked and did not answer
- `unavailable` — **this machine cannot ask this question**. Never rendered as down, never counted
  as downtime, and the state machine treats it as neutral.

`ALIVE_CODES` includes 401 on purpose: v2rayA and Syncthing answer 401 when healthy.

### 4. `state::apply_wave` — the machine

Six invariants, each because it went wrong once:

| | Rule | Why |
|---|---|---|
| **I1** | down after 4 consecutive failures; recovery immediate | one dropped packet is not an outage, but a return is news |
| **I2** | a service missing from a non-empty payload is `down` after 4 absences | "no longer reporting" ≠ healthy |
| **I3** | an empty payload writes nothing at all | an empty payload is a broken poll, not N deaths |
| **I4** | first sighting seeds silently | a new machine must not page you |
| **I5** | ignored services still alert, but leave uptime | the operator said "don't count this", not "don't tell me" |
| **I6** | flags read at serve time, never baked into a snapshot | a toggle must take effect without a restart |

Per row: `seed_new` (I4) or `advance`. `advance` returns `Some(Change)` **only on a real
transition** — and that is what stops a flapping service paging every wave.

A subtle one worth knowing: `up` and `degraded` are both "answering", so a `degraded → up` change
is *recorded* but does not produce a `Change`. Alerting on it would page for a file one second past
its threshold. Before that fix, `degraded` was sticky in the `service_state` table and the card
showed a word that stopped being true hours earlier.

### 5. `notify.rs` — the message

```
changes → events_from_changes() → Digest → RateLimiter → deliver() → sinks
```

- **Silence collapses first.** Seventeen services on a node going quiet is *one* message, not
  seventeen. This is returned early, before the per-service loop.
- **Digest** buffers for a window so a mass outage becomes one page.
- **RateLimiter** is a hard ceiling: 6 messages per 60 s, so a flapping service cannot silence your
  phone by filling it.
- **`deliver` iterates and collects.** One sink failing never suppresses another.
- **`sink.enabled == false`** is skipped *here*, not at load, so switching a route on takes effect
  without editing the file again.

`agent_for(use_proxy)` implements I8: a notification never goes through the proxy unless the sink
opts in. A route that reports a proxy's death must not depend on that proxy.

### 6. The head receives it — `ingest::handle_report`

```
declared_len > 64 KB     → 413
not serving as head      → 503
token                    → node_for_token() by SHA-256 digest, constant-time compare
no such digest           → 401
report.node.id ≠ token's node → 403
parse                    → 400 on anything malformed
apply                    → 200
```

`alerts_for()` is attached to **every** response including 401/403/413/503, because the case that
most needs a message is the one where the head is not serving: a tail that has never paired gets a
401 for ever, and that is exactly the fact it must be able to announce.

`touch_node_with_metrics` upserts the node row. `name` mirrors `reported_name`; `display_name` is
the operator's override and is never overwritten by the device.

### 7. The dead man's switch — `watchdog.rs`

Lives on the **tail**, because only a tail observes the reports it sends.

```
State:    Routine → Armed → Waiting
Signal:   Down | Degraded | Up
DOWN_AFTER = 300 s, fixed
```

- `Instants`, never wall clock: a laptop that suspends for two hours wakes with a gap
  indistinguishable from an outage and would alarm on every lid close.
- `Outcome::Up` → Routine, and silence.
- `Answered{code}` → `Degraded` once. The head is alive and refusing; that is a pairing problem,
  not an outage.
- Nothing for 300 s → `Down` once.
- One message per *transition*, silence while the state holds.

The classification is a **type**, not string matching: `Outcome::{Up, Answered, Unreachable}`. The
401-vs-timeout distinction cannot be expressed safely by reading prose.

---

# Configuration, and how the GUI edits it

`config.rs` parses TOML into plain structs. Two rules:

- **A broken edit never takes the poller down.** `Watcher::refresh()` keeps the last good config
  and stores the error.
- **A reload returns exactly the file.** No discovery, no rescan. Discovery is an action the
  operator takes (Scan, or `discover`), never something a node does to itself on the way up.

`settings.rs` is the only writer. It edits the file **surgically** — preserving comments, ordering,
and every other block — rather than re-serialising the whole config. Every key it accepts:

```
tls_cert   tls_key   head_name   report_to   poll_interval
node_id    bind      port        report_port
listen                          (the serve switch)
sink                            (a whole route block, or null to delete it)
```

`catalog::to_toml()` is the single renderer for a service block. There used to be a hand-formatted
copy in the CLI that silently dropped fields it did not know about — `iface` vanished from any
block a user pasted.

---

# The HTTP surface

`server.rs` classifies every route into three lists, and one test asserts they do not drift:

- **`REPORTS_PORT_ROUTES`** — `["/api/report", "/api/health"]`. Everything else on 8771 answers
  **404**, not 403: a 403 confirms a route exists, which a LAN scanner should not get for free.
- **`PUBLIC_ROUTES`** — reachable without a session: health, auth status/login/logout, and
  `/api/pair` (a tail must be able to *ask* to join before it has an account).
- **`MACHINE_ROUTES`** — `/api/report`, authenticated by bearer token instead of a cookie.

Then `App` state: one snapshot, one poll lock (a second concurrent poll gets 409), one device cache
(5 s), one notifier, one login limiter.

**Auth gate order** (`route()`), and the order matters:

1. Report-port connection → allow only `REPORTS_PORT_ROUTES`, else 404.
2. Non-API path → serve the static shell (the login page must load before a cookie exists).
3. Public route → allow.
4. Machine route → leave to the report handler.
5. No valid session → 401 `needs_login`.
6. Signed in with the **default password** → 403 for everything except changing it, signing out,
   and checking status. The default password is not a way to read the dashboard.
7. Otherwise → the route.

`/status/...` is stripped as a **prefix only** (`strip_prefix`, not `replace`). A `replace` turned
`/api/status` into `/api/`, and the auth check then compared a path that no longer named the route.

---

# Things that will bite you

Every entry here cost real time once.

**`index.html`, `sw.js`, `catalog/services.toml` are embedded with `include_bytes!`/`include_str!`.**
Any change to them needs a `cargo build` — a binary that looks unchanged will silently serve the old
shell and make a correct fix look broken.

**The `hidden` attribute loses to an explicit `display`.** `.container{display:flex}` stays visible
even with `hidden`. One global `[hidden]{display:none !important;}` is the fix, not per-component
overrides.

**`sysinfo` on macOS returns an empty `cmd` for every process** unless you pass
`ProcessRefreshKind::new().with_cmd(UpdateKind::Always)`. The probe looks at the process table and
finds nothing, silently, and every process service reads down.

**`statvfs` over-reports APFS usage ~10x on macOS.** `metrics::disk()` shells out to `df -k`.

**rustls 0.23 panics on first use** unless a `CryptoProvider` is installed. Two `Once` blocks do it,
in `probes::insecure_tls` and `server::tls_config`.

**`ps`, not `sysinfo`, for the process table.** `sysinfo` cannot read the command line or memory of
a process owned by another user — so on a real machine (vocechat as `vocechat`, xray as `root`) most
services came back unreadable. `ps -Ao` reports them all without privileges.

**Never `cp` over a running binary** on Linux — you get `Killed: 9`. Write `.new`, then `mv -f`.

**A process probe excludes its own PID and every ancestor.** Without it, a probe launched from a
shell whose command line contains the word matches the shell, and every service reads up forever.

**TOML ordering**: a key written *after* a `[table]` header belongs to that table. `report_to` after
`[node]` becomes `node.report_to` and is silently ignored.

**One unit name, one service.** `dev.observerfrog` / `observer-frog.service` for both roles. The role
is a config setting, never part of the service identity — role-suffixed units left a second copy
running that fought for the port.

---

# Known gaps in the code, as of this writing

Not opinions — measured.

**`/api/notify/test` exists and works, but is in no route list.** `DASHBOARD_ROUTES` has 26 entries
and the test asserts that count; the actual `route()` matches 27 paths. The test protects the
constant, not reality, so adding a route passes while adding it to the list is optional. The
notifications test button works; the classification list is one short.

**`catalog::Entry::expected_off` is carried all the way to the API and then ignored.** The field
exists, parses, survives the round trip, and is asserted in a test — but nothing in `state.rs` or
`state::uptime_rows` consults it. A service marked as "installed but normally not running" still
counts toward uptime, which is the exact thing the field's doc comment says it exists to prevent.
The `ignored` table is the working mechanism; `expected_off` is a second, unimplemented one.

**`config::merge_discovery` is unused.** It is a correct function with no callers, left behind when
discovery was reverted to manual-only. It is not dead code in the "delete me" sense — it is the
function a future automatic-discovery feature would use — but nothing calls it today.

---

# Where to change what

| You want to | Change |
|---|---|
| add a probe kind | `probes::run`'s match arm **and** `PROBES` in `config.rs` |
| add a notification channel | `SINK_TYPES` in `config.rs`, a `publish_*` in `notify.rs`, a branch in `Notifier::deliver`, a card in `index.html` |
| add a service to the catalog | `catalog/services.toml` only — the parser needs no change |
| add an API route | the match arm in `server::route` **and** `DASHBOARD_ROUTES` |
| change the schema | a new `if version < N` block in `db::init`, bump `SCHEMA_VERSION`, and a test that climbs from the previous version |
| change a setting from the GUI | a key in `settings::apply`, then wire it in `index.html` |
| change how a service is rendered | `catalog::to_toml` — never hand-format a block anywhere else |

---

# Building and shipping

```
cd rust && cargo build --release        # ~2.6 MB, LTO, opt-level=z, panics abort
cargo test                              # 143 tests
```

Deploy is **build on the Linux box and copy to the other Linux boxes** — a macOS build is an arm64
Mach-O and will `Exec format error` on Linux. The Mac builds its own.

```
Mac     ~/Library/Application Support/observer-frog/observer-frog   (LaunchAgent dev.observerfrog)
Linux   ~/.local/state/observer-frog/observer-frog                  (user unit observer-frog.service)
config  macOS: ~/Library/Application Support/observer-frog/config.toml
        Linux: ~/.config/observer-frog/config.toml
db      macOS: beside the config     Linux: ~/.local/state/observer-frog/status.db
```

`install` copies the binary to the stable path **and restarts**. `enable --now` on an already-running
unit is a no-op, so an install that only copies serves a stale binary until the next reboot.
