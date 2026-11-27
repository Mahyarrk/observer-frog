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


pub const MAX_WORKERS: usize = 12;
pub const WAVE_BUDGET_FRACTION: f64 = 0.8;

/// Parse `ps -Ao pid=,ppid=,rss=,args=`.
///
/// Only `args` is read, never `comm`: `ps` truncates `comm` to a fixed width and pads it, so
/// "next-server (v16.4.2)" arrives as "next-server (v16" and cannot be reconstructed. The first
/// token of `args` is the executable path, which is both unambiguous and what identifies a service.
fn parse_ps(text: &str, excluded: &[u32]) -> Vec<Proc> {
    let mut rows = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(kb)) = (it.next(), it.next(), it.next()) else { continue };
        let (Ok(pid), Ok(ppid), Ok(kb)) = (pid.parse::<u32>(), ppid.parse::<u32>(), kb.parse::<f64>())
        else { continue };
        if excluded.contains(&pid) {
            continue;
        }
        let cmd: String = it.collect::<Vec<_>>().join(" ");
        if cmd.is_empty() {
            continue;
        }
        rows.push(Proc {
            pid,
            parent: if ppid == 0 { None } else { Some(ppid) },
            cmd_lower: cmd.to_lowercase(),
            cmd,
            rss_mb: ((kb / 1024.0) * 10.0).round() / 10.0,
        });
    }
    rows
}

/// One process in the wave snapshot.
pub struct Proc {
    pub pid: u32,
    pub parent: Option<u32>,
    pub cmd_lower: String,
    pub cmd: String,
    pub rss_mb: f64,
}

/// Start-of-wave process table, so N services cost one scan instead of N scans.
pub struct Snapshot {
    rows: Vec<Proc>,
    pub ok: bool,
    pub excluded: Vec<u32>,
}

impl Snapshot {
    pub fn new() -> Self {
        let excluded = probes::self_exclusions();
        // `ps` rather than sysinfo, deliberately: sysinfo cannot read the memory or command line of
        // a process owned by another user — and never of root — so on a real machine (vocechat runs
        // as `vocechat`, xray as `root`) most services came back unreadable. `ps` reports them all
        // without privileges on macOS and Linux. One fork per wave, the same cost as the scan it
        // replaces, and no service needs root to be measured.
        let rows = match std::process::Command::new("ps")
            .args(["-Ao", "pid=,ppid=,rss=,args="])
            .output()
        {
            Ok(out) => parse_ps(&String::from_utf8_lossy(&out.stdout), &excluded),
            Err(e) => {
                eprintln!("cannot read the process table: {e}");
                Vec::new()
            }
        };
        let ok = !rows.is_empty();
        Self { rows, ok, excluded }
    }

    /// Return (found, cmdline, rss_mb) for the first process whose cmdline contains `needle`.
    ///
    /// Used by `process` probes, where the needle is the service's own process name.
    pub fn match_needle(&self, needle: &str) -> (bool, String, f64) {
        let needle = needle.to_lowercase();
        match self.rows.iter().find(|r| r.cmd_lower.contains(&needle)) {
            Some(r) => (true, r.cmd.clone(), r.rss_mb),
            None => (false, String::new(), 0.0),
        }
    }

    pub fn size(&self) -> usize {
        self.rows.len()
    }

    /// RSS (MB) for the single process whose executable name is `needle`, or `None` if the name is
    /// ambiguous.
    ///
    /// This is the unprivileged fallback for Linux, where `ss` hides `pid=` from a non-root caller
    /// so a port cannot be resolved to a PID at all. The rule is deliberately strict: an executable
    /// name that matches exactly one process is that service; a name matching several (`node`,
    /// `python3`) is refused rather than summed into a plausible-looking wrong number.
    pub fn rss_for_unique(&self, needle: &str) -> Option<f64> {
        let needle = needle.trim().to_lowercase();
        if needle.is_empty() {
            return None;
        }
        let matched: Vec<&Proc> = self
            .rows
            .iter()
            .filter(|r| {
                let exe = r.cmd.split_whitespace().next().unwrap_or("");
                let name = exe.rsplit('/').next().unwrap_or(exe).to_lowercase();
                name == needle || name.trim_end_matches("-server") == needle
            })
            .collect();
        match matched.len() {
            1 => Some(matched[0].rss_mb),
            _ => None,
        }
    }

    /// Total RSS (MB) for every process whose command line mentions `needle`, summed.
    ///
    /// Summing is deliberate: Syncthing runs two processes, and reporting one would understate it.
    /// `None` when nothing matches, so the UI renders "—" instead of a confident `0 MB`.
    pub fn rss_for(&self, needle: &str) -> Option<f64> {
        let needle = needle.trim().to_lowercase();
        if needle.is_empty() {
            return None;
        }
        // Match the *exact* executable name, not a substring of the whole command line.
        // A substring match catches every shell wrapper, editor and helper that merely mentions the
        // service -- that is how a 59 MB VoceChat was reported as 888 MB, since the sum swept up
        // unrelated processes whose arguments happened to contain the name.
        let matched: Vec<&Proc> = self
            .rows
            .iter()
            .filter(|r| {
                let exe = r.cmd.split_whitespace().next().unwrap_or("");
                let name = exe.rsplit('/').next().unwrap_or(exe).to_lowercase();
                name == needle || name.trim_end_matches("-server") == needle
            })
            .collect();
        if matched.is_empty() {
            return None;
        }
        let total: f64 = matched.iter().map(|r| r.rss_mb).sum();
        Some((total * 10.0).round() / 10.0)
    }

