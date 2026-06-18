//! X07 substrate backend for KGL. Reuses the existing [`crate::plugins::x07`]
//! AST parse for module/function/import extraction and computes content-addressed
//! node identity by hashing the canonical JSON of each declaration. This is the
//! lowest-effort substrate to stand up because daimonos already integrates X07.
//!
//! v0 scope: solidly extracts Module + Function nodes and `depends_on` edges
//! (from imports). `calls` edges and `effects` are best-effort — the daimonos
//! X07 parse is signature-level and the X07 files present may be sparse, so both
//! degrade safely to empty when the body/effect shape is absent or unknown.

use crate::kgl::model::{DefNode, Derivation, Edge, EdgeKind, EffectFacts, NodeKind, SubstrateKind};
use crate::kgl::substrate::{content_hash, filtered_walk_builder, IndexResult, Substrate};
use crate::plugins::x07::parse_x07_module;
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

/// Extracts a KGL graph from a workspace of `*.x07.json` files. Carries the
/// shared skip-dir list so the walk honors `[kgl] skip_dirs` (and never crawls
/// `target/`, `node_modules/`, etc.).
#[derive(Default)]
pub struct X07Substrate {
    skip_dirs: Vec<String>,
}

impl X07Substrate {
    /// Construct with the configured skip-dir list (from `[kgl] skip_dirs`).
    pub fn new(skip_dirs: Vec<String>) -> Self {
        Self { skip_dirs }
    }
}

impl Substrate for X07Substrate {
    fn kind(&self) -> SubstrateKind {
        SubstrateKind::X07
    }

    fn index(&self, root: &Path) -> Result<IndexResult> {
        let mut out = IndexResult::default();
        for entry in filtered_walk_builder(root, &self.skip_dirs)
            .build()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.ends_with(".x07.json") {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(path) else {
                continue;
            };
            let Ok(ast) = serde_json::from_str::<Value>(&content) else {
                continue;
            };
            let rel = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();
            index_module(&ast, &rel, &mut out);
        }
        Ok(out)
    }
}

fn index_module(ast: &Value, rel: &str, out: &mut IndexResult) {
    let Some(module) = parse_x07_module(ast, rel) else {
        return;
    };

    // Module node: hash the whole canonical module file.
    let module_hash = content_hash(ast);
    out.nodes.push(DefNode {
        hash: module_hash.clone(),
        kind: NodeKind::Module,
        name: Some(module.module_id.clone()),
        substrate: SubstrateKind::X07,
        file: Some(rel.to_string()),
        span: None,
    });

    // depends_on edges: module -> imported module. The imported module's own
    // hash may live in another file, so target a URN now; the store resolves
    // `x07mod:<name>` to a concrete hash once all modules are indexed.
    for import in &module.imports {
        out.edges.push(Edge {
            from: module_hash.clone(),
            to: format!("x07mod:{import}"),
            kind: EdgeKind::DependsOn,
            derivation: Derivation::Derived,
            confidence: 1.0,
        });
    }

    // Function nodes: hash each declaration's canonical JSON (captures the body
    // when present). Use the raw decls array for hashing; the parsed
    // `FunctionInfo` supplies name/kind.
    let decls = ast.get("decls").and_then(|d| d.as_array());
    for func in &module.functions {
        let Some((idx, decl_value)) = decls.and_then(|arr| {
            arr.iter()
                .enumerate()
                .find(|(_, d)| d.get("name").and_then(|n| n.as_str()) == Some(func.name.as_str()))
        }) else {
            continue;
        };

        let fn_hash = content_hash(decl_value);
        out.nodes.push(DefNode {
            hash: fn_hash.clone(),
            kind: NodeKind::Function,
            name: Some(func.name.clone()),
            substrate: SubstrateKind::X07,
            file: Some(rel.to_string()),
            span: Some(format!("/decls/{idx}")),
        });

        out.effects.insert(fn_hash.clone(), effect_facts(decl_value));

        // calls edges (best-effort body scan) -> `x07fn:<name>` URNs; resolved
        // to concrete hashes at store load once all modules are indexed.
        for callee in scan_calls(decl_value) {
            out.edges.push(Edge {
                from: fn_hash.clone(),
                to: format!("x07fn:{callee}"),
                kind: EdgeKind::Calls,
                derivation: Derivation::Derived,
                confidence: 1.0,
            });
        }

        // reads/mutates edges with resource URNs (KGL's own value-add: no
        // substrate splits reads from writes). Inferred from recognized host-op
        // calls in the body; `Declared` edges from the metadata channel can
        // augment/override these later.
        for (kind, urn) in scan_io_ops(decl_value) {
            out.edges.push(Edge {
                from: fn_hash.clone(),
                to: urn,
                kind,
                derivation: Derivation::Inferred,
                confidence: 0.6,
            });
        }
    }
}

