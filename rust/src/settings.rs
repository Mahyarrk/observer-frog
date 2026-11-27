//! Editing the config from the browser.
//!
//! The GUI is a config editor, nothing more: the runtime already watches the file's mtime and
//! rebuilds itself, so writing a file here is the whole feature. No new machinery, no restart.
//!
//! **Byte-preserving by design.** The operator wrote comments in this file. A round-trip through
//! `toml::Table` would silently delete every one of them, so edits are line surgery on the section
//! that changes and the rest of the file is left exactly as it was.
//!
//! No authentication. That is a stated v0.3 decision: the listener binds loopback by default and
//! anyone who can reach the port can reconfigure the node. Auth is v0.4.

use std::path::{Path, PathBuf};

/// Replace the value of `key` inside `[section]`, creating the section if absent.
///
/// Returns whether the file changed, so a no-op write does not touch the mtime and make the
/// runtime reload for nothing.
fn set_in_section(text: &str, section: &str, key: &str, value: &str) -> (String, bool) {
    let header = format!("[{section}]");
    let mut out: Vec<String> = Vec::new();
    let mut in_section = false;
    let mut replaced = false;
    let mut section_seen = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            // Leaving the section without having found the key: insert it there, before the next
            // header swallows the position.
            if in_section && !replaced {
                out.push(format!("{key} = {value}"));
                replaced = true;
            }
            in_section = trimmed == header;
            if in_section {
                section_seen = true;
            }
            out.push(line.to_string());
            continue;
        }
        if in_section {
            let matches_key = trimmed
                .split_once('=')
                .map(|(k, _)| k.trim() == key)
                .unwrap_or(false);
            if matches_key {
                out.push(format!("{key} = {value}"));
                replaced = true;
                continue;
            }
        }
        out.push(line.to_string());
    }
    if in_section && !replaced {
        out.push(format!("{key} = {value}"));
        replaced = true;
    }
    if !section_seen {
        if !out.is_empty() && !out.last().map(|l| l.trim().is_empty()).unwrap_or(true) {
            out.push(String::new());
        }
        out.push(header);
        out.push(format!("{key} = {value}"));
        replaced = true;
    }
    let mut joined = out.join("\n");
    if !joined.ends_with('\n') {
        joined.push('\n');
    }
    (joined, replaced)
}

/// Set a top-level key (not inside any table).
fn set_top_level(text: &str, key: &str, value: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    let mut seen_first_header = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            if !seen_first_header && !replaced {
                out.push(format!("{key} = {value}"));
                replaced = true;
            }
            seen_first_header = true;
            out.push(line.to_string());
            continue;
        }
        if !seen_first_header
            && trimmed
                .split_once('=')
                .map(|(k, _)| k.trim() == key)
                .unwrap_or(false)
        {
            out.push(format!("{key} = {value}"));
            replaced = true;
            continue;
        }
        out.push(line.to_string());
    }
    if !replaced {
        out.insert(0, format!("{key} = {value}"));
    }
    let mut joined = out.join("\n");
    if !joined.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

