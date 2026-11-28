//! Configuration: TOML on disk, validated into plain structs, reloaded on mtime.
//!
//! Mirrors statushub/config.py. A broken edit never takes the poller down: the last good
//! config is kept and the error is surfaced.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

pub const PROBES: &[&str] = &["http", "tcp", "process", "file_age", "file_json"];
pub const SINK_TYPES: &[&str] = &["ntfy", "webhook"];

/// The poll interval is bounded because the dead man's switch uses a fixed 300 s threshold. A 10
/// minute interval would mean the switch fires during normal operation, and a 5 second one would
/// multiply the probe load for no benefit. Outside this range is refused with a reason, never
/// clamped: a clamp hides a disagreement between the file and the behaviour.
pub const MIN_POLL_INTERVAL_S: u64 = 15;
pub const MAX_POLL_INTERVAL_S: u64 = 60;

/// The interval, or a sentence saying why it was refused.
pub fn check_poll_interval(n: u64) -> Result<u64, String> {
    if n < MIN_POLL_INTERVAL_S {
        return Err(format!(
            "poll_interval {n} is below the minimum of {MIN_POLL_INTERVAL_S} seconds"
        ));
    }
    if n > MAX_POLL_INTERVAL_S {
        return Err(format!(
            "poll_interval {n} is above the maximum of {MAX_POLL_INTERVAL_S} seconds"
        ));
    }
    Ok(n)
}
pub const DEFAULT_PORT: u16 = 8770;
pub const DEFAULT_POLL_INTERVAL: u64 = 30;

#[derive(Debug)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ConfigError {}

#[derive(Debug, Clone, Default)]
pub struct Service {
    pub id: String,
    pub probe: String,
    pub target: String,
    pub name: String,
    pub group: String,
    pub category: String,
    pub expect_status: u16,
    pub contains: String,
    pub field: String,
    pub max_age_s: i64,
    pub timeout: f64,
    pub ports: String,
    /// A network interface that also proves this service is up.
    ///
    /// WireGuard is the case this exists for, and it is genuinely two implementations rather than
    /// one program on two platforms. Linux and Windows load the kernel driver: there is no process
    /// at all, only `wg0`. macOS runs the `wireguard-go` userspace daemon, whose command line reads
    /// `wireguard-go utun`. Neither signal alone can see both, so a service may name either and up
    /// means "one of them answered".
    pub iface: String,
}

#[derive(Debug, Clone, Default)]
pub struct Sink {
    pub kind: String,
    /// Off means configured but silent. The block stays so an operator can switch a route back on
    /// without retyping it, which is why this is a field rather than "present means enabled".
    pub enabled: bool,
    pub url: String,
    pub topic: String,
    pub token_env: String,
    pub use_proxy: bool,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub path: Option<PathBuf>,
    pub node_id: String,
    pub node_name: String,
    pub serve: bool,
    pub bind: String,
    pub port: u16,
    pub poll_interval: u64,
    pub db_path: PathBuf,
    /// When a tail has no `[[service]]` blocks, discover them from what the machine is running.
    /// A head never discovers: it probes what the operator chose.
    pub discover: bool,
    /// A second plain-HTTP listener for tail reports. `None` disables it.
    ///
    /// Tails live on a LAN and should not manage certificates; a phone installing a PWA needs
    /// TLS. Running both at once is the normal case, and the config says so explicitly rather
    /// than making the operator choose.
    pub report_port: Option<u16>,
    /// Where this node reports, e.g. `http://head:8771`. `None` means this process does not report.
    ///
    /// One binary is both roles: serving and reporting are independent settings, so a node can do
    /// both, either, or neither. This replaced the `report --to` CLI flag, which could only ever
    /// make a process do the second thing.
    pub report_to: Option<String>,
    /// The head's display name, learned from its answers and kept. Needed at the moment the head
    /// cannot resend it, which is the moment an alert is being written.
    pub head_name: Option<String>,
    /// PEM certificate and key. Imported by the operator, never minted by this program: it
    /// terminates TLS and nothing else.
    ///
    /// In the config rather than only as flags because an installed service cannot pass flags, and
    /// a certificate that does not survive a reboot is not a certificate you can use.
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    /// Name of the environment variable holding this node's token. Never the token itself: a
    /// config file with a secret in it is a config file that gets committed.
    pub token_env: String,
    /// Path to a file holding the token, for when there is no shell to put it in an environment.
    ///
    /// A launchd or systemd service gets no login environment, so `token_env` cannot work
    /// unattended. The file must be mode 600; installation warns if it is not.
    pub token_file: Option<PathBuf>,
    pub services: Vec<Service>,
    pub sinks: Vec<Sink>,
}

