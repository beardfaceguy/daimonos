//! Repowise substrate backend: builds a KGL graph from repowise's index
//! (`.repowise/wiki.db`), replacing the graphify backend this repo carried
//! only to feed KGL (see `docs/adr/012-graphify-as-kgl-substrate.md`).
//!
//! Like graphify this is a DERIVED, AST-level graph: structure only. Effects
//! stay empty and intent/provenance remain agent-declared through the metadata
//! channel. It is however a materially richer source — on this repo, 10,629
//! Rust nodes and 16,790 edges against graphify's 6,050 — and it carries call
//! edges, precise line spans, and qualified names that graphify did not.
//!
//! # Identity
//!
//! `node_id` is repowise's own `path/to/file.rs::Qualified::Name`, so identity
//! is a function of file path and qualified name rather than an opaque counter.
//! That is what makes the swap safe: KGL's agent-declared intent and provenance
//! are keyed by node hash, and a scheme that renumbered on every re-index would
//! detach all of it. The same id shape is what repowise's own MCP tools accept,
//! so a hash here is directly usable against `get_symbol`.
//!
//! Note the graph reads `wiki.db`, not `.repowise/knowledge-graph.json`. That
//! JSON file is a summary export: 2,006 nodes, 3,975 edges, and — decisively —
//! **no `calls` relation at all**. Indexing it would have silently produced a
//! call-free graph and broken `blast_radius`.

use crate::kgl::model::{DefNode, Derivation, Edge, EdgeKind, NodeKind, SubstrateKind};
use crate::kgl::substrate::{IndexResult, Substrate};
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Path of repowise's index relative to a workspace root.
pub const WIKI_DB: &str = ".repowise/wiki.db";

/// Builds a KGL graph from `<root>/.repowise/wiki.db`.
pub struct RepowiseSubstrate;

/// Open repowise's index read-only.
///
/// Read-only is not a detail: repowise owns this database and may be writing to
/// it from its own post-commit refresh while KGL indexes. Opening read-write
/// would let a KGL index acquire a lock and stall — or corrupt — a tool that is
/// not ours.
fn open_readonly(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("open repowise index at {}", path.display()))
}

fn db_path(root: &Path) -> PathBuf {
    root.join(WIKI_DB)
}

impl Substrate for RepowiseSubstrate {
    fn kind(&self) -> SubstrateKind {
        SubstrateKind::Rust
    }

    fn index(&self, root: &Path) -> Result<IndexResult> {
        let mut out = IndexResult::default();
        let path = db_path(root);
        if !path.exists() {
            // No repowise index. Caller falls through to another substrate;
            // returning empty (rather than erroring) mirrors the graphify
            // backend so an absent index is never mistaken for "no nodes",
            // which populate's prune would treat as a licence to wipe.
            return Ok(out);
        }
        let conn = open_readonly(&path)?;

        let mut node_stmt = conn.prepare(
            "SELECT node_id, kind, node_type, name, file_path, start_line, end_line \
             FROM graph_nodes WHERE node_id IS NOT NULL",
        )?;
        let rows = node_stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, Option<i64>>(6)?,
            ))
        })?;

        let mut ids: HashSet<String> = HashSet::new();
        for row in rows {
            let (node_id, kind, node_type, name, file_path, start, end) = row?;
            out.nodes.push(DefNode {
                kind: classify(kind.as_deref(), node_type.as_deref()),
                // Advisory only; identity is the hash. Fall back to the id so a
                // nameless file node still renders as something.
                name: Some(name.unwrap_or_else(|| node_id.clone())),
                substrate: SubstrateKind::Rust,
                file: file_path,
                span: span_of(start, end),
                hash: node_id.clone(),
            });
            ids.insert(node_id);
        }

        let mut edge_stmt = conn.prepare(
            "SELECT source_node_id, target_node_id, edge_type, confidence FROM graph_edges",
        )?;
        let edges = edge_stmt.query_map([], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<f64>>(3)?,
            ))
        })?;

        for edge in edges {
            let (Some(from), Some(to), edge_type, confidence) = edge? else {
                continue;
            };
            // Both endpoints must be nodes we kept. repowise records edges to
            // external systems (`file:external:...`) and to non-code pages;
            // an edge to a node KGL never stored is a dangling reference.
            if !ids.contains(&from) || !ids.contains(&to) {
                continue;
            }
            let Some(kind) = map_relation(edge_type.as_deref().unwrap_or("")) else {
                continue;
            };
            out.edges.push(Edge {
                from,
                to,
                kind,
                derivation: Derivation::Derived,
                confidence: confidence.unwrap_or(1.0) as f32,
            });
        }

        Ok(out)
    }
}

