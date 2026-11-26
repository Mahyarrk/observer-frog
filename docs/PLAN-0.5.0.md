# 0.5.0: a detailed plan

Written from `docs/ROADMAP.md`. The theme there is "close the door". This is the work.

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

Second finding: `handle()` receives no peer address. Rate limiting and any loopback-only rule need
one, because `X-Forwarded-For` is a header a client can write.

Third: there is no cookie handling, no session, no 401 anywhere in the server, and zero 401 handling
in `index.html`. All of it is new.

## What 9router does, and what to take

The shape is right: a signed cookie, a status endpoint the UI asks on load, login and logout, and a
secret generated once and stored `0600`. Take the behaviour, not the bytes.

I would **not** take the JWT format. For one operator it buys nothing over a signed token and costs
base64url plus JSON parsing to verify a claim that is a single number. The behaviour is identical
from the outside.

I would **not** take the `INITIAL_PASSWORD || "123456"` fallback. A dashboard that ships with a known
password is not authenticated, it is authenticated-looking, and that is worse than being open because
nobody thinks to check.

### One new dependency: bcrypt

Decided: **the `bcrypt` crate**, matching 9router. Cost factor 12. The crate is pure Rust, so it
builds on both targets with nothing to link.

Measured, not guessed, on the two machines that will run it:

| Machine | hash (cost 12) | verify |
|---|---|---|
| Mac, Apple M4 | 205 ms | 333 ms |
| Laptop, Celeron N3350 | 376 ms | 365 ms |

Both are fine for a login that happens once a day. Cost 12 is the floor I would accept; 10 would be
0.1 s and is not worth the trade.

Two consequences worth writing down before the code exists:

- **bcrypt truncates at 72 bytes.** A 200-character passphrase and its first 72 characters are the
  same password. The set-password route rejects anything over 72 bytes with a plain sentence rather
  than silently accepting a shorter secret than the operator typed.
- **The cost factor lives inside the hash string** (`$2b$12$...`), so raising it later needs no
  migration: old hashes keep verifying at their own cost, and each is upgraded on next successful
  login.

The cookie still needs HMAC-SHA256, and `sha2` is already a dependency, so that is ~25 lines verified
against the RFC 4231 vectors. The constant-time compare already in `ingest.rs` verifies it. Hex
encoding is four lines. No base64, no JWT, no `rand`.

## Work packages

Ordered so that each one leaves the tree working. Nothing here needs another to be finished to be
mergeable.

### WP1: listener roles

The foundation. No password yet, but this one halves the exposure on its own.

- `listen()` takes a role: `Role::Dashboard` or `Role::Reports`.
- `handle()` takes it too, and admits only `/api/report` and `/api/health` on `Role::Reports`.
- Everything else on the report listener answers `404`, not `403`. A 403 tells a probe that something
  is there; a 404 tells it nothing.
- `handle()` also takes the peer `SocketAddr` from `accept()`, kept in `Request` for WP5.

**Acceptance:** `/api/settings` on 8771 returns 404. `/api/report` on 8771 still works. The dashboard
port is unchanged. Existing machines keep reporting.

**Tests:** role table test, one case per route, run against both roles.

### WP2: credentials

- A `settings` table (schema v10) holding `password_hash` and `password_set_ts`. Not the config file:
  a hash in `config.toml` gets copied into dotfiles repos and pasted into issues.
- Format: bcrypt's own `$2b$<cost>$<salt>$<hash>`. The cost travels inside the string, so raising it
  later needs no migration.
- `set_password`, `verify_password`, `password_is_set`.
- bcrypt compares in constant time; nothing extra to write.

**Acceptance:** setting a password twice replaces it. A wrong password fails. The stored value never
appears in logs, `status` output, or `/api/settings`.

**Tests:** round trip, wrong password, empty password refused, a short password refused, iteration
count parsed from the stored string rather than assumed.

### WP3: sessions

- Secret: 32 random bytes from `/dev/urandom` on first use, written `0600` beside the database as
  `session.key`. Environment override `OF_SESSION_KEY` for anyone running it from a container-less
  host manager that prefers env.
