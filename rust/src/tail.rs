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

/// The same report, classified. Used by the poll wave, which has to tell an outage from a refusal.
pub fn report_outcome(cfg: &Config, head: &str, rows: &[Value], observed_at: i64) -> Result<Outcome, String> {
    let opts = TailOptions { report_to: head.to_string(), token: cfg.token()? };
    Ok(post_outcome(&opts, &build_report(cfg, rows, observed_at)))
}

/// What a report attempt told us about the head.
///
/// Three verdicts, because they need three different responses. `Answered` is the one that is easy to
/// get wrong: a 401 means the head is alive and refusing this machine, which is a fault the operator
/// fixes by pairing, not an outage. Matching on the prose of an error string cannot express that
/// distinction safely, which is why this is a type.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// 200. The head applied the report.
    Up(Value),
    /// The head answered with a status that is not 200. It is alive; it is not accepting this report.
    /// The name travels with it, because an unpaired tail learns the head's name from its 401 and
    /// needs it for the alert it is about to write.
    Answered { code: u16, why: String, head_name: Option<String> },
    /// Nothing answered: timeout, refused, DNS. The head may be dead or the network may be.
    Unreachable { why: String },
}

impl Outcome {
    pub fn is_up(&self) -> bool {
        matches!(self, Outcome::Up(_))
    }

    pub fn answered(&self) -> bool {
        matches!(self, Outcome::Answered { .. })
    }

    /// The reason, whatever the verdict, for a log line.
    pub fn why(&self) -> String {
        match self {
            Outcome::Up(_) => "ok".into(),
            Outcome::Answered { code, why, .. } => format!("{code}: {why}"),
            Outcome::Unreachable { why } => why.clone(),
        }
    }

    /// The head's own name, when the answer carried one. Both a success and a refusal do, which is
    /// how a tail learns the name before it needs to use it.
    pub fn head_name(&self) -> Option<&str> {
        match self {
            Outcome::Up(v) => v.get("head_name").and_then(|n| n.as_str()),
            Outcome::Answered { head_name, .. } => head_name.as_deref(),
            Outcome::Unreachable { .. } => None,
        }
    }

    /// The status the head answered with, when it answered.
    pub fn code(&self) -> Option<u16> {
        match self {
            Outcome::Answered { code, .. } => Some(*code),
            _ => None,
        }
    }
}

/// Post one report and classify what came back.
pub fn post_outcome(opts: &TailOptions, body: &Value) -> Outcome {
    match post_raw(opts, body) {
        Ok(v) => {
            // A 200 whose body is not JSON still means the head answered and applied it; the answer
            // is only ever used for a verbose log line.
            Outcome::Up(v)
        }
        Err((Some(code), why, raw)) => {
            // The name rides on refusals too, so a tail that has never paired still learns it.
            let head_name = serde_json::from_str::<Value>(&raw)
                .ok()
                .and_then(|v| v.get("head_name").and_then(|n| n.as_str()).map(|s| s.to_string()));
            Outcome::Answered { code, why, head_name }
        }
        Err((None, why, _)) => Outcome::Unreachable { why },
    }
}

/// Post one report. Returns the head's answer or a readable failure.
pub fn post(opts: &TailOptions, body: &Value) -> Result<Value, String> {
    post_raw(opts, body).map_err(|(_, why, _)| why)
}

/// The same post, but keeping the status code so a caller can tell "answered and refused" from
/// "nothing answered" without inspecting the message. `None` means the request never completed.
fn post_raw(opts: &TailOptions, body: &Value) -> Result<Value, (Option<u16>, String, String)> {
    let base = opts.report_to.trim_end_matches('/');
    if base.is_empty() {
        return Err((None, "report_to is empty".into(), String::new()));
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
            let text = r.into_string().unwrap_or_default();
            match serde_json::from_str::<Value>(&text) {
                Ok(v) => Ok(v),
                // Answered, so still a life sign; the body is only read for the verbose log.
                Err(_) => Ok(serde_json::json!({ "ok": true })),
            }
        }
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            let raw = text.clone();
            let why = match code {
                // The wording is kept for the log and for the operator. Nothing decides anything by
                // reading it: `Outcome::Answered { code, .. }` is what a caller tests.
                401 => "unknown token; this head has no enrolment for this machine".to_string(),
                503 => format!("the head is not accepting reports: {text}"),
                403 => format!("rejected: {text}"),
                413 => "report too large".to_string(),
                _ => text,
            };
            Err((Some(code), why, raw))
        }
        Err(e) => Err((None, format!("cannot reach {}: {e}", opts.report_to), String::new())),
    }
}

/// Run the tail loop until stopped.
pub fn run(mut cfg: Config, opts: TailOptions, halt: Arc<AtomicBool>) -> i32 {
    // Same merge the served path uses — see `config::merge_discovery` for why it is shared.
    let before = cfg.services.len();
    cfg.services = crate::config::merge_discovery(cfg.services, cfg.discover);
    let added = cfg.services.len().saturating_sub(before);
    if added > 0 {
        for s in &cfg.services[before..] {
            println!("    discovered {:<16} {}", s.id, s.target);
        }
        println!("  discovered {added} service(s) on this machine");
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
            head_name: None,
            tls_cert: None,
            tls_key: None,
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
        // The verdicts a caller can act on, asserted without a server: the classification is a
        // function of the status code, never of the wording of the message.
        let refused = Outcome::Answered { code: 401, why: "unknown token".into(), head_name: None };
        assert!(refused.answered() && !refused.is_up());
        assert_eq!(refused.code(), Some(401));
        let silent = Outcome::Unreachable { why: "cannot reach".into() };
        assert!(!silent.answered() && !silent.is_up());
        assert_eq!(silent.code(), None, "no answer means no status");
        let ok = Outcome::Up(serde_json::json!({}));
        assert!(ok.is_up() && !ok.answered());

        let err = post(&opts, &serde_json::json!({})).unwrap_err();
        assert!(err.contains("report_to is empty"), "got {err}");
    }
}
