//! Pairing: a tail asks to join a head, and a person decides.
//!
//! Discovery already tells a tail which heads exist. What it must not do is trust one, or hand one
//! a password, without the operator on the other end agreeing. So the tail *proposes* a token it
//! generated itself; the head stores the request with only the digest; and nothing is a credential
//! until someone clicks approve. That way no secret travels from head to tail, an unapproved machine
//! is not a known node, and revoking is the same act as rejecting was.
//!
//! The shape mirrors Syncthing's device handshake on purpose: a pending list, a tick, a cross.

use crate::db;
use serde_json::{json, Value};
use std::path::Path;

/// A token for a new pairing, 32 bytes of OS randomness in hex.
///
/// `getrandom` is one dependency away, but `/dev/urandom` is the same source on every platform this
/// builds for, and reading it needs no build-time machinery. Windows has no /dev, so that branch is
/// called out rather than silently producing zeros.
pub fn new_token() -> Result<String, String> {
    let mut buf = [0u8; 32];
    #[cfg(unix)]
    {
        use std::io::Read;
        let mut f = std::fs::File::open("/dev/urandom").map_err(|e| format!("no /dev/urandom: {e}"))?;
        f.read_exact(&mut buf).map_err(|e| format!("cannot read random bytes: {e}"))?;
    }
    #[cfg(not(unix))]
    {
        return Err("token generation is not implemented on this platform yet".into());
    }
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Record a tail's request against this head.
///
/// A repeat request from the same node replaces the pending one rather than stacking: a tail that
/// restarts should not require the operator to dismiss a dozen stale rows.
pub fn request(db_path: &Path, node_id: &str, name: &str, token: &str, address: &str, interval_s: i64) -> Result<(), String> {
    if node_id.trim().is_empty() {
        return Err("a pairing request needs the node's id".into());
    }
    let digest = crate::ingest::token_digest(token);
    let conn = db::connect(db_path).map_err(|e| e.to_string())?;
    conn.execute("DELETE FROM pair_requests WHERE node_id = ?1 AND status = 'pending'", [node_id])
        .map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT INTO pair_requests (node_id, name, token_sha256, address, interval_s, requested_ts, status) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending')",
        (node_id, name, digest, address, interval_s.max(1), db::now()),
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Requests waiting for a decision, newest first.
pub fn pending(db_path: &Path) -> Vec<Value> {
    let Ok(conn) = db::connect(db_path) else { return Vec::new() };
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, node_id, name, address, interval_s, requested_ts \
         FROM pair_requests WHERE status = 'pending' ORDER BY requested_ts DESC",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map([], |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?,
            "node_id": r.get::<_, String>(1)?,
            "name": r.get::<_, String>(2)?,
            "address": r.get::<_, String>(3)?,
            "interval_s": r.get::<_, i64>(4)?,
            "requested_ts": r.get::<_, i64>(5)?,
        }))
    });
    rows.map(|it| it.flatten().collect()).unwrap_or_default()
}