    /// Total RSS (MB) for the process holding `port`, plus its direct children.
    ///
    /// A listening service often forks a worker (nginx master/worker, a Node supervisor), so
    /// crediting only the socket holder understates it. The PID is resolved from the socket table
    /// and then matched inside this snapshot — no second process scan, which is the point of I12.
    pub fn rss_for_port(&self, port: u16) -> Option<f64> {
        let holders = crate::discover::pids_listening_on(port);
        if holders.is_empty() {
            return None;
        }
        let matched: Vec<&Proc> = self
            .rows
            .iter()
            .filter(|r| {
                holders.contains(&r.pid)
                    || r.parent.map(|par| holders.contains(&par)).unwrap_or(false)
            })
            .collect();
        if matched.is_empty() {
            return None;
        }
        let total: f64 = matched.iter().map(|r| r.rss_mb).sum();
        Some((total * 10.0).round() / 10.0)
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
                        // A process probe may also name an interface, because the same software ships
                        // as a process on one platform and a kernel module on another. This branch
                        // used to answer from the process snapshot alone, so WireGuard on Linux read
                        // "not running" forever: no process to find, and the interface never asked.
                        // The fallback belongs here as much as in `probes::run`, and asking is the
                        // same one call.
                        let (state, detail) = if found {
                            (probes::STATE_UP, probes::clip(&cmd))
                        } else if !svc.iface.is_empty() {
                            let t = probes::probe_iface(&svc.iface);
                            (if t.0 == probes::STATE_UP { probes::STATE_UP } else { probes::STATE_DOWN },
                             t.1)
                        } else {
                            (probes::STATE_DOWN, "not running".to_string())
                        };
                        ((state.to_string(), detail, 0i64), rss)
                    } else {
                        // A listening service: resolve the PID that holds the port in this same
                        // wave and sum its RSS with its children's. Unresolvable stays 0.0 here and
                        // the UI renders it "—", because 0 reads as "using no memory".
                        // Port → PID → RSS is the only attribution that is *exact*: it names the
                        // process that actually holds the socket. A name match is not a substitute —
                        // "node" matches every Node process on the box and reported a 67 MB router
                        // as 229 MB. So an unresolvable port stays None and the card shows "—".
                        // macOS reports the PID in `lsof`; Linux only does so under elevation, where
                        // this deliberately returns "—" rather than a plausible-looking wrong number.
                        let ram = crate::config::port_of(&svc.target)
                            .parse::<u16>()
                            .ok()
                            .and_then(|p| snap.rss_for_port(p))
                            .or_else(|| snap.rss_for_unique(&svc.id))
                            .unwrap_or(0.0);
                        (probes::run(svc), ram)
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
        // Only a real measurement is published. Absence becomes "—" in the UI; a literal 0 would
        // read as "this service uses no memory", which we have not measured and must not claim.
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

#[cfg(test)]
mod ps_parse_tests {
    use super::*;

    #[test]
    fn the_process_table_parses_without_privileges() {
        // Real shape, including a command line with spaces and a root-owned process.
        let text = "    1     0  28496 /sbin/launchd\n 742   1  60628 /usr/local/bin/vocechat-server /etc/vocechat/config.toml\n 1532  1145 259232 node /opt/9router/server.js\n";
        let rows = parse_ps(text, &[]);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].pid, 742);
        assert_eq!(rows[1].parent, Some(1));
        assert_eq!(rows[1].rss_mb, 59.2);
        // A service is identified by the executable, not by a substring of its arguments.
        assert_eq!(rows[1].cmd.split_whitespace().next(), Some("/usr/local/bin/vocechat-server"));
    }

    #[test]
    fn an_own_process_is_excluded() {
        let text = " 742 1 60628 /usr/local/bin/vocechat-server\n";
        assert!(parse_ps(text, &[742]).is_empty());
    }
}

#[cfg(test)]
mod ram_tests {
    use super::*;

    fn proc_(pid: u32, cmd: &str, rss: f64) -> Proc {
        Proc { pid, parent: None, cmd_lower: cmd.to_lowercase(), cmd: cmd.to_string(), rss_mb: rss }
    }

    fn snap(rows: Vec<Proc>) -> Snapshot {
        Snapshot { rows, ok: true, excluded: vec![] }
    }

    #[test]
    fn an_unambiguous_name_resolves_to_its_own_memory() {
        let s = snap(vec![proc_(742, "/usr/local/bin/vocechat-server /etc/vocechat/config.toml", 59.2)]);
        assert_eq!(s.rss_for_unique("vocechat"), Some(59.2));
    }

    #[test]
    fn an_ambiguous_name_is_refused_rather_than_summed() {
        // Two Node processes: neither is "the" service, and summing them invented 229 MB for a
        // 67 MB router. Refusing is the correct answer; the card shows a dash.
        let s = snap(vec![
            proc_(1, "node /opt/9router/server.js", 100.0),
            proc_(2, "node /opt/other/index.js", 50.0),
        ]);
        assert_eq!(s.rss_for_unique("9router"), None);
    }

    #[test]
    fn a_wrapper_that_merely_mentions_the_service_is_not_it() {
        // The 59 MB -> 888 MB bug: a substring match swept up any process whose arguments contained
        // the name. Only the executable identifies a service.
        let s = snap(vec![
            proc_(742, "/usr/local/bin/vocechat-server /etc/vocechat/config.toml", 59.2),
            proc_(900, "/bin/sh -c tail -f /var/log/vocechat.log", 800.0),
        ]);
        assert_eq!(s.rss_for_unique("vocechat"), Some(59.2), "the wrapper must not be summed");
    }
}