/// Replace the `[[sink]]` block of one kind, leaving every other block untouched.
///
/// `sink` is an array of tables, which `set_top_level` and `set_in_section` cannot express: a block
/// spans several lines and reappears further down the file. The whole file is walked once, blocks of
/// other kinds are copied verbatim, and the target block is rebuilt from scratch.
///
/// Two rules that exist because getting them wrong is silent:
///   `enabled` is always written explicitly, so a switched-off route can never be mistaken for a
///   config written before the field existed, which the reader defaults to true.
///   A block belonging to another kind is never rewritten, not even to reformat it, because that is
///   how an operator's disabled route comes back to life.
pub fn write_sink(text: &str, kind: &str, fields: &serde_json::Value) -> String {
    // The kind lives inside the block, so a block cannot be identified until it has been collected.
    // One walk: gather blocks, then decide.
    let mut blocks: Vec<Vec<String>> = Vec::new();
    let mut preamble: Vec<String> = Vec::new();
    let mut current: Option<Vec<String>> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "[[sink]]" {
            if let Some(b) = current.take() {
                blocks.push(b);
            }
            current = Some(vec![line.to_string()]);
            continue;
        }
        if trimmed.starts_with('[') {
            if let Some(b) = current.take() {
                blocks.push(b);
            }
            preamble.push(line.to_string());
            continue;
        }
        match current.as_mut() {
            Some(b) => b.push(line.to_string()),
            None => preamble.push(line.to_string()),
        }
    }
    if let Some(b) = current.take() {
        blocks.push(b);
    }

    let kind_of = |block: &[String]| -> String {
        for l in block {
            if let Some((k, v)) = l.trim().split_once('=') {
                if k.trim() == "type" {
                    return v.trim().trim_matches('"').to_string();
                }
            }
        }
        String::new()
    };

    let mut kept: Vec<Vec<String>> = Vec::new();
    let mut replaced = false;
    for b in blocks {
        if kind_of(&b) == kind {
            if fields.is_null() {
                replaced = true;
                continue; // cleared: drop the block entirely
            }
            kept.push(rebuild_sink(&b, kind, fields));
            replaced = true;
        } else {
            kept.push(b);
        }
    }
    if !replaced && !fields.is_null() {
        kept.push(rebuild_sink(&[], kind, fields));
    }

    let mut out: Vec<String> = preamble;
    while out.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        out.pop();
    }
    if !out.is_empty() && !kept.is_empty() {
        out.push(String::new());
    }
    for (i, b) in kept.iter().enumerate() {
        if i > 0 {
            out.push(String::new());
        }
        out.extend(b.iter().cloned());
    }
    let mut joined = out.join("\n");
    if !joined.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

/// One `[[sink]]` block. Values present in `fields` are set, values absent keep what the block had,
/// except `enabled`, which is always written.
fn rebuild_sink(old: &[String], kind: &str, fields: &serde_json::Value) -> Vec<String> {
    let get = |k: &str| old.iter().find_map(|l| {
        let (a, b) = l.trim().split_once('=')?;
        (a.trim() == k).then(|| b.trim().to_string())
    });
    let str_of = |k: &str| fields.get(k).and_then(|v| v.as_str()).map(|s| format!("{s:?}"));
    let bool_of = |k: &str| fields.get(k).and_then(|v| v.as_bool()).map(|b| b.to_string());

    let mut out = vec!["[[sink]]".to_string()];
    out.push(format!("type = {kind:?}"));
    // Explicit always: a missing key would read back as enabled and silently resurrect a route the
    // operator switched off.
    let enabled = bool_of("enabled")
        .or_else(|| get("enabled"))
        .unwrap_or_else(|| "true".to_string());
    out.push(format!("enabled = {enabled}"));
    out.push(format!("url = {}", str_of("url").or_else(|| get("url")).unwrap_or_else(|| "\"\"".into())));
    // Only ntfy has a topic; writing an empty one to a webhook would be noise in the file.
    if kind == "ntfy" {
        out.push(format!("topic = {}", str_of("topic").or_else(|| get("topic")).unwrap_or_else(|| "\"\"".into())));
    }
    let token = str_of("token_env").or_else(|| get("token_env")).unwrap_or_else(|| "\"\"".into());
    out.push(format!("token_env = {token}"));
    let proxy = bool_of("use_proxy")
        .or_else(|| get("use_proxy"))
        .unwrap_or_else(|| "false".to_string());
    out.push(format!("use_proxy = {proxy}"));
    out
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    // Write beside the target and rename, so a crash mid-write cannot leave a truncated config
    // that the runtime would then load as broken.
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))?;
    Ok(())
}

