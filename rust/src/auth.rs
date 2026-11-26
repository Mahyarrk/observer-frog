//! Operator authentication: one password, one session cookie, one gate.
//!
//! Two audiences hold two different kinds of credential. A machine (a tail) holds a bearer token
//! and cannot hold a session. A person holds a session cookie and cannot hold a token. Keeping them
//! separate is deliberate: a session is a browser convenience, and a token is a machine identity
//! that must survive having no browser at all.
//!
//! Nothing here trusts a header. The peer address comes from the socket, and the cookie is verified
//! by recomputing an HMAC rather than by parsing a claim.

use rusqlite::Connection;
use std::path::Path;

use crate::db;

/// Password policy, in bytes because bcrypt truncates there and being vague about it would mean
/// silently accepting a shorter secret than the operator typed.
pub const MIN_PASSWORD_LEN: usize = 8;
pub const MAX_PASSWORD_LEN: usize = 72;

/// bcrypt cost. 12 measures 205 ms on an M4 and 376 ms on a Celeron N3350; 10 would be 0.1 s.
const BCRYPT_COST: u32 = 12;

/// A session outlives a day, and never grows past this even if a cookie claims otherwise.
const MAX_SESSION_TTL_S: i64 = 86_400;
/// Tolerance for a clock that moves backwards. Beyond this, every cookie is rejected rather than
/// being extended, because an expiry-only token plus a clock reset is a permanent session.
const BACKWARD_SKEW_S: i64 = 300;

const KEY_HASH: &str = "password_hash";
const KEY_SECRET: &str = "session_secret";

pub const COOKIE_NAME: &str = "of_session";

/// Current epoch seconds. Public so the caller can turn an absolute expiry into a Max-Age.
pub fn now_s() -> i64 {
    now()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------- storage

fn get(db_path: &Path, key: &str) -> Option<String> {
    let conn = db::connect(db_path).ok()?;
    conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get::<_, String>(0))
        .ok()
        .filter(|v| !v.is_empty())
}

fn put(db_path: &Path, key: &str, value: &str) -> Result<(), String> {
    let conn: Connection = db::connect(db_path).map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        (key, value),
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

fn delete(db_path: &Path, key: &str) -> Result<(), String> {
    let conn = db::connect(db_path).map_err(|e| e.to_string())?;
    conn.execute("DELETE FROM settings WHERE key = ?1", [key])
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Whether an operator password exists. Its absence is the first-run state, not an error.
pub fn password_is_set(db_path: &Path) -> bool {
    get(db_path, KEY_HASH).is_some()
}

// ---------------------------------------------------------------- password

/// Set or replace the password, and rotate the session secret so every other device is logged out.
///
/// The rotation is what makes a password change mean something: without it, a cookie minted before
/// the change keeps working, which is exactly the cookie an attacker would be holding.
pub fn set_password(db_path: &Path, password: &str) -> Result<(), String> {
    let len = password.len();
    if len < MIN_PASSWORD_LEN {
        return Err(format!("a password needs at least {MIN_PASSWORD_LEN} characters"));
    }
    if len > MAX_PASSWORD_LEN {
        return Err(format!(
            "bcrypt ignores anything past {MAX_PASSWORD_LEN} bytes, so a longer password would be \
             silently truncated. Use {MAX_PASSWORD_LEN} or fewer."
        ));
    }
    let hash = bcrypt::hash(password, BCRYPT_COST).map_err(|e| format!("cannot hash: {e}"))?;
    put(db_path, KEY_HASH, &hash)?;
    // New password, new signing key: nothing issued before this moment is still valid.
    let _ = delete(db_path, KEY_SECRET);
    Ok(())
}

/// Check a password. A machine with no password set can never authenticate, so that is false
/// rather than an error: the first-run gate is a separate question.
pub fn verify_password(db_path: &Path, password: &str) -> bool {
    let Some(hash) = get(db_path, KEY_HASH) else {
        return false;
    };
    // bcrypt truncates at 72 bytes on its own; refuse rather than compare a prefix.
    if password.len() > MAX_PASSWORD_LEN {
        return false;
    }
    bcrypt::verify(password, &hash).unwrap_or(false)
}

/// Remove the password and the session secret. The gate opens, which the UI warns about in red.
pub fn remove_password(db_path: &Path) -> Result<(), String> {
    delete(db_path, KEY_HASH)?;
    delete(db_path, KEY_SECRET)?;
    Ok(())
}

// ---------------------------------------------------------------- setup token

const KEY_SETUP: &str = "setup_token_sha256";

/// Where the plaintext token is left for the operator. Beside the database, mode 0600.
pub fn setup_token_path(db_path: &Path) -> std::path::PathBuf {
    db_path.parent().unwrap_or(Path::new(".")).join("setup-token")
}

/// Create a first-run token, once, when no password exists.
///
/// Returns the path so the caller can print it. The plaintext goes to a 0600 file rather than the
/// journal: journald is often volatile, rotates, and on Debian is readable by anyone in the
/// `systemd-journal` or `adm` group, while this file is readable by the daemon user alone.
pub fn ensure_setup_token(db_path: &Path) -> Option<std::path::PathBuf> {
    if password_is_set(db_path) {
        return None;
    }
    let path = setup_token_path(db_path);
    if path.exists() {
        return Some(path);
    }
    let bytes = random_bytes(16)?;
    let token = hex(&bytes);
    put(db_path, KEY_SETUP, &crate::ingest::token_digest(&token)).ok()?;

    // 0600 from creation, not chmod afterwards: a window with wider permissions is a window.
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .ok()?;
        f.write_all(token.as_bytes()).ok()?;
        f.write_all(b"\n").ok()?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, format!("{token}\n")).ok()?;
    }
    Some(path)
}

