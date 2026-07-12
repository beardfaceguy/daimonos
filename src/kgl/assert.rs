//! `kgl_assert` — the agent-facing WRITE path. Lets an authoring agent declare
//! the INTENTIONAL, non-derivable layer (intent/purpose, provenance, contracts/
//! edges) that no substrate provides. Pairs with `observe` (which captures the
//! OBSERVED). Wired into the MCP layer as the `kgl_assert` tool.

use crate::config::KglConfig;
use crate::kgl::model::{EdgeKind, Intent, Provenance};
use crate::kgl::store::KglStore;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::path::Path;

/// Apply one assertion to the workspace's KGL store. `now` is a host-supplied
/// ISO-8601 timestamp (KGL never invents time).
pub fn run(
    workspace: &Path,
    action: &str,
    args: &Value,
    now: &str,
    cfg: &KglConfig,
) -> Result<Value> {
    let store = KglStore::open_workspace_with(workspace, cfg)?;
    match action {
        "intent" => {
            let hash = str_arg(args, "hash")?;
            let intent = Intent {
                purpose: str_arg(args, "purpose")?.to_string(),
                rationale: args
                    .get("rationale")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                open_questions: str_array(args, "open_questions"),
            };
            if !store.set_intent(hash, &intent)? {
                return Err(anyhow!("kgl_assert: no node with hash '{hash}'"));
            }
            Ok(json!({ "action": "intent", "hash": hash, "updated": true }))
        }
        "provenance" => {
            let hash = str_arg(args, "hash")?;
            let prov = Provenance {
                authored_by: str_arg(args, "authored_by")?.to_string(),
                session_id: args
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                timestamp: now.to_string(),
                assumptions: str_array(args, "assumptions"),
                supersedes: str_array(args, "supersedes"),
            };
            if !store.set_provenance(hash, &prov)? {
                return Err(anyhow!("kgl_assert: no node with hash '{hash}'"));
            }
            Ok(json!({ "action": "provenance", "hash": hash, "updated": true }))
        }
        "declare_edge" => {
            let from = str_arg(args, "from")?;
            let to = str_arg(args, "to")?;
            let kind = EdgeKind::from_wire(str_arg(args, "kind")?).ok_or_else(|| {
                anyhow!("kgl_assert: invalid edge kind (calls|depends_on|reads|mutates)")
            })?;
            // Verify the source node exists (mirrors intent/provenance), so a
            // typo'd `from` hash can't create an orphan edge that later reads as
            // a real relation. `to` is intentionally NOT checked: it may be a
            // resource URN (file:///x, secret:..., io:unknown), not a node.
            if store.node(from)?.is_none() {
                return Err(anyhow!("kgl_assert: no node with hash '{from}'"));
            }
            store.add_declared_edge(from, to, kind, None)?;
            Ok(json!({ "action": "declare_edge", "from": from, "to": to, "ok": true }))
        }
        other => Err(anyhow!("unknown kgl_assert action: {other}")),
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("kgl_assert: missing string arg '{key}'"))
}

fn str_array(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kgl::store::Direction;
    use crate::kgl::substrate_x07::X07Substrate;
    use std::io::Write;

    fn ws_with_graph() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let mut f = std::fs::File::create(tmp.path().join("m.x07.json")).unwrap();
        f.write_all(
            br#"{"module_id":"m","schema_version":"1.0","kind":"library","imports":[],
                "decls":[{"kind":"defn","name":"foo","params":[],"result":"Unit","body":[]}]}"#,
        )
        .unwrap();
        let mut store = KglStore::open(&tmp.path().join(".kgl").join("kgl.db")).unwrap();
        store
            .populate(&X07Substrate::default(), tmp.path(), "r1")
            .unwrap();
        tmp
    }

    fn foo_hash(ws: &Path) -> String {
        KglStore::open_workspace(ws)
            .unwrap()
            .find("foo", usize::MAX)
            .unwrap()
            .into_iter()
            .find(|r| r.node.name.as_deref() == Some("foo"))
            .unwrap()
            .node
            .hash
    }

    #[test]
    fn assert_intent_is_visible() {
        let tmp = ws_with_graph();
        let ws = tmp.path();
        let foo = foo_hash(ws);
        let out = run(
            ws,
            "intent",
            &json!({"hash": foo, "purpose": "does foo", "open_questions": ["q1"]}),
            "t0",
            &KglConfig::default(),
        )
        .unwrap();
        assert_eq!(out["updated"], json!(true));

        let store = KglStore::open_workspace(ws).unwrap();
        assert_eq!(
            store.node(&foo).unwrap().unwrap().intent.unwrap().purpose,
            "does foo"
        );
    }

    #[test]
    fn declare_edge_creates_edge() {
        let tmp = ws_with_graph();
        let ws = tmp.path();
        let foo = foo_hash(ws);
        run(
            ws,
            "declare_edge",
            &json!({"from": foo, "to": "file:///x", "kind": "mutates"}),
            "t0",
            &KglConfig::default(),
        )
        .unwrap();
        let store = KglStore::open_workspace(ws).unwrap();
        let edges = store
            .neighbors(&foo, Some(EdgeKind::Mutates), Direction::Out)
            .unwrap();
        assert!(edges.iter().any(|e| e.to == "file:///x"));
    }

    #[test]
    fn declare_edge_rejects_unknown_source_node() {
        // W5: a non-existent `from` hash must error, not create an orphan edge.
        let tmp = ws_with_graph();
        let ws = tmp.path();
        let r = run(
            ws,
            "declare_edge",
            &json!({"from": "deadbeef", "to": "file:///x", "kind": "mutates"}),
            "t0",
            &KglConfig::default(),
        );
        assert!(r.is_err());
        // and no edge leaked into the store
        let store = KglStore::open_workspace(ws).unwrap();
        let edges = store
            .neighbors("deadbeef", Some(EdgeKind::Mutates), Direction::Out)
            .unwrap();
        assert!(edges.is_empty());
    }
}