- Token: `<exp>.<hmac-hex>`, 24 h, HMAC-SHA256 over the expiry.
- Cookie: `of_session`, `HttpOnly`, `SameSite=Lax`, `Path=/`, `Max-Age=86400`, and `Secure` only when
  the listener has TLS. Setting `Secure` unconditionally locks out the plain-HTTP setup that most
  installs start with.
- Verifier: split, parse expiry, reject past, recompute HMAC, constant-time compare.

**Acceptance:** a valid cookie admits, a tampered one does not, an expired one does not, a cookie
signed with a different secret does not.

**Tests:** all four, plus a token that is valid but for another node's secret.

### WP4: the gate

One function, called near the top of `handle()`, deciding by path and method:

| Class | Routes | Rule |
|---|---|---|
| Public | `/`, `sw.js`, `manifest.json`, icons, `/api/health`, `/api/auth/status`, `/api/auth/login`, `POST /api/pair` | no session |
| Machine | `POST /api/report` | bearer token, unchanged |
| Operator | everything else | session or 401 |

`POST /api/pair` must stay public. A tail with no account has to be able to ask to join; the token it
proposes is not a credential until a human approves it. Gating it breaks pairing entirely.

`/api/auth/status` returns `{required, authenticated, has_password}` and nothing more. It does not
name the operator, and it gives no hint about whether a password is close to right.

**Acceptance:** the done-when list in the roadmap, plus: `curl` without a cookie gets 401 on every
operator route; the same `curl` with a cookie gets 200.

**Tests:** a table driven from a single list of routes, asserting the expected class for each, so a
new route cannot be added without deciding which class it belongs to.

### WP5: rate limiting

- Per peer address, in memory: 5 failures in 15 minutes, then `429` with `Retry-After`.
- The peer address comes from the socket, never from a header.
- A successful login clears the counter for that address.
- Resets on restart, which is the right trade: persisting lockouts is a way to lock yourself out.

**Acceptance:** the sixth attempt is refused; a different address is unaffected.

**Tests:** counter, window expiry, reset on success.

### WP6: the login screen

- `index.html` gains a login view. Any `401` from a fetch switches to it and stops the poll timer,
  because a timer hammering a 401 every 30 seconds is noise in the log and battery on the phone.
- After login the page reloads into the dashboard with the existing cached-data paint intact.
- Logout is a button, and it clears the cache as well as the cookie: a cached snapshot on a shared
  phone is the same leak the password exists to prevent.
- The service worker must not cache the login response. `/api/*` is already network-only, so this
  only needs a check that the shell cache does not capture it.

**Acceptance:** on a phone, nothing is visible before login. After login the dashboard appears.
After logout and reload, nothing is visible again.

### WP7: password management, as its own part of Settings

Its own inner tab, not a card inside "This Node". Three changes of state, each with its own button:

1. **Set a password** when none exists. No current password asked, because there is nothing to prove.
2. **Change it.** Current password required, then the new one twice.
3. **Remove it.** Current password required. Removing turns the gate off, and the tab says so in red
   before you press it, because the next visitor to that port owns the machine.

Plus the one you asked for, which is the only way back in after a forgotten password:

4. **Reset without the old password, from `127.0.0.1` only.** The route refuses any request whose peer
   address is not loopback. Someone at the keyboard can recover a lost password; someone on the LAN
   cannot. This is the correct trust boundary because it is the same one that already lets you edit
   `config.toml` and read the token file.

The reset route must take the peer address from the socket, never from a header. `X-Forwarded-For` is
a value the client writes, so a remote attacker could otherwise claim to be local. And because a
reverse proxy in front would make every request look loopback, the plan is: if a proxy is configured
the operator's host is trusted anyway, and the route is still no weaker than the config file beside it.

Rules: minimum 8 characters, refused with a reason rather than a red star. Changing or removing the
password invalidates every existing session by rotating the signing secret, which is cheaper than
tracking sessions and gets the same result.

