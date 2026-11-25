//! Install and autostart: the binary owns its own service definition.
//!
//! A user should not be hand-writing launchd plists or systemd units, and neither should the
//! operator of this repo. The templates live here, embedded, so a single downloaded binary
//! carries everything it needs to become a service on the machine it lands on.
//!
//! One unit name (`dev.observerfrog` / `observer-frog.service`), regardless of role. The role is a
//! setting inside the config, not an identity: switching head↔tail must restart the service that is
//! already installed, so the name has to stay put across the switch.
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

/// The one service label for this node, whatever its role.
pub const LABEL: &str = "dev.observerfrog";

pub fn role_of(cfg: &config::Config) -> String {
    if cfg.serve {
        "head".to_string()
    } else {
        "tail".to_string()
    }
}

/// Names used before the role stopped being part of the service identity.
///
/// Leaving one of these behind would keep a second copy running: it binds the same port, fails, and
/// `KeepAlive`/`Restart=always` retries it forever. `install` retires them.
fn legacy_units() -> Vec<PathBuf> {
    ["head", "tail"]
        .iter()
        .filter_map(|r| unit_path_of(r).ok())
        .filter(|p| p.exists())
        .collect()
}

/// Retire a unit that exists on disk but must not run. Best effort: a failure here is reported, not
/// fatal, because the new unit is what actually has to work.
fn retire(path: &Path) {
    if cfg!(target_os = "macos") {
        let _ = std::process::Command::new("launchctl").arg("unload").arg("-w").arg(path).output();
        let label = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        if let Ok(o) = std::process::Command::new("id").arg("-u").output() {
            let uid = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let _ = std::process::Command::new("launchctl")
                .args(["disable", &format!("gui/{uid}/{label}")])
                .output();
        }
    } else {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "disable", "--now", &name])
            .output();
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
    }
    if let Err(e) = fs::remove_file(path) {
        eprintln!("warning: cannot remove old unit {}: {e}", path.display());
    } else {
        println!("retired old unit {}", path.display());
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

/// The unit for this node. `role` is accepted for the legacy sweep and ignored, so every call site
/// keeps compiling and every caller lands on the same stable path.
pub fn unit_path(_role: &str) -> Result<PathBuf, String> {
    if !(cfg!(target_os = "macos") || cfg!(target_os = "linux")) {
        return Err(format!(
            "autostart is not implemented on {}; macOS and Linux only",
            std::env::consts::OS
        ));
    }
    let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
    let home = PathBuf::from(home);
    Ok(if cfg!(target_os = "macos") {
        home.join(format!("Library/LaunchAgents/{LABEL}.plist"))
    } else {
        home.join(".config/systemd/user/observer-frog.service")
    })
}

/// The old role-suffixed path, only so the sweep can find and retire it.
fn unit_path_of(role: &str) -> Result<PathBuf, String> {
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
        Err(format!("autostart is not implemented on {}", std::env::consts::OS))
    }
}

/// Render the unit for this platform. Pure, so it can be asserted on without touching the system.
pub fn render(role: &str, binary: &Path, config_path: &Path, log_path: &Path) -> Result<String, String> {
    if cfg!(target_os = "macos") {
        Ok(LAUNCHD
            .replace("{label}", LABEL)
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

/// The commands that make an installed unit actually run the binary on disk.
///
/// Pure and separate so a test can assert the shape without touching the real service manager.
/// The property that matters: after `install` the process must be **restarted**, because the copy it
/// was running has just been replaced. Enabling is not restarting, and the difference is a whole
/// version of stale code silently serving.
fn activation_argv(unit: &Path, role: &str) -> Vec<Vec<String>> {
    if cfg!(target_os = "macos") {
        let uid = std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "501".into());
        let _ = role;
        vec![
            vec!["launchctl".into(), "load".into(), "-w".into(), unit.display().to_string()],
            vec!["launchctl".into(), "kickstart".into(), "-k".into(), format!("gui/{uid}/{LABEL}")],
        ]
    } else {
        let name = unit.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        vec![
            vec!["systemctl".into(), "--user".into(), "daemon-reload".into()],
            vec!["systemctl".into(), "--user".into(), "enable".into(), name.clone()],
            vec!["systemctl".into(), "--user".into(), "restart".into(), name],
        ]
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
    for old in legacy_units() {
        if old != unit {
            retire(&old);
        }
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
    // `install` is also how an upgrade lands, and it copies the new binary over the one the service
    // is running. Enabling alone is a no-op on a service that is already up, so the old process
    // kept running the old code while the file on disk changed — every upgrade needed a manual
    // restart that nothing told the operator about. Reload, enable, then restart explicitly.
    let mut activated = true;
    for argv in activation_argv(&unit, &role) {
        let Some((bin, rest)) = argv.split_first() else { continue };
        if let Ok(o) = std::process::Command::new(bin).args(rest).output() {
            // Only the final step decides success: `load` on an already-loaded job returns non-zero
            // on some macOS versions, and that must not be reported as a failed install.
            activated &= o.status.success();
        }
    }

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
    // A unit that does not exist is a definite "no", never an "I could not tell": defaulting to
    // true here is what let a tail report a LaunchAgent it does not have.
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
                !text.contains(&format!("\"{LABEL}\" => disabled"))
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
        let target = format!("gui/{uid}/{LABEL}");
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
fn is_running(_role: &str) -> bool {
    let label = LABEL;
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
    fn installing_restarts_the_service_it_just_replaced() {
        // The bug this guards: install copies a new binary over the one the service is running, and
        // `enable` on an already-running service does nothing, so the old code kept serving while
        // the file on disk moved on. Every upgrade needs an explicit restart.
        let unit = unit_path("head").unwrap();
        let argv = activation_argv(&unit, "head");
        let flat: Vec<String> = argv.iter().map(|a| a.join(" ")).collect();
        assert!(
            flat.iter().any(|c| c.contains("restart") || c.contains("kickstart")),
            "install must restart the service, got: {flat:?}"
        );
    }

    #[test]
    fn the_unit_name_does_not_follow_the_role() {
        // The role is a setting, not an identity: switching head↔tail restarts the unit that is
        // already installed, so both roles must resolve to one path.
        assert_eq!(unit_path("head").unwrap(), unit_path("tail").unwrap());
        assert_eq!(unit_path("").unwrap(), unit_path("head").unwrap());
    }

    #[test]
    fn the_old_role_suffixed_units_are_still_findable() {
        // They have to be, or an upgrade leaves a second copy racing for the same port.
        assert_ne!(unit_path_of("head").unwrap(), unit_path("head").unwrap());
        assert!(unit_path_of("head").unwrap().to_string_lossy().contains("head"));
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
