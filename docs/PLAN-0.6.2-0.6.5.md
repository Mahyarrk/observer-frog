# 0.6.1 → 0.6.5: the route to breadth

Where we are: **0.6.1 shipped**, three machines reporting to one head, ntfy and webhook proven
end to end, retention running, discovery back where it belongs as an operator action.

This document covers every version up to 0.6.5. Web push is 0.7.0 and is described only in
outline at the end, because it needs a decision before it needs a plan.

## The version map

| Version | Theme | One line |
|---|---|---|
| 0.6.2 | Telegram | A BotFather bot, and the same rules ntfy already obeys |
| 0.6.3 | SimpleX | The channel the operator already runs |
| 0.6.4 | Graphs | Thirty days of uptime, drawn from a table that already exists |
| 0.6.5 | Windows | The last platform with no proof |
| 0.7.0 | Web push | A decision first: whose keys |

**A fourth machine is not scheduled.** Three tails share one head today and nothing in the code
counts machines; a fourth would exercise nothing the third did not.

---

# The rule every channel inherits

This is the load-bearing part of the plan. Telegram and SimpleX are not new features so much as
new rows in a contract that ntfy already satisfies, and the value of the next two versions is that
they arrive *without* teaching the codebase a second way to notify.

Every channel must satisfy all fourteen of these, or it does not ship:

**Configuration**

1. One `[[sink]]` block per route, `kind` as the discriminator, validated against `SINK_TYPES`.
   A typo in `kind` is refused at load, not silently ignored.
2. `enabled` is always written explicitly, so a save cannot resurrect a route the operator turned
   off. Absent means enabled only for configs written before the field existed.
3. Secrets live in `token_env` or `token_file`, never in the block itself. A token in a config
   file is a token in a backup, a screenshot and a support paste.
4. `use_proxy` is honoured per sink, defaulting to off. A notification must not depend on the
   proxy whose death it may be reporting (I8).

**Delivery**

5. A failure in one route never suppresses another. The notifier iterates and collects, it does
   not short-circuit.
6. Messages are plain text with no markup that a channel must parse to render (I7). Where a
   channel supports richer formatting, it is opt-in and losing it degrades to plain text.
7. Every outbound request has a timeout. A hung notification must not stall the wave that
   produced it.
8. One message per transition. Down once, recovery once, and silence while the state holds. The
   dead man's switch uses the same routes and the same rule.

**The GUI**

9. A card in the existing template: `enabled` tick, the fields that route needs, a test button.
   A channel that needs a different *shape* of card needs an argument first.
10. The test button says what it actually proved. For ntfy that sentence is: *the server accepted
    it. If nothing appeared on your phone, check that the topic matches.* Every new channel gets
    the equivalent sentence, not a green tick that overclaims.
11. The tab is greyed out on a tail. Notifications are the head's job; a tail that also notified
    would duplicate every message.

**Proof**

12. A test that asserts the request shape against a canned response — status codes, headers,
    truncation, and what happens when the channel answers 401 versus 500 versus nothing at all.
13. A live run against the real service, recorded in the version's notes. "It compiles" is not a
    route.
14. The failure modes are written down where an operator will find them, in the same place ntfy's
    topic-mismatch paragraph lives.

---

# 0.6.2: Telegram

**Why it is next:** it is the smallest possible test of the rule above. Telegram sends a message
with an HTTP POST and a token, which is structurally what ntfy already does — so if the abstraction
is sound, this version is a new `publish_*` function and a card. If it turns out to need a second
mechanism, that is worth knowing before SimpleX, which genuinely is different.

## The detail that matters

**The bot comes from BotFather.** The operator talks to `@BotFather` in Telegram, sends `/newbot`,
picks a name and a username, and receives a token of the form
`123456789:AAExxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx`. That token is the whole credential.

The plan assumes nothing else about the operator's Telegram setup. No user account, no phone
number, no MTProto client, no library — the Bot HTTP API over the same `ureq` agent every other
call uses.

