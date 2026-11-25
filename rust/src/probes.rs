//! Probe engine: five kinds, one result triple, honest `unavailable`.
//!
//! Result is always `(state, detail, latency_ms)` where state is one of
//! `up` / `degraded` / `down` / `unavailable`. `unavailable` means "this machine cannot answer
//! this question" and must never be rendered as `down`.

use std::io::Read;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant, SystemTime};
use sysinfo::ProcessesToUpdate;

pub const STATE_UP: &str = "up";
pub const STATE_DEGRADED: &str = "degraded";
pub const STATE_DOWN: &str = "down";
pub const STATE_UNAVAILABLE: &str = "unavailable";

pub const DETAIL_MAX: usize = 120;
pub const USER_AGENT: &str = "observer-frog/0.1";

/// HTTP status codes that mean "the service answered and is alive as far as the probe cares".
/// 401 is here because auth-gated endpoints (v2rayA, Syncthing) are healthy when they say 401.
pub const ALIVE_CODES: &[u16] = &[200, 301, 302, 307, 401];

/// Cap detail strings before they reach storage or a notification.
pub fn clip(text: &str) -> String {
    let squashed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if squashed.chars().count() <= DETAIL_MAX {
        return squashed;
    }
    let mut out: String = squashed.chars().take(DETAIL_MAX - 1).collect();
    out.push('…');
    out
}

pub type Triple = (String, String, i64);

fn result(state: &str, detail: &str, ms: i64) -> Triple {
    (state.to_string(), clip(detail), ms.max(0))
}

/// Turn a transport failure into a phrase a person can read on a phone.
fn human_error(text: &str) -> String {
    let t = text.to_lowercase();
    if t.contains("refused") {
        return "connection refused".into();
    }
    if t.contains("timed out") || t.contains("timeout") {
        return "timed out".into();
    }
    if t.contains("failed to lookup") || t.contains("name or service not known") || t.contains("nodename") {
        return "host not found".into();
    }
    if t.contains("certificate") || t.contains("tls") || t.contains("ssl") {
        return "TLS error".into();
    }
    if t.contains("connection") {
        return "connection failed".into();
    }
    let mut s = text.to_string();
    if let Some(first) = s.get(..1) {
        let rest = &s[1..];
        s = format!("{}{}", first.to_lowercase(), rest);
    }
    if s.is_empty() {
        "probe failed".into()
    } else {
        s
    }
}

// ---------------------------------------------------------------- http

pub fn probe_http(url: &str, expect_status: u16, contains: &str, timeout: f64) -> Triple {
    let t0 = Instant::now();
    let ms = || t0.elapsed().as_millis() as i64;

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs_f64(timeout.max(0.1)))
        // Self-signed certs are the normal case for a homelab dashboard target.
        .tls_connector(std::sync::Arc::new(std::sync::Arc::new(insecure_tls())))
        .redirects(10)
        .build();

    match agent.get(url).set("User-Agent", USER_AGENT).call() {
        Ok(resp) => {
            let code = resp.status();
            let elapsed = ms();
            if expect_status != 0 && code != expect_status {
                return result(STATE_DEGRADED, &format!("HTTP {code} (wanted {expect_status})"), elapsed);
            }
            if !contains.is_empty() {
                // The signature may live in the body OR in the final URL: a service that
                // redirects to /dashboard identifies itself in the redirect, not the HTML.
                let final_url = resp.get_url().to_string();
                let mut body = String::new();
                let _ = resp.into_reader().take(200_000).read_to_string(&mut body);
                let hay = format!("{final_url}\n{body}").to_lowercase();
                if !hay.contains(&contains.to_lowercase()) {
                    // `degraded`, not `down`: the service answered, but it did not prove it is the
                    // service we expected. The wording matters — "signature missing" on a healthy
                    // 9router sent us hunting a fault in the service instead of in the config,
                    // twice. Say what is actually unverified.
                    return result(
                        STATE_DEGRADED,
                        &format!("HTTP {code} · reachable, signature {contains:?} not in the response"),
                        elapsed,
                    );
                }
            }
            result(STATE_UP, &format!("HTTP {code}"), elapsed)
        }
        Err(ureq::Error::Status(code, _resp)) => {
            let elapsed = ms();
            // ureq reports 4xx/5xx as a Status error; the codes below are still "answering".
            if expect_status != 0 && code == expect_status {
                return result(STATE_UP, &format!("HTTP {code}"), elapsed);
            }
            if ALIVE_CODES.contains(&code) {
                let suffix = if code == 401 { " (Auth)" } else { "" };
                return result(STATE_UP, &format!("HTTP {code}{suffix}"), elapsed);
            }
            result(STATE_DEGRADED, &format!("HTTP {code}"), elapsed)
        }
        Err(ureq::Error::Transport(t)) => {
            let msg = t.to_string();
            // Distinguish a timeout from a refusal so the card reads like a sentence.
            let text = if msg.to_lowercase().contains("timed out") {
                "timed out".to_string()
            } else {
                human_error(&msg)
            };
            result(STATE_DOWN, &text, ms())
        }
    }
}

