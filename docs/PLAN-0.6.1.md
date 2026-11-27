# 0.6.1: Ubuntu support

A third machine joins the fleet, and the first one nobody is sitting in front of.

0.6.0 was about the head staying honest when a tail goes silent. 0.6.1 is about the same
software running somewhere with no GUI, no desktop session, and no operator watching it boot.
The code should not care. The proving of it will find where it does.

## The three orders

1. observer-frog works on the Ubuntu server.
2. Install the lighter parts of the catalog there, and prove each one reads `up`.
3. Pair the Ubuntu box as a tail under the laptop head.

Orders 1 and 3 are code work. Order 2 is mostly installation, and it is the only order that
can fail for reasons that are not ours.

## What is already known about the target

Measured, not assumed:

```
host       miix320            the Atom x5-Z8350 2-in-1, not the Surface
cpu        Intel Atom x5-Z8350 @ 1.44GHz    4 cores
os         Ubuntu 26.04.1 LTS               kernel 7.0.0-30-generic
arch       x86_64
ram        3537 MB                          3773 MB swap
disk       19 GB free of 27 GB
glibc      2.43
systemd    259 (259.5-0ubuntu3.4)
sudo       needs a password
ssh        key ~/.ssh/mahyar_miix, the existing `miix` alias
toolchain  no rustc, no gcc, python 3.14.4, curl, no docker
linger     Linger=no                          user manager running
listening  ssh only
```

No compiler on the box settles how this ships: **build on the laptop, copy the binary.**
A build-from-source plan would mean installing a Rust toolchain and a linker on a 4 GB Atom
to produce a binary the laptop can already produce in ninety seconds.

Ubuntu 26.04 is much newer than Debian 13's glibc 2.41. That direction is the safe one: a
binary built on the laptop runs on the Ubuntu box, and a binary built on the Ubuntu box may
refuse to start on the laptop. **Build order therefore matters and is fixed: build on the
laptop, deploy outward.** It is the same rule that already governs the Mac-to-laptop path.

## The finding that shapes this version

**The head has never served two tails.**

`status.db` on the laptop holds one row in `nodes` — `MacM4`. The `service_state` table carries
a second `node_id`, `watchdog`, but that is from earlier testing and no node row backs it.

Every assumption about multi-node operation is therefore untested: that two tails reporting on
different intervals do not overwrite each other, that the dashboard's node selector stays
correct, that silence detection is per-node rather than global, that the dead man's switch in
one tail does not fire because a different tail is late, and that the pairing table tolerates a
second request without disturbing the first.

This gets its own work package, and it runs **before** the Ubuntu pairing. Proving two tails
work is worth more than proving Ubuntu works, because the Ubuntu box is only interesting as
the second tail.

## A second finding, smaller

`expected_off` has been in the config since 0.5.0 and **no catalog entry uses it.**

That field exists for software installed but normally not running, and it has never been
exercised because every machine so far runs everything it has installed. A server is the first
place where that is untrue — software gets installed for a purpose and left stopped until the
purpose arrives. This version is where the field either proves itself or gets deleted.

## What "lighter parts" has to mean

The target is an Atom x5-Z8350: four slow cores, 3.5 GB of RAM, 19 GB of disk, and 3.7 GB of
swap already in use as a cushion. That is a machine that will accept nearly any single one of
these services and object to several at once. The light set is therefore sequenced, not
parallel: install one, watch what it costs, keep it only if the cost is small. `n8n` failed on
the Debian laptop for exactly this reason — a native build pegged the CPU until SSH stopped
answering — so the same caution applies here and the measurement is not optional. The catalog also contains three entries that **cannot run on Ubuntu at all**:

| Entry | Why not |
|---|---|
| `lmstudio` | macOS and Windows only |
| `hermes-desktop` | macOS bundle path (documented in the entry itself) |
| `sunshine` | wants a GPU and a display session, not a headless server |

None of these need code. Discovery already reports an entry as not-found rather than dangling it
in front of the user — proven on the laptop in the last version — so the correct handling is to
leave them alone and record the exclusion here.

The rest, sorted by how well they fit a headless server:

**Fits cleanly, install and test:**

- `nginx` — apt, the oldest package on the box, http 80
- `syncthing` — apt, http 8384
- `ssh` — already present, tcp 22
- `ntfy` — single static binary, http 2586
- `vocechat` — single static binary, http 3009
- `vaultwarden` — single static binary, https 8443; the only catalog entry that proves the
  TLS probe path on a non-Mac host
- `smp`, `xftp` — single static binaries, tcp 5223/5224; the tcp path
- `osiris` — binary, http 3008
- `searxng` — Python venv, http 8080. Remembered cost on the Debian laptop is ~240 MB resident.
  On a 3.5 GB Atom that is affordable alone and questionable next to everything else, so it is
  installed **last** and measured before anything else is added on top of it. If it does not fit,
  it comes out and the exclusion gets written down.

