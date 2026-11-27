# 0.6.1: Ubuntu support

A third machine joins the fleet, and the first one nobody is sitting in front of.

0.6.0 was about the head staying honest when a tail goes silent. 0.6.1 is about the same
software running somewhere with no GUI, no desktop session, and no operator watching it boot.
The code should not care. The proving of it will find where it does.

## The orders, restated

1. observer-frog works on the Ubuntu server.
2. **Everything the laptop runs, miix runs.** Not a curated subset — the whole inventory, so the
   two Linux boxes become comparable machines.
3. **v2rayA tested on both**, without disturbing the laptop's Xray.
4. **Sunshine Linux tested on both**, as a catalog entry.
5. Pair miix as a tail under the laptop head.

Orders 1 and 5 are code work. Order 2 is installation. Orders 3 and 4 need care, because both
touch running systems and one of them can break the laptop's proxy path.

## What is already known about the target

Measured, not assumed:

```
host       miix320            the Atom x5-Z8350 2-in-1, not the Surface
cpu        Intel Atom x5-Z8350 @ 1.44GHz    4 cores
os         Ubuntu 26.04.1 LTS               kernel 7.0.0-30-generic
arch       x86_64
ram        3454 MB      2954 MB available   0 MB swap in use
disk       19 GB free of 27 GB
glibc      2.43
systemd    259 (259.5-0ubuntu3.4)
sudo       needs a password
ssh        key ~/.ssh/mahyar_miix, the existing `miix` alias
toolchain  no rustc, no gcc, python 3.14.4, curl, no docker
linger     Linger=no                          user manager running
listening  ssh only
dri        /dev/dri/card1 + renderD128       i915, cgroup v2
display    no X server, no Wayland           headless
```

No compiler on the box settles how this ships: **build on the laptop, copy the binary.**
A build-from-source plan would mean installing a Rust toolchain and a linker on a 4 GB Atom
to produce a binary the laptop can already produce in ninety seconds.

Ubuntu 26.04's glibc is much newer than Debian 13's 2.41. That direction is the safe one: a
binary built on the laptop runs on miix, not necessarily the reverse. **Build order is fixed:
laptop first, deploy outward.** Same rule that already governs the Mac-to-laptop path.

## What "everything the laptop runs" means

The laptop's real inventory, split by unit scope:

**System units**

```
9router     ntfy        searxng      smp-server   syncthing@mahyar
tailscaled  tun2socks   vocechat     xftp-server  xray        xray-watchdog
```

**User units**

```
observer-frog   osiris   status-node   terraria-server   vaultwarden
```

Not all of this goes on miix. Three catalog entries are not portable at all, and two services
are deliberate judgement calls:

| Entry | Decision | Why |
|---|---|---|
| `lmstudio` | exclude | macOS and Windows only |
| `hermes-desktop` | exclude | macOS bundle path, documented in the entry |
| `9router` | exclude | a second router in front of the same model backend is not a comparison |
| `status-node` | exclude | laptop-specific telemetry, superseded by observer itself |
| `hermes` | excluded by omission | the agent is not a catalog service; its API is |

Everything else gets installed, `terraria` included — it is 60 MB and idle, and "everything the
laptop runs" was the order.

**The measured cost case, and where it actually bites.** miix has 2.9 GB free; the laptop has
1.6 GB. RAM is not the constraint — the laptop is the more loaded machine. **CPU is.** Four
Atom cores at 1.44 GHz is a different class from the laptop's Celeron N3350, and `n8n` failed on
the laptop precisely by pegging the CPU until SSH stopped answering. So the sequence is install
one, measure, keep it only if the cost is small. The measurement is not optional.

**The heaviest single item** is searxng at ~240 MB resident plus a Python venv. It goes in
last, after everything else is proven, so a failure there is unambiguous.

## Order 3: v2rayA, and the collision

**This is the part that can break the laptop, so it is designed defensively.**

The laptop does not run v2rayA. It runs **Xray**, holding:

```
20171  HTTP   ← xray.service
20170  SOCKS  ← xray.service
```

v2rayA's proxy defaults are those same two ports. They do not collide *at the listener* —
v2rayA binds 20170/20171 only once an outbound is connected, which is why it can be installed
and probed beside another proxy. They collide **the moment a v2rayA node is connected on the
laptop**, and that would take the proxy out from under `tun2socks` and therefore under SearXNG
and anything else routing through Xray.

The rules, therefore:

- **miix first.** It has nothing on 2017, 20170 or 20171. Install v2rayA there, point it at a
  subscription, connect a node, let it have the default ports. That is the real test of the
  catalog entry and it costs nothing.
- **The laptop gets v2rayA installed and its UI probed on a non-default port** —
  `v2raya --lite -a 0.0.0.0:2018` — and **no node is ever connected there.** Connecting one is
  the action that moves the proxy ports, so it does not happen. The entry is proven by the
  admin UI answering on 2018; the proxy path stays Xray's.
- **Before and after every laptop step**, capture `ss -lnt` and confirm 20170/20171 still belong
  to the same process and `curl` through 20171 still answers. If that check ever fails,
  `systemctl stop v2raya` and Xray is untouched.
- **Never run v2rayA as root on the laptop.** `--lite` is implied for a non-root user and keeps
  the config under the home directory, so it cannot rewrite anything system-wide.

The whole safety argument in one sentence: the dangerous action is *connecting a node*, and the
plan never takes it on the laptop.

## Order 4: Sunshine on Linux

Sunshine is not in the Ubuntu archives, so both machines get it from the upstream release
tarball. It installs and runs on Linux; what it cannot do here is **capture**.

