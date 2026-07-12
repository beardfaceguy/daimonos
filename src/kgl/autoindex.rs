//! Startup auto-indexing for KGL: build the graph when the daemon starts,
//! mirroring how the trigram WorkspaceIndex is built once at startup (main.rs).
//!
//! Gated by `DAIMONOS_KGL_AUTOINDEX` (off by default); best-effort — runs on a
//! blocking task so it never blocks or breaks startup. The substrate is
//! auto-detected from the workspace (a graphify graph if present, else x07
//! sources). This removes the "stale manual snapshot" problem for the common
//! case: every daemon session starts with a current graph, no manual index.

use crate::config::KglConfig;
use crate::kgl::store::KglStore;
use crate::kgl::substrate::{filtered_walk_builder, Substrate};
use crate::kgl::substrate_graphify::GraphifySubstrate;
use crate::kgl::substrate_x07::X07Substrate;
use anyhow::Result;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Whether startup auto-indexing is enabled (env `DAIMONOS_KGL_AUTOINDEX`).
pub fn enabled() -> bool {
    std::env::var("DAIMONOS_KGL_AUTOINDEX")
        .map(|v| !v.is_empty() && v != "0" && v != "false")
        .unwrap_or(false)
}

/// Detect a substrate for the workspace: graphify if `graphify-out/graph.json`
/// exists (cheap check, common for real repos), else x07 if any `*.x07.json`
/// file exists, else None (nothing to index). The x07 probe honors
/// `cfg.skip_dirs` so it never crawls `target/`, `node_modules/`, etc. Public so
/// the `kgl_query index` tool can default its substrate the same way startup
/// does (avoids an empty x07 scan pruning a graphify graph).
pub fn detect(workspace: &Path, cfg: &KglConfig) -> Option<(&'static str, Box<dyn Substrate>)> {
    // Only choose graphify when its graph actually has code content. A missing,
    // empty, or stub graph.json must fall through to x07 — otherwise an empty
    // graphify index would prune a usable graph built from *.x07.json sources.
    if graphify_has_code_nodes(&workspace.join("graphify-out").join("graph.json")) {
        return Some(("graphify", Box::new(GraphifySubstrate)));
    }
    let has_x07 = filtered_walk_builder(workspace, &cfg.skip_dirs)
        .build()
        .filter_map(|e| e.ok())
        .any(|e| {
            e.path()
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".x07.json"))
        });
    if has_x07 {
        return Some(("x07", Box::new(X07Substrate::new(cfg.skip_dirs.clone()))));
    }
    None
}

/// True if the graphify graph file parses and contains at least one `code`
/// node. Missing/empty/stub graphs return false so `detect` falls through to
/// x07 rather than picking an empty graphify index whose prune wipes the graph.
/// `pub(crate)` so the `kgl_query index` tool can apply the same guard to an
/// explicit `substrate:"graphify"` request (not just the auto-detect path).
pub(crate) fn graphify_has_code_nodes(path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(g) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    g.get("nodes")
        .and_then(|n| n.as_array())
        .map(|nodes| {
            nodes
                .iter()
                .any(|n| n.get("file_type").and_then(|f| f.as_str()) == Some("code"))
        })
        .unwrap_or(false)
}

/// Build/refresh the KGL graph for `workspace` (best-effort). Returns
/// `Some((substrate, nodes, edges))` when a substrate was found, else `None`.
pub fn run_startup(
    workspace: &Path,
    now: &str,
    cfg: &KglConfig,
) -> Result<Option<(&'static str, usize, usize)>> {
    let Some((name, sub)) = detect(workspace, cfg) else {
        return Ok(None);
    };
    let mut store = KglStore::open_workspace_with(workspace, cfg)?;
    let (nodes, edges) = store.populate(sub.as_ref(), workspace, now)?;
    Ok(Some((name, nodes, edges)))
}

