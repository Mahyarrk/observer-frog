# Roadmap

This is the shape of the project from where it stands to 0.7.0. Each version below is a theme, not
a task list. The detailed plan for a version gets written when that version starts, from this
document.

Everything here is scoped by one rule: the thing must earn its place. A feature that adds a
dependency, a moving part, or an attack surface needs a reason stronger than "it would be nice".

---

## Shipped

| Version | What landed |
|---|---|
| 0.1 | One binary, one config, five probe kinds, catalog, PWA dashboard |
| 0.2 | Two machines: head aggregates, tail reports over HTTP, dual listener (TLS for phones, plain for tails) |
| 0.3 | Install and autostart on macOS and Linux, Settings GUI, service RAM on cards, silence and 413 gates |
| 0.4 | Role switching as a live setting, pairing with operator approval, one stable unit name, service discovery and add-from-panel |
| 0.5 | Operator sign-in, listener identity, TLS upload from Settings, scheme discovery for pairing |
| 0.6 | Notification routes in Settings (ntfy and webhook), a dead man's switch, bounded poll interval |
| 0.6.1 | Ubuntu 26.04 support, discovery merged with explicit services on every path, retention actually running |

0.6.1 is the current release. 142 tests, one source tree, macOS and two Linux distributions.

---

## 0.5.0: close the door (shipped)

**Why it was first:** the admin API had been unauthenticated and reachable on the plain-HTTP report
port. Anyone on the LAN could add services, flip the role, approve pairing requests or unpair
machines.

### What landed

- **Operator sign-in.** One password, bcrypt cost 12. Session cookie is `HttpOnly`, `SameSite=Lax`,
  `Secure` when the listener has TLS. Logins are serialised, because bcrypt on a two-core box would
  otherwise starve the poller, and rate-limited per peer: five failures, fifteen minutes.
- **Listener identity.** A dashboard route asked of the report port answers 404, as if it did not
  exist there. The two listeners finally know which one they are.
- **Every admin route gated, reads included.** Service and machine names are the thing an attacker
  wants.
- **Machines keep their own scheme.** A tail presents a bearer token and cannot hold a session. Token
  for machines, cookie for people.
- **Nothing worth reading in the shell.** The dashboard is not in the page source while signed out,
  and the header and progress strip hide with it: they name the machine and its port.
- **TLS upload from Settings.** Two file pickers, not path fields. The pair is parsed, proven to
  match, and only then written: random temp names, `0600` key, `0644` certificate, atomic rename. A
  refused upload writes nothing at all.
- **Pairing finds the dashboard by itself.** The port comes from discovery, the scheme is discovered
  by asking the listener. Neither is assumed, so a head with a certificate and one without are both
  reachable from the same button.
- **No way to remove the password.** Decided against while testing: a dashboard that anyone holding
  an unlocked phone can open to the whole LAN is not a setting worth having.

### Not in scope, and still not

Per-user accounts. Roles. 2FA. Password recovery. One operator, one password.

### Verified on real machines

Every operator route returns 401 without a session. A dashboard route on the report port returns 404
while tail reports still work. The phone loads the dashboard over HTTPS with an uploaded certificate.
Backgrounding the app and reopening shows a blank screen. With the LAN unplugged it opens to a
neutral message. `passwd --reset` from the console changes the password and ends every session. Two
machines, one acting as head, pairing approved by hand.

The plan, kept for the reasoning rather than as a task list: [PLAN-0.5.0.md](PLAN-0.5.0.md).

---

## 0.6.0: notifications that reach you (shipped)

**Why it was next:** the dashboard answers when you look at it. This is the version where it tells you
without being asked.

### What landed

- **Notification routes in Settings.** ntfy and webhook, one shape: a tick, an address, an optional
  token variable, a proxy opt-in, and a test button that publishes through the real route code. Head
  only; a tail's tab says why it is greyed.
- **A disabled route is quiet, not absent.** Unticking keeps every field and stops sending. Clearing
  deletes the block, and asks first.
- **`enabled` is always written explicitly**, so a route the operator switched off can never be read
  back as a pre-0.6.0 block and silently resurrected by the default.
- **A dead man's switch.** A tail notices its head has stopped answering and says so once. The
  heartbeat is the report it already sends. Three states, one message per transition, a fixed five
  minute threshold, and a monotonic clock so a sleeping laptop does not alarm on waking.
- **Bounded poll interval**, 15 to 60 seconds, refused rather than clamped. This is what lets the
  threshold be a constant.
- **The head hands its alert routes to tails** on every report answer, errors included, because the
  cases that most need a message are the ones where the head is not serving.

### Not in scope, and still not

Web push (VAPID, subscriptions, a service worker handler). Telegram and SimpleX. Per-service or
per-severity routing. Digest and quiet hours. A watchdog for a single-machine install.

The plan, kept for the reasoning rather than as a task list: [PLAN-0.6.0.md](PLAN-0.6.0.md).

---

## 0.6.1: a third machine (shipped)

**Why it was next:** the software ran on two machines I controlled and had never met one I did not.
A headless Ubuntu server is the honest test of that.

### What landed

- **Ubuntu 26.04 support.** Same static binary, built on Debian and copied outward, because a binary
  built on the newer glibc would not run on the older one. Fifteen services monitored on a machine
  with no display server and no toolchain.
