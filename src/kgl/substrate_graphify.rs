//! Graphify substrate backend: builds a KGL graph from a graphify `graph.json`
//! (networkx node-link format) so KGL's enforced intent/provenance/orientation
//! layer can sit over a REAL codebase — e.g. the daimonos Rust source — without
//! waiting for native agent-language sources.
//!
//! graphify is a DERIVED, AST-level graph: it gives structure (defs + calls +
//! containment) but no effects/contracts/provenance. So this backend yields
//! nodes + `calls`/`depends_on` edges only; effects are empty (no reads/mutates
//! inferred) and intent/provenance are agent-declared via the metadata channel.
//! Node identity is graphify's stable node `id` (not a content hash) — a Tacit
//! backend would instead supply BLAKE3 definition hashes.
//!
//! Context: docs/adr/013-repowise-as-kgl-substrate.md. KGL now indexes
//! [`crate::kgl::substrate_repowise`]; this backend is a **supported fallback**,
//! not the default, and is no longer refreshed automatically. It earns its keep
//! because the repowise substrate reads that tool's private SQLite schema with
//! no compatibility promise, whereas `graph.json` is networkx node-link — so if
//! repowise renames a column, this is the recovery path. Waking it is one
//! command: `graphify update .`, then request `substrate:"graphify"`.

use crate::kgl::model::{DefNode, Derivation, Edge, EdgeKind, NodeKind, SubstrateKind};
use crate::kgl::substrate::{IndexResult, Substrate};
use anyhow::Result;
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;

/// Builds a KGL graph from `<root>/graphify-out/graph.json`.
pub struct GraphifySubstrate;

impl Substrate for GraphifySubstrate {
    fn kind(&self) -> SubstrateKind {
        SubstrateKind::Rust
    }

    fn index(&self, root: &Path) -> Result<IndexResult> {
        let mut out = IndexResult::default();
        let path = root.join("graphify-out").join("graph.json");
        let content = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            // File absent — no graphify substrate; caller falls back gracefully.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            // File present but unreadable: propagate so populate aborts rather
            // than treating an IO failure as "no nodes" and pruning the graph.
            Err(e) => return Err(e.into()),
        };
        let g: Value = serde_json::from_str(&content)?;

        // Code nodes only (graphify also has document/concept/rationale nodes).
        let mut code_ids: HashSet<String> = HashSet::new();
        if let Some(nodes) = g.get("nodes").and_then(|n| n.as_array()) {
            for n in nodes {
                if n.get("file_type").and_then(|f| f.as_str()) != Some("code") {
                    continue;
                }
                let Some(id) = n.get("id").and_then(|i| i.as_str()) else {
                    continue;
                };
                let label = n.get("label").and_then(|l| l.as_str()).unwrap_or(id);
                let source_file = n.get("source_file").and_then(|s| s.as_str());
                out.nodes.push(DefNode {
                    hash: id.to_string(),
                    kind: classify(label, source_file),
                    name: Some(label.to_string()),
                    substrate: SubstrateKind::Rust,
                    file: source_file.map(str::to_string),
                    span: n
                        .get("source_location")
                        .and_then(|s| s.as_str())
                        .map(str::to_string),
                });
                code_ids.insert(id.to_string());
            }
        }

        // Structural edges between code nodes (drop edges to docs/concepts).
        if let Some(links) = g.get("links").and_then(|l| l.as_array()) {
            for l in links {
                let (Some(src), Some(tgt)) = (
                    l.get("source").and_then(|s| s.as_str()),
                    l.get("target").and_then(|t| t.as_str()),
                ) else {
                    continue;
                };
                if !code_ids.contains(src) || !code_ids.contains(tgt) {
                    continue;
                }
                let Some(kind) =
                    map_relation(l.get("relation").and_then(|r| r.as_str()).unwrap_or(""))
                else {
                    continue;
                };
                let confidence = l
                    .get("confidence_score")
                    .and_then(|c| c.as_f64())
                    .unwrap_or(1.0) as f32;
                out.edges.push(Edge {
                    from: src.to_string(),
                    to: tgt.to_string(),
                    kind,
                    derivation: Derivation::Derived,
                    confidence,
                });
            }
        }
        Ok(out)
    }
}

