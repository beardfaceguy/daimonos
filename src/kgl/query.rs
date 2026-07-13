//! `kgl_query` — the agent-facing orientation API. Opens the per-workspace store
//! and dispatches the read actions (plus `index` to (re)build and `check` for
//! completeness), returning JSON shaped so an agent can orient WITHOUT reading
//! source. Wired into the MCP layer as the `kgl_query` tool.

use crate::config::KglConfig;
use crate::kgl::model::{DefNode, Edge, EdgeKind};
use crate::kgl::store::{Direction, KglStore, NodeRecord, Violation};
use crate::kgl::substrate::Substrate;
use crate::kgl::substrate_graphify::GraphifySubstrate;
use crate::kgl::substrate_x07::X07Substrate;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::path::Path;

/// Dispatch one query action against the workspace's KGL store. `now` is an
/// ISO-8601 timestamp supplied by the host (used as the index run stamp; KGL
/// never invents time).
pub fn run(
    workspace: &Path,
    action: &str,
    args: &Value,
    now: &str,
    cfg: &KglConfig,
) -> Result<Value> {
    let mut store = KglStore::open_workspace_with(workspace, cfg)?;
    match action {
        "index" => {
            // Substrate is swappable: x07 reads .x07.json sources; graphify reads
            // a derived graphify-out/graph.json (real codebases). When the caller
            // omits `substrate`, auto-detect exactly like startup (graphify if a
            // graph.json exists, else x07) — never blindly default to x07, which
            // on a graphify-only workspace would run an empty scan whose prune
            // wipes the existing graph and its agent metadata.
            let (which, sub): (&str, Box<dyn Substrate>) = match args
                .get("substrate")
                .and_then(|v| v.as_str())
            {
                Some("graphify") => {
                    // Explicit graphify still needs the code-node guard: a
                    // missing/empty/stub graph.json indexes empty, and populate's
                    // prune would then wipe the existing graph and its agent
                    // metadata. Refuse non-destructively instead (mirrors the
                    // auto-detect fall-through in autoindex::detect).
                    if !crate::kgl::autoindex::graphify_has_code_nodes(
                        &workspace.join("graphify-out").join("graph.json"),
                    ) {
                        return Ok(json!({
                            "indexed": false,
                            "substrate": "graphify",
                            "nodes": 0,
                            "edges": 0,
                            "reason": "graphify substrate requested but graphify-out/graph.json is missing, empty, or has no code nodes — refusing to index (would prune the existing graph)",
                        }));
                    }
                    ("graphify", Box::new(GraphifySubstrate))
                }
                Some("x07") => {
                    // Symmetric to the graphify guard: an explicit x07 request
                    // with no *.x07.json sources indexes empty, and populate's
                    // prune would wipe the existing graph. Refuse non-destructively.
                    if !crate::kgl::autoindex::has_x07_sources(workspace, cfg) {
                        return Ok(json!({
                            "indexed": false,
                            "substrate": "x07",
                            "nodes": 0,
                            "edges": 0,
                            "reason": "x07 substrate requested but no *.x07.json sources found — refusing to index (would prune the existing graph)",
                        }));
                    }
                    ("x07", Box::new(X07Substrate::new(cfg.skip_dirs.clone())))
                }
                Some(other) => {
                    return Err(anyhow!(
                        "kgl_query: unknown substrate '{other}' (expected x07|graphify)"
                    ))
                }
                None => match crate::kgl::autoindex::detect(workspace, cfg) {
                    Some(pair) => pair,
                    None => {
                        return Ok(json!({
                            "indexed": false,
                            "substrate": Value::Null,
                            "nodes": 0,
                            "edges": 0,
                            "reason": "no substrate detected (no graphify-out/graph.json or *.x07.json)",
                        }))
                    }
                },
            };
            let (nodes, edges) = store.populate(sub.as_ref(), workspace, now)?;
            Ok(json!({ "indexed": true, "substrate": which, "nodes": nodes, "edges": edges }))
        }
        "node" => Ok(match store.node(str_arg(args, "hash")?)? {
            Some(rec) => record_json(&rec),
            None => Value::Null,
        }),
        "neighbors" => {
            let hash = str_arg(args, "hash")?;
            let kind = args
                .get("kind")
                .and_then(|v| v.as_str())
                .and_then(EdgeKind::from_wire);
            let dir = match args.get("dir").and_then(|v| v.as_str()).unwrap_or("out") {
                "in" => Direction::In,
                "both" => Direction::Both,
                _ => Direction::Out,
            };
            let edges = store.neighbors(hash, kind, dir)?;
            Ok(json!(edges.iter().map(edge_json).collect::<Vec<_>>()))
        }
        "find" => Ok(records_json(store.find(str_arg(args, "q")?, cfg.find_max)?)),
        "writers_of" => Ok(records_json(store.writers_of(str_arg(args, "resource")?)?)),
        "blast_radius" => Ok(records_json(
            store.blast_radius(str_arg(args, "hash")?, cfg.blast_radius_max)?,
        )),
        "open_questions" => Ok(records_json(store.open_questions()?)),
        "orient" => {
            // One bundled call (vs many round-trips): task-matching defs with their
            // intent/open-questions, their outgoing edges, and their dependents.
            let task = str_arg(args, "task")?;
            let top: Vec<_> = store
                .find(task, cfg.find_max)?
                .into_iter()
                .take(cfg.orient_max_matches)
                .collect();
            let mut matches = Vec::new();
            let mut edges = Vec::new();
            let mut dependents = std::collections::BTreeSet::new();
            for rec in &top {
                matches.push(record_json(rec));
                for e in store.neighbors(&rec.node.hash, None, Direction::Out)? {
                    edges.push(edge_json(&e));
                }
                for dep in store.blast_radius(&rec.node.hash, cfg.blast_radius_max)? {
                    if let Some(n) = dep.node.name {
                        dependents.insert(n);
                    }
                }
            }
            Ok(json!({
                "task": task,
                "matches": matches,
                "edges": edges,
                "dependents": dependents.into_iter().collect::<Vec<_>>(),
            }))
        }
        "check" => {
            let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("draft");
            let v = store.check_completeness()?;
            let complete = v.is_empty();
            Ok(json!({
                "mode": mode,
                "complete": complete,
                // DRAFT surfaces violations as warnings; COMMIT makes them blocking.
                "blocking": mode == "commit" && !complete,
                "violations": v.iter().map(violation_json).collect::<Vec<_>>(),
            }))
        }
        other => Err(anyhow!("unknown kgl_query action: {other}")),
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("kgl_query: missing string arg '{key}'"))
}

