//! CLI entry point: `observer-frog [status|config|catalog|add|metrics|serve]`.

use observer_frog::{catalog, config, db, metrics};
use std::path::PathBuf;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(run(&args));
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(|s| s.as_str())
}

fn has(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn run(args: &[String]) -> i32 {
    if has(args, "--version") || has(args, "-V") {
        println!("observer-frog {VERSION}");
        return 0;
    }
    if has(args, "--help") || has(args, "-h") {
        usage();
        return 0;
    }

    let config_path = flag(args, "--config").map(PathBuf::from);
    let cmd = args.iter().find(|a| !a.starts_with('-') && Some(a.as_str()) != flag(args, "--config")).cloned();

    let cfg = match config::load(config_path.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            return 2;
        }
    };
    let entries = match catalog::load(None) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("config error: {e}");
            return 2;
        }
    };

    match cmd.as_deref() {
        Some("config") => cmd_config(&cfg),
        Some("catalog") => cmd_catalog(&entries, args.get(positional_after(args, "catalog")).map(|s| s.as_str())),
        Some("discover") => cmd_discover(&entries),
        Some("add") => cmd_add(&cfg, &entries, args),
        Some("metrics") => cmd_metrics(&cfg),
        Some("enrol") => cmd_enrol(&cfg, args),
        Some("report") => cmd_report(&cfg, args),
        Some("install") => observer_frog::install::install(&cfg, config_path.as_deref()),
        Some("uninstall") => observer_frog::install::uninstall(&cfg),
        Some("serve") | None => cmd_serve(&cfg, args, config_path),
        Some("status") => cmd_status(&cfg),
        Some(other) => {
            eprintln!("unknown command {other:?}");
            usage();
            2
        }
    }
}

fn positional_after(args: &[String], word: &str) -> usize {
    args.iter().position(|a| a == word).map(|i| i + 1).unwrap_or(args.len())
}

fn usage() {
    println!(
        "observer-frog {VERSION} — status dashboard for your machines

usage: observer-frog [--config PATH] <command>

commands:
  status     show resolved configuration and database state
  config     print resolved settings
  catalog    list services that ship with a ready-made definition
  discover   find what this machine is running and print ready-to-use config blocks
  add        print the config block for a catalog service
  metrics    print this machine's status line (no server)
  enrol      register a node and its token (token from env, never the config)
  install    run this binary at boot (LaunchAgent on macOS, user unit on Linux)
  uninstall  stop running at boot, and remove the unit
  report     one-shot: probe this machine and post to a head
  serve      serve, report, or both — decided by `serve` and `report_to` in the config
               (the default when no command is given)

serve options:
  --bind ADDR       address to bind (default 127.0.0.1)
  --port N          port to bind
  --tls-cert FILE   PEM certificate; requires --tls-key
  --tls-key FILE    PEM private key; requires --tls-cert
  -v, --verbose     debug logging"
    );
}

fn cmd_status(cfg: &config::Config) -> i32 {
    let ver = db::init(&cfg.db_path).unwrap_or(0);
    println!("config      {}", cfg.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "(defaults, no file)".into()));
    println!("database    {}  (schema v{ver})", cfg.db_path.display());
    println!("node        {} / {}", cfg.node_id, cfg.node_name);
    println!("serve       {} on {}:{}  poll {}s",
        if cfg.serve { "True" } else { "False" }, cfg.bind, cfg.port, cfg.poll_interval);
    println!("services    {}", cfg.services.len());
    for s in &cfg.services {
        println!("  {:<24} {:<9} {}", s.id, s.probe, s.target);
    }
    println!("sinks       {}", if cfg.sinks.is_empty() { "none".to_string() } else { cfg.sinks.len().to_string() });
    if let Some(rp) = cfg.report_port {
        println!("reports in  {}:{}", cfg.bind, rp);
    }
    if let Some(rt) = &cfg.report_to {
        match cfg.token() {
            Ok(_) => println!("reports out {rt}  (token resolves)"),
            Err(e) => println!("reports out {rt}  (cannot resolve the token: {e})"),
        }
    }
    println!("autostart   {}", if observer_frog::install::is_installed(cfg) { "on" } else { "off" });
    0
}

fn cmd_config(cfg: &config::Config) -> i32 {
    println!("{:<14} {}", "path", cfg.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default());
    println!("{:<14} {}", "node_id", cfg.node_id);
    println!("{:<14} {}", "node_name", cfg.node_name);
    // The role is what the two settings add up to, so print it rather than making the operator
    // work it out from `serve` and `report_to`.
    let role = match (cfg.serve, cfg.report_to.is_some()) {
        (true, true) => "head + tail",
        (true, false) => "head",
        (false, true) => "tail",
        (false, false) => "idle (nothing configured)",
    };
    println!("{:<14} {}", "role", role);
    println!("{:<14} {}", "serve", if cfg.serve { "True" } else { "False" });
    println!("{:<14} {}", "bind", cfg.bind);
    println!("{:<14} {}", "port", cfg.port);
    println!("{:<14} {}", "report_port", cfg.report_port.map(|p| p.to_string()).unwrap_or_else(|| "-".into()));
    println!("{:<14} {}", "report_to", cfg.report_to.clone().unwrap_or_else(|| "-".into()));
    println!("{:<14} {}", "poll_interval", cfg.poll_interval);
    println!("{:<14} {}", "db_path", cfg.db_path.display());
    // Where this state actually lives. The whole reason v0.2 died in a power cut is that nobody
    // could tell at a glance that both paths were under /tmp.
    println!("{:<14} {}", "state_dir", config::state_home().display());
    0
}

