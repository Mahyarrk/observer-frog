# observer-frog

A status dashboard for the machines you own. It watches the services you care about, remembers when
they were down, and serves a phone-installable PWA.

**Pre-alpha:** the single-machine version runs and is verifiable by its test suite. Install packaging
and remote nodes do not exist yet.

## Quick start

```bash
git clone https://github.com/Mahyarrk/observer-frog.git
cd observer-frog
uv venv --python 3.12 && uv pip install -p .venv/bin/python psutil
.venv/bin/python -m statushub          # serves http://127.0.0.1:8770/
```

It works with no configuration: this machine's CPU, memory, disk and uptime appear immediately, and
zero services until you add one.

## Adding a service

Pick from the shipped catalog instead of writing probes by hand:

```bash
.venv/bin/python -m statushub catalog             # list everything available
.venv/bin/python -m statushub catalog monitoring  # filter by group
.venv/bin/python -m statushub add syncthing       # prints a config block
.venv/bin/python -m statushub add 9router --port 20127   # custom port
.venv/bin/python -m statushub add sonarr --instance 2    # a second copy
```

Paste the block into `config.toml`; a running server picks it up without a restart.

Config lives at `~/Library/Application Support/observer-frog/config.toml` (macOS),
`~/.config/observer-frog/config.toml` (Linux), `%APPDATA%\observer-frog\config.toml` (Windows).

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

## The five probe kinds

Everything in the catalog is one of these, and every one can be written by hand:

| probe | asks | example target |
|-------|------|----------------|
| `http` | does it answer, and does it look like itself | `http://127.0.0.1:8384/rest/noauth/health` |
| `tcp` | is the port accepting connections | `127.0.0.1:5432` |
| `process` | is a matching command line running | `dockerd` |
| `file_age` | was this file written recently | `~/backups/.last-run` |
| `file_json` | does a field in this JSON hold this value | `~/.state.json` + `proxy.node` |

`process` exists for services with no port: a sync client, a VM, a job with no socket. It is
best-effort by nature — a hung process still reads as present. Where that distinction matters, put
an `http` probe on the same service.

`file_age` is the honest probe for anything periodic: "did it run in the last 15 minutes" is a
better health question than "is a process alive right now", which flaps every time a job exits.

`http` entries carry a `contains` signature where the body is public, so a coincidental listener on
the default port is not mistaken for the service.

## What it deliberately does not do

- **No Docker.** A container cannot see host processes, its own cgroup is not the host's memory,
  and published ports are not the real listeners. Run it natively — measured ~40 MB resident.
- **No TLS termination, no CA.** It accepts `--tls-cert`/`--tls-key`; exposure is your reverse proxy
  or your VPN. Bind loopback and front it with Caddy, nginx, WireGuard or Tailscale.
- **No service control.** There is no start/stop endpoint in this version.
- **No public status pages and no notification zoo.** ntfy only, for now.

## Honest limits

One person maintains this around other work, so issues may sit. GPU utilization is unavailable on
macOS without root; temperatures need elevation on Windows and lie on some desktops. Both report as
unavailable rather than guessing. Process matching is name-based. The catalog is a curated list, not
a discovery engine.

## Endpoints

| path | purpose |
|------|---------|
| `/` | the dashboard |
| `/api/status` | current snapshot (never cached by the service worker) |
| `/api/history?days=N` | incidents, uptime and KPIs |
| `/api/poll` | force one wave (a second concurrent caller gets 409) |
| `/api/ignore` | POST `{id, ignored}` — one flag, no second meaning |
| `/api/health` | liveness |

## Checking a machine without starting the server

```bash
.venv/bin/python -m statushub metrics   # this host's numbers
.venv/bin/python -m statushub status    # resolved config + database
```

## Tests

```bash
.venv/bin/python -m unittest discover -s tests -t .
```

Every behavioural invariant in `docs/DESIGN.md` has a test named after it.

## Licence

MIT.
