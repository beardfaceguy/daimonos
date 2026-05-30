//! BUILD 6 — the orient demo / v0 success test (project goal).
//!
//! Proves the KGL thesis on real ground: a NEW agent, given ONLY `kgl_query`
//! over an unfamiliar codebase, can orient — find relevant defs, trace
//! dependencies/calls, see what state they read/mutate, read prior agents'
//! open questions, and compute blast radius — WITHOUT reading the source.
//!
//! The flow: author a small auth service in x07 -> build the graph (what
//! `kgl_query index` does) -> inject the intent/provenance/declared edges a
//! prior authoring agent would have left -> answer the orientation questions
//! through the query API alone. Run with `--nocapture` to see the agent's-eye
//! view printed as a narrative.

use crate::kgl::model::{EdgeKind, Intent, Provenance};
use crate::kgl::query::run;
use crate::kgl::store::KglStore;
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;

const AUTH_MODULE: &str = r#"{
  "module_id": "auth.service",
  "schema_version": "1.0",
  "kind": "library",
  "imports": ["std.io", "std.crypto"],
  "decls": [
    {"kind":"defn","name":"authenticate","params":[{"name":"user","ty":"String"},{"name":"pass","ty":"String"}],"result":"Token","effects":["IO"],
     "body":[{"kind":"call","callee":"load_user"},{"kind":"call","callee":"verify_password"},{"kind":"call","callee":"issue_token"}]},
    {"kind":"defn","name":"load_user","params":[{"name":"id","ty":"String"}],"result":"User","effects":["IO"],
     "body":[{"kind":"call","callee":"read_file","args":["/var/users.db"]}]},
    {"kind":"defn","name":"verify_password","params":[{"name":"pass","ty":"String"},{"name":"hash","ty":"Hash"}],"result":"Bool",
     "body":[{"kind":"call","callee":"hash_password"}]},
    {"kind":"defn","name":"hash_password","params":[{"name":"pass","ty":"String"}],"result":"Hash","body":[]},
    {"kind":"defn","name":"issue_token","params":[{"name":"user","ty":"User"}],"result":"Token","effects":["IO"],
     "body":[{"kind":"call","callee":"sign"}]},
    {"kind":"defn","name":"sign","params":[{"name":"data","ty":"Bytes"}],"result":"Sig","effects":["IO"],
     "body":[{"kind":"call","callee":"read_file","args":["/etc/keys/signing.key"]}]},
    {"kind":"defn","name":"revoke_token","params":[{"name":"tok","ty":"Token"}],"result":"Unit","effects":["IO"],"body":[]},
    {"kind":"defn","name":"logout","params":[{"name":"user","ty":"User"}],"result":"Unit","effects":["IO"],
     "body":[{"kind":"call","callee":"revoke_token"}]}
  ]
}"#;

fn write_module(dir: &Path) {
    let mut f = std::fs::File::create(dir.join("auth.x07.json")).unwrap();
    f.write_all(AUTH_MODULE.as_bytes()).unwrap();
}

fn arr(v: &Value) -> Vec<Value> {
    v.as_array().cloned().unwrap_or_default()
}

fn has_name(v: &Value, n: &str) -> bool {
    arr(v).iter().any(|r| r["name"] == json!(n))
}

fn names(v: &Value) -> Vec<String> {
    arr(v)
        .iter()
        .filter_map(|r| r["name"].as_str().map(String::from))
        .collect()
}