/// Apply the fields the settings page can change. Only the keys present in `changes` move.
pub fn apply(path: &Path, changes: &serde_json::Value) -> Result<Vec<String>, String> {
    let mut text = std::fs::read_to_string(path).unwrap_or_default();
    let mut changed: Vec<String> = Vec::new();

    fn note(changed: &mut Vec<String>, name: &str) {
        changed.push(name.to_string());
    }

    // TLS paths live at the top level, beside report_to. Written with the same quoting rule as
    // every other string, because a bare empty value is invalid TOML and takes the node down.
    for key in ["tls_cert", "tls_key"] {
        if let Some(v) = changes.get(key).and_then(|x| x.as_str()) {
            let before = text.clone();
            text = set_top_level(&text, key, &format!("{v:?}"));
            if before != text {
                note(&mut changed, key);
            }
        }
    }

    // A route's settings arrive as `{"sink": {"kind": "ntfy", "enabled": true, ...}}`. A null sink
    // means the operator cleared that route, which is the only case that deletes the block.
    if let Some(sink) = changes.get("sink") {
        let kind = sink.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        if !crate::config::SINK_TYPES.contains(&kind) && !sink.is_null() {
            return Err(format!(
                "unknown sink type {kind:?}; expected one of {}",
                crate::config::SINK_TYPES.join(", ")
            ));
        }
        let before = text.clone();
        text = write_sink(&text, kind, sink);
        if before != text {
            note(&mut changed, "sink");
        }
    }

    // The head's name, learned by a tail from its answers. Written at the top level, which is what
    // `config` reads, so it survives the restart that would otherwise forget who the head was.
    if let Some(v) = changes.get("head_name").and_then(|x| x.as_str()) {
        let before = text.clone();
        text = set_top_level(&text, "head_name", &format!("{v:?}"));
        if before != text {
            note(&mut changed, "head_name");
        }
    }

    if let Some(v) = changes.get("node_id").and_then(|x| x.as_str()) {
        let before = text.clone();
        text = set_in_section(&text, "node", "id", &format!("{v:?}")).0;
        if before != text {
            note(&mut changed, "node.id");
        }
    }
    if let Some(v) = changes.get("listen").and_then(|x| x.as_bool()) {
        // Must be `[serve] enabled`, not a top-level `serve = …`: the file already has a `[serve]`
        // table for bind/port, and a bare `serve =` beside it is a duplicate key that TOML refuses,
        // taking the whole config — and the node — down with it.
        let before = text.clone();
        text = set_in_section(&text, "serve", "enabled", if v { "true" } else { "false" }).0;
        if before != text {
            note(&mut changed, "serve.enabled");
        }
    }
    if let Some(v) = changes.get("bind").and_then(|x| x.as_str()) {
        let before = text.clone();
        text = set_in_section(&text, "serve", "bind", &format!("{v:?}")).0;
        if before != text {
            note(&mut changed, "serve.bind");
        }
    }
    if let Some(v) = changes.get("port").and_then(|x| x.as_u64()) {
        let before = text.clone();
        text = set_in_section(&text, "serve", "port", &v.to_string()).0;
        if before != text {
            note(&mut changed, "serve.port");
        }
    }
    if let Some(v) = changes.get("report_port").and_then(|x| x.as_u64()) {
        let before = text.clone();
        text = set_in_section(&text, "serve", "report_port", &v.to_string()).0;
        if before != text {
            note(&mut changed, "serve.report_port");
        }
    }
    if let Some(v) = changes.get("report_to").and_then(|x| x.as_str()) {
        let before = text.clone();
        // A bare empty value is not valid TOML and makes the whole file unreadable, which then
        // looks like "the node will not start" with no hint why.
        let value = format!("{v:?}");
        text = set_top_level(&text, "report_to", &value);
        if before != text {
            note(&mut changed, "report_to");
        }
    }
    if let Some(v) = changes.get("poll_interval").and_then(|x| x.as_u64()) {
        // Refused here too, so a bad value cannot enter the file from the GUI even if the interface
        // stops checking. The sentence is the same one the loader uses.
        let v = crate::config::check_poll_interval(v)
            .map_err(|e| format!("poll_interval: {e}"))?;
        let before = text.clone();
        text = set_top_level(&text, "poll_interval", &v.to_string());
        if before != text {
            note(&mut changed, "poll_interval");
        }
    }

    if changed.is_empty() {
        return Ok(changed);
    }
    // Never let an unparseable file reach disk. Nothing exits at startup any more, so this is no
    // longer about spin loops: a config the loader cannot read leaves the node with no settings at
    // all, and the operator's only way back in is editing the file by hand.
    let probe = path.with_extension("probe.toml");
    std::fs::write(&probe, &text).map_err(|e| format!("cannot check the new config: {e}"))?;
    let parsed = crate::config::load(Some(&probe));
    let _ = std::fs::remove_file(&probe);
    parsed.map_err(|e| format!("that change would write a config that does not parse: {e}"))?;
    write(path, &text)?;
    Ok(changed)
}