/// Print what is running here, as config blocks the operator can paste into a tail.
fn cmd_discover(entries: &[catalog::Entry]) -> i32 {
    let listeners = observer_frog::discover::listening();
    println!("{} listening port(s) on this machine", listeners.len());

    let found = observer_frog::discover::discover(entries, "127.0.0.1");
    if found.is_empty() {
        println!("\nno catalog service recognised. Nothing here matches a shipped definition.");
    } else {
        println!("\nrecognised {} service(s):", found.len());
        for s in &found {
            println!("  {:<16} {:<8} {}", s.id, s.probe, s.target);
        }
        println!("\nAdd to a tail config with `discover = true`, or paste the blocks below:");
        for s in &found {
            println!(
                "\n[[service]]\nid = \"{}\"\nprobe = \"{}\"\ntarget = \"{}\"\nname = \"{}\"\ngroup = \"{}\"",
                s.id, s.probe, s.target, s.name, s.group
            );
        }
    }

    let left = observer_frog::discover::unmatched(entries, &listeners);
    if !left.is_empty() {
        println!("\nlistening but unrecognised (add these by hand if you care about them):");
        for l in left.iter().take(12) {
            let who = if l.process.is_empty() { "unknown" } else { l.process.as_str() };
            println!("  port {:<6} {who}", l.port);
        }
    }
    0
}

fn cmd_catalog(entries: &[catalog::Entry], query: Option<&str>) -> i32 {
    let q = query.map(|s| s.to_lowercase());
    let rows: Vec<&catalog::Entry> = entries
        .iter()
        .filter(|e| match &q {
            None => true,
            Some(q) => {
                e.id.to_lowercase().contains(q)
                    || e.name.to_lowercase().contains(q)
                    || e.group.to_lowercase().contains(q)
            }
        })
        .collect();

    let mut group = String::new();
    for e in &rows {
        if e.group != group {
            group = e.group.clone();
            println!("\n{group}");
        }
        let port = if e.default_port > 0 { format!(":{}", e.default_port) } else { String::new() };
        let sig = if e.has_signature() { "signed" } else { "" };
        println!("  {:<18} {:<22} {:<8}{:<6} {}", e.id, e.name, e.probe, port, sig);
    }
    println!("\n{} {}", rows.len(), if rows.len() == 1 { "entry" } else { "entries" });
    0
}

fn cmd_add(cfg: &config::Config, entries: &[catalog::Entry], args: &[String]) -> i32 {
    let idx = positional_after(args, "add");
    let cid = match args.get(idx) {
        Some(c) if !c.starts_with('-') => c.clone(),
        _ => {
            eprintln!("usage: observer-frog add <catalog-id> [--host H] [--port N] [--instance S]");
            return 2;
        }
    };
    let entry = match catalog::by_id(entries, &cid) {
        Some(e) => e,
        None => {
            eprintln!("no catalog entry {cid:?}; run `catalog` to list ids");
            return 2;
        }
    };
    if entry.probe == "file_age" || entry.probe == "file_json" {
        eprintln!("{cid} needs a path and threshold; add it by hand");
        return 2;
    }
    let host = flag(args, "--host").unwrap_or("127.0.0.1");
    let port = flag(args, "--port").and_then(|p| p.parse::<u16>().ok());
    let instance = flag(args, "--instance").unwrap_or("");
    let svc = match catalog::instantiate(entry, host, port, instance) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if cfg.service_ids().contains(svc.id.as_str()) {
        eprintln!(
            "service '{}' already exists in {}",
            svc.id,
            cfg.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "(your config.toml)".into())
        );
        return 1;
    }
    println!(
        "# add to {}",
        cfg.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "(your config.toml)".into())
    );
    println!("{}", catalog::to_toml(&svc));
    0
}

