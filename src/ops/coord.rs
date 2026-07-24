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

use crate::coordination::{
    names, workspace_db_path, AgentRecord, CoordinationStore, Importance, InboxEntry, InboxFilter,
    MessageRecord,
};
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
        "send_message" => send_message(&store, &body, &now, /* is_reply */ false),
        "reply_message" => send_message(&store, &body, &now, /* is_reply */ true),
        "fetch_inbox" => fetch_inbox(
            &store,
            &body,
            cfg.effective_inbox_default_limit(),
            cfg.effective_inbox_max_limit(),
        ),
        "fetch_thread" => fetch_thread(&store, &body, cfg.thread_max_messages.max(1)),
        "mark_read" => mark_read(&store, &body, &now),
        "acknowledge" => acknowledge(&store, &body, &now),
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

/// Extract a required string-array of agent names from `body[key]`. Missing =>
/// empty vec (so a caller can pass only `to` or only `cc`).
fn str_array(body: &Value, key: &str) -> Vec<String> {
    body.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn send_message(store: &CoordinationStore, body: &Value, now: &str, is_reply: bool) -> Response {
    let sender = match body.get("sender").and_then(|v| v.as_str()) {
        Some(s) if names::is_valid(s) => s.to_string(),
        Some(_) => return Response::err(ERR_BAD_REQUEST, "coord: 'sender' is not a valid name"),
        None => return Response::err(ERR_BAD_REQUEST, "coord: 'sender' is required"),
    };
    let subject = body
        .get("subject")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let msg_body = body
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let importance = Importance::parse(
        body.get("importance")
            .and_then(|v| v.as_str())
            .unwrap_or("normal"),
    );
    let ack_required = body
        .get("ack_required")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut to = str_array(body, "to");
    let cc = str_array(body, "cc");

    // reply_message: ALWAYS validate the parent exists (regardless of whether
    // the caller gave explicit recipients), so a bad `reply_to` can't create
    // an orphan thread rooted at a non-existent message (codeJung finding).
    // Default `to` to the parent's sender only when no recipients were given.
    let reply_to = body.get("reply_to").and_then(|v| v.as_i64());
    if is_reply {
        let parent_id = match reply_to {
            Some(id) => id,
            None => {
                return Response::err(ERR_BAD_REQUEST, "coord: reply_message requires 'reply_to'")
            }
        };
        match store.message(parent_id) {
            Ok(Some(parent)) => {
                if to.is_empty() && cc.is_empty() {
                    to = vec![parent.sender];
                }
            }
            Ok(None) => {
                return Response::err(
                    ERR_BAD_REQUEST,
                    &format!("coord: reply_to message {parent_id} not found"),
                )
            }
            Err(e) => {
                return Response::err(
                    ERR_STORE_UNAVAILABLE,
                    &format!("coord: reply lookup failed: {e}"),
                )
            }
        }
    } else if reply_to.is_some() {
        // send_message doesn't take reply_to; steer callers to reply_message.
        return Response::err(
            ERR_BAD_REQUEST,
            "coord: use reply_message (not send_message) to reply within a thread",
        );
    }

    // Pre-validate the no-recipient case as a bad request HERE, so that any
    // error the store still returns is a genuine fault -> ERR_STORE_UNAVAILABLE
    // (matching list_agents/fetch_inbox classification). This keeps a real DB
    // fault from being mislabeled a bad request (codeJung finding).
    if to.is_empty() && cc.is_empty() {
        return Response::err(
            ERR_BAD_REQUEST,
            "coord: at least one recipient required (no broadcast)",
        );
    }

    match store.send_message(
        &sender,
        &to,
        &cc,
        &subject,
        &msg_body,
        importance,
        ack_required,
        if is_reply { reply_to } else { None },
        now,
    ) {
        Ok(receipt) => Response::ok(json!({
            "message_id": receipt.message_id,
            "thread_id": receipt.thread_id,
            "recipients": receipt.recipients,
        })),
        // Recipients were validated above, so a failure here is a store fault.
        Err(e) => Response::err(ERR_STORE_UNAVAILABLE, &format!("coord: send failed: {e}")),
    }
}

fn fetch_inbox(
    store: &CoordinationStore,
    body: &Value,
    default_limit: i64,
    max_limit: i64,
) -> Response {
    let agent = match body.get("agent").and_then(|v| v.as_str()) {
        Some(a) if names::is_valid(a) => a.to_string(),
        Some(_) => return Response::err(ERR_BAD_REQUEST, "coord: 'agent' is not a valid name"),
        None => return Response::err(ERR_BAD_REQUEST, "coord: 'agent' is required"),
    };
    let filter = InboxFilter {
        unread_only: body
            .get("unread_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        min_importance: body
            .get("min_importance")
            .and_then(|v| v.as_str())
            .map(Importance::parse),
        since: body.get("since").and_then(|v| v.as_str()).map(String::from),
    };
    // Clamp a caller-supplied limit to the configured ceiling so a huge value
    // can't force an unbounded inbox read (codeJung finding); the store also
    // floors it at 1.
    let limit = body
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(default_limit)
        .min(max_limit);
    match store.fetch_inbox(&agent, &filter, limit) {
        Ok(entries) => {
            let arr: Vec<Value> = entries.iter().map(inbox_entry_json).collect();
            Response::ok(json!({ "messages": arr, "count": arr.len() }))
        }
        Err(e) => Response::err(
            ERR_STORE_UNAVAILABLE,
            &format!("coord: fetch_inbox failed: {e}"),
        ),
    }
}

fn fetch_thread(store: &CoordinationStore, body: &Value, cap: i64) -> Response {
    let thread_id = match body.get("thread_id").and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => return Response::err(ERR_BAD_REQUEST, "coord: 'thread_id' is required"),
    };
    match store.thread(thread_id, cap) {
        Ok(msgs) => {
            let arr: Vec<Value> = msgs.iter().map(message_json).collect();
            Response::ok(json!({ "thread_id": thread_id, "messages": arr, "count": arr.len() }))
        }
        Err(e) => Response::err(
            ERR_STORE_UNAVAILABLE,
            &format!("coord: fetch_thread failed: {e}"),
        ),
    }
}

fn mark_read(store: &CoordinationStore, body: &Value, now: &str) -> Response {
    let (agent, message_id) = match read_agent_and_id(body) {
        Ok(v) => v,
        Err(msg) => return Response::err(ERR_BAD_REQUEST, &msg),
    };
    match store.mark_read(&agent, message_id, now) {
        Ok(Some(ts)) => Response::ok(json!({ "message_id": message_id, "read_ts": ts })),
        Ok(None) => Response::err(
            ERR_BAD_REQUEST,
            &format!("coord: {agent} is not a recipient of message {message_id}"),
        ),
        Err(e) => Response::err(
            ERR_STORE_UNAVAILABLE,
            &format!("coord: mark_read failed: {e}"),
        ),
    }
}

fn acknowledge(store: &CoordinationStore, body: &Value, now: &str) -> Response {
    let (agent, message_id) = match read_agent_and_id(body) {
        Ok(v) => v,
        Err(msg) => return Response::err(ERR_BAD_REQUEST, &msg),
    };
    match store.acknowledge(&agent, message_id, now) {
        Ok(Some((read_ts, ack_ts))) => Response::ok(json!({
            "message_id": message_id,
            "read_ts": read_ts,
            "ack_ts": ack_ts,
        })),
        Ok(None) => Response::err(
            ERR_BAD_REQUEST,
            &format!("coord: {agent} is not a recipient of message {message_id}"),
        ),
        Err(e) => Response::err(
            ERR_STORE_UNAVAILABLE,
            &format!("coord: acknowledge failed: {e}"),
        ),
    }
}

/// Shared arg extraction for mark_read/acknowledge: `{agent, message_id}`.
/// Returns an error *message* (not a `Response`) so the small-error variant
/// keeps `Result` compact (clippy::result_large_err); callers wrap it.
fn read_agent_and_id(body: &Value) -> Result<(String, i64), String> {
    let agent = match body.get("agent").and_then(|v| v.as_str()) {
        Some(a) if names::is_valid(a) => a.to_string(),
        Some(_) => return Err("coord: 'agent' is not a valid name".to_string()),
        None => return Err("coord: 'agent' is required".to_string()),
    };
    let message_id = match body.get("message_id").and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => return Err("coord: 'message_id' is required".to_string()),
    };
    Ok((agent, message_id))
}