**Acceptance:** each of the three works from the phone. After a change, every other device is logged
out.

### WP8: TLS import

- Two fields, certificate path and key path, in the same Access tab.
- Validated on save: the certificate parses, the key parses, they match each other, and the
  certificate is not expired. Each failure names what is wrong.
- Applies on restart. The tab says so, and offers a restart button that responds first and then
  restarts the unit, since a process cannot reliably restart itself mid-response.
- Paths are read by the service, so a path under the operator's home may not be readable from the
  unit's context. The validation runs in that context, which is what makes it honest.

**Acceptance:** a valid pair accepted, a mismatched pair refused with "the key does not match this
certificate", an expired certificate refused by date, and the phone loads over HTTPS after restart.

### WP9: first run

The one genuinely awkward case: a node that is reachable before a password exists. Two options.

**Chosen: a setup token.** No password means no dashboard. `/api/auth/status` reports `needs_setup`,
and the page asks for a token printed once to stdout and to the journal at first boot. Anyone who can
read the journal can claim the machine, which is the right trust boundary: it is the same access that
already lets you read the token file and edit the config.

Details that make it work rather than annoy:

- The token is generated at first boot only, when no password exists, and shown once in the startup
  output. It is not stored in the config.
- It is valid until a password is set. No expiry, because a box can sit unconfigured for a week and an
  expiring token would turn that into a lockout.
- Setting the password clears it. The route refuses to run once a password exists, so it is not a
  second door.
- If the journal was rotated away before it was read, `observer-frog passwd` on the console prints a
  fresh one. Local shell access is the same trust boundary as reading the journal. This is the
  recovery path for the recovery path, and without it the first-run gate can strand a headless box.

## Decided

1. **Hashing:** bcrypt, cost 12. Matches 9router, and the cost travels inside the hash.
2. **First run:** a setup token printed to the journal, cleared when a password is set.
3. **Reads gated:** `/api/status`, `/api/history` and `/api/nodes` need a session. Service names and
   machine names are exactly what an attacker wants, and a dashboard readable without the password is
   not protected.
4. **Remove-password:** allowed, current password required, with the consequence stated in red.
5. **Reset:** without the old password, from `127.0.0.1` only, peer address taken from the socket.

## Which machine this matters on

The intended topology is **the laptop as head and the Mac as tail**, which is the reverse of how the
two are configured right now. Two consequences for this plan:

- The password, the session secret and the sign-in gate all live on the **laptop**: a Celeron N3350
  with 4 GB. That is the machine that will run bcrypt, the one that serves the phone, and the one
  whose journal holds the setup token. The 376 ms figure above is the one that counts.
- Loopback-only reset means on the laptop's own keyboard, not the Mac's. Fine, but it rules out
  "reset it from the Mac" as a shortcut, and that should be said in the settings text so nobody goes
  looking for it.

This also means the test plan changes: every acceptance check in WP1 to WP7 has to be run with the
laptop serving, which is not the current setup. Flipping the two roles is a prerequisite, not part of
the work.

Still open, and only decidable once it is running:

- Whether the setup token should also be accepted from loopback before it is read out of the journal.
  It probably should, since the two are the same trust boundary, but that is a second path to test.
- Whether the login view should replace the shell or sit over it. Replacing it is simpler and leaves
  nothing cached to leak; overlaying it keeps the app feeling instant on a slow phone.

## Order and rough shape

WP1 is a day and immediately improves things. WP2 to WP5 are the core, maybe two days. WP6 and WP7 are
the frontend, another day. WP8 is small. WP9 is small but needs the decision above.

Nothing in WP1 to WP5 touches the UI, so those can ship and be verified by `curl` before the login
screen exists. That matters: it means the gate is proven before anything depends on it.

## Not in this version

Multi-user. Roles. 2FA. Password recovery by email. OIDC or SAML. Per-session revocation lists.
Session persistence across restart (the cookie survives, the secret is on disk, so this works
implicitly and needs no code).