## Where the messages go: a direct chat, with a channel as the second form

**A bot cannot start a conversation.** Telegram only permits a bot to message a chat after that
chat has contacted the bot first. This single rule decides the design, and it is why the direct
chat is the documented path rather than a footnote.

### Direct chat — the default, and what the card teaches first

```
@BotFather  →  /newbot  →  name, username  →  token
you         →  open the bot, press Start          ← the one manual step, once, ever
bot         →  may now message you for the life of the bot
```

That is the whole setup. Two steps, and the second is a button in Telegram.

`chat_id` is then your own user id: a positive integer that never changes, does not depend on any
group existing, and cannot be confused with anything else. It is found by opening
`api.telegram.org/bot<token>/getUpdates` after pressing Start — the message you sent appears there
with the id in it.

**A second person who wants the same alerts presses Start on the same bot** and gets their own DM.
No group, no channel, no shared membership to administer.

### Channel — supported, documented second

The same `chat_id` field accepts a channel, so this costs one paragraph rather than one feature.
The differences that matter:

- The bot must be **added as an administrator**, and specifically needs permission to post. A bot
  that is merely a member cannot write.
- The id is **negative** and typically starts with `-100`.
- Losing the admin role ends delivery silently, which is the channel's characteristic failure.

A channel is the right choice when the alerts should be one shared surface several people watch,
rather than a personal notification. It is not the default because it is more moving parts for the
same result.

### Why direct is first, in one line each

| | direct chat | channel |
|---|---|---|
| Setup | create bot, press Start | create bot, create channel, make bot admin |
| `chat_id` | positive, stable, one integer | negative, tied to the channel existing |
| Reaches you at 3am | it is a DM, the surface you check | a channel you may not have open |
| Breaks silently if | you block the bot | the bot loses admin |
| Second reader | presses Start on the same bot | already a channel member |

The third row is the argument. This project alerts when a head dies, and an alert in a channel
nobody is watching is an alert that did not happen.

## What lands

- `SINK_TYPES` gains `"telegram"`.
- `Sink.chat_id` — the field that plays `topic`'s role. Telegram needs to know *where* to post, and
  that is a chat id, not a topic name. Reusing `topic` for it would be a lie in the config file.
- `notify::publish_telegram(sink, chat_id, event)`:
  - `POST {base}/bot{token}/sendMessage`
  - JSON body: `chat_id`, `text`, `disable_notification` when the event is low priority
  - The token comes from `token_env`/`token_file`, is never logged, and never appears in an error
    string — a 401 must report "the bot token was refused", not the token.
  - `base` defaults to `https://api.telegram.org`, overridable, because a self-hosted Bot API
    server exists and costs one config field to support.
- Message composition: the event's title and detail become one message. Telegram has no
    title/body split like ntfy, so the title goes first on its own line, then the detail, then the
    node name — the same information, laid out for the surface. A direct chat and a channel get
    the identical message; nothing in the composition branches on which one it is.
- A Settings card: token env/file, chat id, enabled tick, test button, and a link to the BotFather
  instructions written in the same voice as the ntfy topic warning.

## Work packages

### WP1: the field, and the card that explains it

`chat_id` is not something an operator knows offhand, so the card teaches it rather than assuming
it. The instructions are written for the direct chat first, with a channel as a collapsible second
form:

**Direct chat, in three sentences on the card:**

1. Talk to `@BotFather`, send `/newbot`, keep the token it replies with.
2. Open your new bot and press **Start** — nothing can be delivered until you do, because Telegram
   does not let a bot speak first.
3. Open `api.telegram.org/bot<your token>/getUpdates` and copy the number in `"chat":{"id":…}`.

**Channel, one paragraph:**

1. Add the bot to the channel **as an administrator**, with permission to post.
2. The id is negative and usually starts with `-100`. Send a message in the channel, read
   `getUpdates`, and take the id from there.