/// Add a `[[service]]` block. Appending is safe: every service is its own table.
///
/// The id is checked against the file first, because two blocks with one id make the whole config
/// fail to parse — and that failure would take the dashboard down on the next reload, not here.
pub fn add_service(path: &Path, block: &str, id: &str) -> Result<(), String> {
    if id.trim().is_empty() {
        return Err("a service needs an id".into());
    }
    let text = std::fs::read_to_string(path).unwrap_or_default();
    if service_ids_in(&text).iter().any(|x| x == id) {
        return Err(format!("a service with id {id:?} already exists"));
    }
    // An id is what the code keys on; a display name is what the operator reads. Adding the
    // catalog's `hermes` to a dashboard already watching `hermes_api` gave two cards both labelled
    // "Hermes Agent", which is indistinguishable on screen. Refuse by name too, and say which id
    // already holds it so the answer is actionable rather than just "no".
    //
    // A suffixed id (`syncthing-8385`) is a deliberate second instance, so it is allowed the same
    // name: the cards are distinguishable by the port the operator asked for, and blocking it left
    // no way to watch two copies of anything.
    if let Some(name) = block_field(block, "name") {
        if let Some(existing) = service_with_name(&text, &name) {
            let is_second_instance = id.contains('-') && existing != id;
            if !is_second_instance {
                return Err(format!(
                    "a service already shows as {name:?} (id {existing:?}); rename it or remove that one first"
                ));
            }
        }
    }
    let mut out = text.clone();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(block.trim_end());
    out.push('\n');
    write(path, &out)
}

/// Remove the `[[service]]` block whose `id` matches.
pub fn remove_service(path: &Path, id: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();

    // Each block spans from its `[[service]]` header to the next header of any kind.
    let headers: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim().starts_with('['))
        .map(|(i, _)| i)
        .collect();

    for (n, &start) in headers.iter().enumerate() {
        if lines[start].trim() != "[[service]]" {
            continue;
        }
        let end = headers.get(n + 1).copied().unwrap_or(lines.len());
        let block_id = lines[start..end]
            .iter()
            .find_map(|l| {
                let t = l.trim();
                t.strip_prefix("id")
                    .and_then(|r| r.trim().strip_prefix('='))
                    .map(|v| v.trim().trim_matches('"').to_string())
            })
            .unwrap_or_default();
        if block_id != id {
            continue;
        }
        let mut skip = end;
        if skip < lines.len() && lines[skip].trim().is_empty() {
            skip += 1; // swallow one separator so removals do not leave gaps
        }
        let head = lines[..start].join("\n");
        let tail = lines[skip..].join("\n");
        let mut out = head;
        if !out.is_empty() && !tail.is_empty() {
            out.push('\n');
        }
        out.push_str(&tail);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        return write(path, &out);
    }
    Err(format!("no such service {id:?}"))
}

/// Read one `key = "value"` from a config block.
fn block_field(block: &str, key: &str) -> Option<String> {
    block.lines().find_map(|l| {
        let t = l.trim();
        t.strip_prefix(key)
            .and_then(|r| r.trim_start().strip_prefix('='))
            .map(|v| v.trim().trim_matches('"').to_string())
            .filter(|v| !v.is_empty())
    })
}

/// The id of the existing service whose display name matches, if any.
pub fn service_with_name(text: &str, name: &str) -> Option<String> {
    let name = name.trim().to_lowercase();
    for block in text.split("[[service]]").skip(1) {
        if block_field(block, "name").map(|n| n.to_lowercase()) == Some(name.clone()) {
            return block_field(block, "id");
        }
    }
    None
}

/// Every `id` in a `[[service]]` block, so a duplicate can be refused before it breaks the config.
pub fn service_ids_in(text: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut in_service = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_service = t == "[[service]]";
            continue;
        }
        if in_service {
            if let Some(rest) = t.strip_prefix("id") {
                if let Some(v) = rest.trim().strip_prefix('=') {
                    ids.push(v.trim().trim_matches('"').to_string());
                }
            }
        }
    }
    ids
}

