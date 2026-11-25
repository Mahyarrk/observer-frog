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
}

#[derive(Debug, Clone, Default)]
pub struct Sink {
    pub kind: String,
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
    pub services: Vec<Service>,
    pub sinks: Vec<Sink>,
}

impl Config {
    pub fn service_ids(&self) -> HashSet<&str> {
        self.services.iter().map(|s| s.id.as_str()).collect()
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
        ports: str_of(raw.get("ports")),
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
    let poll_interval = match int_of(raw.get("poll_interval")) {
        0 => DEFAULT_POLL_INTERVAL,
        n => n.max(1) as u64,
    };
    let db_path = match raw.get("db_path").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => PathBuf::from(s),
        _ => default_db_path(),
    };

    Ok(Config {
        path: if exists { Some(p) } else { None },
        node_id,
        node_name,
        serve: bool_of(serve_cfg.and_then(|t| t.get("enabled")), true),
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
        services,
        sinks,
    })
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
