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

### Zero new dependencies

Worth deciding deliberately, because it is achievable:

- `sha2` is already a dependency. HMAC-SHA256 on top of it is about 25 lines, verified against the
  RFC 4231 vectors.
- PBKDF2-HMAC-SHA256 is about 15 more, and is the same primitive at 600,000 iterations.
- The signed cookie is `<exp>.<hmac-hex>`, verified with the constant-time compare that already
  exists in `ingest.rs`.
- Hex encoding is a four-line function. We need no base64.

So: no `bcrypt`, no `argon2`, no `jwt`, no `rand`. Cost is roughly 300 to 500 ms per login attempt at
600k iterations, which on a single-user dashboard happens about once a day.

The alternative is the `bcrypt` crate to match 9router exactly. Smaller to write, one more dependency,
and it changes nothing the operator can see.

**Recommendation:** hand-rolled PBKDF2 on existing `sha2`. It is the same family of primitive, it is
testable against published vectors, and it keeps the dependency list honest for a binary that
advertises having one.

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
- Format: `pbkdf2$<iterations>$<salt-hex>$<hash-hex>`, so the iteration count travels with the hash
  and can be raised later without a migration.
- `set_password`, `verify_password`, `password_is_set`.
- Constant-time compare on the derived key.

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

**A. Loopback-only setup.** The set-password route answers only when the peer address is loopback.
Clean, and a headless box is set up over SSH or through a tunnel. But the first visit from a phone
shows a dashboard that cannot be secured, which is a confusing state to explain in a red banner.

**B. Setup token.** No password means no dashboard. `/api/auth/status` reports `needs_setup`, and the
page asks for a setup token printed once to stdout and the journal at first boot. Anyone who can read
the journal can claim the machine, which is exactly the right trust boundary.

**Recommendation:** B. It works from a phone, it does not depend on getting loopback right through a
reverse proxy, and it makes the trust boundary explicit instead of implied.

## Decisions I need from you

1. **Hashing:** hand-rolled PBKDF2 on existing `sha2` (no new dependency), or the `bcrypt` crate to
   match 9router? Same behaviour either way.
2. **First run:** loopback-only, or a setup token? I recommend the token.
3. **Reads gated too?** I have assumed yes: `/api/status` and `/api/history` need a session, because
   service names and machine names are the thing an attacker wants. Say if you would rather the
   dashboard be readable and only writes gated.
4. **Remove-password:** allowed at all, or always required once set? Allowed is friendlier and
   indistinguishable in risk from never having set one.

## Order and rough shape

WP1 is a day and immediately improves things. WP2 to WP5 are the core, maybe two days. WP6 and WP7 are
the frontend, another day. WP8 is small. WP9 is small but needs the decision above.

Nothing in WP1 to WP5 touches the UI, so those can ship and be verified by `curl` before the login
screen exists. That matters: it means the gate is proven before anything depends on it.

## Not in this version

Multi-user. Roles. 2FA. Password recovery by email. OIDC or SAML. Per-session revocation lists.
Session persistence across restart (the cookie survives, the secret is on disk, so this works
implicitly and needs no code).
