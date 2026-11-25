//! Tail mode: probe locally, post the result to a head.
//!
//! A tail keeps no server of its own. It runs the same probe engine the head uses, then reports
//! what it observed. The device names itself: `node.name` comes from the machine, never from a
//! config field, because a hardcoded name goes stale the moment a host is renamed and lies about
//! which machine you are looking at.
//!
//! Nothing here buffers. A tail with no route to the head loses those samples; v0.1 has no
//! store-and-forward, and pretending otherwise would mean a queue nobody drains.

use crate::config::Config;
use crate::db;
use crate::metrics;
use crate::poller;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct TailOptions {
    /// Where reports go, e.g. `https://head:8770`.
    pub report_to: String,
    /// Bearer token for this node.
    pub token: String,
}

/// One report body, exactly the shape `POST /api/report` parses.
pub fn build_report(cfg: &Config, rows: &[Value], observed_at: i64) -> Value {
    let card = metrics::device_card(&cfg.node_id);
    // The device names itself, always: the hostname is what this machine calls itself right now.
    // A config field would go stale on rename and then the head would show a name that is a lie.
    let name = crate::config::hostname();

    json!({
        "node": {
            "id": cfg.node_id,
            "name": name,
            "interval": cfg.poll_interval,
        },
        "observed_at": observed_at,
        "metrics": card,
        "services": rows.iter().map(|r| {
            json!({
                "id": r["id"],
                "name": r["name"],
                "group": r["group"],
                "status": r["status"],
                "detail": r["detail"],
                "latency_ms": r["latency_ms"],
                "ram_mb": r["ram_mb"],
                "ports": r["ports"],
                "address": r["address"],
            })
        }).collect::<Vec<_>>(),
    })
}

/// Probe once and report once, resolving the token from the configured environment variable.
///
/// Both roles use this: a pure tail from its own loop, and a head from inside its poll wave. That
/// is the whole point of one binary — the reporting code is not a second program, it is a call.
pub fn report_once(cfg: &Config, head: &str, rows: &[Value], observed_at: i64) -> Result<Value, String> {
    let opts = TailOptions { report_to: head.to_string(), token: cfg.token()? };
    post(&opts, &build_report(cfg, rows, observed_at))
}

/// Post one report. Returns the head's answer or a readable failure.
pub fn post(opts: &TailOptions, body: &Value) -> Result<Value, String> {
    let base = opts.report_to.trim_end_matches('/');
    if base.is_empty() {
        return Err("report_to is empty".into());
    }
    // A token that reaches the head through the proxy would be reported on by the thing it is
    // monitoring, so the reporting path never uses a proxy.
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .try_proxy_from_env(false)
        .build();
    let resp = agent
        .post(&format!("{base}/api/report"))
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {}", opts.token))
        .set("User-Agent", "observer-frog/0.1")
        .send_string(&body.to_string());

    match resp {
        Ok(r) => {
            let code = r.status();
            let text = r.into_string().unwrap_or_default();
            match serde_json::from_str::<Value>(&text) {
                Ok(v) => Ok(v),
                Err(_) => Err(format!("HTTP {code}: {text}")),
            }
        }
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            Err(match code {
                401 => "401 unauthorised: the head does not know this token".to_string(),
                403 => format!("403 rejected: {text}"),
                413 => "413 report too large".to_string(),
                _ => format!("HTTP {code}: {text}"),
            })
        }
        Err(e) => Err(format!("cannot reach {}: {e}", opts.report_to)),
    }
}

