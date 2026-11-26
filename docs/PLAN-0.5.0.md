# 0.5.0: a detailed plan

The theme is "close the door". This is the work, finalised.

## The finding that shapes everything

Both listeners call the same router:

```rust
server::listen(Arc::clone(&app), &cfg.bind, cfg.port, tls.clone())   // dashboard
server::listen(Arc::clone(&app), &cfg.bind, rp, None)                // tail reports
```

`listen()` passes nothing to `handle()` that says which listener accepted the connection. So the
plain-HTTP report port, deliberately certificate-free so LAN boxes can reach it, answers
`/api/settings`, `/api/services/add`, `/api/pair/decide` and everything else. That is why the
exposure exists, and it is not fixable by adding a password check alone. A listener needs to know
what it is.

Second finding: `handle()` receives no peer address, so rate limiting and any loopback rule need one.
`X-Forwarded-For` is a header a client can write.

Third: there is no cookie handling, no session, no 401 anywhere in the server, and no 401 handling in
`index.html`. All of it is new.

## What this reverses

**F11 in `docs/DESIGN.md` says:** paint from `localStorage` before any network call, so an offline
launch is instant.

**The page does exactly that today**, caching the full `/api/status` payload per node. That payload
carries every service and machine name. Under a password gate it leaks precisely what the password
protects, to anyone holding an unlocked phone.

**0.5.0 reverses F11.** No service data in persistent browser storage. The shell may stay cached so
the app launches instantly; the data may not. A cold launch shows a neutral view until the LAN
answers, and that wait is the price of the gate.

`DESIGN.md` changes in the same commit, because a document disagreeing with the code is a bug and
here it is the code that is wrong.

## Decisions

1. **Hashing:** bcrypt, cost 12.
2. **First run:** a 0600 token file, cleared when a password is set.
3. **Reads gated:** `/api/status`, `/api/history`, `/api/nodes` need a session.
4. **Remove-password:** allowed, current password required.
5. **Reset:** a CLI subcommand, not an HTTP route.
6. **Session invalidation:** rotate the signing secret, re-issue a cookie in the same response.
7. **Cookie payload:** `v1.{iat}.{exp}.{hmac_hex}`.
8. **Pairing:** stays on the dashboard port, gains a cap, a TTL and an operator-armed window.
9. **F11 reversed:** no service data in persistent storage.

Measured, not guessed, on the two machines that will run it:

| Machine | hash (cost 12) | verify |
|---|---|---|
| Mac, Apple M4 | 205 ms | 333 ms |
| Laptop, Celeron N3350 | 376 ms | 365 ms |

Both fine for a login that happens once a day. Cost 12 is the floor; 10 would be 0.1 s and is not
worth the trade.

bcrypt truncates at 72 bytes. A 200-character passphrase and its first 72 characters are the same
password, so the set-password route rejects anything over 72 bytes with a plain sentence rather than
silently accepting a shorter secret than the operator typed.

The cost factor lives inside the hash string, so raising it later needs no migration: old hashes keep
verifying at their own cost and each is upgraded on next successful login.

### What 9router got right, and what it did not

Take the shape: a signed cookie, a status endpoint the UI asks on load, login and logout, a secret
stored `0600`. Skip its JWT, which for one operator costs base64url and JSON parsing to verify a
single number. Skip its `INITIAL_PASSWORD || "123456"` fallback, because a dashboard that ships with
a known password is authenticated-looking rather than authenticated, and that is worse than being
open since nobody thinks to check.

## Which machine this matters on

The intended topology is **the laptop as head and the Mac as tail**, the reverse of how the two are
configured right now. Consequences:

- The password, the session secret and the sign-in gate all live on the laptop: a Celeron N3350 with
  4 GB. The 376 ms figure is the one that counts, and the login mutex in WP5 is not optional there.
- Reset from the console means the laptop's keyboard.
- Every acceptance check runs with the laptop serving. Flipping the two roles is a prerequisite, not
  part of the work.

## Work packages

Ordered so each leaves the tree working. Nothing needs another finished to be mergeable.

### WP1: listener identity

The foundation. No password yet, but this halves the exposure on its own.

- A `ConnMeta { role: ListenerRole, peer: SocketAddr }` passed into `handle()`, built by `listen()`
  from the socket it accepted on.
- Dispatch splits into three stages: `resolve_route(method, path) -> Option<Route>`,
  `policy(route) -> RoutePolicy`, `execute(route, ...)`.
- `policy()` is an exhaustive match with no `_` arm. Adding a route and forgetting to classify it
  fails to compile, which is the point.
- Paths normalise once inside `resolve_route()`, so classification and dispatch always see the same
  string. Duplicate slashes and trailing slashes collapse before either decides anything.