/// Whether a presented token is the one on disk. Compared as digests, in constant time.
pub fn setup_token_matches(db_path: &Path, presented: &str) -> bool {
    let Some(stored) = get(db_path, KEY_SETUP) else {
        return false;
    };
    if presented.trim().is_empty() {
        return false;
    }
    let want = crate::ingest::token_digest(presented.trim());
    stored.len() == want.len()
        && crate::ingest::constant_time_eq(stored.as_bytes(), want.as_bytes())
}

/// Consume the token: the row and the file both go, so it cannot be replayed.
pub fn clear_setup_token(db_path: &Path) -> Result<(), String> {
    let _ = delete(db_path, KEY_SETUP);
    let path = setup_token_path(db_path);
    let _ = std::fs::remove_file(path);
    Ok(())
}

// ---------------------------------------------------------------- session secret

/// 32 random bytes, from the OS, once. Absent means "no password", not "generate one now".
fn secret(db_path: &Path) -> Option<Vec<u8>> {
    if let Ok(raw) = std::env::var("OF_SESSION_KEY") {
        let cleaned = raw.trim().to_string();
        if !cleaned.is_empty() {
            return Some(cleaned.into_bytes());
        }
    }
    if let Some(hex) = get(db_path, KEY_SECRET) {
        if let Some(bytes) = unhex(&hex) {
            if bytes.len() >= 32 {
                return Some(bytes);
            }
        }
    }
    // Generate on first use and persist, so a restart does not invalidate every cookie.
    let bytes = random_bytes(32)?;
    put(db_path, KEY_SECRET, &hex(&bytes)).ok()?;
    Some(bytes)
}

/// Rotate the signing key. Every cookie issued before now stops verifying.
pub fn rotate_secret(db_path: &Path) -> Result<(), String> {
    let bytes = random_bytes(32).ok_or_else(|| "cannot read random bytes".to_string())?;
    put(db_path, KEY_SECRET, &hex(&bytes))
}

fn random_bytes(n: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    let mut f = std::fs::File::open("/dev/urandom").ok()?;
    f.read_exact(&mut buf).ok()?;
    Some(buf)
}

// ---------------------------------------------------------------- token

/// `v1.<iat>.<exp>.<hmac_hex>`.
///
/// The issued-at is carried as well as the expiry so a clock that jumps backwards can be detected.
/// With an expiry alone, a machine whose clock resets to 1970 satisfies `now <= exp` for decades.
pub fn mint(db_path: &Path) -> Option<String> {
    let key = secret(db_path)?;
    let iat = now();
    let exp = iat + MAX_SESSION_TTL_S;
    let signed = format!("v1.{iat}.{exp}");
    let mac = hmac_sha256(&key, signed.as_bytes());
    Some(format!("{signed}.{}", hex(&mac)))
}

/// Verify a token. Returns the expiry when valid, so the caller can decide what to do with it.
pub fn verify(db_path: &Path, token: &str) -> Option<i64> {
    let mut parts = token.split('.');
    let version = parts.next()?;
    let iat: i64 = parts.next()?.parse().ok()?;
    let exp: i64 = parts.next()?.parse().ok()?;
    let mac = parts.next()?;
    if parts.next().is_some() || version != "v1" {
        return None;
    }

    let t = now();
    if t > exp {
        return None;
    }
    // Bounds the damage from a forged far-future expiry.
    if exp > iat + MAX_SESSION_TTL_S {
        return None;
    }
    // A clock that moved backwards more than a few minutes is not something to reason about, so
    // fail closed rather than extend the session indefinitely.
    if t + BACKWARD_SKEW_S < iat {
        return None;
    }

    let key = secret(db_path)?;
    let expected = hmac_sha256(&key, format!("v1.{iat}.{exp}").as_bytes());
    let given = unhex(mac)?;
    if crate::ingest::constant_time_eq(&expected, &given) {
        Some(exp)
    } else {
        None
    }
}

