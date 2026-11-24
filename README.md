# observer-frog

I run a handful of services across two machines: a Mac that does the work and a small Debian box
that holds the pieces I do not want to babysit. Checking on them meant SSH, then `systemctl status`,
then remembering which port belongs to what. I wanted one page I could look at from my phone, so I
wrote this.

It watches the services you list, records when they go down and come back, keeps 30 days of uptime,
and serves a PWA you can install on your home screen.

Pre-alpha. The single-machine version runs, and its test suite is the thing I trust most about it.
Remote nodes and install packaging do not exist yet.

## Quick start

```bash
git clone https://github.com/Mahyarrk/observer-frog.git
cd observer-frog
uv venv --python 3.12 && uv pip install -p .venv/bin/python psutil
.venv/bin/python -m statushub
```

That serves `http://127.0.0.1:8770/`. No configuration is required to get value out of it: the
machine's CPU, memory, disk and uptime show up immediately, and you start with zero services.

## Adding a service

There is a catalog of about fifty common self-hosted things, each with a default port and, where the
service exposes one, a signature that proves you are looking at it.

```bash
.venv/bin/python -m statushub catalog             # everything available
.venv/bin/python -m statushub catalog monitoring  # just one group
.venv/bin/python -m statushub add syncthing       # prints a config block
.venv/bin/python -m statushub add 9router --port 20127   # you moved it off the default
.venv/bin/python -m statushub add sonarr --instance 2    # a second copy
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
published ports are not the real listeners. Run it natively; mine sits around 40 MB resident.

No TLS termination and no certificate authority. It accepts `--tls-cert` and `--tls-key` and
otherwise expects you to front it with your reverse proxy or a VPN. Bind loopback by default and put
Caddy, nginx, WireGuard or Tailscale in front.

No service control. There is no start or stop endpoint, so a bad token cannot restart anything.

No public status pages and no notification zoo. ntfy only, for now.

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
.venv/bin/python -m statushub metrics   # this host's numbers
.venv/bin/python -m statushub status    # resolved config and database
```

## Tests

```bash
.venv/bin/python -m unittest discover -s tests -t .
```

Every behavioural rule in `docs/DESIGN.md` has a test named after it, and the tests exist because
each rule was learned the hard way. The down-confirmation delay is there because every restart used
to page me. The vanished-service rule is there because a dead remote node once left sixteen cards
reading healthy for hours.

## How this was built, and by whom

The code was written by an AI agent (Claude-class models running in Hermes Agent) under my direction.
I did not type the Python, and I am not going to imply otherwise.

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

What that means for you: read the code before you trust it with anything that matters, expect rough
edges, and treat support as best-effort. The licence says the software comes with no warranty, and in
this case that is not boilerplate.

## Licence

MIT.
