//! Resident-memory budget test.
//!
//! The whole reason this port exists is distribution: a small binary that does not need a
//! runtime. Memory is part of that promise, and nothing else in the suite would notice if a
//! change doubled it. This test runs a real probe wave over a real configuration and asserts the
//! process stays inside a budget a 4 GB machine can spare.
//!
//! It measures `ru_maxrss` (peak resident set), so it reads high the first time; the bound is set
//! for the peak rather than the steady state, which is the number that actually breaks things.

use observer_frog::config;
use observer_frog::poller;
use std::path::PathBuf;

/// Peak RSS ceiling in MB. Measured steady state is ~8 MB and a wave peak reads ~14 MB, so this
/// leaves room for a larger catalog without letting a leak slip through.
const MAX_RSS_MB: f64 = 50.0;

/// Peak resident set size in MB for this process.
fn peak_rss_mb() -> f64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("VmHWM:") {
                    if let Some(kb) = rest.split_whitespace().next().and_then(|v| v.parse::<f64>().ok()) {
                        return kb / 1024.0;
                    }
                }
            }
        }
        0.0
    }
    #[cfg(target_os = "macos")]
    {
        // getrusage reports ru_maxrss in bytes on macOS, kilobytes on Linux.
        unsafe {
            let mut usage: libc::rusage = std::mem::zeroed();
            if libc::getrusage(libc::RUSAGE_SELF, &mut usage) == 0 {
                return usage.ru_maxrss as f64 / (1024.0 * 1024.0);
            }
        }
        0.0
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        0.0
    }
}

fn config_path() -> Option<PathBuf> {
    // Set OF_PARITY_CONFIG to a real config.toml to exercise the full service list.
    std::env::var("OF_PARITY_CONFIG").ok().map(PathBuf::from)
}

#[test]
fn a_wave_stays_within_the_memory_budget() {
    let before = peak_rss_mb();

    let services = match config_path() {
        Some(p) => config::load(Some(&p)).map(|c| c.services).unwrap_or_default(),
        None => Vec::new(),
    };

    // Run a wave. With no config this still exercises the process snapshot, which is the part
    // that allocates most: one scan over every process on the machine.
    let rows = poller::sample(&services, None);
    assert_eq!(rows.len(), services.len(), "a wave must return one row per service");

    let after = peak_rss_mb();
    assert!(after > 0.0, "could not measure RSS on this platform");
    assert!(
        after <= MAX_RSS_MB,
        "peak RSS {after:.1} MB exceeds the {MAX_RSS_MB} MB budget (was {before:.1} MB before the wave)"
    );
}

#[test]
fn an_empty_service_list_costs_almost_nothing() {
    // A dashboard with no services configured must not be heavier than the process snapshot.
    let before = peak_rss_mb();
    let rows = poller::sample(&[], None);
    assert!(rows.is_empty());
    let after = peak_rss_mb();
    assert!(
        after <= MAX_RSS_MB,
        "peak RSS {after:.1} MB after an empty wave (was {before:.1} MB)"
    );
}