/// Best-effort effect detection from a declaration's JSON. Conservative: only
/// flags effects when the X07 AST makes them explicit (an `effects` array), so
/// we never fabricate a `mutates`/`reads` edge from nothing.
fn effect_facts(decl: &Value) -> EffectFacts {
    let mut facts = EffectFacts::default();
    if let Some(eff) = decl.get("effects").and_then(|e| e.as_array()) {
        for e in eff.iter().filter_map(|e| e.as_str()) {
            let e = e.to_ascii_lowercase();
            if e.contains("io") || e.contains("fs") || e.contains("net") {
                facts.touches_io = true;
            }
            if e.contains("mut") || e.contains("write") || e.contains("state") {
                facts.mutates_state = true;
            }
        }
    }
    facts
}

/// Best-effort scan of a declaration body for call references. Recurses the JSON
/// for call/application nodes and collects callee names. The exact X07 body
/// schema is not surfaced by the daimonos parse, so this matches common shapes
/// and yields nothing when the shape is unknown — safe by construction.
fn scan_calls(decl: &Value) -> Vec<String> {
    let mut out = Vec::new();
    collect_calls(decl, &mut out);
    out.sort();
    out.dedup();
    out
}

fn collect_calls(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            let kind = map.get("kind").and_then(|k| k.as_str()).unwrap_or("");
            if matches!(kind, "call" | "app" | "apply") {
                for key in ["callee", "fn", "ref", "target", "name"] {
                    if let Some(name) = call_target_name(map.get(key)) {
                        out.push(name);
                        break;
                    }
                }
            }
            for child in map.values() {
                collect_calls(child, out);
            }
        }
        Value::Array(arr) => {
            for child in arr {
                collect_calls(child, out);
            }
        }
        _ => {}
    }
}

fn call_target_name(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Object(m)) => m.get("name").and_then(|n| n.as_str()).map(str::to_string),
        _ => None,
    }
}

/// Best-effort scan for host I/O operations, classifying each into a `reads` or
/// `mutates` edge against a resource URN. The verb match is a documented v0
/// heuristic over callee names; the resource is the first string literal in the
/// call's args, else `io:unknown`. Yields nothing for unrecognized shapes.
fn scan_io_ops(decl: &Value) -> Vec<(EdgeKind, String)> {
    let mut out = Vec::new();
    collect_io_ops(decl, &mut out);
    out
}

fn collect_io_ops(v: &Value, out: &mut Vec<(EdgeKind, String)>) {
    match v {
        Value::Object(map) => {
            let kind = map.get("kind").and_then(|k| k.as_str()).unwrap_or("");
            if matches!(kind, "call" | "app" | "apply") {
                let callee = ["callee", "fn", "ref", "target", "name"]
                    .iter()
                    .find_map(|k| call_target_name(map.get(*k)));
                if let Some(name) = callee {
                    if let Some(ek) = classify_verb(&name) {
                        let urn = first_string_arg(map)
                            .map(resource_urn)
                            .unwrap_or_else(|| "io:unknown".to_string());
                        out.push((ek, urn));
                    }
                }
            }
            for child in map.values() {
                collect_io_ops(child, out);
            }
        }
        Value::Array(arr) => {
            for child in arr {
                collect_io_ops(child, out);
            }
        }
        _ => {}
    }
}

/// Classify a callee name as a read or a write. Contains-match over common
/// verbs; write wins ties. Deliberately coarse for v0 — precise effect typing
/// would come from a substrate that exposes a fine-grained effect set.
fn classify_verb(name: &str) -> Option<EdgeKind> {
    let n = name.to_ascii_lowercase();
    const WRITES: [&str; 8] = ["write", "save", "store", "put", "delete", "create", "send", "set"];
    const READS: [&str; 6] = ["read", "open", "load", "get", "fetch", "recv"];
    if WRITES.iter().any(|w| n.contains(w)) {
        return Some(EdgeKind::Mutates);
    }
    if READS.iter().any(|w| n.contains(w)) {
        return Some(EdgeKind::Reads);
    }
    None
}