impl Config {
    pub fn service_ids(&self) -> HashSet<&str> {
        self.services.iter().map(|s| s.id.as_str()).collect()
    }

    /// Resolve this node's token, or say precisely why it cannot be resolved.
    ///
    /// One resolver for both roles: a head and a tail that disagreed about where the token lives
    /// would be a config that authenticates on one machine and not the other. The file wins when
    /// both are set, because the file is the only thing that works without a shell.
    pub fn token(&self) -> Result<String, String> {
        if let Some(p) = &self.token_file {
            let text = fs::read_to_string(p)
                .map_err(|e| format!("cannot read token_file {}: {e}", p.display()))?;
            let t = text.trim().to_string();
            if t.is_empty() {
                return Err(format!("token_file {} is empty", p.display()));
            }
            return Ok(t);
        }
        if self.token_env.trim().is_empty() {
            return Err("neither token_file nor token_env is configured".into());
        }
        match std::env::var(self.token_env.trim()) {
            Ok(t) if !t.trim().is_empty() => Ok(t.trim().to_string()),
            _ => Err(format!("environment variable {} is unset or empty", self.token_env.trim())),
        }
    }
}

fn env_or(var: &str, fallback: PathBuf) -> PathBuf {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => fallback,
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

pub fn config_home() -> PathBuf {
    if cfg!(target_os = "macos") {
        return home().join("Library").join("Application Support").join("observer-frog");
    }
    if cfg!(windows) {
        let base = std::env::var("APPDATA").map(PathBuf::from).unwrap_or_else(|_| home().join("AppData").join("Roaming"));
        return base.join("observer-frog");
    }
    env_or("XDG_CONFIG_HOME", home().join(".config")).join("observer-frog")
}

pub fn state_home() -> PathBuf {
    if cfg!(target_os = "macos") {
        return config_home();
    }
    if cfg!(windows) {
        let base = std::env::var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|_| home().join("AppData").join("Local"));
        return base.join("observer-frog");
    }
    env_or("XDG_STATE_HOME", home().join(".local").join("state")).join("observer-frog")
}

pub fn default_path() -> PathBuf {
    config_home().join("config.toml")
}

pub fn default_db_path() -> PathBuf {
    state_home().join("status.db")
}

pub fn hostname() -> String {
    // sysinfo's hostname is portable and needs no libc call of our own.
    sysinfo::System::host_name().unwrap_or_else(|| "localhost".to_string())
}

fn str_of(v: Option<&toml::Value>) -> String {
    v.and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn int_of(v: Option<&toml::Value>) -> i64 {
    v.and_then(|x| x.as_integer()).unwrap_or(0)
}

fn float_of(v: Option<&toml::Value>) -> Option<f64> {
    v.and_then(|x| x.as_float().or_else(|| x.as_integer().map(|i| i as f64)))
}

fn bool_of(v: Option<&toml::Value>, fallback: bool) -> bool {
    v.and_then(|x| x.as_bool()).unwrap_or(fallback)
}

/// Accept both `[[service]]` (array of tables) and a single `[service]` table.
fn tables<'a>(value: Option<&'a toml::Value>, field: &str, where_: &str) -> Result<Vec<&'a toml::Table>, ConfigError> {
    match value {
        None => Ok(vec![]),
        Some(toml::Value::Table(t)) => Ok(vec![t]),
        Some(toml::Value::Array(a)) => {
            let mut out = Vec::with_capacity(a.len());
            for v in a {
                match v {
                    toml::Value::Table(t) => out.push(t),
                    _ => return Err(ConfigError(format!("{where_}: {field} must be a table or an array of tables"))),
                }
            }
            Ok(out)
        }
        Some(_) => Err(ConfigError(format!("{where_}: {field} must be a table or an array of tables"))),
    }
}

