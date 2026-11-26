//! Runtime wiring: poller thread + notifier + HTTP server, with self-monitoring last (I9).

use crate::config::{Config, Watcher};
use crate::notify::Notifier;
use crate::poller;
use crate::server::{self, App};
// Declared here so the poller closure can hold it: asking once per head is the difference
// between a pairing request and a flood of them.
use crate::silence;
use crate::state;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub fn init_logging(verbose: bool) {
    let level = if verbose { "debug" } else { "info" };
    let _ = std::env::var("RUST_LOG");
    // Minimal logger: timestamped lines, no crate needed.
    LOG_VERBOSE.store(verbose, Ordering::Relaxed);
    eprintln!("[observer-frog] log level: {level}");
}

static LOG_VERBOSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Called when a node is marked silent. Kept as a function so the event is greppable and a test
/// can assert it happened without scraping stdout.
pub fn note_silence(node_id: &str, services: usize) {
    log_line(&format!("node {node_id} silent: {services} service(s) unavailable"));
}

fn log_line(msg: &str) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    println!("{secs} {msg}");
}

/// Assemble the runtime without binding anything.
pub fn build(cfg: &Config, poller_ref: Arc<Mutex<Option<Arc<poller::Poller>>>>) -> Arc<App> {
    let _ = poller_ref;
    let mut topics = std::collections::HashMap::new();
    if let Some(first) = cfg.sinks.first() {
        topics.insert("ntfy".to_string(), first.topic.clone());
    }
    let notifier = Notifier::new(
        cfg.sinks.clone(),
        topics,
        &format!("http://{}:{}/", cfg.bind, cfg.port),
    );
    let app = Arc::new(App::new(cfg.clone(), cfg.poll_interval));
    *app.notifier.lock().unwrap() = Some(notifier);
    app
}

