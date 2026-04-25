use crate::tool_runner::{QuickFix, ToolCommand, ToolDescriptor, ToolPlugin};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

/// X07-specific plugin with deep integration:
/// - Semantic indexing of JSON AST files
/// - Manifest caching
/// - Content-addressable declaration cache
/// - Custom quickfix extraction from X07 lint output
pub struct X07Plugin {
    descriptor: ToolDescriptor,
    #[allow(dead_code)] // used via tests + future wiring
    decl_cache: Arc<RwLock<DeclCache>>,
    symbol_index: Arc<RwLock<SymbolIndex>>,
}

/// Cache of lint/verify results keyed on declaration content hash.
#[allow(dead_code)]
struct DeclCache {
    entries: HashMap<String, CacheEntry>,
}

#[allow(dead_code)]
struct CacheEntry {
    lint_ok: bool,
    diagnostics: Option<serde_json::Value>,
}

/// Semantic index extracted from X07 JSON AST files.
#[derive(Default)]
struct SymbolIndex {
    modules: Vec<ModuleInfo>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ModuleInfo {
    pub module_id: String,
    pub file: String,
    pub schema_version: String,
    pub kind: String,
    pub functions: Vec<FunctionInfo>,
    pub imports: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct FunctionInfo {
    pub name: String,
    pub kind: String,
    pub params: Vec<ParamInfo>,
    pub result: String,
    pub has_requires: bool,
    pub has_ensures: bool,
    pub contract_count: usize,
}

#[derive(Debug, Serialize, Clone)]
pub struct ParamInfo {
    pub name: String,
    pub ty: String,
}

impl X07Plugin {
    pub fn new(x07_bin: &str) -> Self {
        let mut commands = HashMap::new();
        commands.insert(
            "build".into(),
            ToolCommand {
                bin: x07_bin.to_string(),
                args: vec!["build".into()],
                output: "json".into(),
            },
        );
        commands.insert(
            "run".into(),
            ToolCommand {
                bin: x07_bin.to_string(),
                args: vec!["run".into(), "--stdin".into()],
                output: "json".into(),
            },
        );
        commands.insert(
            "lint".into(),
            ToolCommand {
                bin: x07_bin.to_string(),
                args: vec!["lint".into()],
                output: "json".into(),
            },
        );
        commands.insert(
            "test".into(),
            ToolCommand {
                bin: x07_bin.to_string(),
                args: vec!["test".into()],
                output: "json".into(),
            },
        );
        commands.insert(
            "fmt".into(),
            ToolCommand {
                bin: x07_bin.to_string(),
                args: vec!["fmt".into()],
                output: "json".into(),
            },
        );
        commands.insert(
            "verify".into(),
            ToolCommand {
                bin: x07_bin.to_string(),
                args: vec!["verify".into()],
                output: "json".into(),
            },
        );

        let descriptor = ToolDescriptor {
            id: "x07".into(),
            commands,
            source_pattern: Some("**/*.x07.json".into()),
            manifest: Some("x07.json".into()),
            diagnostics_format: "json".into(),
            supports_quickfix: true,
            quickfix_format: Some("json_patch".into()),
        };

        Self {
            descriptor,
            decl_cache: Arc::new(RwLock::new(DeclCache {
                entries: HashMap::new(),
            })),
            symbol_index: Arc::new(RwLock::new(SymbolIndex::default())),
        }
    }

    /// Scan workspace for .x07.json files and build a semantic symbol index.
    #[allow(dead_code)] // called by x07 integration flows, not by core daemon
    pub async fn index_workspace(&self, root: &Path) -> Vec<ModuleInfo> {
        let root = root.to_path_buf();
        let root_clone = root.clone();

        let modules = tokio::task::spawn_blocking(move || {
            let mut modules = Vec::new();
            for entry in ignore::WalkBuilder::new(&root_clone)
                .hidden(true)
                .git_ignore(true)
                .build()
                .flatten()
            {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !name.ends_with(".x07.json") {
                    continue;
                }

                if let Ok(content) = std::fs::read_to_string(path) {
                    if let Ok(ast) = serde_json::from_str::<serde_json::Value>(&content) {
                        let rel = path
                            .strip_prefix(&root_clone)
                            .unwrap_or(path)
                            .to_string_lossy()
                            .to_string();
                        if let Some(module) = parse_x07_module(&ast, &rel) {
                            modules.push(module);
                        }
                    }
                }
            }
            modules
        })
        .await
        .unwrap_or_default();

        let mut idx = self.symbol_index.write().await;
        idx.modules = modules.clone();

        modules
    }

    /// Query the semantic index for symbols matching a query.
    #[allow(dead_code)] // called from tests; wired into MCP in a future release
    pub async fn search_symbols(&self, query: &str, max: usize) -> Vec<serde_json::Value> {
        let idx = self.symbol_index.read().await;
        let query_lower = query.to_lowercase();
        let mut results = Vec::new();

        for module in &idx.modules {
            if module.module_id.to_lowercase().contains(&query_lower) {
                results.push(serde_json::json!({
                    "type": "module",
                    "module": module.module_id,
                    "file": module.file,
                    "kind": module.kind,
                    "functions": module.functions.len(),
                    "imports": module.imports,
                }));
            }

            for func in &module.functions {
                if func.name.to_lowercase().contains(&query_lower) {
                    results.push(serde_json::json!({
                        "type": "function",
                        "module": module.module_id,
                        "name": func.name,
                        "kind": func.kind,
                        "file": module.file,
                        "params": func.params,
                        "result": func.result,
                        "contracts": func.contract_count,
                    }));
                }
            }

            if results.len() >= max {
                break;
            }
        }

        results.truncate(max);
        results
    }

    /// Check if a declaration's content hash is already cached with passing lint.
    #[allow(dead_code)]
    pub async fn is_cached_clean(&self, content_hash: &str) -> bool {
        let cache = self.decl_cache.read().await;
        cache
            .entries
            .get(content_hash)
            .map(|e| e.lint_ok)
            .unwrap_or(false)
    }

    /// Store lint result for a declaration hash.
    #[allow(dead_code)]
    pub async fn cache_lint_result(
        &self,
        content_hash: &str,
        lint_ok: bool,
        diagnostics: Option<serde_json::Value>,
    ) {
        let mut cache = self.decl_cache.write().await;
        cache.entries.insert(
            content_hash.to_string(),
            CacheEntry {
                lint_ok,
                diagnostics,
            },
        );
    }

    /// Compute content hash for a file.
    #[allow(dead_code)]
    pub fn content_hash(content: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content);
        hex::encode(hasher.finalize())
    }
}

#[async_trait::async_trait]
impl ToolPlugin for X07Plugin {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    fn extract_quickfixes(&self, lint_output: &serde_json::Value) -> Vec<QuickFix> {
        let mut fixes = Vec::new();

