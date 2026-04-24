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
    decl_cache: Arc<RwLock<DeclCache>>,
    symbol_index: Arc<RwLock<SymbolIndex>>,
}

/// Cache of lint/verify results keyed on declaration content hash.
struct DeclCache {
    entries: HashMap<String, CacheEntry>,
}

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
        commands.insert("build".into(), ToolCommand {
            bin: x07_bin.to_string(),
            args: vec!["build".into()],
            output: "json".into(),
        });
        commands.insert("run".into(), ToolCommand {
            bin: x07_bin.to_string(),
            args: vec!["run".into(), "--stdin".into()],
            output: "json".into(),
        });
        commands.insert("lint".into(), ToolCommand {
            bin: x07_bin.to_string(),
            args: vec!["lint".into()],
            output: "json".into(),
        });
        commands.insert("test".into(), ToolCommand {
            bin: x07_bin.to_string(),
            args: vec!["test".into()],
            output: "json".into(),
        });
        commands.insert("fmt".into(), ToolCommand {
            bin: x07_bin.to_string(),
            args: vec!["fmt".into()],
            output: "json".into(),
        });
        commands.insert("verify".into(), ToolCommand {
            bin: x07_bin.to_string(),
            args: vec!["verify".into()],
            output: "json".into(),
        });

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
    pub async fn is_cached_clean(&self, content_hash: &str) -> bool {
        let cache = self.decl_cache.read().await;
        cache
            .entries
            .get(content_hash)
            .map(|e| e.lint_ok)
            .unwrap_or(false)
    }

    /// Store lint result for a declaration hash.
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
fn parse_x07_module(ast: &serde_json::Value, file: &str) -> Option<ModuleInfo> {
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
