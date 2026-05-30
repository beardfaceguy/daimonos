//! Startup auto-indexing for KGL: build the graph when the daemon starts,
//! mirroring how the trigram WorkspaceIndex is built once at startup (main.rs).
//!
//! Gated by `DAIMONOS_KGL_AUTOINDEX` (off by default); best-effort — runs on a
//! blocking task so it never blocks or breaks startup. The substrate is
//! auto-detected from the workspace (a graphify graph if present, else x07
//! sources). This removes the "stale manual snapshot" problem for the common
//! case: every daemon session starts with a current graph, no manual index.

use crate::kgl::store::KglStore;
use crate::kgl::substrate::Substrate;
use crate::kgl::substrate_graphify::GraphifySubstrate;
use crate::kgl::substrate_x07::X07Substrate;
use anyhow::Result;
use std::path::Path;

/// Whether startup auto-indexing is enabled (env `DAIMONOS_KGL_AUTOINDEX`).
pub fn enabled() -> bool {
    std::env::var("DAIMONOS_KGL_AUTOINDEX")
        .map(|v| !v.is_empty() && v != "0" && v != "false")
        .unwrap_or(false)
}

/// Detect a substrate for the workspace: graphify if `graphify-out/graph.json`
/// exists (cheap check, common for real repos), else x07 if any `*.x07.json`
/// file exists, else None (nothing to index).
fn detect(workspace: &Path) -> Option<(&'static str, Box<dyn Substrate>)> {
    if workspace.join("graphify-out").join("graph.json").is_file() {
        return Some(("graphify", Box::new(GraphifySubstrate)));
    }
    let has_x07 = walkdir::WalkDir::new(workspace)
        .into_iter()
        .filter_map(|e| e.ok())
        .any(|e| {
            e.path()
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".x07.json"))
        });
    if has_x07 {
        return Some(("x07", Box::new(X07Substrate)));
    }
    None
}

/// Build/refresh the KGL graph for `workspace` (best-effort). Returns
/// `Some((substrate, nodes, edges))` when a substrate was found, else `None`.
pub fn run_startup(workspace: &Path, now: &str) -> Result<Option<(&'static str, usize, usize)>> {
    let Some((name, sub)) = detect(workspace) else {
        return Ok(None);
    };
    let mut store = KglStore::open_workspace(workspace)?;
    let (nodes, edges) = store.populate(sub.as_ref(), workspace, now)?;
    Ok(Some((name, nodes, edges)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn detect_prefers_graphify_then_x07_then_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(detect(tmp.path()).is_none());

        std::fs::File::create(tmp.path().join("m.x07.json"))
            .unwrap()
            .write_all(br#"{"module_id":"m","decls":[]}"#)
            .unwrap();
        assert_eq!(detect(tmp.path()).unwrap().0, "x07");

        std::fs::create_dir_all(tmp.path().join("graphify-out")).unwrap();
        std::fs::File::create(tmp.path().join("graphify-out").join("graph.json"))
            .unwrap()
            .write_all(br#"{"nodes":[],"links":[]}"#)
            .unwrap();
        assert_eq!(detect(tmp.path()).unwrap().0, "graphify");
    }

    #[test]
    fn run_startup_indexes_x07_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::File::create(tmp.path().join("m.x07.json"))
            .unwrap()
            .write_all(
                br#"{"module_id":"m","schema_version":"1.0","kind":"library","imports":[],
                    "decls":[{"kind":"defn","name":"foo","params":[],"result":"Unit","body":[]}]}"#,
            )
            .unwrap();
        let (sub, nodes, _edges) = run_startup(tmp.path(), "t0").unwrap().unwrap();
        assert_eq!(sub, "x07");
        assert!(nodes >= 2); // module + foo

        let store = KglStore::open_workspace(tmp.path()).unwrap();
        assert!(store
            .find("foo")
            .unwrap()
            .iter()
            .any(|r| r.node.name.as_deref() == Some("foo")));
    }

    #[test]
    fn run_startup_no_substrate_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(run_startup(tmp.path(), "t0").unwrap().is_none());
    }
}