/// The port a service listens on, read off its target when the config does not state it.
///
/// `ports` used to be free text nobody filled in, so every card showed no port. The target
/// already carries it (`http://host:3009/`, `127.0.0.1:22`), so deriving it means a discovered
/// service and a hand-written one both report the truth.
pub fn port_of(target: &str) -> String {
    let rest = match target.find("://") {
        Some(i) => &target[i + 3..],
        None => target,
    };
    let authority = rest.split('/').next().unwrap_or("");
    if authority.starts_with('[') {
        return authority.rsplit_once("]:").map(|(_, p)| p.to_string()).unwrap_or_default();
    }
    match authority.rsplit_once(':') {
        Some((_, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => p.to_string(),
        _ => String::new(),
    }
}

fn as_service(raw: &toml::Table, where_: &str) -> Result<Service, ConfigError> {
    let sid = str_of(raw.get("id")).trim().to_string();
    if sid.is_empty() {
        return Err(ConfigError(format!("{where_}: service without an id")));
    }
    let probe = str_of(raw.get("probe")).trim().to_string();
    if !PROBES.contains(&probe.as_str()) {
        return Err(ConfigError(format!(
            "{where_}: service '{sid}' has probe {probe:?}; expected one of {}",
            PROBES.join(", ")
        )));
    }
    let target = str_of(raw.get("target")).trim().to_string();
    if target.is_empty() {
        return Err(ConfigError(format!("{where_}: service '{sid}' has no target")));
    }
    let name = match raw.get("name") {
        Some(v) => str_of(Some(v)),
        None => sid.clone(),
    };
    let group = match raw.get("group") {
        Some(v) => str_of(Some(v)),
        None => "Services".to_string(),
    };
    let derived_port = port_of(&target);
    Ok(Service {
        id: sid,
        probe,
        target,
        name,
        group,
        category: str_of(raw.get("category")),
        expect_status: int_of(raw.get("expect_status")).max(0) as u16,
        contains: str_of(raw.get("contains")),
        field: str_of(raw.get("field")),
        max_age_s: int_of(raw.get("max_age_s")),
        timeout: float_of(raw.get("timeout")).unwrap_or(2.0),
        ports: {
            let explicit = str_of(raw.get("ports"));
            if explicit.is_empty() { derived_port } else { explicit }
        },
        iface: str_of(raw.get("iface")),
    })
}

fn as_sink(raw: &toml::Table, where_: &str) -> Result<Sink, ConfigError> {
    let kind = str_of(raw.get("type")).trim().to_string();
    if !SINK_TYPES.contains(&kind.as_str()) {
        return Err(ConfigError(format!(
            "{where_}: unknown sink type {kind:?}; expected one of {}",
            SINK_TYPES.join(", ")
        )));
    }
    Ok(Sink {
        kind,
        // Absent means enabled, so every config written before this field existed keeps publishing.
        // The writer always writes it explicitly, so absence can only ever mean "old config".
        enabled: bool_of(raw.get("enabled"), true),
        url: str_of(raw.get("url")),
        topic: str_of(raw.get("topic")),
        token_env: str_of(raw.get("token_env")),
        use_proxy: bool_of(raw.get("use_proxy"), false),
    })
}

/// Read the config, or return defaults when the file does not exist (zero-config boot).
pub fn load(path: Option<&Path>) -> Result<Config, ConfigError> {
    let p = path.map(PathBuf::from).unwrap_or_else(default_path);
    let exists = p.is_file();
    let where_ = p.display().to_string();

    let raw: toml::Table = if exists {
        let text = fs::read_to_string(&p)
            .map_err(|e| ConfigError(format!("{where_}: {e}")))?;
        text.parse::<toml::Table>()
            .map_err(|e| ConfigError(format!("{where_}: {e}")))?
    } else {
        toml::Table::new()
    };

    let node = raw.get("node").and_then(|v| v.as_table());
    let serve_cfg = raw.get("serve").and_then(|v| v.as_table());

    let mut services = Vec::new();
    for t in tables(raw.get("service"), "service", &where_)? {
        services.push(as_service(t, &where_)?);
    }

    // A tail with no explicit list asks the machine what it is running. A head does not: it
    // probes what the operator chose, and discovering on a head would silently add services the
    // operator never asked for.
    // Same resolution the `serve` field uses, so `serve = false` disables serving everywhere it
    // is consulted rather than in one place only.
    let serve_enabled = raw
        .get("serve")
        .and_then(|v| v.as_bool())
        .or_else(|| serve_cfg.and_then(|t| t.get("enabled")).and_then(|v| v.as_bool()))
        .unwrap_or(true);
    let discover = raw
        .get("discover")
        .and_then(|v| v.as_bool())
        .unwrap_or(!serve_enabled && services.is_empty());
    let mut seen = HashSet::new();
    let mut dupes: Vec<String> = Vec::new();
    for s in &services {
        if !seen.insert(s.id.clone()) {
            dupes.push(s.id.clone());
        }
    }
    if !dupes.is_empty() {
        dupes.sort();
        return Err(ConfigError(format!("{where_}: duplicate service id(s): {}", dupes.join(", "))));
    }

    let mut sinks = Vec::new();
    for t in tables(raw.get("sink"), "sink", &where_)? {
        sinks.push(as_sink(t, &where_)?);
    }

    let node_id = match node.and_then(|t| t.get("id")) {
        Some(v) if !str_of(Some(v)).trim().is_empty() => str_of(Some(v)),
        _ => "local".to_string(),
    };
    // A blank name means "use this machine's hostname", which is what config.example.toml
    // documents. Taking the empty string literally left the dashboard with no device name.
    let node_name = match node.and_then(|t| t.get("name")) {
        Some(v) if !str_of(Some(v)).trim().is_empty() => str_of(Some(v)),
        _ => hostname(),
    };
    // A port of 0 or an absent key means "no separate report listener".
    let report_port = match int_of(serve_cfg.and_then(|t| t.get("report_port"))) {
        0 => None,
        n if n > 0 && n <= 65535 => Some(n as u16),
        _ => None,
    };
    let poll_interval = match int_of(raw.get("poll_interval")) {
        0 => DEFAULT_POLL_INTERVAL,
        n => check_poll_interval(n.max(1) as u64)
            .map_err(|e| ConfigError(format!("{where_}: {e}")))?,
    };
    let db_path = match raw.get("db_path").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => PathBuf::from(s),
        _ => default_db_path(),
    };

    // A node that reports to itself would double-count its own samples and never show its own
    // local view, so refuse it here rather than loop at runtime.
    // TLS paths are validated here only for being a pair; whether the files parse and match each
    // other is the listener's job, because that is where they are opened.
    let tls_cert = str_of(raw.get("tls_cert")).trim().to_string();
    let tls_key = str_of(raw.get("tls_key")).trim().to_string();
    if tls_cert.is_empty() != tls_key.is_empty() {
        return Err(ConfigError(format!(
            "{where_}: tls_cert and tls_key must be set together, or neither"
        )));
    }

    let report_to = match str_of(raw.get("report_to")) {
        s if s.trim().is_empty() => None,
        s => {
            let addr = s.trim().trim_end_matches('/').to_string();
            let self_port = int_of(serve_cfg.and_then(|t| t.get("report_port")))
                .max(int_of(serve_cfg.and_then(|t| t.get("port"))));
            let points_at_self = addr
                .rsplit(':')
                .next()
                .and_then(|p| p.parse::<u16>().ok())
                .map(|p| self_port > 0 && p as i64 == self_port)
                .unwrap_or(false)
                && (addr.contains("127.0.0.1") || addr.contains("localhost") || addr.contains("0.0.0.0"));
            if points_at_self {
                return Err(ConfigError(format!(
                    "{where_}: report_to {addr} points at this node's own port; a node cannot report to itself"
                )));
            }
            Some(addr)
        }
    };
    let token_env = str_of(raw.get("token_env")).trim().to_string();
    let token_file = match str_of(raw.get("token_file")).trim() {
        "" => None,
        s => Some(PathBuf::from(s)),
    };

    Ok(Config {
        path: if exists { Some(p) } else { None },
        node_id,
        node_name,
        // `serve = false` at the top level is the documented tail form, but this used to read
        // only `[serve] enabled`. A tail written that way silently ran as a head: it bound a port
        // it never needed and skipped discovery. Accept both, preferring the explicit table key.
        serve: match raw.get("serve").and_then(|v| v.as_bool()) {
            Some(b) => b,
            None => bool_of(serve_cfg.and_then(|t| t.get("enabled")), true),
        },
        bind: match serve_cfg.and_then(|t| t.get("bind")) {
            Some(v) => str_of(Some(v)),
            None => "127.0.0.1".to_string(),
        },
        port: match int_of(serve_cfg.and_then(|t| t.get("port"))) {
            0 => DEFAULT_PORT,
            n => n as u16,
        },
        poll_interval,
        db_path,
        discover,
        report_port,
        report_to,
        head_name: {
            let n = str_of(raw.get("head_name")).trim().to_string();
            (!n.is_empty()).then_some(n)
        },
        tls_cert: if tls_cert.is_empty() { None } else { Some(PathBuf::from(tls_cert)) },
        tls_key: if tls_key.is_empty() { None } else { Some(PathBuf::from(tls_key)) },
        token_env,
        token_file,
        services,
        sinks,
    })
}

/// Add every catalog service this machine is running to `services`, keeping what is already there.
///
/// One function, because three places need the same answer and they drifted apart when each had
/// its own copy: `runtime::run` at boot, `tail::run` for the one-shot reporter, and `Watcher::refresh`
/// on every reload. The refresh case was the costly one — it returned the file's list alone, so
/// discovered services vanished a wave after boot while the node kept reporting the rest.
///
/// Explicit entries win on id, so a port chosen by hand is never overwritten by the catalog default.
pub fn merge_discovery(mut services: Vec<Service>, discover: bool) -> Vec<Service> {
    if !discover {
        return services;
    }
    let Ok(entries) = crate::catalog::load(None) else { return services };
    for svc in crate::discover::discover(&entries, "127.0.0.1") {
        if !services.iter().any(|s| s.id == svc.id) {
            services.push(svc);
        }
    }
    services
}

/// Holds the current config and swaps it when the file's mtime changes.
pub struct Watcher {
    pub config: Config,
    path: Option<PathBuf>,
    mtime: Option<std::time::SystemTime>,
    pub error: Option<String>,
}

impl Watcher {
    pub fn new(path: Option<&Path>) -> Result<Self, ConfigError> {
        let config = load(path)?;
        let file = config.path.clone();
        let mtime = file.as_ref().and_then(|p| fs::metadata(p).ok()).and_then(|m| m.modified().ok());
        Ok(Self { config, path: file, mtime, error: None })
    }

    fn stat(&self) -> Option<std::time::SystemTime> {
        self.path.as_ref().and_then(|p| fs::metadata(p).ok()).and_then(|m| m.modified().ok())
    }

    /// Re-read the file when it changes, and merge discovery into the result.
    ///
    /// The merge is the point. Discovery adds services the config file does not name, so a reload
    /// that returned the file's contents alone silently dropped every one of them on the first
    /// wave after boot: the node kept reporting, but only the handful of services that happened to
    /// be written down, and the head marked the rest "no longer reporting" four waves later. A
    /// reload has to rebuild the config the same way boot does, or the two disagree.
    /// Re-read the file when it changes. Nothing else.
    ///
    /// No discovery, and no rescan on a clock: the config is what the operator chose, so a reload
    /// returns exactly that. Discovery had been folded in here and it made the file and the running
    /// state disagree — the file said two services, the dashboard said fifteen, and the difference
    /// was whatever happened to be listening at the moment of the scan.
    pub fn refresh(&mut self) -> &Config {
        let mtime = self.stat();
        if mtime != self.mtime {
            self.mtime = mtime;
            let target = self.path.clone().unwrap_or_else(default_path);
            match load(Some(&target)) {
                Ok(c) => {
                    self.config = c;
                    self.error = None;
                }
                Err(e) => self.error = Some(e.0),
            }
        }
        &self.config
    }
}

#[cfg(test)]
mod port_tests {
    use super::{check_poll_interval, load, port_of, Watcher};

    /// A reload returns the file, and nothing but the file.
    ///
    /// Discovery is an operator action, so the running config must equal what was written down. The
    /// regression this guards against is the opposite of the obvious one: a watcher that "helpfully"
    /// merged whatever it could discover made the dashboard show services the config never named,
    /// and those services alerted when they fell over. `refresh` must stay a plain re-read.
    #[test]
    fn a_reload_returns_exactly_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("tail.toml");
        std::fs::write(
            &f,
            "discover = true\nserve = false\n\n[[service]]\nid = \"explicit\"\nprobe = \"tcp\"\ntarget = \"127.0.0.1:1\"\n",
        )
        .unwrap();

        let mut w = Watcher::new(Some(&f)).unwrap();
        // Touch the file so the mtime moves and the watcher reloads.
        std::fs::write(
            &f,
            "discover = true\nserve = false\n\n[[service]]\nid = \"explicit\"\nprobe = \"tcp\"\ntarget = \"127.0.0.1:1\"\n\n# touched\n",
        )
        .unwrap();
        let after = w.refresh().services.len();

        // One service is written down, so one must be polled — even on a machine running a dozen
        // catalog services. Anything larger means the watcher discovered on its own initiative.
        assert_eq!(after, 1, "a reload must return exactly the file, not what it could discover");
        assert_eq!(
            after,
            load(Some(&f)).unwrap().services.len(),
            "the running config must match the file it was loaded from"
        );
    }

    #[test]
    fn a_top_level_serve_false_means_tail() {
        // The documented tail form. Reading only `[serve] enabled` made every such config run
        // as a head, binding a port and skipping discovery.
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("tail.toml");
        // The bounds exist for the dead man's switch: outside them the fixed five-minute threshold
        // is either meaningless or always true.
        assert!(check_poll_interval(15).is_ok());
        assert!(check_poll_interval(60).is_ok());
        let low = check_poll_interval(5).unwrap_err();
        assert!(low.contains("below the minimum"), "{low}");
        let high = check_poll_interval(600).unwrap_err();
        assert!(high.contains("above the maximum"), "{high}");

        std::fs::write(&f, "poll_interval = 5\n").unwrap();
        let refused = load(Some(&f)).unwrap_err();
        assert!(
            refused.0.contains("below the minimum"),
            "a bad interval must be refused at load, naming the range: {}",
            refused.0
        );

        std::fs::write(&f, "poll_interval = 600\n").unwrap();
        let refused = load(Some(&f)).unwrap_err();
        assert!(refused.0.contains("above the maximum"), "{}", refused.0);

        std::fs::write(&f, "poll_interval = 45\n").unwrap();
        assert_eq!(load(Some(&f)).unwrap().poll_interval, 45);

        std::fs::write(&f, "serve = false\nreport_to = \"http://head:8771\"\n[node]\nid = \"t\"\n").unwrap();
        let cfg = super::load(Some(&f)).unwrap();
        assert!(!cfg.serve, "serve = false must be honoured");
        assert!(cfg.discover, "a tail with no service list should discover");
    }

    #[test]
    fn reads_the_port_off_a_target() {
        // Every card used to show no port because it came from a config field nobody filled in.
        assert_eq!(port_of("http://127.0.0.1:3009/"), "3009");
        assert_eq!(port_of("127.0.0.1:22"), "22");
        assert_eq!(port_of("http://[::1]:8770/api/health"), "8770");
        assert_eq!(port_of("https://example.org/x"), "");
        assert_eq!(port_of(""), "");
    }
}