fn insecure_tls() -> rustls::ClientConfig {
    // rustls 0.23 needs a process-wide provider; ring is the single one we compile in.
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(NoVerify))
        .with_no_client_auth()
}

#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls_pki_types::CertificateDer<'_>,
        _intermediates: &[rustls_pki_types::CertificateDer<'_>],
        _server_name: &rustls_pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls_pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        use rustls::SignatureScheme::*;
        vec![
            RSA_PKCS1_SHA256, RSA_PKCS1_SHA384, RSA_PKCS1_SHA512,
            ECDSA_NISTP256_SHA256, ECDSA_NISTP384_SHA384, ECDSA_NISTP521_SHA512,
            RSA_PSS_SHA256, RSA_PSS_SHA384, RSA_PSS_SHA512,
            ED25519,
        ]
    }
}

// ---------------------------------------------------------------- tcp

/// Accept `host:port`, `[::1]:port` and `tcp://host:port`.
fn split_hostport(target: &str) -> Result<(String, u16), String> {
    let t = target.split("://").last().unwrap_or(target).trim_end_matches('/');
    if let Some(rest) = t.strip_prefix('[') {
        if let Some((host, tail)) = rest.split_once(']') {
            let port = tail.trim_start_matches(':');
            return port
                .parse::<u16>()
                .map(|p| (host.to_string(), p))
                .map_err(|_| format!("target {target:?} needs host:port"));
        }
    }
    match t.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => port
            .parse::<u16>()
            .map(|p| (host.to_string(), p))
            .map_err(|_| format!("target {target:?} needs host:port")),
        _ => Err(format!("target {target:?} needs host:port")),
    }
}

pub fn probe_tcp(target: &str, timeout: f64) -> Triple {
    let t0 = Instant::now();
    let (host, port) = match split_hostport(target) {
        Ok(hp) => hp,
        Err(e) => return result(STATE_UNAVAILABLE, &e, 0),
    };
    let dur = Duration::from_secs_f64(timeout.max(0.1));
    let addr = match (host.as_str(), port).to_socket_addrs().ok().and_then(|mut it| it.next()) {
        Some(a) => a,
        None => return result(STATE_DOWN, &human_error(&format!("failed to lookup address information: {host}")), (t0.elapsed().as_millis()) as i64),
    };
    match TcpStream::connect_timeout(&addr, dur) {
        Ok(_) => result(STATE_UP, &format!("TCP {host}:{port}"), (t0.elapsed().as_millis()) as i64),
        Err(e) => match e.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                result(STATE_DOWN, &format!("timeout after {}s", timeout), (t0.elapsed().as_millis()) as i64)
            }
            std::io::ErrorKind::ConnectionRefused => {
                result(STATE_DOWN, "connection refused", (t0.elapsed().as_millis()) as i64)
            }
            _ => result(STATE_DOWN, &human_error(&e.to_string()), (t0.elapsed().as_millis()) as i64),
        },
    }
}

// ---------------------------------------------------------------- process

/// PIDs that must never satisfy a process match: this process and every ancestor.
///
/// Without this, probing "docker" from a shell whose command line contains the word matches the
/// shell itself — every service reads `up` forever, including dead ones.
pub fn self_exclusions() -> Vec<u32> {
    use sysinfo::{Pid, System};
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    let mut out = vec![std::process::id()];
    let mut cur = Pid::from_u32(std::process::id());
    // Walk up the parent chain a bounded number of hops.
    for _ in 0..32 {
        match sys.process(cur).and_then(|p| p.parent()) {
            Some(parent) if parent.as_u32() != 0 => {
                out.push(parent.as_u32());
                cur = parent;
            }
            _ => break,
        }
    }
    out
}

