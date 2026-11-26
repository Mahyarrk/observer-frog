//! LAN discovery: find a head without typing its address.
//!
//! A tail that has just been installed does not know where the dashboard is, and asking the user
//! to copy an IP address is the step where most people stop. So the head answers a UDP broadcast
//! on the LAN and the tail lists whoever answers.
//!
//! Deliberately not mDNS: multicast is unreliable on the interfaces this project already targets
//! (ADB needs `ADB_MDNS=0` for the same reason), and a broadcast that needs a daemon is not a
//! feature, it is a dependency.
//!
//! **No credentials travel here.** An advertisement carries where the head is and what it calls
//! itself. The token is still read from a file or the environment, because a packet anyone on
//! the network can read must never carry one.

use serde_json::{json, Value};
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// Fixed, so neither side needs configuration. Registered with no authority; a collision is a
/// config error the user can see, not a silent failure.
pub const DISCOVERY_PORT: u16 = 8790;
const QUERY: &[u8] = b"observer-frog:who-is-head";

/// One head's answer.
pub struct Head {
    pub name: String,
    pub url: String,
    pub report_port: Option<u16>,
    pub version: String,
    pub from: SocketAddr,
}

impl Head {
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "url": self.url,
            "report_port": self.report_port,
            "version": self.version,
            "from": self.from.to_string(),
        })
    }
}

/// What the head sends back. Built by the same code that serves the dashboard, so the URL it
/// advertises is the one it is actually reachable on.
pub fn advertisement(name: &str, url: &str, report_port: Option<u16>, version: &str) -> Vec<u8> {
    json!({
        "observer_frog": "head",
        "name": name,
        "url": url,
        "report_port": report_port,
        "version": version,
    })
    .to_string()
    .into_bytes()
}

/// Parse an answer. Returns `None` for anything that is not one of ours, so a stray packet on the
/// port cannot make the tail believe it found a head.
/// Swap a wildcard host for the address the answer arrived from.
///
/// `bind = "0.0.0.0"` means "listen everywhere" and is advertised as-is, because that is the honest
/// description of what a node is listening on. But it is not an address anyone can dial: a client
/// told to report to `0.0.0.0:8771` reports to itself. The packet header already carries the sender's
/// address, and it is by definition one that reaches that machine — so the advertisement states the
/// port and the packet states the address.
fn reachable(url: String, from: SocketAddr) -> String {
    let wildcard_host = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .map(|rest| rest.split(':').next().unwrap_or(""))
        .unwrap_or("");
    if !matches!(wildcard_host, "0.0.0.0" | "[::]" | "::") {
        return url;
    }
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let port_part = rest.split_once(':').map(|(_, p)| p).unwrap_or("");
            format!("{scheme}://{}:{port_part}", from.ip())
        }
        None => url,
    }
}

pub fn parse_head(body: &[u8], from: SocketAddr) -> Option<Head> {
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return None,
    };
    if v.get("observer_frog").and_then(|x| x.as_str()) != Some("head") {
        return None;
    }
    let url = v.get("url").and_then(|x| x.as_str()).unwrap_or("").to_string();
    Some(Head {
        name: v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        url: reachable(url, from),
        report_port: v.get("report_port").and_then(|x| x.as_u64()).map(|p| p as u16),
        version: v.get("version").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        from,
    })
}

/// Answer head queries until `halt`. Runs on the head's own thread; a bind failure is reported and
/// the head carries on serving, because discovery is a convenience and not a reason to die.
pub fn serve_advertisement(
    name: String,
    url: String,
    report_port: Option<u16>,
    version: String,
    halt: std::sync::Arc<std::sync::atomic::AtomicBool>,
    still_head: impl Fn() -> bool + Send + 'static,
) {
    let sock = match UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("discovery: cannot listen on UDP {DISCOVERY_PORT}: {e} (tail discovery will not find this head)");
            return;
        }
    };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
    let mut buf = [0u8; 512];
    while !halt.load(std::sync::atomic::Ordering::SeqCst) {
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                // Only answer the exact query, so this port is not a general-purpose echo.
                if &buf[..n] == QUERY {
                    // Always answer: the dashboard port identifies the machine even when it is not
                    // collecting. Only the report port is conditional, and only that is gated on
                    // serving, so a tail is still findable but never looks like a head.
                    let rp = if still_head() { report_port } else { None };
                    let answer = advertisement(&name, &url, rp, &version);
                    let _ = sock.send_to(&answer, from);
                }
            }
            Err(_) => continue,
        }
    }
}

/// Broadcast once and collect answers for `window`.
pub fn find_heads(window: Duration) -> Vec<Head> {
    let sock = match UdpSocket::bind(("0.0.0.0", 0)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot open a socket for discovery: {e}");
            return vec![];
        }
    };
    let _ = sock.set_broadcast(true);
    let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));

    // Broadcast on the loopback as well, so a single-machine install finds itself and the test
    // path does not depend on network configuration.
    for addr in ["255.255.255.255:8790", "127.0.0.1:8790"] {
        let _ = sock.send_to(QUERY, addr);
    }

    let mut found: Vec<Head> = Vec::new();
    let mut buf = [0u8; 512];
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                if let Some(h) = parse_head(&buf[..n], from) {
                    // One answer per sender. Keyed on the responder's address, not its URL: every
                    // node advertises the same wildcard URL, so URL identity would treat two
                    // machines as one. The same machine answering twice (broadcast and loopback)
                    // still collapses to one entry.
                    if !found.iter().any(|x| x.from.ip() == h.from.ip()) {
                        found.push(h);
                    }
                }
            }
            Err(_) => continue,
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr() -> SocketAddr {
        "192.0.2.10:8790".parse().unwrap()
    }

    #[test]
    fn a_head_is_recognised_by_its_own_packet() {
        let body = advertisement("head-box", "http://192.0.2.10:8771", Some(8771), "0.1.0");
        let h = parse_head(&body, addr()).expect("its own advertisement must parse");
        assert_eq!(h.name, "head-box");
        assert_eq!(h.report_port, Some(8771));
    }

    #[test]
    fn a_wildcard_advertisement_becomes_the_senders_address() {
        // Every node binds 0.0.0.0 and advertises it. A client that dialled that would call itself.
        let body = advertisement("head-box", "http://0.0.0.0:8770/", Some(8771), "0.4.0");
        let from: SocketAddr = "192.168.2.105:8790".parse().unwrap();
        let h = parse_head(&body, from).unwrap();
        assert_eq!(h.url, "http://192.168.2.105:8770/");
        assert_eq!(h.report_port, Some(8771));

        // A real address is left alone, so a node that binds a specific interface keeps it.
        let body = advertisement("head-box", "http://10.0.0.5:8770/", Some(8771), "0.4.0");
        let h = parse_head(&body, from).unwrap();
        assert_eq!(h.url, "http://10.0.0.5:8770/");
    }

    #[test]
    fn a_stray_packet_is_not_a_head() {
        // A tail must never pair with something that merely shares the port.
        assert!(parse_head(b"hello", addr()).is_none());
        assert!(parse_head(br#"{"observer_frog":"tail"}"#, addr()).is_none());
    }

    #[test]
    fn the_advertisement_never_carries_a_secret() {
        let body = String::from_utf8(advertisement("n", "http://h:8771", Some(8771), "0.1.0")).unwrap();
        for forbidden in ["token", "bearer", "secret", "authorization"] {
            assert!(!body.to_lowercase().contains(forbidden), "advertisement leaked {forbidden}: {body}");
        }
    }
}