/// Heuristic node kind from a graphify label: `.foo()` => Function; a label that
/// equals its source file => Module; otherwise a Type (struct/enum/trait).
fn classify(label: &str, source_file: Option<&str>) -> NodeKind {
    if label.contains('(') {
        NodeKind::Function
    } else if Some(label) == source_file {
        NodeKind::Module
    } else {
        NodeKind::Type
    }
}

/// Map graphify relations to KGL edge kinds. `calls` -> Calls; structural
/// containment/definition/impl/reference -> DependsOn; graphify's semantic-pass
/// relations (rationale_for, conceptually_related_to, ...) are dropped.
fn map_relation(rel: &str) -> Option<EdgeKind> {
    match rel {
        "calls" => Some(EdgeKind::Calls),
        "contains" | "defines" | "method" | "implements" | "references" => {
            Some(EdgeKind::DependsOn)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_graph(dir: &Path, json: &str) {
        std::fs::create_dir_all(dir.join("graphify-out")).unwrap();
        let mut f = std::fs::File::create(dir.join("graphify-out").join("graph.json")).unwrap();
        f.write_all(json.as_bytes()).unwrap();
    }

    #[test]
    fn builds_code_only_graph_from_graphify_json() {
        let tmp = tempfile::tempdir().unwrap();
        write_graph(
            tmp.path(),
            r#"{
                "directed": false, "multigraph": false, "graph": {}, "hyperedges": [],
                "nodes": [
                    {"id":"a_foo","label":".foo()","file_type":"code","source_file":"src/a.rs","source_location":"L1"},
                    {"id":"a_bar","label":".bar()","file_type":"code","source_file":"src/a.rs","source_location":"L9"},
                    {"id":"doc1","label":"README","file_type":"document","source_file":"README.md"}
                ],
                "links": [
                    {"relation":"calls","source":"a_foo","target":"a_bar","confidence_score":1.0},
                    {"relation":"references","source":"a_foo","target":"doc1","confidence_score":1.0},
                    {"relation":"rationale_for","source":"a_bar","target":"a_foo","confidence_score":1.0}
                ]
            }"#,
        );

        let idx = GraphifySubstrate.index(tmp.path()).unwrap();
        assert_eq!(idx.nodes.len(), 2); // doc node excluded
        assert!(idx.nodes.iter().all(|n| n.substrate == SubstrateKind::Rust));
        // calls kept; edge to doc dropped (non-code target); semantic edge dropped
        assert_eq!(idx.edges.len(), 1);
        assert_eq!(idx.edges[0].kind, EdgeKind::Calls);
        assert_eq!(idx.edges[0].from, "a_foo");
        assert_eq!(idx.edges[0].to, "a_bar");
    }

    #[test]
    fn missing_graph_degrades_to_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let idx = GraphifySubstrate.index(tmp.path()).unwrap();
        assert!(idx.nodes.is_empty());
    }

    /// Runs KGL over the REAL daimonos graphify graph. Ignored by default
    /// (requires ./graphify-out/graph.json); run with:
    ///   cargo test kgl::substrate_graphify -- --ignored --nocapture
    #[test]
    #[ignore = "requires ./graphify-out/graph.json"]
    fn index_real_daimonos_graph() {
        use crate::kgl::store::KglStore;
        let mut store = KglStore::open_in_memory().unwrap();
        let (nodes, edges) = store
            .populate(&GraphifySubstrate, Path::new("."), "real")
            .unwrap();
        println!("\n[graphify->kgl] daimonos Rust graph: {nodes} nodes, {edges} edges");
        let hits = store.find("config", usize::MAX).unwrap();
        println!(
            "find 'config' -> {} nodes (e.g. {:?})",
            hits.len(),
            hits.iter()
                .take(5)
                .filter_map(|r| r.node.name.clone())
                .collect::<Vec<_>>()
        );
        if let Some(first) = hits.first() {
            let blast = store.blast_radius(&first.node.hash, usize::MAX).unwrap();
            println!(
                "blast_radius({:?}) -> {} dependents",
                first.node.name,
                blast.len()
            );
        }
        assert!(nodes > 100, "expected a substantial real graph");
    }
}