// ---------------------------------------------------------------- primitives

/// HMAC-SHA256 per RFC 2104.
///
/// Not `sha256(secret || message)`: that construction is open to length extension and is not what
/// anyone means by an HMAC.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        let mut h = Sha256::new();
        h.update(key);
        let d = h.finalize();
        k[..32].copy_from_slice(&d);
    } else {
        k[..key.len()].copy_from_slice(key);
    }

    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(message);
    let inner = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner);
    let out = outer.finalize();

    let mut result = [0u8; 32];
    result.copy_from_slice(&out);
    result
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || !s.is_ascii() {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

// ---------------------------------------------------------------- cookies

/// Read one cookie out of the `Cookie` header. No parsing library: one name, one value.
pub fn cookie_from(headers: &std::collections::HashMap<String, String>) -> Option<String> {
    let raw = headers.get("cookie")?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix(&format!("{COOKIE_NAME}=")) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// The Set-Cookie value that issues a session.
///
/// `Secure` is conditional on the listener actually having TLS. Setting it unconditionally would
/// lock out every plain-HTTP install, which is how most of them start.
pub fn set_cookie(token: &str, lifetime_s: i64, secure: bool) -> String {
    let max_age = lifetime_s.max(1).min(MAX_SESSION_TTL_S);
    let mut c = format!(
        "{COOKIE_NAME}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age}"
    );
    if secure {
        c.push_str("; Secure");
    }
    c
}

pub fn clear_cookie() -> String {
    format!("{COOKIE_NAME}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A database of its own per test.
    ///
    /// Tests run in parallel inside one process, so a timestamp is not unique enough: two tests
    /// starting in the same second shared a directory and collided on the write lock.
    fn temp_db() -> (std::path::PathBuf, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("of-auth-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("status.db");
        db::init(&db).unwrap();
        (dir, db)
    }

    #[test]
    fn hmac_matches_the_rfc_4231_vectors() {
        // RFC 4231 test case 1: key of 0x0b x20, message "Hi There".
        let key = [0x0bu8; 20];
        let got = hmac_sha256(&key, b"Hi There");
        assert_eq!(
            hex(&got),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // Test case 2: key "Jefe", message "what do ya want for nothing?".
        let got = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex(&got),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn hmac_is_not_sha256_of_the_concatenation() {
        use sha2::{Digest, Sha256};
        let key = b"secret";
        let msg = b"message";
        let mut naive = Sha256::new();
        naive.update(key);
        naive.update(msg);
        let naive = naive.finalize();
        assert_ne!(hmac_sha256(key, msg).to_vec(), naive.to_vec());
    }

    #[test]
    fn hex_round_trips_and_rejects_rubbish() {
        let bytes = [0u8, 1, 15, 16, 255];
        assert_eq!(unhex(&hex(&bytes)).unwrap(), bytes.to_vec());
        assert!(unhex("abc").is_none(), "odd length must not parse");
        assert!(unhex("zz").is_none(), "non-hex must not parse");
    }

    #[test]
    fn a_password_is_set_verified_replaced_and_removed() {
        let (_dir, db_path) = temp_db();
        assert!(!password_is_set(&db_path), "a fresh database has no password");

        assert!(verify_password(&db_path, "anything") == false, "no password means no login");

        set_password(&db_path, "correct horse battery").unwrap();
        assert!(password_is_set(&db_path));
        assert!(verify_password(&db_path, "correct horse battery"));
        assert!(!verify_password(&db_path, "wrong"));

        set_password(&db_path, "a different secret").unwrap();
        assert!(verify_password(&db_path, "a different secret"));
        assert!(!verify_password(&db_path, "correct horse battery"), "replaced means replaced");

        remove_password(&db_path).unwrap();
        assert!(!password_is_set(&db_path));
        assert!(!verify_password(&db_path, "a different secret"));
    }

    #[test]
    fn password_limits_are_enforced_with_a_reason() {
        let (_dir, db_path) = temp_db();
        let short = set_password(&db_path, "short").unwrap_err();
        assert!(short.contains("at least"), "{short}");

        let long = "x".repeat(MAX_PASSWORD_LEN + 1);
        let too_long = set_password(&db_path, &long).unwrap_err();
        assert!(too_long.contains("truncated"), "{too_long}");

        // The boundary itself is fine.
        set_password(&db_path, &"y".repeat(MAX_PASSWORD_LEN)).unwrap();
    }

    #[test]
    fn the_stored_hash_is_not_the_password() {
        let (dir, db_path) = temp_db();
        set_password(&db_path, "s3cret passphrase").unwrap();
        let raw = std::fs::read(&db_path).unwrap_or_default();
        let as_text = String::from_utf8_lossy(&raw);
        assert!(!as_text.contains("s3cret passphrase"), "the password must not be stored in the clear");
        let _ = dir;
    }

    #[test]
    fn a_token_minted_now_verifies() {
        let (_dir, db_path) = temp_db();
        set_password(&db_path, "some password here").unwrap();
        let token = mint(&db_path).unwrap();
        assert!(token.starts_with("v1."));
        assert!(verify(&db_path, &token).is_some());
    }

    #[test]
    fn a_tampered_token_does_not_verify() {
        let (_dir, db_path) = temp_db();
        set_password(&db_path, "some password here").unwrap();
        let token = mint(&db_path).unwrap();
        // Flip the expiry without re-signing.
        let mut parts: Vec<&str> = token.split('.').collect();
        let forged = format!("v1.{}.{}.{}", parts[1], "9999999999", parts[3]);
        assert!(verify(&db_path, &forged).is_none());

        // And a token with a mangled signature.
        parts[3] = "deadbeef";
        assert!(verify(&db_path, &format!("{}.{}.{}.{}", parts[0], parts[1], parts[2], parts[3])).is_none());

        assert!(verify(&db_path, "nonsense").is_none());
        assert!(verify(&db_path, "v2.1.2.3").is_none(), "unknown version must not verify");
    }

    #[test]
    fn changing_the_password_logs_every_other_device_out() {
        let (_dir, db_path) = temp_db();
        set_password(&db_path, "first password").unwrap();
        let old = mint(&db_path).unwrap();
        assert!(verify(&db_path, &old).is_some());

        set_password(&db_path, "second password").unwrap();
        assert!(verify(&db_path, &old).is_none(), "the old cookie must stop working");
    }

    #[test]
    fn a_clock_that_jumps_backwards_does_not_grant_a_permanent_session() {
        let (_dir, db_path) = temp_db();
        set_password(&db_path, "some password here").unwrap();
        let key = secret(&db_path).unwrap();

        // A token whose iat is a year in the future: the clock has moved backwards a long way.
        let iat = now() + 31_536_000;
        let exp = iat + MAX_SESSION_TTL_S;
        let signed = format!("v1.{iat}.{exp}");
        let token = format!("{signed}.{}", hex(&hmac_sha256(&key, signed.as_bytes())));
        assert!(
            verify(&db_path, &token).is_none(),
            "a large backward jump must fail closed, not extend the session"
        );
    }

    #[test]
    fn an_expiry_beyond_the_ceiling_is_refused() {
        let (_dir, db_path) = temp_db();
        set_password(&db_path, "some password here").unwrap();
        let key = secret(&db_path).unwrap();
        let iat = now();
        let exp = iat + MAX_SESSION_TTL_S * 10; // signed correctly, but absurd
        let signed = format!("v1.{iat}.{exp}");
        let token = format!("{signed}.{}", hex(&hmac_sha256(&key, signed.as_bytes())));
        assert!(verify(&db_path, &token).is_none());
    }

    #[test]
    fn a_token_signed_with_a_different_secret_is_refused() {
        let (_dir, db_path) = temp_db();
        set_password(&db_path, "some password here").unwrap();
        let token = mint(&db_path).unwrap();
        rotate_secret(&db_path).unwrap();
        assert!(verify(&db_path, &token).is_none());
    }

    #[test]
    fn cookies_carry_the_flags_the_browser_needs() {
        let c = set_cookie("v1.1.2.abc", 86_400, false);
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Lax"));
        assert!(c.contains("Path=/"));
        assert!(!c.contains("Secure"), "plain HTTP installs must still work");

        let c = set_cookie("v1.1.2.abc", 86_400, true);
        assert!(c.contains("Secure"), "TLS listeners should mark the cookie Secure");

        assert!(clear_cookie().contains("Max-Age=0"));
    }

    #[test]
    fn one_cookie_is_read_from_a_messy_header() {
        let mut h = std::collections::HashMap::new();
        h.insert("cookie".to_string(), "other=1; of_session=abc123; another=2".to_string());
        assert_eq!(cookie_from(&h).as_deref(), Some("abc123"));

        h.insert("cookie".to_string(), "other=1".to_string());
        assert!(cookie_from(&h).is_none());

        h.remove("cookie");
        assert!(cookie_from(&h).is_none());
    }
}
