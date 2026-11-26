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

0.4.0 is the current release. 108 tests, one source tree, macOS and Linux.

---

## 0.5.0: close the door

**Why first:** the admin API is unauthenticated and reachable on the plain-HTTP report port. Anyone
on the LAN can add services, flip the role, approve pairing requests or unpair machines. This is the
only item on the roadmap where doing nothing is an active exposure.

### Scope

- **Operator authentication.** One password, bcrypt cost 12, session cookie (`HttpOnly`,
  `SameSite=Lax`, `Secure` only when the listener has TLS), login serialised and rate-limited.
- **Listener identity.** The two ports share one router, so the connection carries its role and route
  policy is an exhaustive match. A dashboard route on the report port answers 404.
- **Every admin route gated.** Reads too: service and machine names are what an attacker wants.
- **Ingest keeps its own scheme.** Tails authenticate with their bearer token and cannot hold a
  session. Token for machines, session for people.
- **No service data in persistent browser storage.** This reverses F11. The shell stays cached so the
  app launches instantly; the payload does not, because it names every service and machine. The
  dashboard is also covered on backgrounding, so the iOS app switcher never shows it.
- **Password management as its own Settings tab**, including "log out all other devices".
- **Reset is a CLI subcommand**, not an HTTP route: a loopback rule cannot be made honest behind a
  reverse proxy.
- **First run** is a 0600 token file, cleared when the password is set.
- **TLS import in Settings.** The operator supplies the certificate; the product never mints one.
  Validated for parse, match and expiry. Applies on restart, because a listener binds at boot.
- **GUI first.** Anything the CLI can do with certificates must be doable from the dashboard.

### Not in scope

Per-user accounts. Roles. 2FA. Password recovery. One operator, one password.

### Done when

- Every operator route returns 401 without a session, reads included.
- A dashboard route on the report port returns 404, and tail reports still work.
- The phone loads the dashboard over HTTPS with an imported certificate.
- Backgrounding the app and reopening the switcher shows a blank screen, not the last dashboard.
- With the LAN unplugged, the app opens to a neutral message rather than cached service names.
- `passwd --reset` from the console changes the password and logs out every live session.

The detailed plan lives in [PLAN-0.5.0.md](PLAN-0.5.0.md).

---

## 0.6.0: notifications that reach you

**Why:** the dashboard answers when you look at it. This is the version where it tells you without
being asked.

### Scope

- **PWA push.** VAPID key pair, a subscription table per device, a `push` handler in the service
  worker, and an enable button in Settings. iOS requires the PWA installed to the home screen and a
  trusted certificate, which 0.5.0 provides.
- **Notification routes.** A section in Settings to pick one or several sinks. The operator decides
  where an alert goes; nothing decides for them.
- **ntfy stays.** It is self-hosted on the LAN and works with no internet at all. Push routes through
  a vendor's servers and dies in an international cut. Both exist because they fail at different
  times.
- **Webhook stays.**

### Not in scope

Per-service or per-severity routing. Digest and quiet hours. Telegram and SimpleX, in 0.7.0.

### Done when

- A service going down produces exactly one push, not one per service on the node.
- The same event also reaches ntfy when both are enabled, with the silence rules intact.
- A cut test: with the internet down, push fails and ntfy still delivers on the LAN.
- Toggling a route in Settings takes effect without a restart.

---

## 0.7.0: more routes, and the record

### Scope

- **Telegram sink**, through the existing per-sink `use_proxy`. The field has been in the config
  since 0.2 and has had no sink that needs it.
- **SimpleX sink**, for the same reason: it is the one the operator already runs.
- **Metrics graphs.** The data is already there: `hourly_rollup` keeps 30 days of up/down/degraded
  counts and latency sums. This is frontend work over an existing table, not new collection.
- **Windows.** The first honest attempt at autostart (Task Scheduler). Until this lands, `install`
  refuses on Windows and the dashboard works without a service.
- **Ubuntu and Arch.** The user unit has only been proven on Debian 13. Ubuntu and Arch both ship
  systemd and lingering, but their packaged tooling, firewall defaults and `ss` output are not
  identical, and "it is Linux" is not a test result. Arch in particular is rolling: the point is to
  find out what breaks on a system that updates underneath the binary, not to declare it supported
  from a single install. Support means installed, enabled and surviving a reboot on real machines.
- **Three and four machines.** Two nodes have been proven. Three and four exercise what two cannot:
  roster ordering, the pairing list with several pending requests, and one card per machine at a
  scale where a wrong node id stops being invisible. This is the test the 0.4.0 pairing work never
  had.

### Done when

- Each sink can be enabled and disabled alone, and a failure in one does not suppress another.
- Graphs render 30 days of uptime and latency without a new query pattern.
- Windows: the binary installs, starts at login, and survives a reboot.
- Ubuntu and Arch: same, each on a real install (not a Debian box wearing the name), including a
  reboot and an upgrade on Arch to catch what rolling release breaks.
- Three machines and then four: all report, all appear, pairing approves each, and removing one
  leaves the others untouched.

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

---

## Open decisions

Recorded here so a later version answers them deliberately:

1. **Platform breadth.** Windows, Ubuntu and Arch are all scheduled for 0.7.0. If any of the three
   turns out to need more than a unit file (Windows probably will), it splits into its own release
   rather than delaying the others.
2. **Graph granularity.** Per-service first, or per-node first.
3. **Push client.** Whether to depend on a web-push crate or implement VAPID and payload encryption
   in-tree. The encryption is nontrivial; the dependency is small.
4. **Nothing blocking 0.5.0.** Its plan is decided and in
   [PLAN-0.5.0.md](PLAN-0.5.0.md).

Settled during review: no pairing verification code, because the head's approval already proves what
a code would; the login view replaces the shell rather than covering it, so an authenticated view
never exists in the DOM before a session does; and the setup token is accepted only from its 0600
file, not from loopback as a second path with the same trust assumption.
