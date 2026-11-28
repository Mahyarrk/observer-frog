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

pub fn run(mut cfg: Config, config_path: Option<PathBuf>, tls: Option<(PathBuf, PathBuf)>) -> i32 {
    // Ask the machine what it runs, before anything is built from the config. Shared with the
    // reload path in `Watcher::refresh` and the one-shot reporter in `tail::run`, because when each
    // carried its own copy they disagreed: boot merged discovery, a reload did not, and every
    // discovered service vanished a wave later while the node kept reporting the rest.
    let before = cfg.services.len();
    cfg.services = crate::config::merge_discovery(cfg.services, cfg.discover);
    if cfg.discover {
        for s in &cfg.services[before.min(cfg.services.len())..] {
            eprintln!("    discovered {:<16} {}", s.id, s.target);
        }
        let added = cfg.services.len().saturating_sub(before);
        if added > 0 {
            eprintln!("  discovered {added} service(s) on this machine");
        }
    }

    let watcher_holder: Arc<Mutex<Option<Watcher>>> = Arc::new(Mutex::new(
        Watcher::new(config_path.as_deref().or(cfg.path.as_deref())).ok(),
    ));
    let poller_holder: Arc<Mutex<Option<Arc<poller::Poller>>>> = Arc::new(Mutex::new(None));
    let app = build(&cfg, Arc::clone(&poller_holder));
    // The cookie's Secure flag follows the transport, so the app has to know which one it got.
    app.tls_enabled
        .store(tls.is_some(), std::sync::atomic::Ordering::Relaxed);

    let port = match server::listen(Arc::clone(&app), &cfg.bind, cfg.port, tls.clone(), server::ListenerRole::Dashboard) {
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
            match server::listen(Arc::clone(&app), &cfg.bind, rp, None, server::ListenerRole::Reports) {
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

    // First run: the default password, which has to be replaced before the dashboard opens. The
    // door is never unlocked just because the machine is new.
    if crate::auth::ensure_default_password(&cfg.db_path) {
        println!();
        println!("  First run. Sign in at {scheme}://{}:{}/ with:", cfg.bind, port);
        println!("    password: {}", crate::auth::DEFAULT_PASSWORD);
        println!("  You will be asked to choose a new one before the dashboard opens.");
        println!();
    } else if crate::auth::password_must_change(&cfg.db_path) {
        println!();
        println!("  Still on the default password. Sign in and change it to open the dashboard.");
        println!();
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
    // Remembered from the last discovery answer: the port the head's dashboard listens on. Pairing
    // has to reach that listener, and the report address this machine holds does not name it.
    let known_dashboard_port: Arc<Mutex<Option<u16>>> = Arc::new(Mutex::new(None));
    // The dead man's switch. Only a tail can use it: it observes the reports this machine sends, and a
    // head that reports nowhere has nothing to observe.
    let mut wd = crate::watchdog::Watchdog::new();
    {
        let app = Arc::clone(&app);
        let watcher_holder = Arc::clone(&watcher_holder);
        let halt = p.halt_flag();
        let asked_to_pair = Arc::clone(&asked_to_pair);
        let known_dashboard_port = Arc::clone(&known_dashboard_port);
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
                            // Classified, not string-matched: 401 needs a pairing request, 503 must
                            // not, and neither is an outage. The verdict is a type now.
                            let outcome = crate::tail::report_outcome(&cfg_now, &head, &rows, ts)
                                .unwrap_or_else(|why| crate::tail::Outcome::Unreachable { why });

                            // The switch sees every verdict, whatever else happens next. It publishes
                            // through `Notifier`, so each enabled route carries the message and one
                            // route failing does not suppress another.
                            // The head names itself in every answer. Remember it, so an alert written
                            // after the head has gone silent can still say which machine that was.
                            if let Some(name) = outcome.head_name().map(|s| s.to_string()) {
                                if cfg_now.head_name.as_deref() != Some(name.as_str()) {
                                    let path = crate::settings::target(&cfg_now);
                                    let _ = crate::settings::apply(&path, &serde_json::json!({ "head_name": name }));
                                }
                            }

                            if let Some(signal) = wd.observe(&outcome) {
                                // Remembered name first: after the head dies there is nobody to ask.
                                let head_name = cfg_now
                                    .head_name
                                    .clone()
                                    .or_else(|| crate::pair::host_of(&head))
                                    .unwrap_or_else(|| "the head".to_string());
                                let ev = crate::notify::watchdog_event(
                                    &signal,
                                    &head_name,
                                    wd.last_why(),
                                    app.notifier.lock().unwrap().as_ref().map(|n| n.dashboard_url().to_string()).as_deref().unwrap_or(""),
                                );
                                if let Some(n) = app.notifier.lock().unwrap().as_mut() {
                                    n.submit_event(ev);
                                    n.flush(None);
                                }
                            }

                            match &outcome {
                                crate::tail::Outcome::Up(_) => {
                                    app.reports_ok.fetch_add(1, Ordering::Relaxed);
                                }
                                _ => {
                                    let e = outcome.why();
                                    app.reports_failed.fetch_add(1, Ordering::Relaxed);
                                    *app.last_report_error.lock().unwrap() = Some(e.clone());
                                    log_line(&format!("report to {head} failed: {e}"));
                                    // A head that does not know this machine is the pairing case.
                                    // Ask once, then keep reporting quietly: a tail that re-asked
                                    // every wave would fill the head's list with duplicates, and a
                                    // person still has to click approve either way.
                                    if outcome.code() == Some(401) && !*asked_to_pair.lock().unwrap() {
                                        *asked_to_pair.lock().unwrap() = true;
                                        // Ask the dashboard, not the report port we just failed on:
                                        // `head` is the report address and answers only
                                        // /api/report. The advertised dashboard port is remembered
                                        // per head; without one, the default is assumed.
                                        let dport = known_dashboard_port.lock().unwrap().unwrap_or(8770);
                                        let dash = crate::pair::host_of(&head)
                                            .and_then(|h| crate::pair::probe_dashboard(&h, dport));
                                        let ask: Result<String, String> = match dash {
                                            Some((base, _tls)) => match cfg_now.token() {
                                                Ok(tok) => crate::pair::ask_head(&base, &cfg_now.node_id, &crate::config::hostname(), &tok, cfg_now.poll_interval as i64),
                                                Err(why) => Err(why),
                                            },
                                            None => Err(format!("nothing answered on {dport} as a dashboard")),
                                        };
                                        match ask {
                                            Ok(msg) => log_line(&format!("pairing: {msg}")),
                                            Err(why) => log_line(&format!("pairing request failed: {why}")),
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
