# observer-frog: the details

Everything here was in the README and got too long for it. If you only want to run the thing, the
README is enough.

## Two machines

Every copy always does both jobs: it probes this machine and keeps a dashboard, whether or not
anyone else is looking at it. Two settings decide what leaves the machine.

| Setting | Default | What it does |
|---------|---------|--------------|
| `[serve] enabled` | `true` | Aggregates other machines and watches for their silence |
| `report_to` | empty | Where this machine sends its own results |

A machine with `enabled = true` and nothing in `report_to` is a head: it collects. A machine with
`enabled = false` and a `report_to` is a tail: it sends. A machine with neither is idle. It still
shows its own numbers, and says in Settings that no head is paired. Nothing crashes, nothing
restarts.

Both switches are read while the process is running, so changing one takes effect within a few
seconds.

### One port for phones, one for tails

A phone installing the PWA needs TLS; a tail on a LAN should not be asked to manage certificates.
Both listeners can run at once over the same state:

```toml
[serve]
port = 8770           # TLS here, for browsers
report_port = 8771    # plain HTTP here, for tail reports
```

## Pairing

A tail should not be handed a password and told to trust whoever answers. Discovery finds heads on
the LAN; pairing is what turns one into a relationship.

```
tail:  finds a head, generates a token, asks to join
head:  shows the request in Settings with a tick and a cross
you:   click one
```

Until you click, that request is not a credential: the machine cannot report, and its node does not
appear in the roster. Approving stores the token's digest, so the token itself never travels from head
to tail and a stolen database does not hand anyone a working credential. Rejecting leaves the
machine unable to report.

Settings lists machines already paired, and removing one revokes its token for good: it has to ask
again.

## What survives, and what does not

The head stamps arrival time, so a tail with a wrong clock cannot corrupt the record. Reports are
idempotent: a retry over a flaky link changes nothing. A tail that stops reporting has its services
marked unavailable, never down, and you get one notification per node rather than one per service.

When a machine stops acting as head, it forgets every other machine completely: roster, history and
enrolment. That is a real unpairing, not a pause: the machines must ask again afterwards. Keeping
credentials for hosts it no longer watches would be worse.

Each device names itself from its own hostname. A display name can be set from the dashboard, and
clearing it restores the name the machine reports.

## Adding a service

There is a catalog of 22 common self-hosted things, each with a default port and a signature that
proves you are looking at it. The list is small on purpose: an entry is data, and a wrong entry
costs more than a missing one.

The catalog lives behind a gear icon in the header, in three tabs: This Node, Services, and Pairing.

```bash
FROG=./target/release/observer-frog
$FROG catalog             # everything available
$FROG catalog monitoring  # just one group
$FROG add syncthing       # prints a config block
$FROG add 9router --port 20127   # you moved it off the default
$FROG add sonarr --instance 2    # a second copy
```

Paste the block into `config.toml`. A running server picks the change up without a restart, and the
same actions are in the dashboard's Settings page.

The config lives at `~/Library/Application Support/observer-frog/config.toml` on macOS,
`~/.config/observer-frog/config.toml` on Linux, and `%APPDATA%\observer-frog\config.toml` on
Windows.

```toml
[node]
id = "local"
name = "My Server"

[serve]
bind = "127.0.0.1"
port = 8770

[[service]]
id = "9router"
name = "9router"
group = "Model Routing"
probe = "http"
target = "http://127.0.0.1:20128/"
ports = "20128"
contains = "/dashboard"
```

## Five kinds of probe

Every catalog entry compiles down to one of these, and you can write any of them by hand.

| probe | asks | example target |
|-------|------|----------------|
| `http` | does it answer, and does it look like itself | `http://127.0.0.1:8384/rest/noauth/health` |
| `tcp` | is the port accepting connections | `127.0.0.1:5432` |
| `process` | is a matching command line running | `dockerd` |
| `file_age` | was this file written recently | `~/backups/.last-run` |
| `file_json` | does a field in this JSON hold this value | `~/.state.json` plus `proxy.node` |

`process` is for things with no port: a sync client, a VM, a job that never opens a socket. It is
best-effort, because a hung process still reads as present. When that distinction matters, put an
`http` probe on the same service as well.

`file_age` is the honest probe for anything periodic. "Did it run in the last fifteen minutes" is a
better question than "is a process alive right now", which flaps every time a job finishes and exits.

`http` entries can carry a `contains` signature, checked against the body or the final URL. Without
it, whatever happens to be listening on the default port gets reported as your service.

