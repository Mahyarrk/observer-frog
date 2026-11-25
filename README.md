# observer-frog

I run a handful of services across two machines: a Mac that does the work and a small Debian box
that holds the pieces I do not want to babysit. Checking on them meant SSH, then `systemctl status`,
then remembering which port belongs to what. I wanted one page I could look at from my phone, so I
wrote this.

It watches the services you list, records when they go down and come back, keeps 30 days of uptime,
and serves a PWA you can install on your home screen.

It runs on more than one machine: one install is the head that serves the dashboard, and every
other machine is a tail that reports to it. It ships as one static binary, with no runtime and no
temp directory extracted at launch.

## Quick start

```bash
git clone https://github.com/Mahyarrk/observer-frog.git
cd observer-frog/rust
cargo build --release
./target/release/observer-frog
```

That serves `http://127.0.0.1:8770/`. No configuration is required to get value out of it: the
machine's CPU, memory, disk and uptime show up immediately, and you start with zero services.

With a certificate, the same binary speaks HTTPS itself:

```bash
./target/release/observer-frog --tls-cert cert.pem --tls-key key.pem --bind 0.0.0.0
```

A deployment looks like a staging area and a promotion step: build a candidate, run it beside the
one in use against the same configuration, compare, then promote.

## Two machines

A tail probes its own services and posts the result. It runs no server and needs no certificate.

```bash
# on the head: register the tail and its token (the token comes from the environment)
export TAIL_TOKEN=$(openssl rand -hex 24)
observer-frog enrol laptop --token-env TAIL_TOKEN --interval 30

# on the tail: report every 30s
export TAIL_TOKEN=...            # the same value
observer-frog report --to http://head:8771 --token-env TAIL_TOKEN
```

The head stamps arrival time, so a tail with a wrong clock cannot corrupt the record. Reports are
idempotent: a retry over a flaky link changes nothing. A tail that stops reporting has its
services marked unavailable, never down, and you get one notification per node rather than one
per service.

Each device names itself from its own hostname. A display name can be set from the dashboard, and
clearing it restores the name the machine reports.

### One port for phones, one for tails

A phone installing the PWA needs TLS; a tail on a LAN should not be asked to manage
certificates. Both listeners can run at once over the same state:

```toml
[serve]
port = 8770           # TLS here, for browsers
report_port = 8771    # plain HTTP here, for tail reports
```

## Adding a service

There is a catalog of about fifty common self-hosted things, each with a default port and, where the
service exposes one, a signature that proves you are looking at it.

```bash
FROG=./target/release/observer-frog
$FROG catalog             # everything available
$FROG catalog monitoring  # just one group
$FROG add syncthing       # prints a config block
$FROG add 9router --port 20127   # you moved it off the default
$FROG add sonarr --instance 2    # a second copy
```

Paste the block into `config.toml`. A running server picks the change up without a restart.

The config lives at `~/Library/Application Support/observer-frog/config.toml` on macOS,
`~/.config/observer-frog/config.toml` on Linux, and `%APPDATA%\observer-frog\config.toml` on Windows.

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
best-effort and I will not pretend otherwise, because a hung process still reads as present. When
that distinction matters to you, put an `http` probe on the same service as well.

`file_age` is the honest probe for anything periodic. "Did it run in the last fifteen minutes" is a
better question than "is a process alive right now", which flaps every time a job finishes and exits.

`http` entries can carry a `contains` signature, checked against the body or the final URL. That
matters more than it sounds: without it, whatever happens to be listening on the default port gets
reported as your service.

## Things it deliberately does not do

No Docker. A container cannot see host processes, its own cgroup is not the host's memory, and
published ports are not the real listeners. Run it natively; mine sits around 8 MB resident.

No certificate authority. The binary terminates TLS itself when you give it `--tls-cert` and
`--tls-key`, but it will not issue or renew anything, so the certificate is yours to obtain. Bind
loopback by default and put Caddy, nginx, WireGuard or Tailscale in front if you would rather not
manage certificates at all.

No service control. There is no start or stop endpoint, so a bad token cannot restart anything.

No public status pages and no notification zoo. Two sinks: ntfy, and a generic webhook that
accepts a JSON POST.

No store-and-forward. A tail that cannot reach the head loses those samples rather than queueing
them; there is no buffer to drain later.

## Honest limits

I maintain this around other work, so issues may sit for a while. GPU utilisation is not available on
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

## Checking a machine without starting the server

```bash
./target/release/observer-frog metrics   # this host's numbers
./target/release/observer-frog status    # resolved config and database
```

## Tests

```bash
cd rust && cargo test        # 36 tests
```

Every behavioural rule in `docs/DESIGN.md` has a test named after it, and the tests exist because
each rule was learned the hard way. One test does not check behaviour at all: it runs a probe wave and
fails if peak memory crosses 50 MB, because a small footprint is the reason this port exists and
nothing else would notice a regression in it. The down-confirmation delay is there because every restart used
to page me. The vanished-service rule is there because a dead remote node once left sixteen cards
reading healthy for hours.

## How this was built, and by whom

The code was written by AI agents running in Hermes Agent, under my direction. I did not type it,
and I am not going to imply otherwise.

It began in Python. That original is frozen on the `python-v0.1` branch and no longer part of this
tree. Rust replaced it for distribution: 2.6 MB, against 48 MB for a single-file Python executable
that antivirus software flags. The port was verified rather than asserted, and `rust/parity.py` still
does that comparison against the frozen branch: run both builds on the same configuration and it
checks every endpoint, every service state and every response shape. They serve byte-identical web
assets. Memory is 8 MB against 40.

What I did was the part that decides whether the result is any good. I set the constraints: native
install only, no container, one binary with independent `serve` and `report_to` flags instead of a
mode switch, a catalog of known services instead of a free-form probe builder, one flag for "I know
about this" rather than two that mean nearly the same thing. I chose what to leave out, which was
most of it. I read the diffs, ran it against my own machines, and reported what broke: a probe that
matched its own command line and reported every service healthy, a runtime function that repainted
the old icon over the new one, counters that split on the wrong flag, a `const` used before its
declaration that blanked the whole page.

The tests were written by the AI as well. That is worth knowing when you weigh them: they encode the
same understanding that produced the code, so they catch regressions in behaviour we already thought
about, not mistakes we never considered. The bugs above were found by using the thing, not by the
suite.

The Rust port leans the other way, which is part of why I made it canonical. The compiler rejects
whole classes of mistake that Python accepts silently, and the two most expensive bugs in the port
were both caught by running it rather than compiling it: a process scan that returned an empty command
line for every process unless asked for it explicitly, which made every process probe report "not
running", and a TLS library that panics at first use unless a crypto provider is registered. Loud
failures at the moment of use are worth more than a clean compile.

What that means for you: read the code before you trust it with anything that matters, expect rough
edges, and treat support as best-effort. The licence says the software comes with no warranty, and in
this case that is not boilerplate.

## Licence

MIT.