/// Run the tail loop until stopped.
pub fn run(mut cfg: Config, opts: TailOptions, halt: Arc<AtomicBool>) -> i32 {
    // Ask the machine what it is running rather than trusting a hand-written list. Discovery
    // only ever adds services the catalog can verify, so a stale entry cannot appear here.
    if cfg.discover && cfg.services.is_empty() {
        match crate::catalog::load(None) {
            Ok(entries) => {
                let found = crate::discover::discover(&entries, "127.0.0.1");
                if found.is_empty() {
                    println!("  discovered nothing; no catalog service is listening here");
                } else {
                    println!("  discovered {} service(s) on this machine:", found.len());
                    for s in &found {
                        println!("    {:<16} {}", s.id, s.target);
                    }
                }
                cfg.services = found;
            }
            Err(e) => eprintln!("  cannot load the catalog, no discovery: {e}"),
        }
    }

    println!("observer-frog tail");
    println!("  node {} ({})", cfg.node_id, crate::config::hostname());
    println!("  {} service(s), reporting every {}s", cfg.services.len(), cfg.poll_interval);
    println!("  head {}", opts.report_to);

    let mut consecutive_failures = 0u32;
    let mut last_error = String::new();

    while !halt.load(Ordering::SeqCst) {
        let started = Instant::now();
        let rows = poller::sample(&cfg.services, None);
        let observed_at = db::now();
        let body = build_report(&cfg, &rows, observed_at);

        match post(&opts, &body) {
            Ok(answer) => {
                if consecutive_failures > 0 {
                    println!("head reachable again after {consecutive_failures} failure(s)");
                    consecutive_failures = 0;
                }
                // A silent success is right for a loop that runs every 30s; only log changes.
                if std::env::var("OF_VERBOSE").is_ok() {
                    println!(
                        "reported {} service(s) at {observed_at}: {}",
                        rows.len(),
                        answer.get("ok").and_then(|v| v.as_bool()).unwrap_or(false)
                    );
                }
            }
            Err(e) => {
                consecutive_failures += 1;
                // Log the first failure and then every tenth: a head that is down for an hour
                // would otherwise fill the log with the same line.
                if consecutive_failures == 1 || consecutive_failures % 10 == 0 {
                    let secs = cfg.poll_interval as i64 * consecutive_failures as i64;
                    println!(
                        "report failed ({consecutive_failures} in a row, ~{secs}s of samples lost): {e}"
                    );
                    last_error = e;
                }
            }
        }

        let elapsed = started.elapsed().as_secs_f64();
        let sleep_for = (cfg.poll_interval as f64 - elapsed).max(0.0);
        let mut left = sleep_for;
        while left > 0.0 && !halt.load(Ordering::SeqCst) {
            let slice = left.min(0.25);
            std::thread::sleep(Duration::from_secs_f64(slice));
            left -= slice;
        }
    }
    if !last_error.is_empty() {
        println!("stopped; last error was: {last_error}");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Service};

    fn cfg_with(services: Vec<Service>) -> Config {
        Config {
            path: None,
            node_id: "tail-test".into(),
            node_name: String::new(),
            serve: false,
            bind: "127.0.0.1".into(),
            port: 8770,
            poll_interval: 30,
            db_path: std::path::PathBuf::from("/tmp/of-tail-test.db"),
            discover: false,
            report_port: None,
            report_to: None,
            token_env: String::new(),
            token_file: None,
            services,
            sinks: vec![],
        }
    }

    #[test]
    fn the_report_names_the_device_itself() {
        let cfg = cfg_with(vec![]);
        let r = build_report(&cfg, &[], 1_000);
        let name = r["node"]["name"].as_str().unwrap_or("");
        assert!(!name.is_empty(), "a node must name itself, not leave it blank");
        // It must be this machine's own hostname, not a config value.
        assert_eq!(name, crate::config::hostname());
    }

    #[test]
    fn the_report_carries_metrics_even_with_no_services() {
        // The payload must never be empty: a node with nothing to monitor still proves it is alive.
        let cfg = cfg_with(vec![]);
        let r = build_report(&cfg, &[], 1_000);
        assert!(r["metrics"].is_object(), "metrics must ride every report");
        assert_eq!(r["services"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn the_report_shape_matches_what_ingest_parses() {
        let cfg = cfg_with(vec![]);
        let rows = vec![serde_json::json!({
            "id": "nginx", "name": "Nginx", "group": "Web",
            "status": "up", "detail": "HTTP 200", "latency_ms": 3
        })];
        let r = build_report(&cfg, &rows, 2_000);
        let bytes = serde_json::to_vec(&r).unwrap();
        let parsed = crate::ingest::parse(&bytes).expect("ingest must accept a tail's own report");
        assert_eq!(parsed.node_id, "tail-test");
        assert_eq!(parsed.interval_s, 30);
        assert_eq!(parsed.services.unwrap()[0].id, "nginx");
    }

    #[test]
    fn posting_with_no_head_fails_readably() {
        let opts = TailOptions { report_to: String::new(), token: "x".into() };
        let err = post(&opts, &serde_json::json!({})).unwrap_err();
        assert!(err.contains("report_to is empty"), "got {err}");
    }
}