- Cross-listener hits answer **404, not 403**. A 403 tells a probe something is there.
- One unit test asserts the exact allowlist of public routes and report-port routes. A new route
  marked public fails it until the list is edited deliberately.

**Acceptance:** `/api/settings` on 8771 returns 404. `/api/report` on 8771 works. The dashboard port
is unchanged. A tail already paired keeps reporting untouched.

### WP2: credentials

- A `settings` table (schema v10) holding `password_hash`, `password_set_ts` and the session secret.
  Not the config file: a hash in `config.toml` gets copied into dotfiles repos and pasted into issues.
- `set_password`, `verify_password`, `password_is_set`.
- bcrypt compares in constant time; nothing extra to write.

**Acceptance:** setting twice replaces. A wrong password fails. The stored value never appears in
logs, `status` output or `/api/settings`.

### WP3: sessions

- Secret: 32 random bytes at first use, stored in the database beside the hash so a daemon restart
  does not log every device out. `OF_SESSION_KEY` overrides it for anyone who prefers env.
- Token: `v1.{iat}.{exp}.{hmac_hex}`, HMAC-SHA256 over `v1.{iat}.{exp}`, verified with the
  constant-time compare already in `ingest.rs`.
- **HMAC built from RFC 2104, not `sha256(secret ‖ message)`.** The naive construction is open to
  length extension and is not what the name means. About 15 lines on the existing `sha2`.
- Three checks on verify, each failing closed:
  - `now <= exp`
  - `exp <= iat + MAX_SESSION_TTL`, bounding a forged far-future expiry
  - `now + 300 >= iat`, a five-minute backward tolerance. A clock jumping further back rejects every
    cookie rather than granting a permanent session.
- Cookie: `of_session`, `HttpOnly`, `SameSite=Lax`, `Path=/`, `Max-Age=86400`, and `Secure` only when
  the listener has TLS. Unconditional `Secure` locks out the plain-HTTP setup most installs start with.

**Acceptance:** valid admits; tampered, expired, wrong-secret and clock-skewed all reject.

### WP4: the gate

Three classes, decided by `policy()`:

| Class | Routes | Rule |
|---|---|---|
| Public | `/`, `sw.js`, `manifest.json`, icons, `/api/health`, `/api/auth/status`, `/api/auth/login`, `POST /api/auth/logout`, `POST /api/auth/setup`, `POST /api/pair` | no session |
| Machine | `POST /api/report` | bearer token, unchanged |
| Operator | everything else | session or 401 |

`POST /api/pair` stays public and stays on the dashboard port. A tail has no account and must be able
to ask to join. Moving it to the report port would break pairing outright: `ask_head()` posts to the
head's dashboard URL, so a 404 there means no tail can ever pair.

`POST /api/auth/logout` is public because clearing a cookie has to work even when the session is
already dead.

`/api/health` returns a static 200 with no counts and no names. `/api/auth/status` returns
`{required, authenticated, has_password, needs_setup}` and nothing more.

**Acceptance:** every operator route answers 401 without a cookie and 200 with one. A curl to 8771
gets 404 for operator paths.

### WP5: login serialisation and rate limiting

- Login attempts serialise behind a mutex. At 376 ms per bcrypt, five concurrent attempts saturate two
  cores and starve the poller on the laptop. One at a time caps it at one core.
- Per peer address, in memory: 5 failures in 15 minutes, then `429` with `Retry-After`. The address
  comes from the socket, never a header.
- Success clears the counter for that address.
- Counter resets on restart. Persisting lockouts is a way to lock yourself out.

**Acceptance:** the sixth attempt is refused; another address is unaffected; a login storm does not
stall the poller.

### WP6: the login screen, and F11 reversed

The largest frontend package, because it removes something.

- **No service data in persistent storage.** `saveCachedData` stops writing the payload to
  `localStorage`. The per-node snapshot and the cached history go the same way. What may persist is
  preference (active tab, collapsed groups, settings sub-tab) and nothing that names a service or a
  machine.
- **Initial HTML renders a neutral view.** No hostnames, no counts. The shell stays cached so the app
  launches instantly; the data is fetched.
- **Blank on hide.** On `visibilitychange` to hidden, and on `pagehide`, cover the dashboard at once.
  iOS snapshots the rendered DOM for the app switcher and flashes it on resume, so without this a
  phone locked for a day shows yesterday's dashboard to whoever opens it. That leak exists today,
  before any of this work.
- **The cover stays up on resume** until `/api/auth/status` or the first data fetch answers, with a
  2.5 s timeout. 200 reveals, 401 shows login, timeout shows a clean "LAN unreachable" screen.
- **Any 401 anywhere** clears in-memory state, switches to login and stops the poll timer. A timer
  hammering a 401 every 30 seconds is noise in the log and battery on the phone.
