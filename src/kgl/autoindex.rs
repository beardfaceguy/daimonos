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
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Directories never worth watching (build/vcs churn + our own store).
const WATCH_SKIP_DIRS: &[&str] = &["target", ".git", ".jj", "node_modules", ".kgl"];
/// Hard cap on inotify watches so we never exhaust fs.inotify.max_user_watches.
const MAX_WATCHES: usize = 4096;
/// Debounce window: coalesce change bursts into at most one rebuild per tick.
const DEBOUNCE: Duration = Duration::from_secs(2);

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

/// Spawn a background thread that watches `workspace` and rebuilds the KGL graph
/// (debounced) when files change, keeping it fresh within a session. Owns the
/// watcher for the process lifetime; best-effort; gated by the caller. Because
/// `kgl_query` opens the store fresh per call, background rebuilds are picked up
/// automatically with no handler/session changes.
pub fn spawn_watcher(workspace: PathBuf, quiet: bool) {
    let _ = std::thread::Builder::new()
        .name("kgl-watch".to_string())
        .spawn(move || {
            let dirty = Arc::new(AtomicBool::new(false));
            let Some(_watcher) = build_watcher(&workspace, dirty.clone()) else {
                if !quiet {
                    eprintln!("kgl: file watcher not started (no dirs registered)");
                }
                return;
            };
            loop {
                std::thread::sleep(DEBOUNCE);
                if dirty.swap(false, Ordering::Relaxed) {
                    let now = chrono::Utc::now().to_rfc3339();
                    let _ = run_startup(&workspace, &now);
                }
            }
        });
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

fn build_watcher(workspace: &Path, dirty: Arc<AtomicBool>) -> Option<RecommendedWatcher> {
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
    for entry in ignore::WalkBuilder::new(workspace)
        .hidden(true)
        .git_ignore(true)
        .filter_entry(|e| {
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            !WATCH_SKIP_DIRS.iter().any(|d| *d == name.as_ref())
        })
        .build()
        .flatten()
    {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        if watched >= MAX_WATCHES {
            break;
        }
        if watcher.watch(entry.path(), RecursiveMode::NonRecursive).is_ok() {
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

    #[test]
    fn build_watcher_registers_over_a_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        let dirty = Arc::new(AtomicBool::new(false));
        assert!(build_watcher(tmp.path(), dirty).is_some());
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
        let _w = build_watcher(tmp.path(), dirty.clone()).expect("watcher built");
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
