# observer-frog

I run a handful of services across two machines: a Mac that does the work and a small Debian box
that holds the pieces I do not want to babysit. Checking on them meant SSH, then `systemctl status`,
then remembering which port belongs to what. I wanted one page I could look at from my phone, so I
wrote this.

It watches the services you list, records when they go down and come back, keeps 30 days of uptime,
and serves a PWA you can install on your home screen. One static binary, no runtime, ~8 MB resident.

## Quick start

```bash
git clone https://github.com/Mahyarrk/observer-frog.git
cd observer-frog/rust
cargo build --release
./target/release/observer-frog
```

That serves `http://127.0.0.1:8770/`. No configuration is needed to get value out of it: CPU, memory,
disk and uptime appear immediately, and you start with zero services.

To have it come back on its own:

```bash
observer-frog install     # LaunchAgent on macOS, user unit on Linux
```

## Two machines

Every copy runs the same code and keeps its own dashboard. Which one aggregates is a setting, not an
install profile, and it can be flipped from the dashboard without a restart.

```toml
[serve]
enabled = true       # this machine collects from the others
report_to = ""       # empty: it sends nothing anywhere
```

Set `enabled = false` and give `report_to` an address to make a machine report to another instead.
A machine with neither is idle, not broken: it still shows its own numbers.

Pairing happens in Settings, in the Pairing tab. Scan the LAN, pick the head, and the request goes
out; the head shows it with a tick and a cross, and nothing is trusted until you click. The token
travels one way and only its digest is stored, so a stolen database hands nobody a working
credential.

## Adding a service

There is a catalog of 22 common self-hosted things, or you can write a probe by hand.

```bash
observer-frog catalog              # what is available
observer-frog add syncthing        # prints a config block to paste
observer-frog discover             # what is running on this machine right now
```

The same actions are behind the gear icon in the header, in the Services tab, with custom ports
and removal.

## More

Setup, pairing, probe types, the config format, endpoints, and what it deliberately does not do:
**[docs/GUIDE.md](docs/GUIDE.md)**.

The rules behind the behaviour, and why each exists: [docs/DESIGN.md](docs/DESIGN.md).

Where it is going, and what it deliberately will not do: [docs/ROADMAP.md](docs/ROADMAP.md).

## How this was built, and by whom

The code was written by AI agents running Hermes Agent, under my direction. I did not type it, and I
am not going to imply otherwise.

What I did was the part that decides whether the result is any good. I set the constraints: native
install only, no container, one binary with independent `serve` and `report_to` settings instead of a
mode switch, a catalog of known services instead of a free-form probe builder. I chose what to leave
out, which was most of it. I read the diffs, ran it against my own machines, and reported what broke.

The tests were written by the AI as well, which is worth knowing when you weigh them: they encode the
same understanding that produced the code. Read the code before you trust it with anything that
matters, expect rough edges, and treat support as best-effort. The licence says the software comes
with no warranty, and in this case that is not boilerplate.

## Licence

MIT.
