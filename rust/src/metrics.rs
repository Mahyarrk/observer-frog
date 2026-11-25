//! Host metrics, plus the platform-specific honest extras.
//!
//! Design rule: a metric that cannot be read on this platform is reported as `None` and the
//! renderer shows "n/a". It is never guessed and never rendered as a fault.

use serde_json::{json, Value};
use std::process::Command;
use sysinfo::System;

pub fn kind() -> &'static str {
    if cfg!(target_os = "macos") {
        "mac"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    }
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

const GB: f64 = 1024.0 * 1024.0 * 1024.0;
const MB: f64 = 1024.0 * 1024.0;

pub fn cpu_pct() -> Option<f64> {
    let mut sys = System::new();
    sys.refresh_cpu_all();
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    sys.refresh_cpu_all();
    let usage = sys.global_cpu_usage();
    Some(round1(usage as f64))
}

fn memory_base() -> Value {
    let mut sys = System::new();
    sys.refresh_memory();
    let total = sys.total_memory() as f64;
    let used = sys.used_memory() as f64;
    if total <= 0.0 {
        return json!({
            "mem_used": Value::Null, "mem_total": Value::Null, "mem_pct": Value::Null,
            "swap_used": Value::Null, "mem_cached": Value::Null, "mem_compressed": Value::Null,
        });
    }
    let total_gb = round2(total / GB);
    let used_gb = round1(used / GB);
    let swap_total = sys.total_swap() as f64;
    let swap_used = sys.used_swap() as f64;
    json!({
        "mem_used": format!("{used_gb:.1}/{total_gb:.0} GB"),
        "mem_used_gb": used_gb,
        "mem_total": total_gb,
        "mem_pct": round1(100.0 * used / total),
        "swap_used": if swap_total > 0.0 { Value::String(format!("{:.0} MB", swap_used / MB)) } else { Value::Null },
        "mem_cached": Value::Null,
        "mem_compressed": Value::Null,
    })
}

/// macOS vm_stat accounting: used = App + Wired + Compressed, the definition Activity Monitor uses.
pub fn macos_memory_detail() -> Option<Value> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = Command::new("vm_stat").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();

    let page = text
        .split("page size of ")
        .nth(1)
        .and_then(|s| s.split(' ').next())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(16384.0);

    let pages = |label: &str| -> f64 {
        text.lines()
            .find(|l| l.starts_with(label))
            .and_then(|l| l.split(':').nth(1))
            .and_then(|v| v.trim().trim_end_matches('.').parse::<f64>().ok())
            .unwrap_or(0.0)
    };

    let used = (pages("Anonymous pages") + pages("Pages wired down") + pages("Pages occupied by compressor")) * page;
    let cached = pages("File-backed pages") * page;
    let compressed = pages("Pages occupied by compressor") * page;

    let total: f64 = Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<f64>().ok())
        .unwrap_or(0.0);
    if total <= 0.0 {
        return None;
    }
    Some(json!({
        "mem_used": format!("{:.1}/{:.0} GB", used / GB, total / GB),
        "mem_used_gb": round1(used / GB),
        "mem_total": round2(total / GB),
        "mem_pct": round1(100.0 * used / total),
        "mem_cached": format!("{:.1} GB cached", cached / GB),
        "mem_compressed": format!("{:.1} GB", compressed / GB),
        "_source": "vm_stat",
    }))
}

/// macOS thermal pressure (no root needed). Linux: die temperature when readable.
pub fn thermal() -> Value {
    if cfg!(target_os = "macos") {
        if let Ok(out) = Command::new("pmset").args(["-g", "therm"]).output() {
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            for line in text.lines() {
                if let Some(rest) = line.trim().strip_prefix("CPU_Speed_Limit") {
                    let limit = rest.trim_start_matches([' ', '=']).trim();
                    return json!({"thermal": format!("throttled {limit}%"), "temp_c": Value::Null});
                }
            }
            return json!({"thermal": "Nominal", "temp_c": Value::Null});
        }
        return json!({"thermal": Value::Null, "temp_c": Value::Null});
    }
    // Linux: /sys/class/thermal is the portable no-root source.
    if let Ok(entries) = std::fs::read_dir("/sys/class/thermal") {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with("thermal_zone") {
                continue;
            }
            let temp_path = e.path().join("temp");
            if let Ok(raw) = std::fs::read_to_string(&temp_path) {
                if let Ok(milli) = raw.trim().parse::<f64>() {
                    if milli > 0.0 {
                        return json!({"thermal": Value::Null, "temp_c": round1(milli / 1000.0)});
                    }
                }
            }
        }
    }
    json!({"thermal": Value::Null, "temp_c": Value::Null})
}

pub fn disk() -> Value {
    let root = if cfg!(windows) { "C:\\" } else { "/" };
    match disk_usage(root) {
        // psutil's contract: display total is the volume size, but the percentage is relative
        // to (used + available-to-user), which excludes root-reserved blocks and the APFS
        // snapshot's shared space. Matching it keeps the two builds numerically identical.
        Some((used, avail, total)) => {
            let total_user = used + avail;
            json!({
                "disk_used": format!("{:.1}/{:.0} GB", used / GB, total / GB),
                "disk_pct": round1(100.0 * used / total_user.max(1.0)),
            })
        }
        None => json!({"disk_used": "", "disk_pct": Value::Null}),
    }
}