/// Spawn a background thread that watches `workspace` and rebuilds the KGL graph
/// (debounced) when files change, keeping it fresh within a session. Owns the
/// watcher for the process lifetime; best-effort; gated by the caller. Because
/// `kgl_query` opens the store fresh per call, background rebuilds are picked up
/// automatically with no handler/session changes.
pub fn spawn_watcher(workspace: PathBuf, quiet: bool, cfg: KglConfig) {
    let spawned = std::thread::Builder::new()
        .name("kgl-watch".to_string())
        .spawn(move || {
            let dirty = Arc::new(AtomicBool::new(false));
            let Some(_watcher) = build_watcher(&workspace, dirty.clone(), &cfg) else {
                if !quiet {
                    eprintln!("kgl: file watcher not started (no dirs registered)");
                }
                return;
            };
            let debounce = Duration::from_secs(cfg.debounce_secs);
            loop {
                std::thread::sleep(debounce);
                if dirty.swap(false, Ordering::Relaxed) {
                    let now = chrono::Utc::now().to_rfc3339();
                    // Surface rebuild failures (e.g. SQLITE_BUSY, parse errors)
                    // instead of silently leaving the graph stale.
                    if let Err(e) = run_startup(&workspace, &now, &cfg) {
                        if !quiet {
                            eprintln!("kgl: background re-index failed: {e}");
                        }
                    }
                }
            }
        });
    if let Err(e) = spawned {
        if !quiet {
            eprintln!("kgl: failed to spawn watcher thread: {e}");
        }
    }
}

/// Build a non-recursive watch per surviving directory (mirrors
/// pipeline_cache::start_watcher): walk with the `ignore` crate, skip
/// build/vcs/`.kgl` dirs, cap total watches. The change closure ignores events
/// under `.kgl/` so our own store writes can't self-trigger a rebuild loop.
/// A change is relevant unless it's entirely under our own `.kgl/` store dir —
/// guards against a rebuild's own writes self-triggering the watcher.
fn relevant_event(paths: &[PathBuf]) -> bool {
    paths
        .iter()
        .any(|p| !p.components().any(|c| c.as_os_str() == ".kgl"))
}