pub fn run(cfg: Config, config_path: Option<PathBuf>, tls: Option<(PathBuf, PathBuf)>) -> i32 {
    let watcher_holder: Arc<Mutex<Option<Watcher>>> = Arc::new(Mutex::new(
        Watcher::new(config_path.as_deref().or(cfg.path.as_deref())).ok(),
    ));
    let poller_holder: Arc<Mutex<Option<Arc<poller::Poller>>>> = Arc::new(Mutex::new(None));
    let app = build(&cfg, Arc::clone(&poller_holder));

    let port = match server::listen(Arc::clone(&app), &cfg.bind, cfg.port, tls.clone()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cannot bind {}:{}: {e}", cfg.bind, cfg.port);
            return 1;
        }
    };

    // A second, plain-HTTP listener for tail reports. Tails are LAN boxes that should not manage
    // certificates; the TLS listener above exists for a phone installing the PWA. Both share one
    // App, so a report written through this port is visible immediately over TLS.
    let mut report_bound: Option<u16> = None;
    if let Some(rp) = cfg.report_port {
        if rp == cfg.port {
            eprintln!("report_port {} is the same as port; the report listener is disabled", rp);
        } else {
            match server::listen(Arc::clone(&app), &cfg.bind, rp, None) {
                Ok(p) => report_bound = Some(p),
                Err(e) => eprintln!("cannot bind report port {}:{}: {e} (continuing without it)", cfg.bind, rp),
            }
        }
    }

    let scheme = if tls.is_some() { "https" } else { "http" };
    println!("observer-frog serving {scheme}://{}:{}/", cfg.bind, port);
    println!("  node {} ({})", cfg.node_id, cfg.node_name);
    println!("  {} service(s), poll every {}s", cfg.services.len(), cfg.poll_interval);
    println!("  database {}", cfg.db_path.display());
    if let Some(rp) = report_bound {
        let scheme = if tls.is_some() { "plain HTTP for tails" } else { "plain HTTP" };
        println!("  tail reports on {scheme} {}:{rp}/api/report", cfg.bind);
    }

    // Every node answers discovery with its dashboard port, whether or not it is collecting; only
    // the report port is conditional on serving. A scan therefore finds every machine, and the
    // answer's `report_port` is what says whether that machine will take reports.
    {
        let name = cfg.node_name.clone();
        let url = format!("{scheme}://{}:{}/", cfg.bind, port);
        let report_port = report_bound;
        let halt = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watcher_for_lan = Arc::clone(&watcher_holder);
        let boot_serve = cfg.serve;
        let boot_path = config_path.as_deref().or(cfg.path.as_deref()).map(|p| p.to_path_buf());
        std::thread::spawn(move || {
            let version = env!("CARGO_PKG_VERSION").to_string();
            // The predicate is what keeps this honest: the thread exists from boot, but it only
            // answers while the node is acting as head. Spawning it conditionally meant a node
            // that became head later never advertised until it was restarted, and one that stopped
            // being head advertised for ever.
            let still_head = move || match (&boot_path, watcher_for_lan.lock().unwrap().as_mut()) {
                (_, Some(w)) => w.refresh().serve,
                (Some(path), None) => crate::config::load(Some(path)).map(|c| c.serve).unwrap_or(boot_serve),
                (None, None) => boot_serve,
            };
            crate::lan::serve_advertisement(name, url, report_port, version, halt, still_head);
        });
    }

    // Self-monitoring begins only now that the socket is bound (invariant I9).
    let p = Arc::new(poller::Poller::new());
    *poller_holder.lock().unwrap() = Some(Arc::clone(&p));

    let sweeper_halt = p.halt_flag();
    let asked_to_pair: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    {
        let app = Arc::clone(&app);
        let watcher_holder = Arc::clone(&watcher_holder);
        let halt = p.halt_flag();
        let asked_to_pair = Arc::clone(&asked_to_pair);
        std::thread::spawn(move || {
            while !halt.load(Ordering::SeqCst) {
                let cfg_now = {
                    let mut guard = watcher_holder.lock().unwrap();
                    match guard.as_mut() {
                        Some(w) => w.refresh().clone(),
                        None => app.cfg.clone(),
                    }
                };
                let interval = cfg_now.poll_interval.max(1);
                let started = Instant::now();

                let rows = poller::sample(&cfg_now.services, None);
                let ts = crate::db::now();
                let state_rows: Vec<state::Row> = rows
                    .iter()
                    .map(|r| state::Row {
                        service_id: r["id"].as_str().unwrap_or("").to_string(),
                        name: r["name"].as_str().unwrap_or("").to_string(),
                        group: r["group"].as_str().unwrap_or("").to_string(),
                        status: r["status"].as_str().unwrap_or("down").to_string(),
                        detail: r["detail"].as_str().unwrap_or("").to_string(),
                        latency_ms: r["latency_ms"].as_i64().unwrap_or(0),
                        ram_mb: r["ram_mb"].as_f64().unwrap_or(0.0),
                        ports: r["ports"].as_str().unwrap_or("").to_string(),
                        address: r["address"].as_str().unwrap_or("").to_string(),
                    })
                    .collect();

                match state::apply_wave(&cfg_now.db_path, &cfg_now.node_id, &state_rows, Some(ts)) {
                    Ok(changes) => {
                        app.waves.fetch_add(1, Ordering::Relaxed);
                        *app.last_wave_error.lock().unwrap() = None;
                        app.set_snapshot(rows.clone(), ts);
                        if !changes.is_empty() {
                            if let Some(n) = app.notifier.lock().unwrap().as_mut() {
                                n.submit(&changes);
                                n.flush(None);
                            }
                        }
                        // One binary, both roles: this same wave is what gets reported. A separate
                        // reporter thread would probe every service a second time for nothing.
                        // Not acting as a tail, or no head paired: skip quietly. Silence here is a
                        // configured state the UI explains, not a failure to log every wave.
                        let wants_to_report = cfg_now.report_to.is_some() && cfg_now.token().is_ok();
                        if let Some(head) = cfg_now.report_to.clone().filter(|_| wants_to_report) {
                            match crate::tail::report_once(&cfg_now, &head, &rows, ts) {
                                Ok(_) => {
                                    app.reports_ok.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(e) => {
                                    app.reports_failed.fetch_add(1, Ordering::Relaxed);
                                    *app.last_report_error.lock().unwrap() = Some(e.clone());
                                    log_line(&format!("report to {head} failed: {e}"));
                                    // A head that does not know this machine is the pairing case.
                                    // Ask once, then keep reporting quietly: a tail that re-asked
                                    // every wave would fill the head's list with duplicates, and a
                                    // person still has to click approve either way.
                                    if e.contains("unknown token") && !*asked_to_pair.lock().unwrap() {
                                        *asked_to_pair.lock().unwrap() = true;
                                        let ask = cfg_now.token().map(|t| {
                                            crate::pair::ask_head(&head, &cfg_now.node_id, &crate::config::hostname(), &t, cfg_now.poll_interval as i64)
                                        });
                                        match ask {
                                            Ok(Ok(msg)) => log_line(&format!("pairing: {msg}")),
                                            Ok(Err(why)) => log_line(&format!("pairing request failed: {why}")),
                                            Err(why) => log_line(&format!("cannot ask to pair: {why}")),
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        app.errors.fetch_add(1, Ordering::Relaxed);
                        *app.last_wave_error.lock().unwrap() = Some(e.to_string());
                        log_line(&format!("wave failed: {e}"));
                    }
                }

                let elapsed = started.elapsed().as_secs_f64();
                let budget = interval as f64 * poller::WAVE_BUDGET_FRACTION;
                if elapsed > budget {
                    app.dropped.fetch_add(1, Ordering::Relaxed);
                    log_line(&format!(
                        "wave took {elapsed:.1}s (budget {budget:.1}s): dropping this tick"
                    ));
                }
                let sleep_for = (interval as f64 - elapsed).max(0.0);
                // Sleep in small slices so a stop request is honoured promptly.
                let mut left = sleep_for;
                while left > 0.0 && !halt.load(Ordering::SeqCst) {
                    let slice = left.min(0.25);
                    std::thread::sleep(Duration::from_secs_f64(slice));
                    left -= slice;
                }
            }
        });
    }

    // Silence sweeper: always running. Whether this node *acts* as head is a switch the operator
    // flips in Settings, and a thread that only exists when the box was ticked at boot would need a
    // process restart to follow it — the restart being exactly the thing this design removes.
    // The check is per tick instead, so ticking the box takes effect on the next wave.
    {
        let app = Arc::clone(&app);
        let halt = Arc::clone(&sweeper_halt);
        std::thread::spawn(move || {
            // Sweep four times per shortest interval, floored at 5s, so detection lands within
            // the grace window rather than a whole interval late.
            let tick: f64 = 5.0;
            // Whether the roster may still hold other machines, so it is cleared exactly once
            // after the switch rather than on every tick. Starts true even for a node that boots
            // as a tail: it may have been a head before the last restart, and those rows are still
            // on disk waiting to clutter the node picker.
            let was_head = std::sync::atomic::AtomicBool::new(true);
            while !halt.load(Ordering::SeqCst) {
                // Acting as head is a setting, not an identity: read it every tick so the checkbox
                // takes effect within five seconds, with no restart and no unit rewrite.
                let acting_head = match watcher_holder.lock().unwrap().as_mut() {
                    Some(w) => w.refresh().serve,
                    None => app.cfg.serve,
                };
                // On the tick where this node stops acting as head, drop what it learned about
                // other machines. Doing it on the transition rather than every tick keeps a quiet
                // node cheap, and means the roster is cleared the moment the switch is flipped.
                if !acting_head && was_head.swap(false, Ordering::SeqCst) {
                    match crate::db::forget_other_nodes(&app.cfg.db_path, &app.cfg.node_id) {
                        Ok(n) if n > 0 => log_line(&format!(
                            "stopped acting as head: cleared {n} rows for other machines (keeping {})",
                            app.cfg.node_id
                        )),
                        Ok(_) => {}
                        Err(e) => log_line(&format!("could not clear the node roster: {e}")),
                    }
                } else if acting_head {
                    was_head.store(true, Ordering::SeqCst);
                }
                if !acting_head {
                    let mut left = tick;
                    while left > 0.0 && !halt.load(Ordering::SeqCst) {
                        let slice = left.min(0.25);
                        std::thread::sleep(Duration::from_secs_f64(slice));
                        left -= slice;
                    }
                    continue;
                }
                let now = crate::db::now();
                match silence::sweep(&app.cfg.db_path, now, &app.cfg.node_id) {
                    Ok(swept) => {
                        for (node_id, changes) in swept {
                            crate::runtime::note_silence(&node_id, changes.len());
                            if let Some(n) = app.notifier.lock().unwrap().as_mut() {
                                n.submit(&changes);
                                n.flush(None);
                            }
                        }
                    }
                    Err(e) => log_line(&format!("silence sweep failed: {e}")),
                }
                let mut left = tick;
                while left > 0.0 && !halt.load(Ordering::SeqCst) {
                    let slice = left.min(0.25);
                    std::thread::sleep(Duration::from_secs_f64(slice));
                    left -= slice;
                }
            }
        });
    }

    // Park the main thread; Ctrl-C ends the process.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
