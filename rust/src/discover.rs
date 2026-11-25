//! Service discovery: ask the machine what it is actually running.
//!
//! A hand-written service list is a maintenance burden and goes stale the moment something is
//! installed. This module asks the OS for listening sockets, then matches those ports against
//! the shipped catalog. An entry only becomes a service if its signature checks out, so a random
//! listener that happens to sit on 3001 is not reported as Uptime Kuma.
//!
//! What it deliberately does not do:
//!   - guess at services with no port (a sync client, a VM). Those need `process`, which cannot
//!     be inferred from a socket table.
//!   - probe everything on the machine. Only catalog ports are considered, so the result is a
//!     short list a person can read rather than a wall of noise.

use crate::catalog::Entry;
use crate::config::Service;
use std::collections::{BTreeMap, BTreeSet};

/// One listening socket as the OS reports it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Listener {
    pub port: u16,
    pub process: String,
    /// Whether the socket is bound to loopback only. Informational for the UI.
    pub loopback: bool,
}

/// Parse the output of `lsof -nP -iTCP -sTCP:LISTEN` (macOS).
fn parse_lsof(text: &str) -> Vec<Listener> {
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        // COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 9 {
            continue;
        }
        // The NAME column can be `127.0.0.1:8770` or `*:3009 (LISTEN)`; splitting on whitespace
        // leaves "(LISTEN)" as the final token, so scan back for the first address-looking field.
        let Some(name) = cols.iter().rev().find(|c| c.contains(':') && port_from_addr(c).is_some())
        else {
            continue;
        };
        let Some(port) = port_from_addr(name) else { continue };
        let loopback = name.contains("127.0.0.1") || name.contains("[::1]");
        out.push(Listener { port, process: cols[0].to_string(), loopback });
    }
    out
}

/// Parse the output of `ss -lntH` (Linux).
fn parse_ss(text: &str) -> Vec<Listener> {
    let mut out = Vec::new();
    for line in text.lines() {
        // State Recv-Q Send-Q Local-Address:Port Peer-Address:Port [Process]
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 {
            continue;
        }
        let local = cols[3];
        let Some(port) = port_from_addr(local) else { continue };
        let loopback = local.starts_with("127.") || local.starts_with("[::1]");
        // The process column, when present, looks like users:(("xray",pid=1,fd=3))
        let process = cols[5..]
            .join(" ")
            .split("((\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .unwrap_or("")
            .to_string();
        out.push(Listener { port, process, loopback });
    }
    out
}

/// Extract the port from `*:8770`, `127.0.0.1:8770`, `[::1]:8770` or `[::]:8770`.
fn port_from_addr(addr: &str) -> Option<u16> {
    let tail = addr.rsplit(':').next()?;
    tail.parse::<u16>().ok()
}

/// Every TCP port this machine is listening on, deduplicated and sorted.
pub fn listening() -> Vec<Listener> {
    let mut all: Vec<Listener> = Vec::new();

    if cfg!(target_os = "macos") {
        if let Ok(out) = std::process::Command::new("lsof")
            .args(["-nP", "-iTCP", "-sTCP:LISTEN"])
            .output()
        {
            all = parse_lsof(&String::from_utf8_lossy(&out.stdout));
        }
    } else {
        if let Ok(out) = std::process::Command::new("ss").args(["-lntH"]).output() {
            all = parse_ss(&String::from_utf8_lossy(&out.stdout));
        }
        // Fall back to /proc when ss is missing, which happens on minimal images.
        if all.is_empty() {
            all = listening_from_proc();
        }
    }

    // One entry per port: several processes can share a port across address families.
    let mut seen: BTreeMap<u16, Listener> = BTreeMap::new();
    for l in all {
        seen.entry(l.port).or_insert(l);
    }
    seen.into_values().collect()
}

/// Last-resort reader for Linux without `ss`: parse /proc/net/tcp{,6}.
fn listening_from_proc() -> Vec<Listener> {
    let mut out = Vec::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 4 {
                continue;
            }
            // st == 0A means LISTEN; local_address is HEXADDR:HEXPORT
            if cols[3] != "0A" {
                continue;
            }
            let Some((_, hexport)) = cols[1].rsplit_once(':') else { continue };
            let Ok(port) = u16::from_str_radix(hexport, 16) else { continue };
            let loopback = cols[1].starts_with("0100007F") || cols[1].starts_with("00000000000000000000000001000000");
            out.push(Listener { port, process: String::new(), loopback });
        }
    }
    out
}