fn node_json(n: &DefNode) -> Value {
    serde_json::to_value(n).unwrap_or(Value::Null)
}

fn record_json(rec: &NodeRecord) -> Value {
    let mut v = node_json(&rec.node);
    if let Value::Object(map) = &mut v {
        map.insert(
            "intent".into(),
            serde_json::to_value(&rec.intent).unwrap_or(Value::Null),
        );
        map.insert(
            "provenance".into(),
            serde_json::to_value(&rec.provenance).unwrap_or(Value::Null),
        );
        map.insert("touches_io".into(), json!(rec.touches_io));
        map.insert("mutates_state".into(), json!(rec.mutates_state));
    }
    v
}

fn records_json(recs: Vec<NodeRecord>) -> Value {
    json!(recs.iter().map(record_json).collect::<Vec<_>>())
}

fn edge_json(e: &Edge) -> Value {
    serde_json::to_value(e).unwrap_or(Value::Null)
}

fn violation_json(v: &Violation) -> Value {
    json!({ "hash": v.hash, "name": v.name, "reason": v.reason })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn workspace() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let mut f = std::fs::File::create(tmp.path().join("svc.x07.json")).unwrap();
        f.write_all(
            br#"{
                "module_id":"svc","schema_version":"1.0","kind":"library",
                "imports":["std.io"],
                "decls":[
                    {"kind":"defn","name":"authenticate",
                     "params":[{"name":"user","ty":"String"}],"result":"Token",
                     "effects":["IO"],
                     "body":[{"kind":"call","callee":"read_file","args":["/etc/users.db"]}]}
                ]
            }"#,
        )
        .unwrap();
        tmp
    }

    #[test]
    fn index_then_find_and_check() {
        let tmp = workspace();
        let ws = tmp.path();

        let idx = run(ws, "index", &json!({}), "t0", &KglConfig::default()).unwrap();
        assert_eq!(idx["indexed"], json!(true));
        assert!(idx["nodes"].as_u64().unwrap() >= 2);

        // find by name returns the authenticate node
        let found = run(
            ws,
            "find",
            &json!({"q": "authenticate"}),
            "t0",
            &KglConfig::default(),
        )
        .unwrap();
        let arr = found.as_array().unwrap();
        assert!(arr.iter().any(|r| r["name"] == json!("authenticate")));

        // it reads /etc/users.db
        let hash = arr
            .iter()
            .find(|r| r["name"] == json!("authenticate"))
            .unwrap()["hash"]
            .as_str()
            .unwrap()
            .to_string();
        let nb = run(
            ws,
            "neighbors",
            &json!({"hash": hash, "kind": "reads"}),
            "t0",
            &KglConfig::default(),
        )
        .unwrap();
        assert!(nb
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["to"] == json!("file:///etc/users.db")));

        // check: authenticate has no purpose yet -> incomplete
        let chk = run(
            ws,
            "check",
            &json!({"mode": "commit"}),
            "t0",
            &KglConfig::default(),
        )
        .unwrap();
        assert_eq!(chk["complete"], json!(false));
    }

    #[test]
    fn orient_bundles_matches_and_edges() {
        let tmp = workspace();
        let ws = tmp.path();
        run(ws, "index", &json!({}), "t0", &KglConfig::default()).unwrap();
        let o = run(
            ws,
            "orient",
            &json!({"task": "authenticate"}),
            "t0",
            &KglConfig::default(),
        )
        .unwrap();
        assert!(o["matches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == json!("authenticate")));
        assert!(o["edges"].is_array());
    }

    #[test]
    fn orient_match_count_honors_config_limit() {
        // Two name-matching defs; a non-default orient_max_matches must bound
        // the expanded match set (the cap is not just a default).
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        std::fs::write(
            ws.join("svc.x07.json"),
            r#"{"module_id":"svc","schema_version":"1.0","kind":"library","imports":[],
               "decls":[
                 {"kind":"defn","name":"auth_login","params":[],"result":"Unit","body":[]},
                 {"kind":"defn","name":"auth_logout","params":[],"result":"Unit","body":[]}
               ]}"#,
        )
        .unwrap();
        run(ws, "index", &json!({}), "t0", &KglConfig::default()).unwrap();

        let full = run(
            ws,
            "orient",
            &json!({"task": "auth"}),
            "t0",
            &KglConfig::default(),
        )
        .unwrap();
        assert_eq!(
            full["matches"].as_array().unwrap().len(),
            2,
            "both auth_* defs should match by default"
        );

        let capped = run(
            ws,
            "orient",
            &json!({"task": "auth"}),
            "t0",
            &KglConfig {
                orient_max_matches: 1,
                ..KglConfig::default()
            },
        )
        .unwrap();
        assert_eq!(
            capped["matches"].as_array().unwrap().len(),
            1,
            "orient_max_matches=1 must bound the match set"
        );
    }

    #[test]
    fn index_without_substrate_detects_graphify_not_x07() {
        // Blocker regression at the tool boundary: `index` with no substrate on
        // a graphify-only workspace must pick graphify, not run an empty x07
        // scan (whose prune would wipe the graph).
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("graphify-out")).unwrap();
        std::fs::write(
            ws.join("graphify-out").join("graph.json"),
            r#"{"nodes":[{"id":"n1","label":".foo()","file_type":"code","source_file":"src/a.rs"}],"links":[]}"#,
        )
        .unwrap();
        let idx = run(ws, "index", &json!({}), "t0", &KglConfig::default()).unwrap();
        assert_eq!(idx["substrate"], json!("graphify"));
        assert_eq!(idx["indexed"], json!(true));
        assert!(idx["nodes"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn explicit_graphify_with_stub_graph_refuses_and_preserves_existing_graph() {
        // Blocker regression: an explicit `substrate:"graphify"` request against
        // a missing/empty/stub graph.json must NOT populate (its prune would wipe
        // the graph) — it must refuse non-destructively and leave an existing
        // x07-built graph intact.
        let tmp = workspace(); // has svc.x07.json with `authenticate`
        let ws = tmp.path();
        run(ws, "index", &json!({}), "t0", &KglConfig::default()).unwrap();
        assert!(run(
            ws,
            "find",
            &json!({"q": "authenticate"}),
            "t0",
            &KglConfig::default()
        )
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["name"] == json!("authenticate")));

        // Stub graphify graph: valid JSON, zero code nodes.
        std::fs::create_dir_all(ws.join("graphify-out")).unwrap();
        std::fs::write(
            ws.join("graphify-out").join("graph.json"),
            br#"{"nodes":[],"links":[]}"#,
        )
        .unwrap();

        let idx = run(
            ws,
            "index",
            &json!({"substrate": "graphify"}),
            "t1",
            &KglConfig::default(),
        )
        .unwrap();
        assert_eq!(
            idx["indexed"],
            json!(false),
            "must refuse to index a stub graph"
        );
        assert_eq!(idx["substrate"], json!("graphify"));

        // The pre-existing x07 graph must survive (not pruned).
        assert!(
            run(
                ws,
                "find",
                &json!({"q": "authenticate"}),
                "t2",
                &KglConfig::default()
            )
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == json!("authenticate")),
            "explicit graphify against a stub graph must not prune the existing graph"
        );
    }

    #[test]
    fn explicit_x07_with_no_sources_refuses_and_preserves_existing_graph() {
        // Symmetric to the graphify guard: an explicit `substrate:"x07"` request
        // with no *.x07.json sources must refuse non-destructively rather than
        // populate empty and prune an existing (graphify-built) graph.
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("graphify-out")).unwrap();
        std::fs::write(
            ws.join("graphify-out").join("graph.json"),
            r#"{"nodes":[{"id":"n1","label":".foo()","file_type":"code","source_file":"src/a.rs"}],"links":[]}"#,
        )
        .unwrap();
        // Build the graphify graph first.
        let idx = run(
            ws,
            "index",
            &json!({"substrate": "graphify"}),
            "t0",
            &KglConfig::default(),
        )
        .unwrap();
        assert_eq!(idx["indexed"], json!(true));
        assert!(run(
            ws,
            "find",
            &json!({"q": "foo"}),
            "t0",
            &KglConfig::default()
        )
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["name"] == json!(".foo()")));

        // Explicit x07 with no *.x07.json sources: must refuse, not prune.
        let x = run(
            ws,
            "index",
            &json!({"substrate": "x07"}),
            "t1",
            &KglConfig::default(),
        )
        .unwrap();
        assert_eq!(
            x["indexed"],
            json!(false),
            "must refuse when no x07 sources exist"
        );
        assert_eq!(x["substrate"], json!("x07"));

        // The graphify graph must survive.
        assert!(
            run(
                ws,
                "find",
                &json!({"q": "foo"}),
                "t2",
                &KglConfig::default()
            )
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == json!(".foo()")),
            "explicit x07 with no sources must not prune the existing graph"
        );
    }

    #[test]
    fn index_with_unknown_substrate_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let r = run(
            tmp.path(),
            "index",
            &json!({"substrate": "bogus"}),
            "t0",
            &KglConfig::default(),
        );
        assert!(r.is_err());
    }
}