/// (used_bytes, available_to_user_bytes, total_bytes) for a mount point.
///
/// On macOS `statvfs` over-reports usage on APFS: the read-only system snapshot shares blocks
/// with the data volume, so `total - free` counts them twice (129 GB instead of 13 GB on this
/// machine). psutil works around it with a native call; `df -k` reports the same corrected
/// figure, so macOS uses df and everything else uses statvfs directly.
fn disk_usage(path: &str) -> Option<(f64, f64, f64)> {
    if cfg!(target_os = "macos") {
        let out = Command::new("df").args(["-k", path]).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().nth(1)?;
        let cols: Vec<&str> = line.split_whitespace().collect();
        // Filesystem 1024-blocks Used Available Capacity ... Mounted
        let total_kb: f64 = cols.get(1)?.parse().ok()?;
        let used_kb: f64 = cols.get(2)?.parse().ok()?;
        let avail_kb: f64 = cols.get(3)?.parse().ok()?;
        return Some((used_kb * 1024.0, avail_kb * 1024.0, total_kb * 1024.0));
    }
    let cpath = std::ffi::CString::new(path).ok()?;
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(cpath.as_ptr(), &mut st) != 0 {
            return None;
        }
        let frsize = st.f_frsize as f64;
        let total = st.f_blocks as f64 * frsize;
        let free = st.f_bfree as f64 * frsize;
        let avail = st.f_bavail as f64 * frsize;
        Some(((total - free).max(0.0), avail, total))
    }
}

pub fn uptime_hours() -> Option<f64> {
    Some(round1(sysinfo::System::uptime() as f64 / 3600.0))
}

pub fn load_avg() -> Option<Vec<f64>> {
    let la = System::load_average();
    Some(vec![round2(la.one), round2(la.five), round2(la.fifteen)])
}

pub fn network() -> Value {
    // sysinfo exposes per-interface counters; sum them for the host total.
    let networks = sysinfo::Networks::new_with_refreshed_list();
    let mut sent: u64 = 0;
    let mut recv: u64 = 0;
    for (_name, data) in networks.list() {
        sent += data.total_transmitted();
        recv += data.total_received();
    }
    json!({
        "net_sent_mb": (sent as f64 / MB).round() as i64,
        "net_recv_mb": (recv as f64 / MB).round() as i64,
    })
}

fn cpu_brand() -> String {
    let mut sys = System::new();
    sys.refresh_cpu_all();
    sys.cpus().first().map(|c| c.brand().trim().to_string()).unwrap_or_default()
}

fn mac_chip() -> Option<String> {
    let out = Command::new("sysctl").args(["-n", "machdep.cpu.brand_string"]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn linux_cpu_model() -> Option<String> {
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    for line in text.lines() {
        let lower = line.to_lowercase();
        if lower.starts_with("model name") || lower.starts_with("hardware") || lower.starts_with("model") {
            if let Some((_, v)) = line.split_once(':') {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

fn linux_pretty_name() -> Option<String> {
    let text = std::fs::read_to_string("/etc/os-release").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("PRETTY_NAME=") {
            return Some(rest.trim().trim_matches('"').to_string());
        }
    }
    None
}

/// The `devices[]` entry for this machine, in the shape the UI already renders.
/// The device card.
///
/// The name always comes from the machine, never from a caller or a config field: a hardcoded
/// name goes stale the moment the host is renamed, and then the dashboard lies about which
/// machine you are looking at.
pub fn device_card(node_id: &str) -> Value {
    let k = kind();
    let mut chip = if cfg!(target_os = "macos") {
        mac_chip().unwrap_or_else(cpu_brand)
    } else if cfg!(target_os = "linux") {
        linux_cpu_model().unwrap_or_else(cpu_brand)
    } else {
        cpu_brand()
    };
    if chip.trim().is_empty() {
        chip = std::env::consts::ARCH.to_string();
    }
    let os_name = if cfg!(target_os = "macos") {
        "macOS".to_string()
    } else if cfg!(target_os = "linux") {
        linux_pretty_name().unwrap_or_else(|| "Linux".to_string())
    } else {
        "Windows".to_string()
    };

    let host = crate::config::hostname();

    let mut card = json!({
        "id": format!("dev_{node_id}"),
        "name": host,
        "kind": k,
        "status": "up",
        "detail": format!("{chip} - {os_name}"),
        "cpu_pct": cpu_pct(),
        "uptime_h": uptime_hours(),
    });

    let mem = memory_base();
    merge(&mut card, mem);
    if let Some(detail) = macos_memory_detail() {
        merge(&mut card, detail);
    }
    merge(&mut card, thermal());
    merge(&mut card, disk());
    merge(&mut card, network());
    card["load_avg"] = match load_avg() {
        Some(v) => json!(v),
        None => Value::Null,
    };
    card
}

fn merge(base: &mut Value, extra: Value) {
    if let (Some(b), Value::Object(e)) = (base.as_object_mut(), extra) {
        for (k, v) in e {
            b.insert(k, v);
        }
    }
}
