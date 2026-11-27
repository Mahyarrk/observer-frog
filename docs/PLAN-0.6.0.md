# 0.6.0: a detailed plan

The theme is "notifications that reach you". Two subjects: a Settings tab where the operator chooses
which of several routes alerts go out on, and a dead man's switch that speaks while the head cannot.

Both routes use one shape. That is the rule this version establishes, because the third route should
cost a card and a config block, not a design discussion.

## The finding that shapes everything

The notification machinery already exists and already fires. `notify.rs` has 150 lines, nine tests,
a rate limiter, two sink kinds, and it is wired into three places:

```rust
runtime.rs:188   // a transition this machine observed itself
runtime.rs:315   // a silence sweep found a node gone
ingest.rs:250    // a transition that arrived by report from a tail
```

That last one matters. The head already notifies on a tail's state changes, with a comment saying so:
*"a transition that arrives by report deserves the same message as one this head observed itself."*
So a head-only notification tab covers every service on every machine. Nothing is lost by turning the
tail's sinks off.

What does not exist is any way to configure any of it. `grep -c sink index.html` returns **0**. The
only way to enable ntfy today is a hand-written `[[sink]]` block, and no example of that block appears
in `README.md` or `docs/`. A feature discoverable only by reading the parser is a feature that does
not exist for its users.

Second finding: `poll_interval` is user-editable and unbounded. `settings.rs:195` writes
`v.max(1)` and nothing else. A user who sets 600 gets a dead man's switch that fires on every wave.

Third: the tail classifies report failures by matching strings:
`e.contains("unknown token")`, `e.starts_with("503")`. The dead man's switch needs three distinct
classes from that same response, and substring matching on prose is how the pairing code came to aim
at the wrong port.

## What this reverses

Nothing in `docs/DESIGN.md`. This version adds; it does not overturn. The one design note worth
recording is that an alert channel is a *second* egress path out of this program, and it is the only
one that speaks to a third party.

## Decisions

1. **ntfy is external.** The program is a client. It never bundles, supervises or reimplements a
   server. Rejected after review: bundling a Go binary doubles the artefact per platform; a Rust
   reimplementation is a permanent compatibility burden against a moving API. The premise was also
   wrong: self-hosted ntfy reaching an iPhone must relay through ntfy.sh's Firebase, so a self-hosted
   server never delivered the "no dependency" property it was wanted for.
2. **No authentication on ntfy.** Accepted risk, and the reason is stated plainly in the UI: this
   route was chosen for simplicity. Other routes will carry the security when they arrive.
3. **The notification tab is head-only.** In tail mode the whole tab is greyed out, with a sentence
   saying why. Notifications belong to the machine that aggregates.
4. **`poll_interval` is bounded 15 to 60 seconds.** A named refusal, not a silent clamp.
5. **The down threshold is fixed at 300 seconds.** Five minutes, not derived from the interval. Safe
   precisely because the interval is now bounded: the worst case is 300 s of polling with a 60 s
   interval, which is five pings.
6. **Only a 200 is "up".** Any other answer from the head means degraded. Silence past the threshold
   means down. There is no fourth verdict.
7. **Each state transition sends at most one message.** Nothing repeats while a state holds.
8. **Messages name the head.** A person receiving three phones' worth of alerts needs to know which
   machine is talking.
9. **Duplicate alarms across several tails are accepted.**
10. **No same-machine watchdog.** No second process, no generated script, no second unit. A
    single-machine install has no dead man's switch, and the Settings hint says so rather than
    implying otherwise.
11. **One rule for every route.** ntfy, webhook, and anything added later use the same shape: a
    `[[sink]]` block with `enabled`, `url`, `topic`, `token_env`, `use_proxy`; the same Settings card;
    the same test button; the same rate limiter; the same proxy default. A route that needs its own
    settings shape is a route that needs its own justification.
12. **A sink gains `enabled`.** Today presence means active, so unticking a route would have to delete
    the operator's URL and topic. A field keeps their work while switching the route off.

Measured, not guessed:

| Fact | Value | Source |
|---|---|---|
| ntfy, self-hosted on a LAN | `ntfy 2.11.0`, `auth-default-access: read-write` | `/usr/bin/ntfy`, systemd unit |
| ntfy and the observer are separate service scopes | system unit vs user unit | `systemctl show` |
| a real publish to it | `200`, `{"id":"Pr6S57rxk93y",...}` | direct POST |
| the Celeron's wave duration | roughly 2 seconds | db mtime deltas over 90 s |
| report success body | `{"ok":true,"node":...,"received_at":...,"services":n}` | `ingest.rs:333` |