## Running it without thinking about it

`install` writes the service definition and starts it. No plist, no unit file, no root:

```bash
observer-frog install     # LaunchAgent on macOS, user unit on Linux
observer-frog uninstall   # and it is gone
```

The same command is how an upgrade lands. It copies the binary somewhere durable, points the service
at that copy, and restarts it. So `install` after a rebuild is the whole upgrade procedure. A unit
that pointed at `target/release/` would break the first time anyone ran `cargo clean`.

Autostart can also be switched from Settings, which matters on a machine you reach from a phone.
Turning it off stops it starting at boot without stopping what is running now: those are different
questions, and the switch only answers the first.

On Linux the user unit needs lingering to start before anyone logs in:

```bash
loginctl enable-linger $USER
```

The installer says so rather than pretending the unit is enough on its own.

## Things it deliberately does not do

No Docker. A container cannot see host processes, its own cgroup is not the host's memory, and
published ports are not the real listeners. Run it natively; mine sits around 8 MB resident.

No certificate authority. The binary terminates TLS when you give it `--tls-cert` and `--tls-key`,
but it will not issue or renew anything, so the certificate is yours to obtain. Those flags only work
from a shell: the installed service does not read them yet, so a service-managed head speaks plain
HTTP on its dashboard port. Config-file TLS is the next thing to land. Bind loopback by default and
put Caddy, nginx, WireGuard or Tailscale in front if you would rather not manage certificates.

No service control. There is no start or stop endpoint, so a bad token cannot restart anything.

No public status pages and no notification zoo. Two sinks: ntfy, and a generic webhook that accepts
a JSON POST. ntfy is preferred because it can be self-hosted, which means it still works when the
internet does not.

No store-and-forward. A tail that cannot reach the head loses those samples rather than queueing
them; there is no buffer to drain later.

No second service definition per role. One unit per machine, named for the machine rather than the
job, because a name that changes with a checkbox is a name a restart cannot find.

## Honest limits

Maintained around other work, so issues may sit for a while. GPU utilisation is not available on
macOS without root, and temperatures need elevation on Windows and lie on some desktops; both report
as unavailable rather than inventing a number. Process matching is name-based and will occasionally
surprise you. The catalog is a curated list, not a discovery engine.

## Endpoints

| path | purpose |
|------|---------|
| `/` | the dashboard |
| `/api/status` | current snapshot, never cached by the service worker |
| `/api/history?days=N` | incidents, uptime and KPIs |
| `/api/poll` | run one probe wave now; a second concurrent caller gets 409 |
| `/api/ignore` | POST `{id, ignored}` |
| `/api/health` | liveness |
| `/api/pair` | GET pending requests and paired machines; POST a request (a tail calls this) |
| `/api/pair/ask` | POST `{url}`: write the head and send the request in one call |
| `/api/pair/decide` | POST `{id, approve}`: the tick and the cross |
| `/api/pair/remove` | POST `{node_id}`: revoke one machine |
| `/api/heads` | scan the LAN for heads; never lists this machine |
| `/api/nodes` | machines this head knows about |
| `/api/node/name` | POST a display name for this machine |
| `/api/services`, `/api/services/add`, `/api/services/remove` | what this node watches |
| `/api/catalog` | the shipped service catalog |
| `/api/discover/services` | what is running on this machine right now |
| `/api/settings` | read or change role, interval, autostart |
| `/api/autostart` | read or set the boot service |
| `/api/report` | a tail posts here; bearer token required |

Pairing routes are unauthenticated on purpose: the token is not a credential until someone
approves it. Anything else on the LAN can create a pending request, and only a click on the head
makes it real.

## Checking a machine without starting the server

```bash
./target/release/observer-frog metrics   # this host's numbers
./target/release/observer-frog status    # resolved config and database
```

## Tests

```bash
cd rust && cargo test        # 126 tests
```

Every behavioural rule in `docs/DESIGN.md` has a test named after it. One test does not check
behaviour at all: it runs a probe wave and fails if peak memory crosses 50 MB, because a small
footprint is the reason this port exists and nothing else would notice a regression in it.

## Why Rust

It began in Python. That original is frozen on the `python-v0.1` branch and is no longer part of this
tree. Rust replaced it for distribution: 2.6 MB, against 48 MB for a single-file Python executable
that antivirus software flags. `rust/parity.py` verifies the port against the frozen branch: both
builds on the same configuration, every endpoint, every service state, every response shape. They
serve byte-identical web assets. Memory is 8 MB against 40.
