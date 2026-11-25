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

    if let Some(v) = changes.get("node_id").and_then(|x| x.as_str()) {
        let before = text.clone();
        text = set_in_section(&text, "node", "id", &format!("{v:?}")).0;
        if before != text {
            note(&mut changed, "node.id");
        }
    }
    if let Some(v) = changes.get("listen").and_then(|x| x.as_bool()) {
        let before = text.clone();
        text = set_top_level(&text, "serve", if v { "true" } else { "false" });
        if before != text {
            note(&mut changed, "serve");
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
        let value = if v.trim().is_empty() { "".to_string() } else { format!("{v:?}") };
        text = set_top_level(&text, "report_to", &value);
        if before != text {
            note(&mut changed, "report_to");
        }
    }
    if let Some(v) = changes.get("poll_interval").and_then(|x| x.as_u64()) {
        let before = text.clone();
        text = set_top_level(&text, "poll_interval", &v.max(1).to_string());
        if before != text {
            note(&mut changed, "poll_interval");
        }
    }

    if changed.is_empty() {
        return Ok(changed);
    }
    write(path, &text)?;
    Ok(changed)
}

/// Path this GUI edits.
pub fn target(cfg: &crate::config::Config) -> PathBuf {
    cfg.path.clone().unwrap_or_else(crate::config::default_path)
}

#[cfg(test)]
mod tests {
    use super::*;

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
