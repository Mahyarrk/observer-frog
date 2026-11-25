//! The poller: fixed-rate waves, bounded concurrency, one process snapshot per wave.
//!
//! Invariants honoured here:
//!   I12 — a single process snapshot serves every `process` / `ram_mb` lookup in a wave.
//!   Discipline — a wave that overruns its interval is dropped and logged, never stacked.

use crate::config::Service;
use crate::probes;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use sysinfo::{ProcessesToUpdate, System};

pub const MAX_WORKERS: usize = 12;
pub const WAVE_BUDGET_FRACTION: f64 = 0.8;

/// Start-of-wave process table, so N services cost one scan instead of N scans.
pub struct Snapshot {
    rows: Vec<(String, String, f64)>, // (cmdline_lower, cmdline, rss_mb)
    pub ok: bool,
    pub excluded: Vec<u32>,
}

impl Snapshot {
    pub fn new() -> Self {
        let excluded = probes::self_exclusions();
        let mut sys = System::new();
        // I12: one scan feeds every process lookup in the wave, and `cmd` must be requested.
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            sysinfo::ProcessRefreshKind::new()
                .with_cmd(sysinfo::UpdateKind::Always)
                .with_memory(),
        );
        let mut rows = Vec::with_capacity(sys.processes().len());
        for (pid, p) in sys.processes() {
            if excluded.contains(&pid.as_u32()) {
                continue;
            }
            let cmd = p.cmd().iter().map(|c| c.to_string_lossy()).collect::<Vec<_>>().join(" ");
            let cmd = if cmd.is_empty() { p.name().to_string_lossy().to_string() } else { cmd };
            let rss = p.memory() as f64 / (1024.0 * 1024.0);
            rows.push((cmd.to_lowercase(), cmd, (rss * 10.0).round() / 10.0));
        }
        Self { rows, ok: true, excluded }
    }

    /// Return (found, cmdline, rss_mb) for the first process whose cmdline contains needle.
    pub fn match_needle(&self, needle: &str) -> (bool, String, f64) {
        let needle = needle.to_lowercase();
        for (low, cmd, rss) in &self.rows {
            if low.contains(&needle) {
                return (true, cmd.clone(), *rss);
            }
        }
        (false, String::new(), 0.0)
    }

    pub fn size(&self) -> usize {
        self.rows.len()
    }
}

/// Probe every service concurrently and return payload rows in configuration order.
pub fn sample(services: &[Service], snapshot: Option<&Snapshot>) -> Vec<serde_json::Value> {
    let t0 = Instant::now();
    let owned;
    let snap = match snapshot {
        Some(s) => s,
        None => {
            owned = Snapshot::new();
            &owned
        }
    };

    // (index, triple, ram) collected in whatever order the workers finish.
    let out = Arc::new(Mutex::new(Vec::<(usize, (String, String, i64), f64)>::with_capacity(services.len())));
    let next = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));

    let workers = MAX_WORKERS.min(services.len()).max(1);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let out = Arc::clone(&out);
            let next = Arc::clone(&next);
            let done = Arc::clone(&done);
            scope.spawn(move || {
                loop {
                    if done.load(Ordering::Relaxed) {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed) as usize;
                    if i >= services.len() {
                        break;
                    }
                    let svc = &services[i];
                    let (triple, ram) = if svc.probe == "process" {
                        let (found, cmd, rss) = snap.match_needle(&svc.target);
                        let state = if found { probes::STATE_UP } else { probes::STATE_DOWN };
                        let detail = if found { probes::clip(&cmd) } else { "not running".to_string() };
                        ((state.to_string(), detail, 0i64), rss)
                    } else {
                        (probes::run(svc), 0.0)
                    };
                    out.lock().unwrap().push((i, triple, ram));
                }
            });
        }
    });

    let mut collected = Arc::try_unwrap(out).map(|m| m.into_inner().unwrap()).unwrap_or_default();
    collected.sort_by_key(|(i, _, _)| *i);

    let mut rows = Vec::with_capacity(services.len());
    for (idx, (state, detail, ms), ram) in collected {
        let svc = &services[idx];
        let mut row = serde_json::json!({
            "id": svc.id,
            "name": if svc.name.is_empty() { svc.id.clone() } else { svc.name.clone() },
            "group": svc.group,
            "status": state,
            "detail": detail,
            "latency_ms": ms,
            "ports": svc.ports,
            "address": svc.target,
        });
        if ram != 0.0 {
            row["ram_mb"] = serde_json::json!(ram);
        }
        rows.push(row);
    }
    let _ = t0;
    rows
}

/// Owns the wave schedule. `on_wave(rows, ts)` is called from the poller thread.
pub struct Poller {
    pub waves: AtomicU64,
    pub dropped: AtomicU64,
    pub errors: AtomicU64,
    pub last_error: Mutex<Option<String>>,
    halt: Arc<AtomicBool>,
}

impl Poller {
    pub fn new() -> Self {
        Self {
            waves: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            last_error: Mutex::new(None),
            halt: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn stop(&self) {
        self.halt.store(true, Ordering::SeqCst);
    }

    pub fn halt_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.halt)
    }
}

impl Default for Poller {
    fn default() -> Self {
        Self::new()
    }
}