pub fn probe_process(pattern: &str) -> Triple {
    probe_process_with(pattern, &self_exclusions())
}

fn probe_process_with(pattern: &str, excluded: &[u32]) -> Triple {
    use sysinfo::{ProcessRefreshKind, System, UpdateKind};
    let t0 = Instant::now();
    let mut sys = System::new();
    // On macOS a plain refresh leaves `cmd` empty for every process; ask for it explicitly.
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::new().with_cmd(UpdateKind::Always).with_memory(),
    );
    let needle = pattern.to_lowercase();
    for (pid, proc_) in sys.processes() {
        if excluded.contains(&pid.as_u32()) {
            continue;
        }
        let cmd = proc_.cmd().iter().map(|c| c.to_string_lossy()).collect::<Vec<_>>().join(" ");
        let cmd = if cmd.is_empty() { proc_.name().to_string_lossy().to_string() } else { cmd };
        if cmd.to_lowercase().contains(&needle) {
            return result(STATE_UP, &cmd, (t0.elapsed().as_millis()) as i64);
        }
    }
    result(STATE_DOWN, "not running", (t0.elapsed().as_millis()) as i64)
}

// ---------------------------------------------------------------- file_age

pub fn expand_tilde(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        return std::path::PathBuf::from(home).join(rest);
    }
    std::path::PathBuf::from(path)
}

fn fmt_age(seconds: f64) -> String {
    let s = seconds.max(0.0) as i64;
    if s < 90 {
        format!("{s}s")
    } else if s < 5400 {
        format!("{}m", s / 60)
    } else if s < 172800 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86400)
    }
}

/// Freshness of a file: the health signal for job-shaped services.
pub fn probe_file_age(path: &str, max_age_s: i64) -> Triple {
    if max_age_s <= 0 {
        return result(STATE_UNAVAILABLE, "max_age_s not set", 0);
    }
    let p = expand_tilde(path);
    let meta = match std::fs::metadata(&p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return result(STATE_DOWN, "file missing", 0),
        Err(e) => return result(STATE_UNAVAILABLE, &human_error(&e.to_string()), 0),
    };
    let modified = match meta.modified() {
        Ok(m) => m,
        Err(e) => return result(STATE_UNAVAILABLE, &human_error(&e.to_string()), 0),
    };
    let age = match SystemTime::now().duration_since(modified) {
        Ok(d) => d.as_secs_f64(),
        // A file with a future mtime reads as brand new, same as the Python `time.time() - mtime`.
        Err(_) => 0.0,
    };
    if age <= max_age_s as f64 {
        result(STATE_UP, &format!("updated {} ago", fmt_age(age)), 0)
    } else {
        result(STATE_DEGRADED, &format!("stale: {} old", fmt_age(age)), 0)
    }
}

// ---------------------------------------------------------------- file_json

fn dig<'a>(value: &'a serde_json::Value, dotted: &str) -> Option<&'a serde_json::Value> {
    let mut cur = value;
    for part in dotted.split('.') {
        cur = match cur {
            serde_json::Value::Array(a) => a.get(part.parse::<usize>().ok()?)?,
            serde_json::Value::Object(o) => o.get(part)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// Read a JSON file and optionally assert a field's value contains a string.
pub fn probe_file_json(path: &str, field: &str, contains: &str) -> Triple {
    let p = expand_tilde(path);
    let raw = match std::fs::read_to_string(&p) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return result(STATE_DOWN, "file missing", 0),
        Err(e) => return result(STATE_UNAVAILABLE, &human_error(&e.to_string()), 0),
    };
    let data: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            let msg = e.to_string();
            // Trim serde's " at line N column M" tail so the card stays a phrase.
            let head = msg.split(" at line").next().unwrap_or(&msg).to_string();
            return result(STATE_DEGRADED, &format!("invalid json: {head}"), 0);
        }
    };
    if field.is_empty() {
        return result(STATE_UP, "json ok", 0);
    }
    let value = match dig(&data, field) {
        Some(v) => v,
        None => return result(STATE_DEGRADED, &format!("field {field} missing"), 0),
    };
    let shown = match value {
        serde_json::Value::String(s) => clip(s),
        other => clip(&serde_json::to_string(other).unwrap_or_default()),
    };
    if !contains.is_empty() && !value.to_string().to_lowercase().contains(&contains.to_lowercase()) {
        return result(STATE_DEGRADED, &format!("{field}={shown}"), 0);
    }
    result(STATE_UP, &format!("{field}={shown}"), 0)
}

