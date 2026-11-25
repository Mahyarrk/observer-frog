//! Install and autostart: the binary owns its own service definition.
//!
//! A user should not be hand-writing launchd plists or systemd units, and neither should the
//! operator of this repo. The templates live here, embedded, so a single downloaded binary
//! carries everything it needs to become a service on the machine it lands on.
//!
//! Two roles, two unit names (`-head`, `-tail`): a machine can autostart either, or both.
//!
//! Windows is not supported here. The metrics and config paths already know about Windows, but
//! claiming a Task Scheduler integration we have never run is worse than saying plainly that
//! autostart is macOS and Linux only.

use crate::config;
use std::fs;
use std::path::{Path, PathBuf};

/// macOS: a per-user LaunchAgent. No root, survives reboot, starts at login.
const LAUNCHD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{binary}</string>
    <string>--config</string>
    <string>{config}</string>
    <string>serve</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#;

/// Linux: a user unit. `WantedBy=default.target` is what makes it start without a login shell
/// only once lingering is enabled; the installer says so rather than pretending otherwise.
const SYSTEMD: &str = r#"[Unit]
Description=observer-frog ({role})
After=network-online.target

[Service]
ExecStart={binary} --config {config} serve
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
"#;

pub struct Plan {
    pub binary: PathBuf,
    pub config_path: PathBuf,
    pub unit_path: PathBuf,
    pub log_path: PathBuf,
    pub role: String,
}

pub fn role_of(cfg: &config::Config) -> String {
    if cfg.serve {
        "head".to_string()
    } else {
        "tail".to_string()
    }
}

/// Where the stable copy of the binary goes.
///
/// A unit that points at `target/release/` breaks the first time someone runs `cargo clean`, so
/// the installer copies the binary somewhere durable and points the unit there instead.
pub fn install_dir() -> PathBuf {
    config::state_home()
}

pub fn stable_binary() -> PathBuf {
    install_dir().join(if cfg_is_windows() { "observer-frog.exe" } else { "observer-frog" })
}

fn cfg_is_windows() -> bool {
    cfg!(windows)
}

pub fn unit_path(role: &str) -> Result<PathBuf, String> {
    if cfg!(target_os = "macos") {
        let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
        Ok(PathBuf::from(home)
            .join("Library/LaunchAgents")
            .join(format!("dev.observerfrog.{role}.plist")))
    } else if cfg!(target_os = "linux") {
        let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
        Ok(PathBuf::from(home)
            .join(".config/systemd/user")
            .join(format!("observer-frog-{role}.service")))
    } else {
        Err(format!(
            "autostart is not implemented on {}; macOS and Linux only",
            std::env::consts::OS
        ))
    }
}

/// Render the unit for this platform. Pure, so it can be asserted on without touching the system.
pub fn render(role: &str, binary: &Path, config_path: &Path, log_path: &Path) -> Result<String, String> {
    if cfg!(target_os = "macos") {
        Ok(LAUNCHD
            .replace("{label}", &format!("dev.observerfrog.{role}"))
            .replace("{binary}", &binary.display().to_string())
            .replace("{config}", &config_path.display().to_string())
            .replace("{log}", &log_path.display().to_string()))
    } else if cfg!(target_os = "linux") {
        Ok(SYSTEMD
            .replace("{role}", role)
            .replace("{binary}", &binary.display().to_string())
            .replace("{config}", &config_path.display().to_string()))
    } else {
        Err(format!("autostart is not implemented on {}", std::env::consts::OS))
    }
}