fn build_watcher(
    workspace: &Path,
    dirty: Arc<AtomicBool>,
    cfg: &KglConfig,
) -> Option<RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if let Ok(event) = res {
            if !(event.kind.is_modify() || event.kind.is_create() || event.kind.is_remove()) {
                return;
            }
            if relevant_event(&event.paths) {
                dirty.store(true, Ordering::Relaxed);
            }
        }
    })
    .ok()?;

    let mut watched = 0usize;
    for entry in filtered_walk_builder(workspace, &cfg.skip_dirs)
        .build()
        .flatten()
    {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        if watched >= cfg.max_watches {
            eprintln!(
                "kgl watcher: max_watches ({}) reached — directories beyond this point will \
                 not be watched; changes there will leave the graph stale until the next \
                 manual index. Raise kgl.max_watches in daimonos.toml if needed.",
                cfg.max_watches
            );
            break;
        }
        if watcher
            .watch(entry.path(), RecursiveMode::NonRecursive)
            .is_ok()
        {
            watched += 1;
        }
    }
    if watched == 0 {
        return None;
    }
    Some(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn detect_prefers_graphify_then_x07_then_none() {
        let cfg = KglConfig::default();
        let tmp = tempfile::tempdir().unwrap();
        assert!(detect(tmp.path(), &cfg).is_none());

        std::fs::File::create(tmp.path().join("m.x07.json"))
            .unwrap()
            .write_all(br#"{"module_id":"m","decls":[]}"#)
            .unwrap();
        assert_eq!(detect(tmp.path(), &cfg).unwrap().0, "x07");

        std::fs::create_dir_all(tmp.path().join("graphify-out")).unwrap();
        std::fs::File::create(tmp.path().join("graphify-out").join("graph.json"))
            .unwrap()
            .write_all(
                br#"{"nodes":[{"id":"n1","label":".f()","file_type":"code","source_file":"a.rs"}],"links":[]}"#,
            )
            .unwrap();
        assert_eq!(detect(tmp.path(), &cfg).unwrap().0, "graphify");
    }

    #[test]
    fn detect_falls_through_empty_graphify_to_x07() {
        // An empty/stub graphify graph must not win over usable x07 sources.
        let cfg = KglConfig::default();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("graphify-out")).unwrap();
        std::fs::write(
            tmp.path().join("graphify-out").join("graph.json"),
            br#"{"nodes":[],"links":[]}"#,
        )
        .unwrap();
        std::fs::File::create(tmp.path().join("m.x07.json"))
            .unwrap()
            .write_all(br#"{"module_id":"m","decls":[]}"#)
            .unwrap();
        assert_eq!(detect(tmp.path(), &cfg).unwrap().0, "x07");
    }

    #[test]
    fn detect_x07_probe_skips_configured_dirs() {
        // An .x07.json buried under a skip_dir must not make detect pick x07.
        let cfg = KglConfig::default();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("node_modules")).unwrap();
        std::fs::File::create(tmp.path().join("node_modules").join("m.x07.json"))
            .unwrap()
            .write_all(br#"{"module_id":"m","decls":[]}"#)
            .unwrap();
        assert!(detect(tmp.path(), &cfg).is_none());
    }

    #[test]
    fn run_startup_indexes_x07_workspace() {
        let cfg = KglConfig::default();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::File::create(tmp.path().join("m.x07.json"))
            .unwrap()
            .write_all(
                br#"{"module_id":"m","schema_version":"1.0","kind":"library","imports":[],
                    "decls":[{"kind":"defn","name":"foo","params":[],"result":"Unit","body":[]}]}"#,
            )
            .unwrap();
        let (sub, nodes, _edges) = run_startup(tmp.path(), "t0", &cfg).unwrap().unwrap();
        assert_eq!(sub, "x07");
        assert!(nodes >= 2); // module + foo

        let store = KglStore::open_workspace(tmp.path()).unwrap();
        assert!(store
            .find("foo", usize::MAX)
            .unwrap()
            .iter()
            .any(|r| r.node.name.as_deref() == Some("foo")));
    }

    #[test]
    fn run_startup_no_substrate_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(run_startup(tmp.path(), "t0", &KglConfig::default())
            .unwrap()
            .is_none());
    }

    #[test]
    fn build_watcher_registers_over_a_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        let dirty = Arc::new(AtomicBool::new(false));
        assert!(build_watcher(tmp.path(), dirty, &KglConfig::default()).is_some());
    }

    #[test]
    fn relevant_event_ignores_kgl_store_writes() {
        // our own .kgl/ writes must not count (else the rebuild self-triggers)
        assert!(!relevant_event(&[PathBuf::from("/ws/.kgl/kgl.db")]));
        assert!(!relevant_event(&[])); // pathless events ignored
                                       // real source changes are relevant, even if mixed with a .kgl write
        assert!(relevant_event(&[PathBuf::from("/ws/src/a.rs")]));
        assert!(relevant_event(&[
            PathBuf::from("/ws/.kgl/kgl.db"),
            PathBuf::from("/ws/src/a.rs"),
        ]));
    }

    #[test]
    fn watcher_flags_dirty_on_file_change() {
        // Timing-tolerant: a write in a watched dir should flip `dirty` within
        // a few seconds (inotify delivery is async).
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        let dirty = Arc::new(AtomicBool::new(false));
        let _w =
            build_watcher(tmp.path(), dirty.clone(), &KglConfig::default()).expect("watcher built");
        std::thread::sleep(Duration::from_millis(300)); // let the watch arm
        std::fs::write(tmp.path().join("src").join("f.txt"), b"x").unwrap();
        let mut flipped = false;
        for _ in 0..50 {
            if dirty.load(Ordering::Relaxed) {
                flipped = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(flipped, "watcher did not flag dirty on a file change");
    }
}