        // X07 lint output has diagnostics with quickfix fields containing JSON Patch ops
        let diagnostics = lint_output
            .get("diagnostics")
            .and_then(|d| d.as_array())
            .or_else(|| lint_output.get("errors").and_then(|d| d.as_array()));

        if let Some(diags) = diagnostics {
            for diag in diags {
                let file = diag
                    .get("file")
                    .or_else(|| diag.get("source"))
                    .and_then(|f| f.as_str());
                let quickfix = diag
                    .get("quickfix")
                    .or_else(|| diag.get("fix"))
                    .or_else(|| diag.get("patch"));

                if let (Some(file), Some(patch)) = (file, quickfix) {
                    if !patch.is_null() {
                        fixes.push(QuickFix::JsonPatch {
                            file: file.to_string(),
                            patch: patch.clone(),
                        });
                    }
                }
            }
        }

        fixes
    }
}

/// Parse an X07 JSON AST module into a ModuleInfo struct.
pub(crate) fn parse_x07_module(ast: &serde_json::Value, file: &str) -> Option<ModuleInfo> {
    let module_id = ast.get("module_id")?.as_str()?.to_string();
    let schema_version = ast
        .get("schema_version")
        .and_then(|s| s.as_str())
        .unwrap_or("unknown")
        .to_string();
    let kind = ast
        .get("kind")
        .and_then(|k| k.as_str())
        .unwrap_or("module")
        .to_string();

    let imports: Vec<String> = ast
        .get("imports")
        .and_then(|i| i.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let mut functions = Vec::new();
    if let Some(decls) = ast.get("decls").and_then(|d| d.as_array()) {
        for decl in decls {
            let decl_kind = decl.get("kind").and_then(|k| k.as_str()).unwrap_or("");
            if !matches!(decl_kind, "defn" | "defasync") {
                continue;
            }

            let name = decl
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();

            let params: Vec<ParamInfo> = decl
                .get("params")
                .and_then(|p| p.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|p| {
                            Some(ParamInfo {
                                name: p.get("name")?.as_str()?.to_string(),
                                ty: p.get("ty")?.as_str()?.to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();

            let result = decl
                .get("result")
                .and_then(|r| r.as_str())
                .unwrap_or("void")
                .to_string();

            let requires = decl.get("requires").and_then(|r| r.as_array());
            let ensures = decl.get("ensures").and_then(|e| e.as_array());
            let req_count = requires.map(|r| r.len()).unwrap_or(0);
            let ens_count = ensures.map(|e| e.len()).unwrap_or(0);

            functions.push(FunctionInfo {
                name,
                kind: decl_kind.to_string(),
                params,
                result,
                has_requires: req_count > 0,
                has_ensures: ens_count > 0,
                contract_count: req_count + ens_count,
            });
        }
    }

    Some(ModuleInfo {
        module_id,
        file: file.to_string(),
        schema_version,
        kind,
        functions,
        imports,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_runner::ToolPlugin;
    use serde_json::json;

    fn sample_ast() -> serde_json::Value {
        json!({
            "module_id": "math.core",
            "schema_version": "1.0",
            "kind": "library",
            "imports": ["std.io", "std.fmt"],
            "decls": [
                {
                    "kind": "defn",
                    "name": "add",
                    "params": [
                        {"name": "a", "ty": "i64"},
                        {"name": "b", "ty": "i64"}
                    ],
                    "result": "i64",
                    "requires": [{"expr": "a >= 0"}],
                    "ensures": [{"expr": "result == a + b"}]
                },
                {
                    "kind": "defasync",
                    "name": "fetch",
                    "params": [{"name": "url", "ty": "String"}],
                    "result": "Response"
                },
                {
                    "kind": "type",
                    "name": "Point"
                }
            ]
        })
    }

    #[test]
    fn parse_module_extracts_metadata() {
        let module = parse_x07_module(&sample_ast(), "src/math.x07.json").unwrap();
        assert_eq!(module.module_id, "math.core");
        assert_eq!(module.schema_version, "1.0");
        assert_eq!(module.kind, "library");
        assert_eq!(module.file, "src/math.x07.json");
        assert_eq!(module.imports, vec!["std.io", "std.fmt"]);
    }

    #[test]
    fn parse_module_extracts_functions_only() {
        let module = parse_x07_module(&sample_ast(), "test.x07.json").unwrap();
        assert_eq!(module.functions.len(), 2); // defn + defasync, not type
        assert_eq!(module.functions[0].name, "add");
        assert_eq!(module.functions[0].kind, "defn");
        assert_eq!(module.functions[1].name, "fetch");
        assert_eq!(module.functions[1].kind, "defasync");
    }

    #[test]
    fn parse_module_extracts_params() {
        let module = parse_x07_module(&sample_ast(), "t.x07.json").unwrap();
        let add = &module.functions[0];
        assert_eq!(add.params.len(), 2);
        assert_eq!(add.params[0].name, "a");
        assert_eq!(add.params[0].ty, "i64");
        assert_eq!(add.result, "i64");
    }

    #[test]
    fn parse_module_tracks_contracts() {
        let module = parse_x07_module(&sample_ast(), "t.x07.json").unwrap();
        let add = &module.functions[0];
        assert!(add.has_requires);
        assert!(add.has_ensures);
        assert_eq!(add.contract_count, 2);
        let fetch = &module.functions[1];
        assert!(!fetch.has_requires);
        assert!(!fetch.has_ensures);
        assert_eq!(fetch.contract_count, 0);
    }

    #[test]
    fn parse_module_returns_none_without_module_id() {
        let bad = json!({"kind": "library"});
        assert!(parse_x07_module(&bad, "test").is_none());
    }

    #[test]
    fn parse_module_defaults() {
        let minimal = json!({"module_id": "m"});
        let module = parse_x07_module(&minimal, "f").unwrap();
        assert_eq!(module.schema_version, "unknown");
        assert_eq!(module.kind, "module");
        assert!(module.imports.is_empty());
        assert!(module.functions.is_empty());
    }

    #[test]
    fn x07_plugin_descriptor() {
        let plugin = X07Plugin::new("/usr/bin/x07");
        let desc = plugin.descriptor();
        assert_eq!(desc.id, "x07");
        assert!(desc.commands.contains_key("build"));
        assert!(desc.commands.contains_key("lint"));
        assert!(desc.commands.contains_key("run"));
        assert!(desc.commands.contains_key("test"));
        assert!(desc.commands.contains_key("fmt"));
        assert!(desc.commands.contains_key("verify"));
        assert_eq!(desc.commands.len(), 6);
        assert!(desc.supports_quickfix);
        assert_eq!(desc.quickfix_format.as_deref(), Some("json_patch"));
    }

    #[test]
    fn extract_quickfixes_from_diagnostics() {
        let plugin = X07Plugin::new("/usr/bin/x07");
        let lint_output = json!({
            "diagnostics": [
                {"file": "src/main.x07.json", "quickfix": [{"op": "replace", "path": "/decls/0/name", "value": "fixed"}]},
                {"file": "src/lib.x07.json", "quickfix": null},
                {"source": "src/alt.x07.json", "fix": {"op": "add", "path": "/decls/-", "value": {}}}
            ]
        });
        let fixes = plugin.extract_quickfixes(&lint_output);
        assert_eq!(fixes.len(), 2); // null quickfix is skipped
    }

    #[test]
    fn extract_quickfixes_empty_when_no_diagnostics() {
        let plugin = X07Plugin::new("/usr/bin/x07");
        let fixes = plugin.extract_quickfixes(&json!({"status": "ok"}));
        assert!(fixes.is_empty());
    }

    #[tokio::test]
    async fn decl_cache_miss_then_hit() {
        let plugin = X07Plugin::new("/usr/bin/x07");
        let hash = "abc123";
        assert!(!plugin.is_cached_clean(hash).await);

        plugin.cache_lint_result(hash, true, None).await;
        assert!(plugin.is_cached_clean(hash).await);
    }

    #[tokio::test]
    async fn decl_cache_failing_lint() {
        let plugin = X07Plugin::new("/usr/bin/x07");
        plugin
            .cache_lint_result("h1", false, Some(json!({"err": "bad"})))
            .await;
        assert!(!plugin.is_cached_clean("h1").await);
    }

    #[test]
    fn content_hash_deterministic() {
        let h1 = X07Plugin::content_hash(b"hello");
        let h2 = X07Plugin::content_hash(b"hello");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    #[tokio::test]
    async fn search_symbols_by_module() {
        let plugin = X07Plugin::new("/usr/bin/x07");
        {
            let mut idx = plugin.symbol_index.write().await;
            idx.modules
                .push(parse_x07_module(&sample_ast(), "math.x07.json").unwrap());
        }

        let results = plugin.search_symbols("math", 10).await;
        assert!(!results.is_empty());
        assert_eq!(results[0]["type"], "module");
        assert_eq!(results[0]["module"], "math.core");
    }

    #[tokio::test]
    async fn search_symbols_by_function() {
        let plugin = X07Plugin::new("/usr/bin/x07");
        {
            let mut idx = plugin.symbol_index.write().await;
            idx.modules
                .push(parse_x07_module(&sample_ast(), "math.x07.json").unwrap());
        }

        let results = plugin.search_symbols("add", 10).await;
        assert!(!results.is_empty());
        let func = results.iter().find(|r| r["type"] == "function").unwrap();
        assert_eq!(func["name"], "add");
        assert_eq!(func["module"], "math.core");
    }

    #[tokio::test]
    async fn search_symbols_respects_max() {
        let plugin = X07Plugin::new("/usr/bin/x07");
        {
            let mut idx = plugin.symbol_index.write().await;
            idx.modules
                .push(parse_x07_module(&sample_ast(), "m.x07.json").unwrap());
        }
        let results = plugin.search_symbols("", 1).await;
        assert_eq!(results.len(), 1);
    }
}