/// `observer-frog install`: make this machine run this binary at boot.
pub fn install(cfg: &config::Config, config_path: Option<&Path>) -> i32 {
    let role = role_of(cfg);
    let binary = stable_binary();
    let config_path = match config_path {
        Some(p) => p.to_path_buf(),
        None => config::default_path(),
    };
    let log_path = install_dir().join(format!("{role}.log"));

    if !config_path.exists() {
        eprintln!("no config at {}", config_path.display());
        eprintln!("start the binary once (it writes defaults) or pass --config");
        return 1;
    }
    if let Err(e) = fs::create_dir_all(install_dir()) {
        eprintln!("cannot create {}: {e}", install_dir().display());
        return 1;
    }

    // Copy the running binary to a stable path before writing a unit that points at it.
    // This runs on every `install`, which is also how an upgrade lands: the unit points at the
    // copy, so re-installing is the documented way to replace a running version.
    let self_path = std::env::current_exe().unwrap_or_default();
    if self_path != binary {
        // A running service holds its executable open, and replacing it in place fails with
        // ETXTBSY. Write beside it and rename: rename swaps the directory entry, which is allowed
        // while the old file is still open, and the service picks up the new one on restart.
        let staged = binary.with_extension("new");
        if let Err(e) = fs::copy(&self_path, &staged) {
            eprintln!("cannot copy {} to {}: {e}", self_path.display(), staged.display());
            return 1;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&staged, fs::Permissions::from_mode(0o755));
        }
        if let Err(e) = fs::rename(&staged, &binary) {
            eprintln!("cannot replace {}: {e}", binary.display());
            let _ = fs::remove_file(&staged);
            return 1;
        }
    }

    let unit = match unit_path(&role) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let body = match render(&role, &binary, &config_path, &log_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if let Some(dir) = unit.parent() {
        if let Err(e) = fs::create_dir_all(dir) {
            eprintln!("cannot create {}: {e}", dir.display());
            return 1;
        }
    }
    if let Err(e) = fs::write(&unit, body) {
        eprintln!("cannot write {}: {e}", unit.display());
        return 1;
    }

    // The token must survive a reboot, and a service has no shell to read it from.
    if let Some(p) = &cfg.token_file {
        if let Ok(meta) = fs::metadata(p) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = meta.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    eprintln!("warning: token_file {} is mode {:o}; tighten it to 600", p.display(), mode);
                }
            }
        } else {
            eprintln!("warning: token_file {} does not exist yet", p.display());
        }
    }

    // Activate what was just written. Writing a unit is not installing it: without `enable` the
    // Linux unit never runs at boot, and without `load -w` the Mac plist sits in LaunchAgents
    // doing nothing. Both are cheap and idempotent.
    let activation = if cfg!(target_os = "macos") {
        std::process::Command::new("launchctl")
            .arg("load")
            .arg("-w")
            .arg(&unit)
            .output()
    } else {
        // Reload first so systemd sees the file, then enable and start it. Skipping the reload is
        // what made a re-install print "unit changed on disk" on the next restart — a warning the
        // user did nothing to cause and cannot act on.
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
        let name = unit.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        std::process::Command::new("systemctl")
            .args(["--user", "enable", "--now", &name])
            .output()
    };
    let activated = activation.map(|o| o.status.success()).unwrap_or(false);

    println!("installed as {} ({role})", unit.display());
    println!("  binary {}", binary.display());
    println!("  config {}", config_path.display());
    if cfg!(target_os = "macos") {
        println!(
            "  {}",
            if activated { "loaded and set to start at login" } else { "written; loading it failed, run: launchctl load -w <the path above>" }
        );
    } else {
        println!(
            "  {}",
            if activated { "enabled and started" } else { "written; enabling it failed, run: systemctl --user enable --now <the path above>" }
        );
        println!("  to start at boot without logging in: loginctl enable-linger $USER");
    }
    println!("  autostart is on; `uninstall` removes it");
    if cfg!(target_os = "macos") {
        println!("  the service runs the copy above: re-run `install` after a rebuild, then restart it");
    } else {
        println!("  the service runs the copy above: re-run `install` after a rebuild, then `systemctl --user restart`");
    }
    0
}

/// `observer-frog uninstall`: stop being a service. Idempotent.
pub fn uninstall(cfg: &config::Config) -> i32 {
    let role = role_of(cfg);
    let unit = match unit_path(&role) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let mut removed_any = false;
    if unit.exists() {
        if cfg!(target_os = "macos") {
            let _ = std::process::Command::new("launchctl")
                .args(["unload", "-w"])
                .arg(&unit)
                .output();
        } else {
            let name = unit.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "disable", "--now", &name])
                .output();
        }
        match fs::remove_file(&unit) {
            Ok(_) => {
                println!("removed {}", unit.display());
                removed_any = true;
            }
            Err(e) => {
                eprintln!("cannot remove {}: {e}", unit.display());
                return 1;
            }
        }
    }
    if !removed_any {
        println!("nothing installed for role {role}; nothing to remove");
    }
    println!("autostart is off");
    0
}