The `getUpdates` URL contains the token, so the link is **built in the browser from what the
operator pasted**, never rendered server-side and never logged. A token in a server log is a token
in a log rotation and a support paste.

The card saves a working example of the failure this prevents: a `chat_id` pasted before pressing
Start produces `400 chat not found`, which reads like a wrong id and is almost always a missing
Start.

### WP2: the sender

`publish_telegram` alongside `publish_ntfy`, same signature shape, same `Result<i32, String>`.

Truncation matters here in a way it did not for ntfy: Telegram rejects messages over 4096
characters outright with a 400, where ntfy truncates server-side. The message is composed to fit,
and fitting is asserted in a test rather than hoped for.

### WP3: what a Telegram failure looks like

Three answers, three meanings, and the code must not blur them:

| Telegram says | Reality | What the operator sees |
|---|---|---|
| `200 {"ok":true}` | delivered to the chat | nothing, as usual |
| `401` | token wrong, or the bot was deleted | "the bot token was refused" |
| `400 chat not found` | the id is wrong **or the bot was never started** | "no such chat. If you have not pressed Start in the bot, do that first." |
| `403 bot was blocked by the user` | you blocked your own bot | "this account has blocked the bot; unblock it in Telegram" |
| `403 not enough rights` | channel: the bot lost its admin role | "the bot can no longer post there; make it an administrator again" |
| no answer | Telegram is down, or the network is | "telegram did not answer" |

Those five lines are the Telegram equivalent of ntfy's topic-mismatch paragraph, and the middle
three are each a distinct operator error that a single "failed" would hide. Rule 14 of the contract
exists for exactly this table.

### WP4: the card, and the honest test button

Same template, same position, one `[[sink]]` block. The test button sends a real message and then
reports what came back, in the same cautious wording ntfy uses.

### WP5: the dead man's switch

Nothing new. The switch already publishes through `Notifier`, so a Telegram sink receives down and
recovery for the head automatically. What gets added is a test asserting exactly that, because "it
should work because the plumbing is shared" is the kind of claim that turns out to be wrong.

## Done when

- A BotFather bot delivers a live down **and** recovery message to a direct chat, from a service
  stopped by hand.
- The same bot delivers to a channel it administers, proving the second form costs only the field
  it shares.
- Each of the five failure lines above is produced by a real request, not just a unit test: wrong
  token, wrong chat id, blocked bot, demoted bot, unreachable host.
- With the Telegram sink disabled, no request leaves the machine at all.
- With Telegram and ntfy both enabled and Telegram broken, ntfy still delivers.
- 4096-character truncation and the `disable_notification` rule for low-priority events are
  asserted in unit tests.
- The token never appears in a log line, an error string, or the dashboard's HTML — grepped for,
  not assumed.

## Not doing

- Telegram *receiving* — no bot commands, no two-way control. This sends alerts.
- MTProto, user accounts, or a Telegram client library. The Bot API over HTTP is enough.
- Markdown or HTML parse modes. Plain text, per rule 6.

---

# 0.6.3: SimpleX

**Why it comes after Telegram:** because it is the first channel that does not fit the mould, and
finding that out with a working example beside it is easier than finding it out alone.

## The honest complication

Every channel so far is an HTTP POST. **SimpleX has no such endpoint.** It is a message queue with
its own protocol, and the only practical way to send from this project is to talk to a running
`simplex-chat` client — either by running its CLI as a subprocess or by speaking to its local
WebSocket API.

That is a real difference and this version opens with a **spike**, not a plan: find out which of the
two is viable, in a day, before promising anything. The outcome decides the shape of the rest.

Three candidate approaches, in the order they will be tried:

1. **The `simplex-chat` CLI as a subprocess** — `simplex-chat -e "@bot <message>"` or the documented
   WebSocket equivalent. Simple, no new dependency, but it makes this project depend on another
   binary being installed and configured, which no other sink does.