/// Path this GUI edits.
pub fn target(cfg: &crate::config::Config) -> PathBuf {
    cfg.path.clone().unwrap_or_else(crate::config::default_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sink_block_round_trips_through_the_parser() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("c.toml");
        std::fs::write(&f, "poll_interval = 30\n").unwrap();

        let text = write_sink(
            &std::fs::read_to_string(&f).unwrap(),
            "ntfy",
            &serde_json::json!({
                "kind": "ntfy", "enabled": true, "url": "https://ntfy.example",
                "topic": "frog", "token_env": "", "use_proxy": false
            }),
        );
        assert!(text.contains("poll_interval = 30"), "top-level keys survive");
        std::fs::write(&f, &text).unwrap();

        let cfg = crate::config::load(Some(&f)).unwrap();
        assert_eq!(cfg.sinks.len(), 1);
        let s = &cfg.sinks[0];
        assert_eq!(s.kind, "ntfy");
        assert!(s.enabled);
        assert_eq!(s.url, "https://ntfy.example");
        assert_eq!(s.topic, "frog");
    }

    #[test]
    fn a_disabled_route_stays_disabled_and_its_neighbour_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("c.toml");
        std::fs::write(&f, "").unwrap();

        let text = write_sink(
            "",
            "ntfy",
            &serde_json::json!({ "kind": "ntfy", "enabled": true, "url": "https://n.example", "topic": "t" }),
        );
        let text = write_sink(
            &text,
            "webhook",
            &serde_json::json!({ "kind": "webhook", "enabled": false, "url": "https://w.example" }),
        );
        assert!(text.contains("enabled = false"), "{text}");
        std::fs::write(&f, &text).unwrap();

        // Editing ntfy must not resurrect the webhook.
        let text2 = write_sink(
            &std::fs::read_to_string(&f).unwrap(),
            "ntfy",
            &serde_json::json!({ "kind": "ntfy", "url": "https://changed.example" }),
        );
        std::fs::write(&f, &text2).unwrap();
        let cfg = crate::config::load(Some(&f)).unwrap();
        let hook = cfg.sinks.iter().find(|s| s.kind == "webhook").unwrap();
        assert!(!hook.enabled, "a disabled route must survive its neighbour being edited");
        assert_eq!(hook.url, "https://w.example", "and keep its fields");
        let n = cfg.sinks.iter().find(|s| s.kind == "ntfy").unwrap();
        assert_eq!(n.url, "https://changed.example");
        assert_eq!(n.topic, "t", "unset fields keep what they had");
    }

    #[test]
    fn clearing_a_route_removes_only_that_block() {
        let text = write_sink(
            "",
            "ntfy",
            &serde_json::json!({ "kind": "ntfy", "url": "https://n.example", "topic": "t" }),
        );
        let text = write_sink(
            &text,
            "webhook",
            &serde_json::json!({ "kind": "webhook", "url": "https://w.example" }),
        );
        let cleared = write_sink(&text, "ntfy", &serde_json::Value::Null);

        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("c.toml");
        std::fs::write(&f, &cleared).unwrap();
        let cfg = crate::config::load(Some(&f)).unwrap();
        assert_eq!(cfg.sinks.len(), 1);
        assert_eq!(cfg.sinks[0].kind, "webhook");
    }

    #[test]
    fn a_comment_survives_an_edit() {
        // This is the whole reason the module is line surgery instead of a TOML round-trip.
        let before = "# my notes\nnode_id = \"local\"\n\n[serve]\n# why this port\nport = 8770\n";
        let after = set_top_level(before, "poll_interval", "60");
        assert!(after.contains("# my notes"), "dropped a top-level comment");
        assert!(after.contains("# why this port"), "dropped a section comment");
        assert!(after.contains("poll_interval = 60"));
    }

    #[test]
    fn an_existing_key_is_replaced_not_duplicated() {
        let before = "[serve]\nport = 8770\nbind = \"127.0.0.1\"\n";
        let (after, ok) = set_in_section(before, "serve", "port", "9000");
        assert!(ok);
        assert_eq!(after.matches("port = ").count(), 1, "key duplicated: {after}");
        assert!(after.contains("port = 9000"));
        assert!(after.contains("bind = \"127.0.0.1\""), "clobbered a neighbour");
    }

    fn tmpfile(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, body).unwrap();
        (dir, p)
    }

    #[test]
    fn a_service_can_be_added_and_removed() {
        let body = "poll_interval = 30\n\n[node]\nid = \"local\"\n\n[[service]]\nid = \"keepme\"\nprobe = \"tcp\"\ntarget = \"127.0.0.1:22\"\n";
        let (_d, p) = tmpfile(body);
        add_service(&p, "[[service]]\nid = \"newone\"\nprobe = \"tcp\"\ntarget = \"127.0.0.1:99\"", "newone")
            .expect("adding must succeed");
        let after = std::fs::read_to_string(&p).unwrap();
        assert_eq!(service_ids_in(&after), vec!["keepme", "newone"]);

        remove_service(&p, "newone").expect("removing must succeed");
        let after = std::fs::read_to_string(&p).unwrap();
        assert_eq!(service_ids_in(&after), vec!["keepme"], "wrong block removed: {after}");
        assert!(after.contains("[node]"), "removal ate the node table: {after}");
    }

    #[test]
    fn a_duplicate_display_name_is_refused() {
        // Two cards both labelled "Hermes Agent" are indistinguishable on screen even though their
        // ids differ, so a name collision is refused the same way an id collision is.
        let body = "[[service]]\nid = \"hermes_api\"\nname = \"Hermes Agent\"\nprobe = \"http\"\ntarget = \"http://127.0.0.1:9119/\"\n";
        let (_d, p) = tmpfile(body);
        let block = "[[service]]\nid = \"hermes\"\nname = \"Hermes Agent\"\nprobe = \"http\"\ntarget = \"http://127.0.0.1:9119/\"";
        let err = add_service(&p, block, "hermes").unwrap_err();
        assert!(err.contains("hermes_api"), "the error must name the id already using it: {err}");
        // And nothing was appended.
        assert_eq!(service_ids_in(&std::fs::read_to_string(&p).unwrap()), vec!["hermes_api"]);
    }

    #[test]
    fn a_second_instance_is_allowed_the_same_display_name() {
        // Two sonarrs on two ports are two services; refusing the name made the second one
        // impossible to add at all.
        let body = "[[service]]\nid = \"sonarr\"\nname = \"Sonarr\"\nprobe = \"tcp\"\ntarget = \"127.0.0.1:8989\"\n";
        let (_d, p) = tmpfile(body);
        let second = "[[service]]\nid = \"sonarr-8990\"\nname = \"Sonarr\"\nprobe = \"tcp\"\ntarget = \"127.0.0.1:8990\"\n";
        assert!(
            add_service(&p, second, "sonarr-8990").is_ok(),
            "a suffixed id is a deliberate second instance"
        );
        assert_eq!(service_ids_in(&std::fs::read_to_string(&p).unwrap()).len(), 2);
    }

    #[test]
    fn a_duplicate_id_is_refused_before_it_corrupts_the_file() {
        let body = "[[service]]\nid = \"dup\"\nprobe = \"tcp\"\ntarget = \"127.0.0.1:1\"\n";
        let (_d, p) = tmpfile(body);
        let err = add_service(&p, "[[service]]\nid = \"dup\"", "dup").unwrap_err();
        assert!(err.contains("already exists"), "got {err}");
        // And the file is untouched, so it still parses.
        assert_eq!(service_ids_in(&std::fs::read_to_string(&p).unwrap()), vec!["dup"]);
    }

    #[test]
    fn removing_the_last_block_leaves_a_valid_file() {
        let body = "poll_interval = 30\n\n[[service]]\nid = \"only\"\nprobe = \"tcp\"\ntarget = \"x\"\n";
        let (_d, p) = tmpfile(body);
        remove_service(&p, "only").unwrap();
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(!after.contains("[[service]]"));
        assert!(after.contains("poll_interval = 30"), "lost the top level: {after}");
    }

    #[test]
    fn a_missing_section_is_created() {
        let (after, ok) = set_in_section("node_id = \"x\"\n", "serve", "port", "8770");
        assert!(ok);
        assert!(after.contains("[serve]"));
        assert!(after.contains("port = 8770"));
    }

    #[test]
    fn a_key_missing_from_an_existing_section_lands_inside_it() {
        // The failure this guards: appending after the last line of [serve] puts the key under
        // whichever section happens to come next.
        let before = "[serve]\nport = 8770\n\n[service]\nid = \"ssh\"\n";
        let (after, _) = set_in_section(before, "serve", "report_port", "8771");
        let serve_at = after.find("[serve]").unwrap();
        let service_at = after.find("[service]").unwrap();
        let key_at = after.find("report_port").unwrap();
        assert!(key_at > serve_at && key_at < service_at, "key landed in the wrong section: {after}");
    }
}