/// Is autostart actually on?
///
/// Measured from the init system, not from the file. `enable`/`disable` do not remove the unit, so
/// checking for its existence answered "installed" no matter what the operator had switched off —
/// and the Settings page, which reads this, snapped its checkbox back to "on" after every refresh.
pub fn is_installed(cfg: &config::Config) -> bool {
    let role = role_of(cfg);
    let Ok(unit) = unit_path(&role) else { return false };
    if !unit.exists() {
        return false;
    }
    if cfg!(target_os = "macos") {
        let uid = std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "501".into());
        // `launchctl print-disabled` lists a label as disabled whether or not it is loaded, which is
        // the same state `launchctl disable` writes. Absent from that list means enabled.
        let out = std::process::Command::new("launchctl")
            .arg("print-disabled")
            .arg(format!("gui/{uid}"))
            .output();
        match out {
            Ok(o) => {
                let text = String::from_utf8_lossy(&o.stdout);
                !text.contains(&format!("\"dev.observerfrog.{role}\" => disabled"))
            }
            Err(_) => true,
        }
    } else {
        let name = unit.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        std::process::Command::new("systemctl")
            .args(["--user", "is-enabled", &name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

/// Turn autostart on or off **without killing a running node**.
///
/// Autostart means "start at boot", not "run now". Using `launchctl unload` or
/// `systemctl disable --now` here would kill the very process answering this request, so the
/// switch could never report success — and turning autostart off should not stop a dashboard
/// that is currently serving.
pub fn set_enabled(cfg: &config::Config, on: bool) -> Result<String, String> {
    let role = role_of(cfg);
    let unit = unit_path(&role)?;
    if !unit.exists() {
        return Err(format!(
            "not installed ({}) — run `observer-frog install` once from a shell",
            unit.display()
        ));
    }
    let out = if cfg!(target_os = "macos") {
        let uid = std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "501".into());
        let target = format!("gui/{uid}/dev.observerfrog.{role}");
        // disable/enable only affect what happens at login; a running job is left alone.
        let o = std::process::Command::new("launchctl")
            .args([if on { "enable" } else { "disable" }, &target])
            .output();
        if on {
            // Enabling something that is not running should actually start it.
            if !is_running(&role) {
                let _ = std::process::Command::new("launchctl").arg("load").arg("-w").arg(&unit).output();
            }
        }
        o
    } else {
        let name = unit.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        if on {
            std::process::Command::new("systemctl")
                .args(["--user", "enable", "--now", &name])
                .output()
        } else {
            // No --now: it stops starting at boot, and the current run continues.
            std::process::Command::new("systemctl")
                .args(["--user", "disable", &name])
                .output()
        }
    };
    match out {
        Ok(o) if o.status.success() => Ok(format!("autostart {}", if on { "on" } else { "off" })),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => Err(format!("cannot run the platform service tool: {e}")),
    }
}

/// Whether the unit is currently running, so `enable` can start a stopped node.
fn is_running(role: &str) -> bool {
    let label = format!("dev.observerfrog.{role}");
    std::process::Command::new("launchctl")
        .arg("list")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.contains(&label))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_names_the_binary_and_the_config() {
        let body = render("head", Path::new("/opt/of"), Path::new("/etc/of.toml"), Path::new("/tmp/of.log"))
            .expect("this platform is supported");
        assert!(body.contains("/opt/of"), "the unit must point at the binary: {body}");
        assert!(body.contains("/etc/of.toml"), "the unit must point at the config: {body}");
    }

    #[test]
    fn the_two_roles_get_different_unit_names() {
        assert_ne!(unit_path("head").unwrap(), unit_path("tail").unwrap());
    }

    #[test]
    fn the_role_follows_serving_not_a_command_name() {
        let cfg = |serve: bool| config::Config {
            path: None,
            node_id: "t".into(),
            node_name: String::new(),
            serve,
            bind: "127.0.0.1".into(),
            port: 8770,
            poll_interval: 30,
            db_path: PathBuf::from("/tmp/of-install-test.db"),
            discover: false,
            report_port: None,
            report_to: None,
            token_env: String::new(),
            token_file: None,
            services: vec![],
            sinks: vec![],
        };
        assert_eq!(role_of(&cfg(true)), "head");
        assert_eq!(role_of(&cfg(false)), "tail");
    }
}