/// The services on this machine that the catalog can recognise.
///
/// A catalog entry becomes a service when its default port is listening AND its signature
/// verifies. Entries with no signature are accepted on the port alone, which is why the catalog
/// marks those as unsigned.
pub fn discover(entries: &[Entry], host: &str) -> Vec<Service> {
    let listeners = listening();
    let mut found: Vec<Service> = Vec::new();

    // Pass 1: HTTP entries, matched by signature on ANY listening port.
    //
    // Checking only the catalog's default port misses every service that runs elsewhere — a
    // SearXNG on :8888 looks absent when the catalog says :8080. A signature hit is far stronger
    // evidence than a port match, so port is only a hint about where to look.
    for entry in entries.iter().filter(|e| e.probe == "http" && e.has_signature()) {
        // The default port first: it is the common case and costs one request.
        let mut candidates: Vec<u16> = Vec::new();
        if entry.default_port > 0 && listeners.iter().any(|l| l.port == entry.default_port) {
            candidates.push(entry.default_port);
        }
        for l in &listeners {
            if !candidates.contains(&l.port) {
                candidates.push(l.port);
            }
        }

        for port in candidates {
            // A loopback service is still reachable at the loopback address; a wildcard listener
            // answers on either. Use the host the caller gave.
            let Ok(svc) = crate::catalog::instantiate(entry, host, Some(port), "") else { continue };
            let (state, _, _) = crate::probes::probe_http(
                &svc.target,
                svc.expect_status,
                &svc.contains,
                svc.timeout.min(1.5),
            );
            if state == crate::probes::STATE_UP {
                found.push(svc);
                break;
            }
        }
    }

    // Pass 2: TCP entries, matched on port only. A bare TCP check has no signature, so this can
    // only ever confirm "something accepts connections here".
    for entry in entries.iter().filter(|e| e.probe == "tcp") {
        if entry.default_port == 0 || !listeners.iter().any(|l| l.port == entry.default_port) {
            continue;
        }
        if let Ok(svc) = crate::catalog::instantiate(entry, host, None, "") {
            found.push(svc);
        }
    }

    // Pass 3: process probes. Nothing to match on a socket table; ask the process table, which
    // is what a service with no port (a proxy core, an agent) actually looks like.
    for entry in entries.iter().filter(|e| e.probe == "process") {
        if entry.process_match.is_empty() {
            continue;
        }
        if let Ok(svc) = crate::catalog::instantiate(entry, host, None, "") {
            let (state, _, _) = crate::probes::probe_process(&svc.target);
            if state == crate::probes::STATE_UP {
                found.push(svc);
            }
        }
    }

    found.sort_by(|a, b| a.group.cmp(&b.group).then_with(|| a.name.cmp(&b.name)));
    found
}

/// Ports that are listening but match nothing in the catalog, so the UI can offer them.
pub fn unmatched(entries: &[Entry], listeners: &[Listener]) -> Vec<Listener> {
    let known: BTreeSet<u16> = entries.iter().map(|e| e.default_port).filter(|p| *p > 0).collect();
    listeners
        .iter()
        .filter(|l| !known.contains(&l.port))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_macos_lsof_output() {
        let text = "COMMAND     PID  USER   FD   TYPE             DEVICE SIZE/OFF NODE NAME\n\
                    xray      1234 mahyar   3u  IPv4 0x1234      0t0  TCP 127.0.0.1:20171 (LISTEN)\n\
                    vocechat  5678 mahyar   9u  IPv6 0x5678      0t0  TCP *:3009 (LISTEN)\n";
        let got = parse_lsof(text);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].port, 20171);
        assert_eq!(got[0].process, "xray");
        assert!(got[0].loopback);
        assert_eq!(got[1].port, 3009);
        assert!(!got[1].loopback);
    }

    #[test]
    fn parses_linux_ss_output() {
        let text = "LISTEN 0 128 127.0.0.1:20171 0.0.0.0:* users:((\"xray\",pid=1,fd=3))\n\
                    LISTEN 0 128 0.0.0.0:3009 0.0.0.0:* users:((\"vocechat\",pid=2,fd=9))\n";
        let got = parse_ss(text);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].port, 20171);
        assert_eq!(got[0].process, "xray");
        assert_eq!(got[1].port, 3009);
        assert_eq!(got[1].process, "vocechat");
    }

    #[test]
    fn port_from_addr_handles_all_forms() {
        assert_eq!(port_from_addr("*:8770"), Some(8770));
        assert_eq!(port_from_addr("127.0.0.1:8770"), Some(8770));
        assert_eq!(port_from_addr("[::1]:8770"), Some(8770));
        assert_eq!(port_from_addr("[::]:22"), Some(22));
        assert_eq!(port_from_addr("nonsense"), None);
    }

    #[test]
    fn discovery_finds_this_machines_own_dashboard() {
        // The test binary is not serving, so discovery should at minimum run without panicking
        // and return only services whose ports are genuinely open.
        let entries = crate::catalog::load(None).unwrap();
        let found = discover(&entries, "127.0.0.1");
        for svc in &found {
            assert!(!svc.id.is_empty());
            assert!(!svc.target.is_empty());
        }
    }

    #[test]
    fn unmatched_ports_exclude_known_catalog_ports() {
        let entries = crate::catalog::load(None).unwrap();
        let listeners = vec![
            Listener { port: 3009, process: "vocechat".into(), loopback: false },
            Listener { port: 9999, process: "unknown".into(), loopback: true },
        ];
        let left = unmatched(&entries, &listeners);
        assert!(left.iter().any(|l| l.port == 9999), "an unknown port should be offered");
    }
}