/// Map repowise's `kind` (with `node_type` as the fallback) onto KGL's kinds.
///
/// repowise distinguishes far more than KGL models, so several collapse: a
/// `property` is a field and a `variable` a local, and neither is a definition
/// KGL reasons about separately — both land on `Type` alongside `impl`, which
/// is also where graphify's cruder classifier put everything non-callable.
fn classify(kind: Option<&str>, node_type: Option<&str>) -> NodeKind {
    match kind {
        Some("function") | Some("method") => NodeKind::Function,
        Some("module") => NodeKind::Module,
        Some("constant") => NodeKind::Const,
        // `node_type == "file"` arrives with a NULL kind.
        None if node_type == Some("file") => NodeKind::Module,
        _ => NodeKind::Type,
    }
}

/// Map repowise relations onto KGL edge kinds.
///
/// `co_changes` and `framework` are deliberately dropped. The first is derived
/// from git history rather than structure — two files changing together is a
/// correlation, not a dependency — and the second is a heuristic guess about
/// framework wiring. Admitting either would put edges into the graph that no
/// read of the source can justify, which is the thing KGL's `Derived`
/// derivation is asserting.
fn map_relation(rel: &str) -> Option<EdgeKind> {
    match rel {
        "calls" => Some(EdgeKind::Calls),
        "defines" | "has_method" | "imports" | "implements" | "extends" | "type_use"
        | "dynamic_uses" => Some(EdgeKind::DependsOn),
        _ => None,
    }
}

/// Render a line range the way KGL's `span` field expects: `L804-L816`, or
/// `L804` when the definition is a single line.
fn span_of(start: Option<i64>, end: Option<i64>) -> Option<String> {
    match (start, end) {
        (Some(s), Some(e)) if e > s => Some(format!("L{s}-L{e}")),
        (Some(s), _) => Some(format!("L{s}")),
        _ => None,
    }
}