fn message_json(m: &MessageRecord) -> Value {
    json!({
        "id": m.id,
        "thread_id": m.thread_id,
        "reply_to": m.reply_to,
        "sender": m.sender,
        "subject": m.subject,
        "body": m.body,
        "importance": m.importance,
        "ack_required": m.ack_required,
        "created_ts": m.created_ts,
    })
}

fn inbox_entry_json(e: &InboxEntry) -> Value {
    let mut v = message_json(&e.message);
    if let Value::Object(map) = &mut v {
        map.insert("kind".to_string(), json!(e.kind));
        map.insert("read_ts".to_string(), json!(e.read_ts));
        map.insert("ack_ts".to_string(), json!(e.ack_ts));
    }
    v
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
    fn send_fetch_read_ack_reply_flow() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        // Send BlueLake -> GreenCastle.
        let sent = call(
            &mut s,
            json!({"verb": "send_message", "sender": "BlueLake", "to": ["GreenCastle"],
                   "subject": "plan", "body": "see below", "importance": "high", "ack_required": true}),
        );
        assert!(sent.ok, "send failed: {:?}", sent.m);
        let sent_d = sent.d.unwrap();
        let mid = sent_d["message_id"].as_i64().unwrap();
        assert_eq!(sent_d["recipients"][0], "GreenCastle");

        // GreenCastle fetches inbox.
        let inbox = call(
            &mut s,
            json!({"verb": "fetch_inbox", "agent": "GreenCastle"}),
        );
        assert!(inbox.ok);
        let d = inbox.d.unwrap();
        assert_eq!(d["count"], 1);
        assert_eq!(d["messages"][0]["subject"], "plan");
        assert_eq!(d["messages"][0]["importance"], "high");
        assert!(d["messages"][0]["read_ts"].is_null());

        // Acknowledge (sets read + ack).
        let ack = call(
            &mut s,
            json!({"verb": "acknowledge", "agent": "GreenCastle", "message_id": mid}),
        );
        assert!(ack.ok);
        assert!(ack.d.unwrap()["ack_ts"].is_string());

        // Reply defaults `to` back to the original sender and shares the thread.
        let reply = call(
            &mut s,
            json!({"verb": "reply_message", "sender": "GreenCastle", "reply_to": mid,
                   "subject": "re: plan", "body": "ack"}),
        );
        assert!(reply.ok, "reply failed: {:?}", reply.m);
        assert_eq!(reply.d.unwrap()["thread_id"], sent_d["thread_id"]);

        // BlueLake now has the reply in its inbox.
        let blue = call(&mut s, json!({"verb": "fetch_inbox", "agent": "BlueLake"}));
        assert_eq!(blue.d.unwrap()["count"], 1);
    }

    #[test]
    fn send_without_recipient_is_bad_request_no_broadcast() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let r = call(
            &mut s,
            json!({"verb": "send_message", "sender": "BlueLake", "subject": "x"}),
        );
        assert!(!r.ok);
        assert_eq!(r.e, Some(ERR_BAD_REQUEST));
    }

    #[test]
    fn send_message_rejects_reply_to_steering_to_reply_verb() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let r = call(
            &mut s,
            json!({"verb": "send_message", "sender": "A", "to": ["B"], "reply_to": 1}),
        );
        assert!(!r.ok);
        assert_eq!(r.e, Some(ERR_BAD_REQUEST));
    }

    #[test]
    fn mark_read_and_ack_on_non_recipient_is_bad_request() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let sent = call(
            &mut s,
            json!({"verb": "send_message", "sender": "A", "to": ["B"], "subject": "s"}),
        );
        let mid = sent.d.unwrap()["message_id"].as_i64().unwrap();
        let r = call(
            &mut s,
            json!({"verb": "mark_read", "agent": "Nobody", "message_id": mid}),
        );
        assert!(!r.ok);
        assert_eq!(r.e, Some(ERR_BAD_REQUEST));
    }

    #[test]
    fn fetch_thread_returns_ordered_thread() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let root = call(
            &mut s,
            json!({"verb": "send_message", "sender": "A", "to": ["B"], "subject": "root"}),
        );
        let rd = root.d.unwrap();
        let mid = rd["message_id"].as_i64().unwrap();
        let tid = rd["thread_id"].as_i64().unwrap();
        call(
            &mut s,
            json!({"verb": "reply_message", "sender": "B", "reply_to": mid, "subject": "re"}),
        );
        let thread = call(&mut s, json!({"verb": "fetch_thread", "thread_id": tid}));
        assert!(thread.ok);
        let d = thread.d.unwrap();
        assert_eq!(d["count"], 2);
        assert_eq!(d["messages"][0]["subject"], "root");
        assert_eq!(d["messages"][1]["subject"], "re");
    }

    #[test]
    fn reply_to_nonexistent_parent_is_rejected_even_with_explicit_recipients() {
        // codeJung: an explicit-recipient reply with a bad reply_to must NOT
        // create an orphan thread — the parent is always validated.
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let r = call(
            &mut s,
            json!({"verb": "reply_message", "sender": "A", "to": ["B"], "reply_to": 9999, "subject": "re"}),
        );
        assert!(!r.ok);
        assert_eq!(r.e, Some(ERR_BAD_REQUEST));
    }

    #[test]
    fn fetch_inbox_clamps_huge_limit() {
        let dir = tempfile::tempdir().unwrap();
        let db_dir = dir.path().join("coorddb");
        let mut cfg = Config::default();
        cfg.coordination.db_dir = Some(db_dir.to_string_lossy().to_string());
        cfg.coordination.inbox_max_limit = 2;
        let mut s = Session::new(dir.path().to_path_buf(), Arc::new(cfg));
        for i in 0..5 {
            call(
                &mut s,
                json!({"verb": "send_message", "sender": "A", "to": ["Z"], "subject": format!("m{i}")}),
            );
        }
        // A caller asking for 1000 is clamped to inbox_max_limit (2).
        let inbox = call(
            &mut s,
            json!({"verb": "fetch_inbox", "agent": "Z", "limit": 1000}),
        );
        assert!(inbox.ok);
        assert_eq!(inbox.d.unwrap()["count"], 2);
    }

    #[test]
    fn fetch_thread_requires_thread_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let r = call(&mut s, json!({"verb": "fetch_thread"}));
        assert!(!r.ok);
        assert_eq!(r.e, Some(ERR_BAD_REQUEST));
    }

    #[test]
    fn fetch_inbox_requires_agent() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let r = call(&mut s, json!({"verb": "fetch_inbox"}));
        assert!(!r.ok);
        assert_eq!(r.e, Some(ERR_BAD_REQUEST));
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