#[test]
fn orient_from_graph_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    write_module(ws);

    // --- 1. Build the graph (what `kgl_query index` does). ---
    let idx = run(ws, "index", &json!({}), "2026-05-30T00:00:00Z").unwrap();
    println!("\n[index] {idx}");
    assert_eq!(idx["indexed"], json!(true));

    // --- 2. Inject the metadata a prior authoring agent would have left. ---
    let store = KglStore::open(&ws.join(".kgl").join("kgl.db")).unwrap();
    let hash_of = |name: &str| -> String {
        store
            .find(name)
            .unwrap()
            .into_iter()
            .find(|r| r.node.name.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("def not found: {name}"))
            .node
            .hash
    };
    let declare = |name: &str, purpose: &str, qs: &[&str]| -> String {
        let h = hash_of(name);
        store
            .set_intent(
                &h,
                &Intent {
                    purpose: purpose.into(),
                    rationale: None,
                    open_questions: qs.iter().map(|s| s.to_string()).collect(),
                },
            )
            .unwrap();
        store
            .set_provenance(
                &h,
                &Provenance {
                    authored_by: "agent:opus-4.8".into(),
                    session_id: "sess-bootstrap".into(),
                    timestamp: "2026-05-29T12:00:00Z".into(),
                    assumptions: vec![],
                    supersedes: vec![],
                },
            )
            .unwrap();
        h
    };

    declare("authenticate", "Entry point for the auth flow: validate credentials and return a session token", &[]);
    declare("load_user", "Fetch a user record from the user database by id", &[]);
    declare("verify_password", "Constant-time check of a password against its stored hash", &[]);
    declare("hash_password", "Hash a plaintext password (argon2id) for storage/comparison", &[]);
    let issue = declare(
        "issue_token",
        "Mint a signed session token for an authenticated user",
        &["Token TTL is hard-coded to 24h — should it be configurable per-tenant?"],
    );
    declare("sign", "Sign bytes with the service signing key", &[]);
    let revoke = declare("revoke_token", "Invalidate a session token on logout", &[]);
    declare("logout", "Log a user out by revoking their active token", &[]);
    declare("auth.service", "Authentication service: validate credentials, issue and revoke session tokens", &[]);

    // The session store is mutated through an opaque handle the verb-heuristic
    // can't name, so the authoring agent DECLARES those edges explicitly.
    store.add_declared_edge(&issue, "session:store", EdgeKind::Mutates, None).unwrap();
    store.add_declared_edge(&revoke, "session:store", EdgeKind::Mutates, None).unwrap();

    // ============================================================
    // A NEW agent now orients using ONLY kgl_query — no source reads.
    // ============================================================

    // (a) "fix the auth flow" -> what's relevant?
    let hits = run(ws, "find", &json!({"q": "auth"}), "t").unwrap();
    println!("[find auth] {:?}", names(&hits));
    assert!(has_name(&hits, "authenticate"));

    // (b) what does authenticate call?
    let ah = hash_of("authenticate");
    let calls = run(ws, "neighbors", &json!({"hash": ah, "kind": "calls", "dir": "out"}), "t").unwrap();
    println!("[authenticate calls] {} edges", arr(&calls).len());
    assert!(arr(&calls).len() >= 3); // load_user, verify_password, issue_token

    // (c) what state does the flow read / mutate?
    let lu = hash_of("load_user");
    let reads = run(ws, "neighbors", &json!({"hash": lu, "kind": "reads", "dir": "out"}), "t").unwrap();
    println!("[load_user reads] {:?}", arr(&reads).iter().map(|e| e["to"].clone()).collect::<Vec<_>>());
    assert!(arr(&reads).iter().any(|e| e["to"] == json!("file:///var/users.db")));

    let writers = run(ws, "writers_of", &json!({"resource": "session:store"}), "t").unwrap();
    println!("[writers_of session:store] {:?}", names(&writers));
    assert!(has_name(&writers, "issue_token"));
    assert!(has_name(&writers, "revoke_token"));

    // (d) what did prior agents leave unresolved?
    let oq = run(ws, "open_questions", &json!({}), "t").unwrap();
    println!("[open_questions] {:?}", names(&oq));
    assert!(arr(&oq).iter().any(|r| {
        r["intent"]["open_questions"]
            .as_array()
            .map(|qs| qs.iter().any(|q| q.as_str().unwrap_or("").contains("TTL")))
            .unwrap_or(false)
    }));

    // (e) blast radius: what breaks if hash_password changes?
    let hp = hash_of("hash_password");
    let blast = run(ws, "blast_radius", &json!({"hash": hp}), "t").unwrap();
    println!("[blast_radius hash_password] {:?}", names(&blast));
    assert!(has_name(&blast, "verify_password")); // direct caller
    assert!(has_name(&blast, "authenticate")); // transitive caller

    // (f) completeness gate
    let chk = run(ws, "check", &json!({"mode": "commit"}), "t").unwrap();
    println!("[check commit] complete={} blocking={}", chk["complete"], chk["blocking"]);
    // With every def documented and effects accounted for (incl. transitively,
    // e.g. `authenticate`/`logout` whose I/O is via callees), the gate passes.
    assert_eq!(
        chk["complete"],
        json!(true),
        "fully-documented graph should pass the commit gate; violations: {}",
        chk["violations"]
    );

    drop(store);
    println!("\nORIENT DEMO: a new agent answered (a)-(e) from the graph alone — no source read.\n");
}