- **Discovery merged with explicit services, on every path that reads a config.** This was the
  release's real work. Discovery had been gated on `services.is_empty()`, which meant a config
  naming even one service switched it off; and the three places that read a config — boot, the
  one-shot reporter, and the config watcher — each had their own copy of the logic, so they
  disagreed. On the Ubuntu box that read as twelve services present at boot and absent one wave
  later, with the head marking each one "no longer reporting". One function now, called from all
  three.
- **Retention that runs.** `db::prune` and its 24-hour and 30-day constants had existed since the
  schema was written and were called only by a test. The table had reached 93k rows and 56 hours of
  history against a documented 24. It now prunes hourly on whichever node owns the database, and the
  first tick reclaimed 39,866 rows.
- **Notifications proven end to end.** The sink pointed at a port nothing listened on, so no alert
  had ever been delivered. Fixed, then verified by stopping a real service and watching the down and
  recovery messages arrive on a phone.
- **The catalog carries what it actually knows.** Sunshine's entry now says that a headless Linux
  box binds its web UI while being unable to capture a desktop, so `up` is not proof of a working
  stream host. WireGuard's says it is two implementations, not one program on two platforms.

### What it did not do

Windows still has no machine, so it has no proof. `9router` and `osiris` were left off the Ubuntu box
by decision — one is a 820 MB Next.js app on a 3.5 GB Atom, the other publishes no release assets.
The `docker` entry still names the macOS process, so it reads down on a Linux host that runs Docker.

The plan: [PLAN-0.6.1.md](PLAN-0.6.1.md).

---

## 0.7.0: more routes, and the record

### Scope

- **Telegram sink**, through the existing per-sink `use_proxy`. The field has been in the config
  since 0.2 and has had no sink that needs it.
- **SimpleX sink**, for the same reason: it is the one the operator already runs.
- **Both arrive on the shape 0.6.0 established**: a `[[sink]]` block, a card in the same template, an
  `enabled` tick and a test button. A route that needs a different settings shape needs an argument
  first.
- **Web push**: VAPID key pair, a subscription table per device, a `push` handler in the service
  worker, and an enable button. It is the only route that reaches a phone without a third-party
  relay or a server the operator runs — and, unlike ntfy, it cannot be lost to a wrong port number
  going unnoticed for a week.
- **Metrics graphs.** The data is already there: `hourly_rollup` keeps 30 days of up/down/degraded
  counts and latency sums. This is frontend work over an existing table, not new collection.
- **Windows.** The first honest attempt at autostart (Task Scheduler). Until this lands, `install`
  refuses on Windows and the dashboard works without a service.
- **A fourth machine.** Three nodes now report to one head, so roster ordering and multiple approved
  pairings have been exercised for real. Four adds what three cannot: a second machine running the
  same software, which is where a wrong node id stops being invisible, and where one card per
  machine either scales or does not.

### Done when

- Each route can be enabled and disabled alone, and a failure in one does not suppress another. This
  is already true of the two that shipped; the new ones inherit it rather than re-earning it.
- Graphs render 30 days of uptime and latency without a new query pattern.
- Windows: the binary installs, starts at login, and survives a reboot.
- Ubuntu: the binary installs as a user unit, starts at boot without a login, and survives a reboot.
  (Three machines reporting to one head is already proven; what is untested is the reboot.)
- A fourth machine: all report, all appear, pairing approves each, and removing one leaves the others
  untouched.

---

## Continuous, not version-gated

- **Catalog expansion.** Every entry is data. Adding one is a config block plus a signature that
  proves identity, never code.
- **Probe accuracy.** When a probe is wrong, the entry is wrong, not the platform.
- **The 10-minute rule.** Never work silently; every long operation reports progress or stops.

---

## Deliberately not doing

These have been decided against, and are recorded so they are not re-argued:

- **Prometheus export.** No demand, and a second metrics surface to keep correct.
- **Multi-head or failover.** One head, always. Two heads are two dashboards, not a cluster.
- **Container packaging.** A container cannot see host processes or the host's memory; the entire
  point is measuring the real machine.
- **Store-and-forward.** A tail that cannot reach its head loses those samples. A buffer is a second
  database with its own failure modes.
- **A certificate authority.** The product terminates TLS; it does not issue or renew.
- **Free-form probe builder.** The catalog exists so that a card means a known thing.
- **Arch Linux.** Rolling release means the runtime moves underneath a static binary that was tested
  against a snapshot, and the failure mode is subtle rather than loud. Two stable distributions cover
  the Linux case; a third that changes weekly buys breadth at the cost of a support promise nobody
  can keep.

---

## Open decisions

Recorded here so a later version answers them deliberately:

1. **Platform breadth.** Windows is the only platform left unscheduled. If it needs more than a
   trivial service definition (it probably will — Task Scheduler is not systemd), it gets its own
   release rather than delaying the rest of 0.7.0.
2. **Graph granularity.** Per-service first, or per-node first.
3. **Push client.** Whether to depend on a web-push crate or implement VAPID and payload encryption
   in-tree. The encryption is nontrivial; the dependency is small.
4. **Nothing blocking 0.6.0.** Push needs a VAPID key pair, which is the one generated secret this
   project would own. Where it lives, and whether it rotates, is the first decision of that version.

Settled during review: no pairing verification code, because the head's approval already proves what
a code would; the login view replaces the shell rather than covering it, so an authenticated view
never exists in the DOM before a session does; and the first-run password is the default `123456`
rather than a setup token, because a token file is a second place to look and a default that must be
changed is one place to remember. A password cannot be removed: only replaced.