- **Logout** clears in-memory state and switches to login. It does **not** send
  `Clear-Site-Data: storage`, which on Safari unregisters the service worker and breaks offline
  launch until the next online visit.
- The service worker must not cache `/api/*`. It already does not, so this needs a test, not a change.

The service worker is never the gate. WebKit terminates idle workers within about 30 seconds and an
`HttpOnly` cookie is invisible to them, so no access decision may depend on a worker running.

**Acceptance:** on a phone, nothing is visible before login; after login the dashboard appears; after
logout and reload nothing is visible; backgrounding and reopening the switcher shows a blank screen;
and with the LAN unplugged the app opens to a neutral message rather than stale names.

### WP7: password management

Its own inner tab in Settings. Four actions, each with its own button:

1. **Set a password** when none exists.
2. **Change it.** Current password, then the new one twice.
3. **Remove it.** Current password required, consequence stated in red first, because the next visitor
   to that port owns the machine.
4. **Log out all other devices.** Rotates the signing secret and re-issues a cookie to the browser that
   pressed it. This exists for one reason: a lost phone.

Rules: minimum 8 characters, maximum 72 bytes, each refusal naming the limit rather than a red star.
Changing or removing the password invalidates every other session by rotating the secret.

**Acceptance:** each action works from the phone. After a change another device is logged out, and the
one that made the change is not.

### WP8: TLS import

- Certificate and key paths, in the Access tab.
- Validated on save: certificate parses, key parses, they match, certificate is not expired. Each
  failure names what is wrong.
- Applies on restart. The tab says so and offers a restart button that responds before restarting,
  since a process cannot reliably restart itself mid-response.
- Validation runs in the service's own context, which is what makes it honest: a path under the
  operator's home may not be readable by the unit.

**Acceptance:** valid pair accepted; mismatched refused with "the key does not match this certificate";
expired refused by date; phone loads over HTTPS after restart.

### WP9: first run and reset

**Setup.** No password means no dashboard. A 32-hex-character token is generated at first boot, its
SHA-256 stored in the database and the plaintext written to `<state_dir>/setup-token` with mode 0600.
The journal gets the path, never the secret.

A file rather than the journal, because journald is often volatile, rotates, and on Debian is readable
by anyone in `systemd-journal` or `adm`. A 0600 file is readable by the daemon user alone, and it
survives a reboot and a month sitting unconfigured.

No expiry. A 128-bit random token is not brute-forceable in a week, and an expiry only guarantees
annoyance for a machine left unconfigured. It is single-use by consumption: the password write and the
token clear happen in one transaction.

**Reset without the old password is a CLI subcommand, not an HTTP route.**

```
observer-frog passwd          # set or change, reads from the TTY with echo off
observer-frog passwd --reset  # no current password, also rotates the secret
```

Not for simplicity. The loopback rule cannot be made honest: behind nginx or Caddy every request
looks loopback, so the restriction evaporates; a `behind_proxy` flag fails open the day someone adds a
proxy and forgets it; header sniffing dies against a proxy that sets no forwarded headers. An HTTP
reset on `127.0.0.1` is also reachable by DNS rebinding and by any local browser tab.

Anyone who can run the command already has shell access, the same trust boundary as reading the token
file. The daemon picks the change up by comparing `PRAGMA data_version` before verifying a session, so
a CLI reset also invalidates live sessions with no signal and no restart.

**Acceptance:** a fresh install shows the setup screen and accepts the token from the file; the token
is refused once a password exists; `passwd --reset` changes the password from the console and logs out
every live session.

## Order and shape

WP1 is a day and immediately halves the exposure. WP2 to WP5 are the core, about two days. WP6 is the
largest frontend piece because it removes an invariant, another day. WP7 and WP8 are a day between
them. WP9 is small provided the CLI plumbing is done once.

WP1 to WP5 touch no UI, so the gate can be proven with `curl` before the login screen exists. That
ordering is deliberate: the thing everything depends on gets verified first, and the frontend then has
something real to talk to.

## Not in this version

Multi-user. Roles. 2FA. Password recovery by email. OIDC or SAML. Per-session revocation lists.
Session persistence needs no code: the cookie survives and the secret is on disk.

## Still open, and only decidable once it runs

- Whether the setup token should also be accepted from loopback before it is read from the file. Same
  trust boundary, but a second path to test.
- Whether the login view replaces the shell or overlays it. Replacing leaves nothing cached to leak;
  overlaying keeps the app feeling instant. WP6 as written assumes replace.
- Whether pairing needs the 6-character verification code in addition to the operator-armed window.
  The window alone stops anonymous writes; the code stops a race between a real tail and a spoofed
  one during the window.