fn first_string_arg(map: &serde_json::Map<String, Value>) -> Option<String> {
    for key in ["args", "arguments", "params"] {
        if let Some(a) = map.get(key) {
            if let Some(s) = find_first_string(a) {
                return Some(s);
            }
        }
    }
    None
}

fn find_first_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(arr) => arr.iter().find_map(find_first_string),
        Value::Object(m) => {
            if let Some(Value::String(s)) = m.get("value") {
                return Some(s.clone());
            }
            m.values().find_map(find_first_string)
        }
        _ => None,
    }
}

fn resource_urn(lit: String) -> String {
    if lit.contains('/') || lit.contains('.') {
        format!("file://{lit}")
    } else {
        format!("res:{lit}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_fixture(dir: &Path, name: &str, json: &str) {
        let mut f = std::fs::File::create(dir.join(name)).unwrap();
        f.write_all(json.as_bytes()).unwrap();
    }

    #[test]
    fn indexes_modules_functions_imports_calls_effects() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture(
            tmp.path(),
            "math.x07.json",
            r#"{
                "module_id": "math.core",
                "schema_version": "1.0",
                "kind": "library",
                "imports": ["std.io"],
                "decls": [
                    {"kind":"defn","name":"add",
                     "params":[{"name":"a","ty":"i64"},{"name":"b","ty":"i64"}],
                     "result":"i64",
                     "body":[{"kind":"call","callee":"checked_add"}]},
                    {"kind":"defn","name":"checked_add",
                     "params":[{"name":"a","ty":"i64"}],"result":"i64",
                     "effects":["IO"]},
                    {"kind":"defn","name":"load_settings",
                     "params":[],"result":"Settings","effects":["IO"],
                     "body":[
                        {"kind":"call","callee":"read_file","args":[{"kind":"str","value":"/etc/app/conf.json"}]},
                        {"kind":"call","callee":"write_log","args":["app.log"]}
                     ]}
                ]
            }"#,
        );

        let idx = X07Substrate::default().index(tmp.path()).unwrap();

        let modules: Vec<_> = idx.nodes.iter().filter(|n| n.kind == NodeKind::Module).collect();
        let funcs: Vec<_> = idx.nodes.iter().filter(|n| n.kind == NodeKind::Function).collect();
        assert_eq!(modules.len(), 1);
        assert_eq!(funcs.len(), 3);

        // reads/mutates with resource URNs (BUILD 3)
        let load = funcs.iter().find(|n| n.name.as_deref() == Some("load_settings")).unwrap();
        let io: Vec<_> = idx.edges.iter().filter(|e| e.from == load.hash).collect();
        assert!(io.iter().any(|e| e.kind == EdgeKind::Reads && e.to == "file:///etc/app/conf.json"));
        assert!(io.iter().any(|e| e.kind == EdgeKind::Mutates && e.to == "file://app.log"));
        assert_eq!(modules[0].name.as_deref(), Some("math.core"));

        // depends_on from import
        assert!(idx
            .edges
            .iter()
            .any(|e| e.kind == EdgeKind::DependsOn && e.to == "x07mod:std.io"));

        // calls edge add -> checked_add (best-effort body scan)
        assert!(idx
            .edges
            .iter()
            .any(|e| e.kind == EdgeKind::Calls && e.to == "x07fn:checked_add"));

        // effects: checked_add touches io, add does not
        let checked = funcs.iter().find(|n| n.name.as_deref() == Some("checked_add")).unwrap();
        let add = funcs.iter().find(|n| n.name.as_deref() == Some("add")).unwrap();
        assert!(idx.effects.get(&checked.hash).unwrap().touches_io);
        assert!(!idx.effects.get(&add.hash).unwrap().touches_io);

        // hashes are stable, distinct, sha256-hex
        assert_ne!(add.hash, checked.hash);
        assert_eq!(add.hash.len(), 64);
    }

    #[test]
    fn empty_workspace_yields_empty_graph() {
        let tmp = tempfile::tempdir().unwrap();
        let idx = X07Substrate::default().index(tmp.path()).unwrap();
        assert!(idx.nodes.is_empty());
        assert!(idx.edges.is_empty());
    }
}