2. **The local WebSocket API** — `simplex-chat -p 5225` exposes a documented API. More code, no
   subprocess per message, and the connection can be held open.
3. **Decline, and say why** — if neither is dependable, the honest answer may be that SimpleX is
   not a notification sink for this project, and webhook-to-someone-else's-bridge is the documented
   path. That outcome is allowed and would be written up rather than quietly dropped.

## What lands, if the spike succeeds

- `SINK_TYPES` gains `"simplex"`.
- A `recipient` field — a SimpleX contact or group address.
- `notify::publish_simplex`, whatever shape the spike chose.
- The same card, the same test button, the same honest failure messages.
- **A dependency on the operator's SimpleX client is stated on the card**, because it is a
  different class of requirement from "an ntfy server exists somewhere".

## Work packages

### WP1: the spike, and a written verdict

One day, timeboxed. Six questions:

- Can a message be sent without a per-message subprocess?
- What happens when the client is not running — does it hang, fail fast, or queue?
- Does it need the operator to have a bot contact pre-created, and how is that documented?
- Can two sinks of the same kind coexist (two recipients)?
- What does an unreachable client look like, and can it be told apart from a refused message?
- Does any of it require a file the CLI owns, which a systemd unit could not read?

The verdict is written into this document before code is committed.

### WP2: the sender, against the spike's answer

Following whichever approach WP1 endorsed, with the timeout rule (7) enforced harder than for HTTP:
a subprocess that does not return must be killed, not waited on.

### WP3: the degradation path

SimpleX's client may not be running when an alert fires. That is not an error in this project, and
the message must say so plainly: "the SimpleX client is not running on this machine", not a timeout
or an empty error.

## Done when

- The spike's verdict is written down, including a "declined" outcome if that is the answer.
- If shipped: a live message reaches the operator's SimpleX contact from a real transition.
- If declined: the reason, and the documented alternative, are on the card.

## Not doing

- Bundling or managing a SimpleX client. Same principle as ntfy: this project talks to a service,
  it does not host one.
- SimpleX's file-transfer or group management. Messages only.

---

# 0.6.4: thirty days, drawn

**Why here:** the data has been accumulating since 0.1 and nobody has ever seen it. `hourly_rollup`
holds `up_n`, `deg_n`, `down_n`, `lat_sum`, `lat_n` per service per hour, pruned at thirty days —
and the dashboard shows a snapshot.

This version is unusual in that it is almost entirely frontend work over an existing table, which
makes it the one place where a chart is not scope creep.

## What lands

- A history view per service: uptime as a band over thirty days, and latency as a line over the
  same window.
- Rollup rows become an API answer, not new collection. `lat_sum / lat_n` is the mean latency for
  that hour; a bucket with `lat_n = 0` is a gap, not a zero.
- Two visual rules, both because a chart that misleads is worse than no chart:
  - **A gap is drawn as a gap.** Hours with no data are empty, never interpolated to zero. A service
    that was off for a day must not look like it had perfect uptime.
  - **Degraded is visible.** A band that shows only up and down turns a service that spent a week
    answering slowly into a green line.

## Work packages

### WP1: the API shape

`/api/history?days=N` already returns incidents, uptime rows and KPIs. The graphs need a per-hour
series, which is a second query over the same table, not a new table.

`{service, hour_ts, up, deg, down, latency_ms|null}` — with the null preserved through the API,
because the frontend cannot draw a gap the backend has already filled with zero.

### WP2: the chart

No charting dependency. Thirty days is 720 points; an inline SVG polyline and a stack of `<rect>`s
is smaller than any library, has no build step, and matches the project's one-binary rule. The
existing CSS variables carry the colours.

### WP3: the interaction

The chart lives behind the existing card, opened by a click, and respects the node and service
already selected. No new navigation, no new tab.

## Done when

- Thirty days render for a service that has been up the whole time, and for one with a real outage.
- An hour with no samples draws as a gap, asserted against a fixture.
- Degraded hours are visually distinct from both up and down.
- The page still works with JavaScript-heavy history absent — that is, it degrades to the current
  snapshot rather than breaking.