/// Approve a request: the machine becomes a known node and its token starts working.
///
/// Writing the token digest into `nodes` is what turns the request into a credential. Until this
/// runs, a report from that machine is an unknown token and is refused.
pub fn approve(db_path: &Path, id: i64) -> Result<String, String> {
    let conn = db::connect(db_path).map_err(|e| e.to_string())?;
    let (node_id, name, token_sha256, address, interval_s): (String, String, String, String, i64) = conn
        .query_row(
            "SELECT node_id, name, token_sha256, address, interval_s FROM pair_requests WHERE id = ?1 AND status = 'pending'",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .map_err(|_| "that pairing request is no longer waiting".to_string())?;

    // Another machine may hold the same token digest only if this is the same node returning.
    let taken: Option<String> = conn
        .query_row(
            "SELECT node_id FROM nodes WHERE token_sha256 = ?1 AND node_id <> ?2",
            (&token_sha256, &node_id),
            |r| r.get(0),
        )
        .ok();
    if let Some(other) = taken {
        return Err(format!("that token is already assigned to {other}; each machine needs its own"));
    }

    let now = db::now();
    let display = if name.trim().is_empty() { node_id.clone() } else { name.clone() };
    conn.execute(
        "INSERT INTO nodes (node_id, name, reported_name, interval_s, token_sha256, first_seen_ts, last_seen_ts) \
         VALUES (?1, ?2, ?2, ?3, ?4, ?5, ?5) \
         ON CONFLICT(node_id) DO UPDATE SET \
           token_sha256 = excluded.token_sha256, \
           interval_s = excluded.interval_s, \
           name = CASE WHEN excluded.name <> '' THEN excluded.name ELSE nodes.name END",
        (&node_id, &display, interval_s.max(1), &token_sha256, now),
    )
    .map_err(|e| e.to_string())?;
    conn.execute("UPDATE pair_requests SET status = 'approved' WHERE id = ?1", [id])
        .map_err(|e| e.to_string())?;
    Ok(format!("{node_id} approved from {address}"))
}

/// Machines currently paired with this head, newest sighting first.
///
/// Distinct from the pending list: these are trusted, and the only action that applies to one is
/// removal. `last_seen_ts` is 0 for a machine that has been paired but has not reported since.
pub fn paired(db_path: &Path) -> Vec<Value> {
    let Ok(conn) = db::connect(db_path) else { return Vec::new() };
    let Ok(mut stmt) = conn.prepare(
        "SELECT node_id, name, reported_name, last_seen_ts, interval_s, first_seen_ts \
         FROM nodes WHERE token_sha256 <> '' ORDER BY last_seen_ts DESC",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map([], |r| {
        let node_id: String = r.get(0)?;
        let name: String = r.get(1)?;
        let reported: String = r.get(2)?;
        Ok(json!({
            "node_id": node_id,
            // The device's own word wins over the enrolment name, the same rule the dashboard uses.
            "name": if reported.is_empty() { name } else { reported },
            "last_seen_ts": r.get::<_, i64>(3)?,
            "interval_s": r.get::<_, i64>(4)?,
            "paired_ts": r.get::<_, i64>(5)?,
        }))
    });
    rows.map(|it| it.flatten().collect()).unwrap_or_default()
}

/// Unpair one machine: its token and everything it ever reported.
///
/// Scoped to a single node rather than "all but me", because the operator picks a row. Removing it
/// means the next report is an unknown token, so the machine has to ask again — which is what
/// revoking trust should mean.
pub fn unpair(db_path: &Path, node_id: &str) -> Result<String, String> {
    if node_id.trim().is_empty() {
        return Err("which machine?".into());
    }
    let conn = db::connect(db_path).map_err(|e| e.to_string())?;
    for sql in [
        "DELETE FROM service_state WHERE node_id = ?1",
        "DELETE FROM service_checks WHERE node_id = ?1",
        "DELETE FROM hourly_rollup WHERE node_id = ?1",
        "DELETE FROM incidents WHERE node_id = ?1",
        "DELETE FROM ignored WHERE node_id = ?1",
        "DELETE FROM pair_requests WHERE node_id = ?1",
        "DELETE FROM nodes WHERE node_id = ?1",
    ] {
        if let Ok(mut stmt) = conn.prepare(sql) {
            let _ = stmt.execute([node_id]);
        }
    }
    Ok(format!("{node_id} removed"))
}

/// Reject a request: the row is kept as a record of the decision, but the token never becomes real.
pub fn reject(db_path: &Path, id: i64) -> Result<String, String> {
    let conn = db::connect(db_path).map_err(|e| e.to_string())?;
    let node_id: String = conn
        .query_row("SELECT node_id FROM pair_requests WHERE id = ?1 AND status = 'pending'", [id], |r| r.get(0))
        .map_err(|_| "that pairing request is no longer waiting".to_string())?;
    conn.execute("UPDATE pair_requests SET status = 'rejected' WHERE id = ?1", [id])
        .map_err(|e| e.to_string())?;
    Ok(format!("{node_id} rejected"))
}

/// Ask a head to pair with this machine.
///
/// The token goes out with the request because the tail owns it: the head stores a digest of what it
/// was given, and approval is the only thing that makes it a credential. A head that never answers,
/// or answers with a refusal, leaves the tail with a token nobody accepts — harmless, and retryable.
pub fn ask_head(head_url: &str, node_id: &str, name: &str, token: &str, interval_s: i64) -> Result<String, String> {
    let body = json!({
        "node_id": node_id,
        "name": name,
        "token": token,
        "interval_s": interval_s.max(1),
    });
    let url = format!("{}/api/pair", head_url.trim_end_matches('/'));
    let resp = ureq::post(&url)
        .timeout(std::time::Duration::from_secs(8))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string());
    // ureq 2 has no `into_json`; parse the text the same way the report path does.
    let parse = |t: String| -> Value { serde_json::from_str(&t).unwrap_or(json!({})) };
    match resp {
        Ok(r) => {
            let v = parse(r.into_string().unwrap_or_default());
            Ok(v.get("detail").and_then(|x| x.as_str()).unwrap_or("request sent").to_string())
        }
        Err(ureq::Error::Status(code, r)) => {
            let v = parse(r.into_string().unwrap_or_default());
            Err(format!(
                "the head answered {code}: {}",
                v.get("error").and_then(|x| x.as_str()).unwrap_or("no reason given")
            ))
        }
        Err(e) => Err(format!("cannot reach {url}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("of-pair-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("pair.db");
        let _ = std::fs::remove_file(&db);
        db::init(&db).unwrap();
        db
    }

    #[test]
    fn a_token_is_long_and_not_repeated() {
        let a = new_token().unwrap();
        let b = new_token().unwrap();
        assert_eq!(a.len(), 64, "32 bytes of hex");
        assert_ne!(a, b);
    }

    #[test]
    fn a_request_is_not_a_credential_until_it_is_approved() {
        let db = fresh("approve");
        request(&db, "laptop", "test-machine", "secret-token", "192.0.2.5", 30).unwrap();
        assert_eq!(pending(&db).len(), 1);
        // The whole point: before approval this token is not recognised, so the machine cannot
        // report and cannot appear in the roster.
        assert!(crate::ingest::node_for_token(&db, "secret-token").is_none());

        approve(&db, pending(&db)[0]["id"].as_i64().unwrap()).unwrap();
        assert_eq!(crate::ingest::node_for_token(&db, "secret-token").as_deref(), Some("laptop"));
        assert!(pending(&db).is_empty(), "an approved request leaves the waiting list");
    }

    #[test]
    fn rejecting_leaves_the_machine_unable_to_report() {
        let db = fresh("reject");
        request(&db, "stranger", "", "nope-token", "192.0.2.9", 30).unwrap();
        reject(&db, pending(&db)[0]["id"].as_i64().unwrap()).unwrap();
        assert!(pending(&db).is_empty());
        assert!(
            crate::ingest::node_for_token(&db, "nope-token").is_none(),
            "a rejected machine must stay unknown"
        );
    }

    #[test]
    fn a_repeat_request_replaces_rather_than_stacks() {
        let db = fresh("repeat");
        request(&db, "laptop", "old", "t1", "192.0.2.5", 30).unwrap();
        request(&db, "laptop", "new", "t2", "192.0.2.5", 30).unwrap();
        let p = pending(&db);
        assert_eq!(p.len(), 1, "a restarting tail must not pile up requests");
        assert_eq!(p[0]["name"], "new");
    }

    #[test]
    fn removing_a_paired_machine_revokes_it_and_leaves_the_others() {
        let db = fresh("unpair");
        request(&db, "keep", "", "tok-keep", "192.0.2.1", 30).unwrap();
        approve(&db, pending(&db)[0]["id"].as_i64().unwrap()).unwrap();
        request(&db, "drop", "", "tok-drop", "192.0.2.2", 30).unwrap();
        approve(&db, pending(&db)[0]["id"].as_i64().unwrap()).unwrap();
        assert_eq!(paired(&db).len(), 2);

        unpair(&db, "drop").unwrap();

        assert_eq!(paired(&db).len(), 1, "only the chosen machine goes");
        assert!(crate::ingest::node_for_token(&db, "tok-drop").is_none(), "its token is revoked");
        assert!(
            crate::ingest::node_for_token(&db, "tok-keep").is_some(),
            "removing one machine must not disturb another"
        );
    }

    #[test]
    fn one_token_cannot_be_approved_for_two_machines() {
        let db = fresh("dupe");
        request(&db, "a", "", "same", "192.0.2.1", 30).unwrap();
        approve(&db, pending(&db)[0]["id"].as_i64().unwrap()).unwrap();
        request(&db, "b", "", "same", "192.0.2.2", 30).unwrap();
        let err = approve(&db, pending(&db)[0]["id"].as_i64().unwrap()).unwrap_err();
        assert!(err.contains("already assigned"), "got: {err}");
    }
}
