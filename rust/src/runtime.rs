//! Runtime wiring: poller thread + notifier + HTTP server, with self-monitoring last (I9).

use crate::config::{Config, Watcher};
use crate::notify::Notifier;
use crate::poller;
use crate::server::{self, App};
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

    // Self-monitoring begins only now that the socket is bound (invariant I9).
    let p = Arc::new(poller::Poller::new());
    *poller_holder.lock().unwrap() = Some(Arc::clone(&p));

    let sweeper_halt = p.halt_flag();
    {
        let app = Arc::clone(&app);
        let watcher_holder = Arc::clone(&watcher_holder);
        let halt = p.halt_flag();
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
                    })
                    .collect();

                match state::apply_wave(&cfg_now.db_path, &cfg_now.node_id, &state_rows, Some(ts)) {
                    Ok(changes) => {
                        app.waves.fetch_add(1, Ordering::Relaxed);
                        *app.last_wave_error.lock().unwrap() = None;
                        app.set_snapshot(rows, ts);
                        if !changes.is_empty() {
                            if let Some(n) = app.notifier.lock().unwrap().as_mut() {
                                n.submit(&changes);
                                n.flush(None);
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

    // Silence sweeper: its own clock, because it must run even on a head that probes nothing.
    {
        let app = Arc::clone(&app);
        let halt = Arc::clone(&sweeper_halt);
        std::thread::spawn(move || {
            // Sweep four times per shortest interval, floored at 5s, so detection lands within
            // the grace window rather than a whole interval late.
            let tick: f64 = 5.0;
            while !halt.load(Ordering::SeqCst) {
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