---

# 0.6.5: Windows

**Why last among the platform work:** everything else on this list is provable on machines that
exist. Windows is not, and pretending otherwise would be the same mistake as declaring Arch
supported from a Debian box — which is exactly why Arch was dropped from this roadmap.

## The first thing this version does is answer a question

**Is there a Windows machine to test on?** If there is not, this version does not start. A Task
Scheduler definition written blind and committed is a claim nobody can check, and the roadmap would
be lying in the same way a catalog entry with an unverified process name lies.

So WP0 is: get a machine, or defer the version and say so on the roadmap.

## What lands, if WP0 succeeds

- `install` stops refusing on Windows. Today the binary's metrics and config paths already know
  Windows exists; only the service definition is missing.
- A **Task Scheduler** registration: run at logon, restart on failure, no console window. The
  equivalent of the LaunchAgent and the user unit, with the same properties — one name, install
  copies the binary somewhere durable, `install` after a rebuild is the whole upgrade.
- Process probes that work. `sysinfo` on Windows reports command lines differently, and the
  process probe is name-based today; this needs checking rather than assuming.
- The honest gaps, stated on the dashboard rather than hidden: temperatures need elevation and
  lie on some desktops; GPU utilisation is not available without a vendor SDK.

## Work packages

### WP0: a machine, or a decision not to

Not a work package so much as a gate. Everything below is conditional on it.

### WP1: the service definition

Task Scheduler via `schtasks` or its COM interface. The test is the same one that mattered on
Ubuntu: **registered, started, and still running after a reboot with nobody logged in.** If Task
Scheduler cannot do that without a stored password — and it may not — that limitation is what the
card says.

### WP2: install and uninstall

The same contract as the other platforms: copy to a durable location, point the definition at that
copy, restart. `uninstall` removes the task and stops the process.

### WP3: what Windows does differently

`sysinfo`'s process table, path separators in the DB location, and whether the loopback bind
behaves. Each is small; each is a place a Unix assumption leaks.

## Done when

- One binary, `observer-frog.exe`, installs, survives a reboot, and reports.
- All five probe kinds verified on Windows.
- The dashboard is reachable and the PWA installs in Edge or Chrome.
- The things Windows cannot do are written on screen, not in a commit message.

---

# Testing, and who does what

**I will run** every work package's verification on the machines that exist, with output pasted:
the request-shape tests, the live sends, the truncation bounds, the chart against fixture data.

**You must run**, because only you can:

- **0.6.2** — create the bot with BotFather, and confirm a real alert arrives on your phone.
  Nobody else can do this step.
- **0.6.3** — confirm the SimpleX message arrives in the client you actually use.
- **0.6.4** — open the graph on the phone and say whether it reads at a glance. A chart that needs
  explaining has failed regardless of its correctness.
- **0.6.5** — provide the Windows machine, or tell me there isn't one.

---

# Open questions

1. **SimpleX's transport.** The spike answers it, but if you already know which approach works in
   your setup, that saves a day.
2. ~~Telegram `chat_id`: direct chat or channel?~~ **Settled: direct chat is the documented path,
   channel is the supported second form.** A bot cannot initiate a conversation, so the direct
   chat needs one press of Start and nothing else; a channel needs an admin grant that can be
   revoked. Both are the same field.
3. **A Windows machine.** Exists, or defer 0.6.5 indefinitely?
4. **Graph granularity.** Per-service first is assumed. Per-node aggregated could come later.

---

# 0.7.0, in outline only

Web push: VAPID keys, a subscription table per device, a `push` handler in the service worker, an
enable button. It reaches a phone with no third-party relay and no server to keep alive — the only
route in the project with that property.

It needs one decision before it needs a plan: **where the VAPID key pair lives, and whether it
rotates.** That key is the first generated secret this project would own. Everything else follows
from the answer.