/// True if repowise's index exists and holds at least one graph node.
///
/// The same non-destructive guard the graphify backend needs: an index that is
/// missing, empty, or mid-build must not be indexed, because populate's prune
/// would read zero nodes as "everything was deleted" and wipe the graph along
/// with its agent-declared metadata.
pub(crate) fn repowise_has_code_nodes(root: &Path) -> bool {
    let path = db_path(root);
    if !path.exists() {
        return false;
    }
    let Ok(conn) = open_readonly(&path) else {
        return false;
    };
    conn.query_row("SELECT COUNT(*) FROM graph_nodes", [], |r| {
        r.get::<_, i64>(0)
    })
    .map(|n| n > 0)
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal wiki.db with repowise's real column names.
    /// (node_id, kind, node_type, name, start_line, end_line)
    type NodeRow<'a> = (&'a str, Option<&'a str>, &'a str, &'a str, i64, i64);

    fn write_db(dir: &Path, nodes: &[NodeRow<'_>], edges: &[(&str, &str, &str)]) {
        std::fs::create_dir_all(dir.join(".repowise")).unwrap();
        let conn = Connection::open(dir.join(WIKI_DB)).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS graph_nodes (node_id TEXT, kind TEXT, node_type TEXT, name TEXT,
                                       file_path TEXT, start_line INTEGER, end_line INTEGER);
             CREATE TABLE IF NOT EXISTS graph_edges (source_node_id TEXT, target_node_id TEXT,
                                       edge_type TEXT, confidence FLOAT);",
        )
        .unwrap();
        for (id, kind, ntype, name, s, e) in nodes {
            conn.execute(
                "INSERT INTO graph_nodes VALUES (?1,?2,?3,?4,?5,?6,?7)",
                rusqlite::params![id, kind, ntype, name, "src/a.rs", s, e],
            )
            .unwrap();
        }
        for (from, to, rel) in edges {
            conn.execute(
                "INSERT INTO graph_edges VALUES (?1,?2,?3,1.0)",
                rusqlite::params![from, to, rel],
            )
            .unwrap();
        }
    }

    #[test]
    fn builds_graph_from_repowise_index() {
        let tmp = tempfile::tempdir().unwrap();
        write_db(
            tmp.path(),
            &[
                ("src/a.rs::foo", Some("function"), "symbol", "foo", 1, 9),
                ("src/a.rs::Bar", Some("impl"), "symbol", "Bar", 11, 20),
                ("src/a.rs", None, "file", "a.rs", 1, 40),
            ],
            &[
                ("src/a.rs::foo", "src/a.rs::Bar", "calls"),
                ("src/a.rs", "src/a.rs::foo", "defines"),
                // Dropped: correlation from git history, not structure.
                ("src/a.rs::foo", "src/a.rs::Bar", "co_changes"),
                // Dropped: endpoint is not a node we stored.
                ("src/a.rs::foo", "file:external:serde", "imports"),
            ],
        );

        let out = RepowiseSubstrate.index(tmp.path()).unwrap();

        assert_eq!(out.nodes.len(), 3);
        let foo = out
            .nodes
            .iter()
            .find(|n| n.hash == "src/a.rs::foo")
            .unwrap();
        assert_eq!(foo.kind, NodeKind::Function);
        assert_eq!(foo.span.as_deref(), Some("L1-L9"));
        assert_eq!(
            out.nodes
                .iter()
                .find(|n| n.hash == "src/a.rs")
                .unwrap()
                .kind,
            NodeKind::Module,
            "a file node carries a NULL kind and must still classify as a module"
        );

        assert_eq!(out.edges.len(), 2, "co_changes and the dangling edge drop");
        assert!(out
            .edges
            .iter()
            .any(|e| e.kind == EdgeKind::Calls && e.from == "src/a.rs::foo"));
    }

    /// An absent index must read as empty, never as an error and never as
    /// "zero nodes, prune everything".
    #[test]
    fn missing_index_degrades_to_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let out = RepowiseSubstrate.index(tmp.path()).unwrap();
        assert!(out.nodes.is_empty() && out.edges.is_empty());
        assert!(!repowise_has_code_nodes(tmp.path()));
    }

    /// The guard is what stops populate's prune from wiping agent metadata, so
    /// an empty-but-present index must read as "not usable".
    #[test]
    fn empty_index_is_not_a_usable_substrate() {
        let tmp = tempfile::tempdir().unwrap();
        write_db(tmp.path(), &[], &[]);
        assert!(!repowise_has_code_nodes(tmp.path()));

        write_db(
            tmp.path(),
            &[("src/a.rs::foo", Some("function"), "symbol", "foo", 1, 2)],
            &[],
        );
        assert!(repowise_has_code_nodes(tmp.path()));
    }

    /// Identity must be the path::name id, because KGL's agent-declared intent
    /// and provenance are keyed by it. If this ever became an opaque counter,
    /// every re-index would orphan that metadata.
    #[test]
    fn identity_is_derived_from_path_and_name() {
        let tmp = tempfile::tempdir().unwrap();
        write_db(
            tmp.path(),
            &[("src/a.rs::foo", Some("function"), "symbol", "foo", 1, 2)],
            &[],
        );
        let first = RepowiseSubstrate.index(tmp.path()).unwrap();
        let second = RepowiseSubstrate.index(tmp.path()).unwrap();
        assert_eq!(first.nodes[0].hash, "src/a.rs::foo");
        assert_eq!(
            first.nodes[0].hash, second.nodes[0].hash,
            "identity must be stable across re-indexes"
        );
    }

    /// Indexes this repository's real repowise database. `--ignored` because it
    /// depends on a local index existing; run with:
    ///   cargo test kgl::substrate_repowise -- --ignored --nocapture
    #[test]
    #[ignore]
    fn index_real_daimonos_repowise_index() {
        let out = RepowiseSubstrate.index(Path::new(".")).unwrap();
        let calls = out
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .count();
        println!(
            "\n[repowise->kgl] {} nodes, {} edges ({} calls)",
            out.nodes.len(),
            out.edges.len(),
            calls
        );
        assert!(out.nodes.len() > 1000, "expected a populated graph");
        assert!(
            calls > 0,
            "call edges are the relation graphify's summary export lacked"
        );
    }
}