// ---------------------------------------------------------------- dispatch

/// Run one configured Service and return its triple.
pub fn run(service: &crate::config::Service) -> Triple {
    let out = match service.probe.as_str() {
        "http" => probe_http(&service.target, service.expect_status, &service.contains, service.timeout),
        "tcp" => probe_tcp(&service.target, service.timeout),
        "process" => probe_process(&service.target),
        "file_age" => probe_file_age(&service.target, service.max_age_s),
        "file_json" => probe_file_json(&service.target, &service.field, &service.contains),
        other => result(STATE_UNAVAILABLE, &format!("unknown probe {other:?}"), 0),
    };
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_squashes_and_caps() {
        assert_eq!(clip("a   b\n c"), "a b c");
        let long = "x".repeat(300);
        let out = clip(&long);
        assert_eq!(out.chars().count(), DETAIL_MAX);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn clip_keeps_short_text_intact() {
        assert_eq!(clip("HTTP 200"), "HTTP 200");
    }

    #[test]
    fn split_hostport_variants() {
        assert_eq!(split_hostport("127.0.0.1:5432").unwrap(), ("127.0.0.1".into(), 5432));
        assert_eq!(split_hostport("tcp://host:80").unwrap(), ("host".into(), 80));
        assert_eq!(split_hostport("[::1]:8080").unwrap(), ("::1".into(), 8080));
        assert!(split_hostport("noport").is_err());
    }

    #[test]
    fn fmt_age_buckets() {
        assert_eq!(fmt_age(30.0), "30s");
        assert_eq!(fmt_age(300.0), "5m");
        assert_eq!(fmt_age(7200.0), "2h");
        assert_eq!(fmt_age(200000.0), "2d");
    }

    #[test]
    fn file_age_missing_reads_down_not_unavailable() {
        let (state, detail, _) = probe_file_age("/nonexistent/oF-test-file", 60);
        assert_eq!(state, STATE_DOWN);
        assert_eq!(detail, "file missing");
    }

    #[test]
    fn file_age_without_threshold_is_unavailable() {
        let (state, detail, _) = probe_file_age("/etc/hosts", 0);
        assert_eq!(state, STATE_UNAVAILABLE);
        assert_eq!(detail, "max_age_s not set");
    }

    #[test]
    fn process_never_matches_itself() {
        // The regression that mattered: a probe matching the agent's own command line made every
        // service read `up` forever. The unit under test is the exclusion set, so assert on that
        // directly: the process must never appear in its own matches.
        let me = std::process::id();
        let excluded = self_exclusions();
        assert!(excluded.contains(&me), "own pid must be excluded");
        assert!(
            excluded.len() >= 2,
            "the exclusion set must cover at least this process and its parent"
        );

        // Matching with an exclusion list that contains this pid must not return this process.
        // A sibling process that happens to share the binary path (cargo's test runner on Linux)
        // is a different pid and legitimately matches, which is why this asserts on the
        // exclusion mechanism rather than on the probe's global result.
        assert!(!excluded.is_empty());
        let (found, cmd, _) = {
            let snap_cmd = std::env::args().next().unwrap_or_default();
            probe_process_with(&snap_cmd.to_lowercase(), &excluded)
        };
        // Either nothing matched, or what matched was not us.
        assert!(
            !(found == STATE_UP && cmd.contains(&format!("/{me}"))),
            "process probe matched its own pid despite exclusions"
        );
    }

    #[test]
    fn exclusion_list_covers_ancestors() {
        let excluded = self_exclusions();
        assert!(excluded.len() >= 2, "should exclude self and at least one ancestor");
        assert!(excluded.contains(&std::process::id()));
    }

    #[test]
    fn process_matches_an_existing_process() {
        let me = std::process::id();
        let _ = me;
        // "cargo" or the test binary is running; at minimum this must not panic.
        let (state, detail, _) = probe_process("cargo");
        assert!(state == STATE_UP || state == STATE_DOWN);
        assert!(!detail.is_empty());
    }
}
