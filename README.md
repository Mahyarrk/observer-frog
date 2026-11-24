# observer-frog

A small, self-hosted status dashboard for the machines you own. It watches the services you care
about, records when they go down and come back, and serves a phone-installable PWA you can check
from anywhere on your network.

**Status: pre-alpha.** Nothing here is installable yet. The single-machine version is being built
now; remote nodes come after. If you found this early, watch the repo and wait.

## What it will do

- Watch local services via HTTP, TCP, process, file age or a JSON file — or pick from a shipped
  catalog of common self-hosted software with sensible defaults.
- Show machine health (CPU, memory, disk, network, uptime) with no configuration at all.
- Keep 24 hours of raw checks and 30 days of hourly rollups in SQLite.
- Notify you when something stays down long enough to matter, not on every flap.
- Run as one native binary with no runtime and no container.

## What it will not do

- Run inside Docker. A container cannot see host processes, its own cgroup is not the host's memory,
  and the published ports are not the real listeners. Keep it native.
- Terminate TLS for you. Put it behind your own reverse proxy or a VPN. (It can be given a
  certificate; it will not pretend to be a CA.)
- Replace your existing uptime monitor's public status pages or 40 notification channels.

## Honest limits

Best-effort support. One person maintains this around other work, so issues may sit. GPU utilization
is unavailable on macOS and temperatures need elevation on Windows; both report as unavailable rather
than guessing. Process matching is name-based and therefore best-effort.

## Licence

MIT.