The last row is the dead man's switch: **the ping already happens**. A tail reports every interval and
a 200 *is* proof of life. No new network call is needed for the heartbeat, only a clock and a verdict.

## The state machine

Three states, because the operator can act on three different things.

| From | Observed | Message | To |
|---|---|---|---|
| Routine | 200 | none | Routine |
| Routine | any non-200 answer | **degraded**, once | Waiting |
| Routine | no answer, under 300 s | none | Routine |
| Routine | no answer, 300 s or more | **down** | Armed |
| Armed | 200 | **up** | Routine |
| Armed | any non-200 answer | **degraded**, once | Waiting |
| Armed | no answer | none | Armed |
| Waiting | 200 | **up** | Routine |
| Waiting | any non-200 answer | none, already said it | Waiting |
| Waiting | no answer, 300 s or more | **down** | Armed |

"Routine" is the everyday case: reporting succeeds and nobody is told anything. "Armed" is the
interesting one: the head has been declared dead and the switch is waiting to confirm or contradict
that. "Waiting" exists so a degraded head does not generate a message per wave; the operator has been
told once and the next news will be either up or down.

A recovery is only a recovery if a 200 is observed. A head that comes back and dies again inside five
minutes was never up, and the operator is still holding an alarm that is still true.

## Work packages

### WP1: a typed outcome for a report

`tail::report_once` returns `Result<Value, String>`. Replace the error side with an enum:

```rust
pub enum Outcome {
    Up(Value),                    // 200
    Answered { code: u16, why: String },  // 401, 403, 413, 503: the head is alive
    Unreachable { why: String },  // timeout, refused, dns
}
```

Every caller that currently matches on prose gets simpler, including the pairing trigger, which today
tests `e.contains("unknown token")` and will test `Outcome::Answered { code: 401, .. }`.

No behaviour change. This is the refactor that makes WP3 possible without adding a third string match.

**Acceptance:** the existing pairing-on-401 behaviour still works, proven by the same log line; no
`contains(` remains in the tail's classification path.

### WP2: bound the interval

`poll_interval` is read at `config.rs:365` as `n.max(1)`, and written at `settings.rs:195` as
`v.max(1)`. Both gain bounds.

```
MIN_POLL_INTERVAL_S = 15
MAX_POLL_INTERVAL_S = 60
```

A value outside the range is **refused with a sentence naming the range**, not clamped. The reason is
that a clamp changes behaviour the operator asked for and says nothing about it, and the next person
to read the config sees a number the program is not honouring.

The GUI's interval field refuses at the edge with the same sentence, so the config file is the second
line of defence rather than the only one. Same shape as the password length rule, which already names
its reason.

**Acceptance:** `poll_interval = 5` and `= 600` both produce a named refusal at load; the Settings field
will not save either; `15` and `60` both work.

### WP3: the dead man's switch

New module `rust/src/watchdog.rs`. It holds no socket and does no probing: it is a clock and a verdict,
fed by the tail loop.

```rust
pub struct Watchdog {
    state: State,          // Routine | Armed | Waiting
    last_up: Option<Instant>,  // monotonic, never wall clock
    alerted_down: bool,
}
```

Monotonic rather than wall clock, because suspend is the normal case: a laptop that sleeps for two
hours wakes with a timestamp gap that looks exactly like an outage. `Instant` does not advance across
suspend on macOS and Linux in the way `SystemTime` does, so a sleeping machine does not wake into a
false alarm.

Called from the tail loop after each report, with the outcome from WP1:

```rust
if let Some(msg) = wd.observe(outcome) { publish(msg) }
```

The clock is only advanced by `Outcome::Up`. Neither an answer nor silence counts as life.

Publishing goes through the existing `Notifier`, so the rate limiter, the proxy rule, the per-sink
failure list and both routes are shared rather than reimplemented. The switch feeds the same `Event`
that a service transition produces, which is why the message shapes in WP5 are the only new text.

If one route fails and another succeeds, the switch still counts as delivered: the operator has been
told, and the failure is recorded where the dashboard already shows it.