**Installs and proves a probe without needing the service configured:**

- `wireguard` — apt installs it, and `ip link add wg0 type wireguard` creates the interface with
  no peer and no config. This is the honest way to test the kernel-driver signal on Linux
  without building a tunnel that serves no purpose.

**Deliberately excluded:**

- `docker`, `portainer` — heavy, root, and a second container runtime is not "lighter parts"
- `ollama` — models are measured in gigabytes
- `9router`, `hermes` — both already run on the laptop; a second instance proves nothing about
  Ubuntu and duplicates an agent
- `terraria` — a game server is not light, and nothing is playing it
- `tailscale` — joins his tailnet as a third node. Needs a decision, not a default. If it goes
  in, it is his call and after the pairing works.

**Two probe kinds with no catalog entry at all:** `file_age` and `file_json` exist in the code
and are used by nobody. They cannot be exercised by installing catalog software, so a service
of his own — a file that must stay fresh — is the only way to prove them on Ubuntu. That is one
small work package, and it is the same class of check that caught the WireGuard fault.

## Work packages

### WP1: a third tail does not disturb the first

Before touching the Ubuntu box, prove the head handles two. A second scratch tail on the laptop
reporting to the live head, with a distinct node id, on its own interval. Then check:

- both nodes stay in `nodes`, each with its own `last_seen_ts`
- `service_state` keeps them separate; one node's silence does not grey the other
- the dashboard lists two and the selector switches between them
- silence detection is per-node: stopping the scratch tail must not mark `MacM4` stale
- the dead man's switch in the scratch tail does not fire on the first tail's behalf

The last one is the one I expect to find something in.

### WP2: run it on Ubuntu

- copy the already-built laptop binary; there is no toolchain on the box and none is going on it
- confirm it starts at all before configuring anything: `--version`, then `discover`. A binary
  that dies on a missing glibc symbol dies here, which is the cheapest place to find out
- `observer-frog install` as a **user** unit; confirm no root is needed for anything except apt
- `loginctl enable-linger mahyar` so the unit survives logout — the same requirement the laptop
  already has, and easy to forget on a fresh box
- verify: metrics read correctly from `/proc`, the device card names Ubuntu not Linux, a process
  probe works, an interface probe works, `ss -lntH` parsing works
- verify discovery recognises at least three catalog entries by signature

The known risk here is small: the Linux paths were written against Debian and are `/proc`-based,
so Ubuntu 26.04 should behave identically. "Should" is why WP2 exists.

### WP3: install and prove the light set

Per service, in order of increasing difficulty: install, start, confirm discovery finds it by
signature, add it, confirm the dashboard reads `up`, then stop it and confirm `down`. A service
that reads `up` while stopped is worse than one that never appears.

`vaultwarden` is the important one: it is the only entry that proves `scheme = "https"` on a
machine that is not the Mac.

### WP4: the two unused probe kinds

One `file_age` service and one `file_json` service in the Ubuntu config, pointed at files that
exist. Proves both paths on Linux, and settles whether they earn their keep.

### WP5: pair Ubuntu as a tail

Leave the laptop as head, untouched. Pair the Ubuntu box the normal way: it proposes a token,
the head approves. Then confirm:

- the Ubuntu node appears in the dashboard with its own services
- the Mac tail and the Ubuntu tail are both green at once, with the Mac's thirty services intact
- stopping the Ubuntu unit produces a `down` notification naming the Ubuntu node, and only that
- the dead man's switch on the Ubuntu tail names the laptop as the head

### WP6: documents

`ROADMAP.md` gains the 0.6.1 row. `GUIDE.md` gains a Ubuntu section, short: what to install,
what needs root, what does not. `README.md`'s opening currently says "a Mac and a small Debian
box"; three machines make that line wrong.

## What I need from you

1. **The sudo password for the Ubuntu box**, or you run the apt commands yourself and I do the
   rest. Nothing else in this plan needs root. Everything in WP2, WP4 and WP5 runs as `mahyar`.
2. **Whether Tailscale joins as a third node.** Default is no.

## Tests I will do, and tests you must do

I will: every work package's verification, on both machines, with the output pasted.

You must:

- install the PWA on a phone and confirm three nodes render, if the selector changes
- confirm a real notification arrives on the phone when the Ubuntu tail stops — the one claim
  this project has never proven for any machine
- confirm the Ubuntu box reboots with the unit up and unsupervised, which is the entire point of
  a headless tail

## Not in this version

- Windows. Still no machine, so still no proof.
- The `docker` Linux process name. It stays a known gap; `dockerd` is unverified and guessing
  process names is how the WireGuard note went wrong.
- Any new sink. 0.7.0 owns Telegram and SimpleX.
- CI. One Ubuntu box tested by hand is evidence; a workflow written without a second Linux
  machine to run it on is decoration.
