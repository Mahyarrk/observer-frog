//! The service catalog: shipped data that compiles to probe kinds.
//!
//! An entry is a *template*, not a service. `instantiate()` turns one catalog row plus a user's
//! host/port into a `config::Service`. Ports are defaults, never identities.

use crate::config::{ConfigError, Service, PROBES};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub group: String,
    pub probe: String,
    pub default_port: u16,
    pub verify_path: String,
    pub verify_contains: String,
    /// `http` unless the service speaks TLS on its own port. Vaultwarden and Portainer both do,
    /// and assuming http probed them with a plain-text request that could only ever fail.
    pub scheme: String,
    pub process_match: String,
    pub notes: String,
    /// Installed but normally not running. Still reported, still alerts on transitions, but kept
    /// out of uptime arithmetic — a service that is off on purpose would otherwise read 0% for ever.
    pub expected_off: bool,
}

impl Entry {
    pub fn is_http(&self) -> bool {
        self.probe == "http"
    }

    /// An HTTP entry without a body signature can only be trusted by status code.
    pub fn has_signature(&self) -> bool {
        self.is_http() && !self.verify_contains.is_empty()
    }
}

/// The catalog ships beside the binary in the repo and is embedded at build time, so a
/// downloaded binary needs no data files next to it.
const EMBEDDED: &str = include_str!("../../catalog/services.toml");

pub fn load(path: Option<&Path>) -> Result<Vec<Entry>, ConfigError> {
    let (source, label) = match path {
        Some(p) => (
            std::fs::read_to_string(p).map_err(|_| ConfigError(format!("catalog not found at {}", p.display())))?,
            p.display().to_string(),
        ),
        None => (EMBEDDED.to_string(), "<embedded>".to_string()),
    };
    parse(&source, &label)
}

fn parse(source: &str, label: &str) -> Result<Vec<Entry>, ConfigError> {
    let doc: toml::Table = source
        .parse::<toml::Table>()
        .map_err(|e| ConfigError(format!("{label}: {e}")))?;

    let rows: Vec<toml::Table> = match doc.get("catalog") {
        Some(toml::Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_table().cloned())
            .collect(),
        Some(toml::Value::Table(t)) => vec![t.clone()],
        _ => vec![],
    };

    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (i, r) in rows.iter().enumerate() {
        let get = |k: &str| -> String { r.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string() };
        let cid = get("id").trim().to_string();
        if cid.is_empty() {
            return Err(ConfigError(format!("{label}: catalog entry #{} has no id", i + 1)));
        }
        if !seen.insert(cid.clone()) {
            return Err(ConfigError(format!("{label}: duplicate catalog id {cid:?}")));
        }
        let probe = get("probe").trim().to_string();
        if !PROBES.contains(&probe.as_str()) {
            return Err(ConfigError(format!(
                "{label}: {cid}: probe {probe:?} is not one of {}",
                PROBES.join(", ")
            )));
        }
        let default_port = r.get("default_port").and_then(|v| v.as_integer()).unwrap_or(0);
        if (probe == "http" || probe == "tcp") && default_port == 0 {
            return Err(ConfigError(format!("{label}: {cid}: probe {probe} needs a default_port")));
        }
        // An HTTP entry without a body signature can only be trusted by status code, and a status
        // code is not an identity: macOS AirPlay answers 403 on port 5000, which a signature-less
        // Frigate entry read as "Frigate is degraded". Refusing the entry is the fix.
        let verify_contains = get("verify_contains").trim().to_string();
        if probe == "http" && verify_contains.is_empty() {
            return Err(ConfigError(format!(
                "{label}: {cid}: an http entry needs verify_contains — without it any listener on \
                 port {} would be mistaken for {cid}",
                default_port
            )));
        }
        let name = {
            let n = get("name");
            if n.is_empty() { cid.clone() } else { n }
        };
        let group = {
            let g = get("group");
            if g.is_empty() { "Services".to_string() } else { g }
        };
        entries.push(Entry {
            id: cid,
            name,
            group,
            probe,
            default_port: default_port.max(0) as u16,
            verify_path: get("verify_path"),
            verify_contains,
            process_match: get("process_match"),
            notes: get("notes"),
            scheme: r
                .get("scheme")
                .and_then(|v| v.as_str())
                .unwrap_or("http")
                .to_string(),
            expected_off: r.get("expected_off").and_then(|v| v.as_bool()).unwrap_or(false),
        });
    }
    entries.sort_by(|a, b| a.group.cmp(&b.group).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(entries)
}

pub fn by_id<'a>(entries: &'a [Entry], cid: &str) -> Option<&'a Entry> {
    entries.iter().find(|e| e.id == cid)
}

/// Build a runnable Service from a catalog entry.
pub fn instantiate(entry: &Entry, host: &str, port: Option<u16>, instance: &str) -> Result<Service, ConfigError> {
    let sid = if instance.is_empty() {
        entry.id.clone()
    } else {
        format!("{}-{}", entry.id, instance)
    };
    match entry.probe.as_str() {
        "http" => {
            let p = port.unwrap_or(entry.default_port);
            Ok(Service {
                id: sid,
                probe: "http".into(),
                target: format!(
            "{}://{host}:{p}{}",
            if entry.scheme.is_empty() { "http" } else { &entry.scheme },
            entry.verify_path
        ),
                name: entry.name.clone(),
                group: entry.group.clone(),
                contains: entry.verify_contains.clone(),
                ports: p.to_string(),
                ..Default::default()
            })
        }
        "tcp" => {
            let p = port.unwrap_or(entry.default_port);
            Ok(Service {
                id: sid,
                probe: "tcp".into(),
                target: format!("{host}:{p}"),
                name: entry.name.clone(),
                group: entry.group.clone(),
                ports: p.to_string(),
                ..Default::default()
            })
        }
        "process" => Ok(Service {
            id: sid,
            probe: "process".into(),
            target: if entry.process_match.is_empty() { entry.id.clone() } else { entry.process_match.clone() },
            name: entry.name.clone(),
            group: entry.group.clone(),
            ..Default::default()
        }),
        other => Err(ConfigError(format!("{}: probe {other:?} cannot be instantiated from a catalog entry", entry.id))),
    }
}

