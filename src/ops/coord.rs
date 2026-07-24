//! Coordination opcode handler (ADR-009). Decodes the JSON body carried in the
//! `COORD` opcode's `s` field (a `{"verb": ..., ...}` object), opens the
//! per-workspace coordination store **fresh per call** (like `kgl_query`), and
//! dispatches the verb.
//!
//! ## Fail-open (ADR-009 D7 / #1053 lesson 2)
//!
//! Every failure path here is a soft [`Response::err`], never a panic: a
//! disabled feature, a bad request, or an unopenable/broken store all return an
//! error `Response` so the agent's turn continues. Nothing in this module may
//! `unwrap`/`expect` on a store or I/O result.
//!
//! Slice 1 (#1058) implements the identity verbs `register_agent` and
//! `list_agents`. Later slices add messaging and reservation verbs to the same
//! `match`.

use serde_json::{json, Value};

use crate::coordination::{names, workspace_db_path, AgentRecord, CoordinationStore};
use crate::protocol::{Op, Response};
use crate::session::Session;

/// Soft error codes for coordination (kept distinct from the generic dispatch
/// codes so callers/tests can classify). All are non-fatal by construction.
const ERR_DISABLED: u32 = 40;
const ERR_BAD_REQUEST: u32 = 41;
const ERR_STORE_UNAVAILABLE: u32 = 42;

/// Entry point from `ops::dispatch_op` for the `COORD` opcode.
pub fn coord(session: &mut Session, op: &Op) -> Response {
    // The JSON body rides in `s`. Parse it into a `{verb, ...}` object.
    let body: Value = match &op.s {
        Some(raw) => match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => {
                return Response::err(ERR_BAD_REQUEST, &format!("coord: invalid JSON body: {e}"))
            }
        },
        None => return Response::err(ERR_BAD_REQUEST, "coord: missing JSON body in 's'"),
    };
    let verb = match body.get("verb").and_then(|v| v.as_str()) {
        Some(v) => v.to_string(),
        None => return Response::err(ERR_BAD_REQUEST, "coord: missing 'verb'"),
    };

    let cfg = &session.cfg.coordination;
    if !cfg.enabled {
        return Response::err(
            ERR_DISABLED,
            "coordination is disabled ([coordination] enabled=false)",
        );
    }

    // Open the store fresh per call. A failure here is fail-open: soft error,
    // agent continues (constraint 2).
    let base = cfg.resolved_db_dir();
    let db_path = workspace_db_path(&base, &session.workspace);
    let store = match CoordinationStore::open_with(&db_path, cfg.effective_busy_timeout_ms()) {
        Ok(s) => s,
        Err(e) => {
            return Response::err(
                ERR_STORE_UNAVAILABLE,
                &format!("coordination store unavailable: {e}"),
            )
        }
    };

    let now = chrono::Utc::now().to_rfc3339();
    match verb.as_str() {
        "register_agent" => register_agent(&store, &body, session, &now),
        "list_agents" => list_agents(&store, &body, cfg.effective_inbox_default_limit()),
        other => Response::err(ERR_BAD_REQUEST, &format!("coord: unknown verb '{other}'")),
    }
}

fn register_agent(
    store: &CoordinationStore,
    body: &Value,
    session: &Session,
    now: &str,
) -> Response {
    // Name: caller-supplied (validated) or minted. Fall back to the session's
    // external id only for the profile field, never as the name.
    let requested = body.get("name").and_then(|v| v.as_str());
    let name = match requested {
        Some(n) => {
            if !names::is_valid(n) {
                return Response::err(
                    ERR_BAD_REQUEST,
                    "coord: 'name' must be ASCII alphanumeric, 1..=64 chars",
                );
            }
            n.to_string()
        }
        None => match mint_available_name(store) {
            Some(n) => n,
            None => {
                return Response::err(
                    ERR_STORE_UNAVAILABLE,
                    "coord: could not mint a free agent name",
                )
            }
        },
    };

    let session_id = body
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| session.external_session_id.clone());
    let program = body.get("program").and_then(|v| v.as_str());
    let model = body.get("model").and_then(|v| v.as_str());
    let task = body.get("task").and_then(|v| v.as_str());

    match store.register_agent(&name, session_id.as_deref(), program, model, task, now) {
        Ok(rec) => Response::ok(json!({ "agent": agent_json(&rec) })),
        Err(e) => Response::err(
            ERR_STORE_UNAVAILABLE,
            &format!("coord: register failed: {e}"),
        ),
    }
}

/// Try to mint a name not already taken, bounded by the name space so a nearly
/// full store can't loop unbounded (constraint 1). Salt the seed with the clock
/// and attempt index so retries differ.
fn mint_available_name(store: &CoordinationStore) -> Option<String> {
    // Seed from the wall clock (nanoseconds, not the second-granular `now`
    // string) mixed with the process id, so two processes minting in the same
    // second start from different candidate sequences and don't churn the same
    // collision-retry walk on the shared WAL DB.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let base_seed = nanos ^ ((std::process::id() as u64) << 32);
    let attempts = names::name_space().min(256);
    for i in 0..attempts {
        let candidate = names::mint(base_seed.wrapping_add(i as u64));
        match store.agent(&candidate) {
            Ok(None) => return Some(candidate),
            Ok(Some(_)) => continue,
            // A read error mid-mint is treated as "unavailable" — fail-open.
            Err(_) => return None,
        }
    }
    None
}

fn list_agents(store: &CoordinationStore, body: &Value, default_limit: i64) -> Response {
    let limit = body
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(default_limit);
    match store.list_agents(limit) {
        Ok(agents) => {
            let arr: Vec<Value> = agents.iter().map(agent_json).collect();
            Response::ok(json!({ "agents": arr, "count": arr.len() }))
        }
        Err(e) => Response::err(ERR_STORE_UNAVAILABLE, &format!("coord: list failed: {e}")),
    }
}