/// Register a node and its token. The token is read from the named environment variable so it
/// never appears in shell history or in the config file.
fn cmd_enrol(cfg: &config::Config, args: &[String]) -> i32 {
    let idx = positional_after(args, "enrol");
    let node = match args.get(idx) {
        Some(n) if !n.starts_with('-') => n.clone(),
        _ => {
            eprintln!("usage: observer-frog enrol <node-id> --token-env VAR [--name NAME] [--interval S]");
            return 2;
        }
    };
    let var = match flag(args, "--token-env") {
        Some(v) => v,
        None => {
            eprintln!("--token-env is required: the token is read from the environment, never the config file");
            return 2;
        }
    };
    let token = match std::env::var(var) {
        Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => {
            eprintln!("environment variable {var} is unset or empty");
            return 2;
        }
    };
    let name = flag(args, "--name").unwrap_or(&node);
    let interval: i64 = flag(args, "--interval").and_then(|v| v.parse().ok()).unwrap_or(cfg.poll_interval as i64);
    if let Err(e) = db::init(&cfg.db_path) {
        eprintln!("cannot open database {}: {e}", cfg.db_path.display());
        return 1;
    }
    match observer_frog::ingest::touch_node(
        &cfg.db_path, &node, name, interval, &observer_frog::ingest::token_digest(&token), db::now(),
    ) {
        Ok(()) => {
            println!("enrolled {node} (name {name:?}, interval {interval}s)");
            println!("token digest stored; the token itself was not saved");
            0
        }
        Err(e) => {
            let text = e.to_string();
            if text.contains("UNIQUE") && text.contains("token_sha256") {
                eprintln!(
                    "cannot enrol {node}: that token is already assigned to another node. \
                     Each machine needs its own token so revoking one does not revoke the rest."
                );
            } else {
                eprintln!("cannot enrol {node}: {e}");
            }
            1
        }
    }
}

/// Tail mode: probe this machine and post to a head.
fn cmd_report(cfg: &config::Config, args: &[String]) -> i32 {
    let head = match flag(args, "--to") {
        Some(h) if !h.trim().is_empty() => h.trim().to_string(),
        _ => {
            eprintln!("usage: observer-frog report --to https://head:8770 --token-env VAR");
            return 2;
        }
    };
    let var = match flag(args, "--token-env") {
        Some(v) => v,
        None => {
            eprintln!("--token-env is required: the token is read from the environment");
            return 2;
        }
    };
    let token = match std::env::var(var) {
        Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => {
            eprintln!("environment variable {var} is unset or empty");
            return 2;
        }
    };
    let halt = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let opts = observer_frog::tail::TailOptions { report_to: head, token };
    observer_frog::tail::run(cfg.clone(), opts, halt)
}

fn cmd_metrics(cfg: &config::Config) -> i32 {
    let card = metrics::device_card(&cfg.node_id);
    let s = |k: &str| -> String {
        match card.get(k) {
            Some(serde_json::Value::String(v)) => v.clone(),
            Some(serde_json::Value::Null) | None => "n/a".to_string(),
            Some(v) => v.to_string(),
        }
    };
    println!("{}  ({})  {}", s("name"), s("kind"), s("detail"));
    println!("  cpu      {}%   uptime {} h", s("cpu_pct"), s("uptime_h"));
    println!("  memory   {}  ({}%)", s("mem_used"), s("mem_pct"));
    if card.get("swap_used").map(|v| !v.is_null()).unwrap_or(false) {
        println!("  swap     {}", s("swap_used"));
    }
    if !s("disk_used").is_empty() {
        println!("  disk     {}", s("disk_used"));
    }
    if card.get("thermal").map(|v| !v.is_null()).unwrap_or(false) {
        println!("  thermal  {}", s("thermal"));
    }
    if card.get("temp_c").map(|v| !v.is_null()).unwrap_or(false) {
        println!("  temp     {} °C", s("temp_c"));
    }
    0
}

fn cmd_serve(cfg: &config::Config, args: &[String], config_path: Option<PathBuf>) -> i32 {
    let mut cfg = cfg.clone();
    if let Some(b) = flag(args, "--bind") {
        cfg.bind = b.to_string();
    }
    if let Some(p) = flag(args, "--port").and_then(|v| v.parse::<u16>().ok()) {
        cfg.port = p;
    }
    let cert = flag(args, "--tls-cert");
    let key = flag(args, "--tls-key");
    if cert.is_some() != key.is_some() {
        eprintln!("--tls-cert and --tls-key must be given together");
        return 2;
    }
    let verbose = has(args, "-v") || has(args, "--verbose");
    observer_frog::runtime::init_logging(verbose);

    if cfg.bind != "127.0.0.1" && cfg.bind != "localhost" && cfg.bind != "::1" {
        eprintln!(
            "binding {}: this is reachable from your network. Put it behind your proxy or a VPN.",
            cfg.bind
        );
    }
    // One binary, both roles, one runtime. A node always binds its own port — so a tail has its
    // own settings page on its own IP — and reports upstream when `report_to` is set. The only
    // thing `serve` decides is whether this node also sweeps other nodes for silence.
    // A node with neither duty is idle, not broken. It keeps its own dashboard, its own probes and
    // its own Settings page, and says plainly in the UI that no head is paired — instead of exiting
    // and leaving a supervisor to restart it forever.
    if cfg.report_to.is_some() {
        if let Err(e) = cfg.token() {
            // Warn, do not exit: the operator can still open Settings and fix the token from there.
            eprintln!("cannot report yet: {e}");
        }
    }

    if let Err(e) = db::init(&cfg.db_path) {
        eprintln!("cannot open database {}: {e}", cfg.db_path.display());
        return 1;
    }

    let tls = match (cert, key) {
        (Some(c), Some(k)) => Some((PathBuf::from(c), PathBuf::from(k))),
        _ => None,
    };
    observer_frog::runtime::run(cfg, config_path, tls)
}