/// Render a Service as the `[[service]]` block a user would paste into config.toml.
pub fn to_toml(service: &Service) -> String {
    let mut lines = vec![
        "[[service]]".to_string(),
        format!("id = \"{}\"", service.id),
        format!("name = \"{}\"", service.name),
        format!("group = \"{}\"", service.group),
        format!("probe = \"{}\"", service.probe),
        format!("target = \"{}\"", service.target),
    ];
    if !service.ports.is_empty() {
        lines.push(format!("ports = \"{}\"", service.ports));
    }
    if !service.contains.is_empty() {
        lines.push(format!("contains = \"{}\"", service.contains));
    }
    if service.max_age_s != 0 {
        lines.push(format!("max_age_s = {}", service.max_age_s));
    }
    if !service.field.is_empty() {
        lines.push(format!("field = \"{}\"", service.field));
    }
    lines.join("\n")
}

/// Where a fallback on-disk catalog would live, for debugging a custom build.
pub fn default_path() -> PathBuf {
    PathBuf::from("catalog/services.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_http_entry_carries_a_signature() {
        // The invariant that makes discovery trustworthy. Before this was enforced, 16 of 50 entries
        // shipped with an empty signature and port 5000 (macOS AirPlay) was reported as Frigate.
        let entries = load(None).expect("embedded catalog must parse");
        let unsigned: Vec<&str> = entries
            .iter()
            .filter(|e| e.is_http() && !e.has_signature())
            .map(|e| e.id.as_str())
            .collect();
        assert!(unsigned.is_empty(), "http entries without a signature: {unsigned:?}");
    }

    #[test]
    fn an_unsigned_http_entry_is_refused_at_load() {
        let bad = r#"
[[catalog]]
id = "frigate"
name = "Frigate"
group = "Cameras"
probe = "http"
default_port = 5000
verify_path = "/api/version"
verify_contains = ""
"#;
        let err = parse(bad, "<test>").unwrap_err().to_string();
        assert!(err.contains("verify_contains"), "got: {err}");
    }

    #[test]
    fn the_shipped_catalog_is_the_arena_not_a_speculation() {
        // Deliberately small. A private script or a project only this machine runs does not belong
        // here; neither does software nobody in the fleet has installed.
        let entries = load(None).unwrap();
        assert!(!entries.is_empty(), "catalog must not be empty");
        assert!(entries.len() <= 40, "catalog drifted back into speculation: {}", entries.len());
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        for expected in ["9router", "syncthing", "v2raya", "ntfy", "vocechat", "searxng", "vaultwarden", "docker", "ssh"] {
            assert!(ids.contains(&expected), "fleet service {expected} is missing from the catalog");
        }
        // Absence is a feature: these were all speculative.
        for gone in ["grafana", "prometheus", "postgres", "jellyfin", "frigate"] {
            assert!(!ids.contains(&gone), "{gone} is not run anywhere and should not be catalogued");
        }
    }

    #[test]
    fn expected_off_survives_the_round_trip() {
        let entries = load(None).unwrap();
        let lm = by_id(&entries, "lmstudio").expect("lmstudio entry");
        assert!(lm.expected_off, "lmstudio is off on purpose and must say so");
        let syn = by_id(&entries, "syncthing").expect("syncthing entry");
        assert!(!syn.expected_off, "syncthing runs continuously");
    }

    #[test]
    fn instantiate_builds_expected_targets() {
        let entries = load(None).unwrap();
        let e = by_id(&entries, "syncthing").expect("syncthing entry");
        let svc = instantiate(e, "127.0.0.1", None, "").unwrap();
        assert!(svc.target.starts_with("http://127.0.0.1:8384"), "got {}", svc.target);

        let svc2 = instantiate(e, "10.0.0.5", Some(9999), "2").unwrap();
        assert_eq!(svc2.id, "syncthing-2");
        assert!(svc2.target.contains(":9999"));
        assert_eq!(svc2.ports, "9999");
    }

    #[test]
    fn to_toml_round_trips_through_the_config_parser() {
        let entries = load(None).unwrap();
        let e = by_id(&entries, "nginx").unwrap();
        let svc = instantiate(e, "127.0.0.1", None, "").unwrap();
        let block = to_toml(&svc);
        let parsed: toml::Table = block.parse().expect("generated block must be valid TOML");
        let service = parsed.get("service").and_then(|v| v.as_array()).and_then(|a| a.first());
        assert!(service.is_some(), "generated block did not produce a [[service]] row");
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let bad = "[[catalog]]\nid=\"a\"\nprobe=\"http\"\ndefault_port=1\n[[catalog]]\nid=\"a\"\nprobe=\"http\"\ndefault_port=2\n";
        assert!(parse(bad, "test").is_err());
    }

    #[test]
    fn http_entry_needs_a_port() {
        let bad = "[[catalog]]\nid=\"a\"\nprobe=\"http\"\n";
        assert!(parse(bad, "test").is_err());
    }
}