**Acceptance:** a head stopped with `SIGKILL` produces exactly one *down* message after 300 s and no
second one; restarting it produces exactly one *up*; a head answering 503 produces one *degraded* and
then nothing until it changes.

### WP4: the alert endpoint travels to the tail

The tail cannot notify about a head it has never reached, so the endpoint must be handed over on
**every** response, including the ones that say the head is not serving.

Add to the report response, on success and on 401, 403, 413 and 503:

```json
"alerts": [
  { "kind": "ntfy",    "url": "https://ntfy.example.net", "topic": "observer-frog" },
  { "kind": "webhook", "url": "https://hooks.example.net/frog" }
]
```

A list, not one object, so a second route is data rather than a second field. Only enabled sinks are
included, and the list is empty when none are, which is how a tail knows to stay quiet.

What deliberately does **not** travel: `token_env`. That names an environment variable on the head, and
a tail reading the same name would find either nothing or, worse, a different secret. A tail publishes
to an unauthenticated route or stays quiet.

The tail caches it in its own config, because it is needed at the moment the head cannot resend it.
Refreshed on every successful or answered report.

This makes the head's response into instructions, not merely an acknowledgement. A tampered tail
already holds a token that can write state, so this grants no new privilege, but it does mean nothing
sensitive may ever be added to that block. That is a comment in the code, not a mechanism.

**Acceptance:** a tail that has never paired still learns the endpoint from its 401 and can announce
that it is unpaired; a head with no sink configured produces a silent tail.

### WP5: the message shapes

Three kinds, each naming the head, using the existing priority constants:

| Kind | Title | Priority | Tags |
|---|---|---|---|
| down | `HeadName: Observer Frog head is down` | `PRIORITY_OUTAGE` (4) | `rotating_light` |
| degraded | `HeadName: Observer Frog head is up but not accepting reports` | 3 | `warning` |
| up | `HeadName: Observer Frog head is back up` | `PRIORITY_RECOVERY` (2) | `white_check_mark` |

`PRIORITY_OUTAGE` and `PRIORITY_RECOVERY` are reused unchanged. Degraded takes 3, between the two,
because it is a fault that needs attention but not the siren.

The body carries the machine that noticed and the last reason, because with several tails the reader
needs to tell one outage from three.

These shapes are what ntfy renders as a title and headers, and what a webhook receives as JSON fields
(`title`, `message`, `priority`, `tags`). One `Event`, every route, no route-specific text.

**Acceptance:** each message renders with a title, and the title contains the head's display name.

### WP6: the Settings tab, and one card per route

A fourth sub-tab beside This Node, Services and Pairing: **Notifications**.

Two cards, one per route, built from one component:

**ntfy.** Enable checkbox, base URL, topic, optional token environment variable, proxy opt-in,
**Send a test message**.

**Webhook.** Enable checkbox, URL, optional token environment variable, proxy opt-in, **Send a test
message**. No topic field: a webhook POSTs JSON to the URL and the receiver decides what to do with it.

Each card is the same markup driven by a small table, not a copy. The fields differ by one: ntfy has a
topic, a webhook has none. Everything else, including the proxy default and the test button, is
identical, which is the whole point of decision 11.

The test button matters more than it looks. A notification route that has never been exercised is a
belief, not a configuration; the button turns it into a fact in one click and is the single highest
value control on the tab. It publishes a real event through the real sink code, not a separate probe
path that could disagree with what production does.

In tail mode the whole tab renders greyed with one sentence: *"This machine is a tail. Notifications
belong to the head, which sees every machine's services."* Disabled, focusable-skipped, and it does not
poll.

### WP6a: the config writer

`settings.rs::apply` handles top-level keys and simple sections. A `[[sink]]` array-of-tables is a
third shape, and it is the real work in this package.

The writer replaces sinks **by kind**, leaving any other block untouched:

- set fields on an existing `[[sink]]` of that kind, or append a new one
- remove the block when a route is cleared rather than merely unticked
- never leave a partial block, because a malformed `[[sink]]` takes the node down at load

Round-trip is the acceptance test: write, reload through `config::load`, compare field by field.
A writer that produces TOML the parser reads differently is worse than no writer, since it fails at
the next restart instead of at the click.