Upstream is explicit: *"Only X11 and Wayland capture are supported."* Both machines are
headless with no display server. `/dev/dri` exists on both (`i915`), so the binary should start
and bind its web UI — which is exactly what the catalog entry probes (tcp 47990). It will
report **up** while being unable to stream a desktop.

That distinction is real and the entry's `notes` must say so, or the next person trusts a green
light that means less than it appears to. **The entry earns its place on the port signature
alone**; the honest note is what keeps it from lying.

Two things to settle during the work, not before:

- whether Sunshine binds 47990 or exits when capture fails at startup. If it exits, the entry
  reads `down` and the note becomes "starts only with a display server", which is also honest.
- whether miix's i915 does VAAPI encode. If it does not, Sunshine is a web UI and nothing more
  on this hardware.

Sunshine on the **Mac** is untouched — it runs there already and its entry is proven.

## Work packages

### WP1: a third tail does not disturb the first

Before touching miix, prove the head handles two. A second scratch tail on the laptop reporting
to the live head, distinct node id, own interval. Then check:

- both nodes in `nodes`, each with its own `last_seen_ts`
- `service_state` keeps them separate; one node's silence does not grey the other
- the dashboard lists two and the selector switches between them
- silence detection is per-node: stopping the scratch tail must not mark `MacM4` stale
- the scratch tail's dead man's switch does not fire on the first tail's behalf

The last one is the one I expect to find something in.

### WP2: run it on Ubuntu

- copy the laptop's built binary; no toolchain goes on the box
- `--version`, then `discover`, before any config — a missing glibc symbol dies here, the
  cheapest place to find out
- `observer-frog install` as a **user** unit; nothing needs root except apt
- `loginctl enable-linger mahyar` — the laptop needs this too and it is easy to forget
- verify: `/proc` metrics, the device card naming Ubuntu rather than Linux, a process probe,
  an interface probe, `ss -lntH` parsing
- verify discovery recognises at least three entries by signature

The risk is small: the Linux paths were written against Debian and are `/proc`-based, so Ubuntu
26.04 should behave identically. "Should" is why WP2 exists.

### WP3: the laptop's inventory, on miix

Install in ascending order of risk, measuring after each:

1. `ssh` (present), `nginx`, `syncthing`, `wireguard` (+ `ip link add wg0 type wireguard`)
2. `ntfy`, `vocechat`, `vaultwarden`, `smp`, `xftp`, `osiris`
3. `terraria`, `tailscaled`, `tun2socks` if the WG path is to mirror the laptop's
4. `searxng` last, with its venv, measured before anything is stacked on top

Per service: install, start, confirm discovery finds it **by signature**, add it, confirm `up`,
stop it, confirm `down`. A service that reads `up` while stopped is worse than one that never
appears.

`vaultwarden` is the important one — the only entry proving `scheme = "https"` off the Mac.

### WP4: v2rayA, in the safe order

Separated from WP3 because it is the one that can damage a running system.

- **miix:** install, `--lite`, default ports, connect a node, verify the UI on 2017 and the
  proxy answering on 20171. Add the entry, confirm `up`.
- **laptop:** snapshot `ss -lnt`; install; `-a 0.0.0.0:2018`; probe the UI; **connect nothing**;
  re-snapshot and confirm 20170/20171 still belong to the same process; confirm
  `curl -x http://127.0.0.1:20171` still reaches the internet. Then stop it and leave Xray as
  the only proxy, which is the current state.
- If any step makes 20171 stop answering, `systemctl stop v2raya` immediately and re-verify.

### WP5: Sunshine Linux on both

- install from upstream on both machines
- confirm it binds 47990; if it refuses to start without a display, record that instead
- the entry stays `tcp 47990`, with the note corrected to say what it actually proves
- explicitly do **not** add a display server to either box to make streaming work

### WP6: the two unused probe kinds

One `file_age` and one `file_json` service in the miix config, pointed at files that exist.
Proves both paths on Linux and settles whether they earn their keep. Same class of check that
caught the WireGuard fault.

### WP7: pair miix as a tail

Laptop stays head, untouched. miix proposes a token, the head approves. Then:

- the miix node appears with its own services
- Mac tail and miix tail both green at once, Mac's services intact
- stopping the miix unit produces a `down` naming the miix node, and only that
- the miix dead man's switch names the laptop as the head

### WP8: documents

`ROADMAP.md` gains the 0.6.1 row. `GUIDE.md` gains a Ubuntu section: what to install, what needs
root, what does not, and the v2rayA port warning. `README.md`'s opening says "a Mac and a small
Debian box"; three machines make that wrong.

## What I need from you

1. **The sudo password for miix**, or you run the apt commands and I do the rest. WP2, WP6 and
   WP7 need no root at all.
2. **Tailscale on miix: yes or no.** Plan assumes yes now that miix is a real node.
3. **A v2rayA subscription for miix.** The Mac has one; miix needs its own or a share of it.

## Tests I will do, and tests you must do

I will: every work package's verification on all three machines, with output pasted.

You must:

- confirm the PWA renders three nodes on your phone
- confirm a real notification arrives when the miix tail stops — the one claim this project has
  never proven for any machine
- confirm miix reboots with the unit up and unsupervised — the whole point of a headless tail
- confirm Moonlight still works against the Mac, if Sunshine Linux turns out to matter

## Not in this version

- **Windows.** Still no machine, so still no proof.
- **The `docker` Linux process name.** Stays a known gap; `dockerd` is unverified and guessing
  process names is how the WireGuard note went wrong.
- **A display server on either Linux box.** Sunshine's capture limitation is recorded, not
  worked around.
- **Any new sink.** 0.7.0 owns Telegram and SimpleX.
- **CI.** One Ubuntu box tested by hand is evidence; a workflow with no second Linux machine to
  run it on is decoration.