fn agent_json(rec: &AgentRecord) -> Value {
    json!({
        "name": rec.name,
        "session_id": rec.session_id,
        "program": rec.program,
        "model": rec.model,
        "task": rec.task,
        "inception_ts": rec.inception_ts,
        "last_seen_ts": rec.last_seen_ts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::Arc;

    fn session_in(dir: &std::path::Path) -> Session {
        Session::new(dir.to_path_buf(), Arc::new(Config::default()))
    }

    fn call(session: &mut Session, body: Value) -> Response {
        let op = Op {
            c: crate::protocol::op::COORD,
            s: Some(body.to_string()),
            ..Default::default()
        };
        coord(session, &op)
    }

    #[test]
    fn register_and_list_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let reg = call(
            &mut s,
            json!({"verb": "register_agent", "name": "BlueLake", "program": "codex-cli"}),
        );
        assert!(reg.ok, "register should succeed: {:?}", reg.m);
        let d = reg.d.unwrap();
        assert_eq!(d["agent"]["name"], "BlueLake");

        let list = call(&mut s, json!({"verb": "list_agents"}));
        assert!(list.ok);
        let d = list.d.unwrap();
        assert_eq!(d["count"], 1);
        assert_eq!(d["agents"][0]["name"], "BlueLake");
    }

    #[test]
    fn register_without_name_mints_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let reg = call(&mut s, json!({"verb": "register_agent"}));
        assert!(reg.ok);
        let name = reg.d.unwrap()["agent"]["name"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(names::is_valid(&name), "minted name {name:?} invalid");
    }

    #[test]
    fn register_rejects_bad_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let reg = call(
            &mut s,
            json!({"verb": "register_agent", "name": "../etc/passwd"}),
        );
        assert!(!reg.ok);
        assert_eq!(reg.e, Some(ERR_BAD_REQUEST));
    }

    #[test]
    fn unknown_verb_is_soft_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let r = call(&mut s, json!({"verb": "frobnicate"}));
        assert!(!r.ok);
        assert_eq!(r.e, Some(ERR_BAD_REQUEST));
    }

    #[test]
    fn missing_body_and_verb_are_soft_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let op = Op {
            c: crate::protocol::op::COORD,
            ..Default::default()
        };
        let r = coord(&mut s, &op);
        assert!(!r.ok);
        assert_eq!(r.e, Some(ERR_BAD_REQUEST));

        let r2 = call(&mut s, json!({"name": "x"}));
        assert!(!r2.ok);
        assert_eq!(r2.e, Some(ERR_BAD_REQUEST));
    }

    #[test]
    fn disabled_config_returns_soft_error_and_no_db() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.coordination.enabled = false;
        // Point the DB dir somewhere we can assert nothing was created.
        let db_dir = dir.path().join("coorddb");
        cfg.coordination.db_dir = Some(db_dir.to_string_lossy().to_string());
        let mut s = Session::new(dir.path().to_path_buf(), Arc::new(cfg));

        let r = call(
            &mut s,
            json!({"verb": "register_agent", "name": "BlueLake"}),
        );
        assert!(!r.ok);
        assert_eq!(r.e, Some(ERR_DISABLED));
        assert!(
            !db_dir.exists(),
            "disabled coordination must not touch disk"
        );
    }

    #[test]
    fn reregister_is_idempotent_across_calls() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        call(&mut s, json!({"verb": "register_agent", "name": "Ridge"}));
        call(
            &mut s,
            json!({"verb": "register_agent", "name": "Ridge", "task": "phase 2"}),
        );
        let list = call(&mut s, json!({"verb": "list_agents"}));
        assert_eq!(
            list.d.unwrap()["count"],
            1,
            "re-register must not duplicate"
        );
    }

    #[test]
    fn broken_store_fails_open_no_panic() {
        // ADR-009 D7 / #1053 lesson 2: an unopenable store must yield a soft
        // error, never panic. Force failure by making the resolved DB file path
        // collide with a directory (SQLite can't open a dir as a database).
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("coorddb");
        let mut cfg = Config::default();
        cfg.coordination.db_dir = Some(base.to_string_lossy().to_string());
        let ws = dir.path().to_path_buf();
        // Precompute the exact db file path and create a DIRECTORY there.
        let db_path = workspace_db_path(&base, &ws);
        std::fs::create_dir_all(&db_path).unwrap();
        let mut s = Session::new(ws, Arc::new(cfg));

        let r = call(
            &mut s,
            json!({"verb": "register_agent", "name": "BlueLake"}),
        );
        assert!(!r.ok, "broken store must not succeed");
        assert_eq!(r.e, Some(ERR_STORE_UNAVAILABLE));
    }

    #[test]
    fn two_sessions_same_workspace_share_store() {
        // Distinct Session instances over the same workspace + db_dir must see
        // each other's agents — proving the shared-file model (ADR-009 D1).
        let dir = tempfile::tempdir().unwrap();
        let db_dir = dir.path().join("coorddb");
        let mk = || {
            let mut cfg = Config::default();
            cfg.coordination.db_dir = Some(db_dir.to_string_lossy().to_string());
            Session::new(dir.path().to_path_buf(), Arc::new(cfg))
        };
        let mut a = mk();
        let mut b = mk();
        call(&mut a, json!({"verb": "register_agent", "name": "AgentA"}));
        call(&mut b, json!({"verb": "register_agent", "name": "AgentB"}));
        // B lists and sees both.
        let list = call(&mut b, json!({"verb": "list_agents"}));
        let names: Vec<String> = list.d.unwrap()["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"AgentA".to_string()));
        assert!(names.contains(&"AgentB".to_string()));
    }
}