**Acceptance:** enabling ntfy, typing a URL and topic, and pressing test produces a real ntfy message;
the same for webhook, with a JSON body a receiver can read; unticking keeps the URL and stops sending;
clearing removes the block; the written config reloads to identical values; the tab is greyed on a
tail.

### WP6b: honour `enabled` on every sink

`config::as_sink` gains `enabled`, defaulting to **true** when the key is absent, so every config
written before this version keeps working unchanged. `Notifier::deliver` skips a sink whose `enabled`
is false, in the same place it currently skips an unknown kind.

Defaulting to true rather than false is the only choice that does not silently stop alerts on an
upgrade. A monitoring tool that goes quiet because of a schema change is the worst possible failure.

**Acceptance:** an existing `[[sink]]` with no `enabled` key still publishes; adding `enabled = false`
stops it without removing the block.

### WP7: documentation and the honest hint

`docs/GUIDE.md` gains the worked example it has never had: a complete `[[sink]]` block and a sentence
on where to get the ntfy URL. `README.md` gains one line saying notifications exist.

The Settings hint for the dead man's switch states its limit in plain words: *"This alerts you when
this head's process stops, if another machine is running the dashboard. If this machine loses power
or the network, nobody is told."* A limitation stated in the UI is a design decision; the same
limitation discovered in an outage is a defect.

**Acceptance:** a reader who has never seen the project can enable ntfy from the guide alone.

## Order and shape

WP1 first, because it is a pure refactor that makes WP3 small. WP2 is an hour and removes a whole class
of false alarm before it exists. WP3 and WP4 are the heart and belong together. WP5 is message text.
WP6b is small and must land before WP6, since a toggle has nothing to write to otherwise. WP6a is the
largest single piece because it needs a config writer that round-trips. WP7 last, written against what
was actually built.

WP1 to WP5 touch no UI, so the switch can be proven with a killed process and a log line before the
tab exists.

Two routes rather than one cost roughly half a package over the ntfy-only plan: the writer is written
once, the card is a template, and the dispatcher already branches by kind. The reason to do it now is
that webhook is the route that works when ntfy does not, and having two is what makes the tab
"routes" rather than a single vendor's settings.

## Not in this version

Web push (VAPID, subscriptions, a service worker handler). Telegram and SimpleX. Per-service or
per-severity routing. Digest and quiet hours. A watchdog for a single-machine install. Authentication
on any route.

The tab is built so that a third route is a `kind` string, a card in the same template, and a
`[[sink]]` block. It is deliberately not built as a plugin system: three shapes of settings do not
justify one.

## Settled after review

- **No embedded ntfy server.** Bundling cannot link Go into Rust, so it ships a second binary per
  platform; reimplementing means tracking a moving API forever; and neither delivers what it was
  wanted for, because iOS delivery from a self-hosted ntfy must relay through ntfy.sh anyway.
- **No write-only topic token.** It would be the right shape, but this ntfy has no `auth-file`, so
  per-topic ACLs are not available. Accepted: unauthenticated, and the recovery message can be
  forged by anyone on the LAN. Chosen for simplicity, with other routes to carry security later.
- **No same-machine watchdog.** A watchdog running beside the thing it watches, under the same service
  manager, dies with it. Covering the single-machine case needs a second unit and a second process,
  which is more machinery than the case deserves. Accepted that a single-box install is silent about
  its own death.
- **A fixed 300 s threshold, not a multiple of the interval.** Made safe by binding the interval.
- **Degraded is its own state.** A head answering 503 is demonstrably alive, so calling it down would
  be a lie, and staying silent would leave an unpaired machine quietly reporting into a void.
- **A recovery must be observed.** A head that returns and dies within the threshold was never up.
- **Duplicate alarms across tails are accepted.**
- **Every route shares one shape.** ntfy and webhook already exist in the dispatcher and differ by one
  field, so a per-route settings design would be inventing difference where there is none. Telegram
  and SimpleX, when they arrive, get the same treatment or they get an argument.
- **`enabled` defaults to true.** An upgrade must never silence a working installation. The field
  exists so unticking a box preserves the operator's URL and topic rather than deleting them.
- **`token_env` does not travel to tails.** The name refers to an environment variable on the head; the
  same name on another machine is at best absent and at worst a different secret.
- **The switch publishes through `Notifier`, not to ntfy directly.** One route failing while another
  succeeds still counts as delivered.
