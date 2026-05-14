use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::RwLock;

/// Descriptor for a single command a tool exposes (build, lint, run, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCommand {
    pub bin: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_output_format")]
    pub output: String,
}

fn default_output_format() -> String {
    "json".to_string()
}

/// Full descriptor for a registered tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub id: String,
    pub commands: HashMap<String, ToolCommand>,
    #[serde(default)]
    pub source_pattern: Option<String>,
    #[serde(default)]
    pub manifest: Option<String>,
    #[serde(default = "default_diag_format")]
    pub diagnostics_format: String,
    #[serde(default)]
    pub supports_quickfix: bool,
    #[serde(default)]
    pub quickfix_format: Option<String>,
}

fn default_diag_format() -> String {
    "json".to_string()
}

/// Result from running a tool command.
#[derive(Debug, Serialize)]
pub struct ToolResult {
    pub tool: String,
    pub command: String,
    pub exit_code: i32,
    pub output: serde_json::Value,
    pub stderr: String,
}

/// Result from a repair loop iteration.
#[derive(Debug, Serialize)]
pub struct RepairIteration {
    pub iteration: u32,
    pub diagnostics: serde_json::Value,
    pub fixes_applied: u32,
    pub clean: bool,
}

/// Full result from the repair loop engine.
#[derive(Debug, Serialize)]
pub struct RepairResult {
    pub tool: String,
    pub iterations: Vec<RepairIteration>,
    pub total_iterations: u32,
    pub total_fixes: u32,
    pub final_clean: bool,
}

/// Result from a pipeline run.
#[derive(Debug, Serialize)]
pub struct PipelineResult {
    pub tool: String,
    pub stages: Vec<ToolResult>,
    pub all_ok: bool,
    pub short_circuited_at: Option<String>,
}

/// Trait for language-specific plugins.
/// The generic CLI plugin implements this with no special logic.
/// Language-specific plugins (X07) override methods for deeper optimization.
#[async_trait::async_trait]
pub trait ToolPlugin: Send + Sync {
    fn descriptor(&self) -> &ToolDescriptor;

    /// Run a single command. Plugins can override for custom execution.
    /// `args` carries tool-specific parameters (e.g. limit, mode, path filter).
    async fn run_command(
        &self,
        command: &str,
        cwd: &Path,
        env: &HashMap<String, String>,
        stdin_data: Option<&[u8]>,
        args: Option<&serde_json::Value>,
    ) -> Result<ToolResult, String> {
        let _ = args;
        let desc = self.descriptor();
        let cmd_desc = desc
            .commands
            .get(command)
            .ok_or_else(|| format!("unknown command '{}' for tool '{}'", command, desc.id))?;

        let mut proc = Command::new(&cmd_desc.bin);
        proc.args(&cmd_desc.args)
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if stdin_data.is_some() {
            proc.stdin(Stdio::piped());
        }

        for (k, v) in env {
            proc.env(k, v);
        }

        let mut child = proc.spawn().map_err(|e| format!("spawn: {e}"))?;

        if let Some(data) = stdin_data {
            use tokio::io::AsyncWriteExt;
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(data).await;
                drop(stdin);
            }
        }

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| format!("wait: {e}"))?;
        let exit_code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let parsed = if cmd_desc.output == "json" {
            serde_json::from_str(&stdout)
                .unwrap_or_else(|_| serde_json::json!({"raw": stdout.trim_end()}))
        } else {
            serde_json::json!({"raw": stdout.trim_end()})
        };

        Ok(ToolResult {
            tool: desc.id.clone(),
            command: command.to_string(),
            exit_code,
            output: parsed,
            stderr: stderr.trim_end().to_string(),
        })
    }

    /// Extract quickfixes from lint output. Plugins override for language-specific formats.
    fn extract_quickfixes(&self, lint_output: &serde_json::Value) -> Vec<QuickFix> {
        if !self.descriptor().supports_quickfix {
            return Vec::new();
        }

        match self.descriptor().quickfix_format.as_deref() {
            Some("json_patch") => extract_json_patch_fixes(lint_output),
            Some("string_replace") => extract_string_replace_fixes(lint_output),
            _ => Vec::new(),
        }
    }

    /// Apply a quickfix. Plugins can override for custom application logic.
    async fn apply_quickfix(&self, fix: &QuickFix, cwd: &Path) -> Result<bool, String> {
        match fix {
            QuickFix::JsonPatch { file, patch } => {
                let path = cwd.join(file);
                let content = tokio::fs::read_to_string(&path)
                    .await
                    .map_err(|e| format!("read: {e}"))?;
                let mut doc: serde_json::Value =
                    serde_json::from_str(&content).map_err(|e| format!("parse: {e}"))?;
                let patch_obj: json_patch::Patch = serde_json::from_value(patch.clone())
                    .map_err(|e| format!("patch parse: {e}"))?;
                json_patch::patch(&mut doc, &patch_obj).map_err(|e| format!("patch apply: {e}"))?;
                let out =
                    serde_json::to_string_pretty(&doc).map_err(|e| format!("serialize: {e}"))?;
                tokio::fs::write(&path, out)
                    .await
                    .map_err(|e| format!("write: {e}"))?;
                Ok(true)
            }
            QuickFix::StringReplace { file, old, new } => {
                let path = cwd.join(file);
                let content = tokio::fs::read_to_string(&path)
                    .await
                    .map_err(|e| format!("read: {e}"))?;
                if content.contains(old.as_str()) {
                    let updated = content.replacen(old, new, 1);
                    tokio::fs::write(&path, updated)
                        .await
                        .map_err(|e| format!("write: {e}"))?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
        }
    }
}

/// Quickfix types the repair loop can apply.
#[derive(Debug, Clone)]
pub enum QuickFix {
    JsonPatch {
        file: String,
        patch: serde_json::Value,
    },
    StringReplace {
        file: String,
        old: String,
        new: String,
    },
}

/// Registry of all registered tool plugins.
pub struct ToolRegistry {
    plugins: Arc<RwLock<HashMap<String, Arc<dyn ToolPlugin>>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            plugins: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn register(&self, plugin: Arc<dyn ToolPlugin>) {
        let id = plugin.descriptor().id.clone();
        self.plugins.write().await.insert(id, plugin);
    }

    pub async fn get(&self, id: &str) -> Option<Arc<dyn ToolPlugin>> {
        self.plugins.read().await.get(id).cloned()
    }

    pub async fn list(&self) -> Vec<ToolDescriptor> {
        self.plugins
            .read()
            .await
            .values()
            .map(|p| p.descriptor().clone())
            .collect()
    }

    /// Run a command on a registered tool.
    pub async fn run(
        &self,
        tool_id: &str,
        command: &str,
        cwd: &Path,
        env: &HashMap<String, String>,
        stdin_data: Option<&[u8]>,
        args: Option<&serde_json::Value>,
    ) -> Result<ToolResult, String> {
        let plugin = self
            .get(tool_id)
            .await
            .ok_or_else(|| format!("unknown tool: {tool_id}"))?;
        plugin
            .run_command(command, cwd, env, stdin_data, args)
            .await
    }

    /// Run a repair loop: lint -> apply fixes -> re-lint, up to max_iterations.
    pub async fn repair(
        &self,
        tool_id: &str,
        cwd: &Path,
        env: &HashMap<String, String>,
        max_iterations: u32,
    ) -> Result<RepairResult, String> {
        let plugin = self
            .get(tool_id)
            .await
            .ok_or_else(|| format!("unknown tool: {tool_id}"))?;

        if !plugin.descriptor().commands.contains_key("lint") {
            return Err(format!("tool '{}' has no 'lint' command", tool_id));
        }

        let mut iterations = Vec::new();
        let mut total_fixes: u32 = 0;

        for i in 0..max_iterations {
            let lint_result = plugin.run_command("lint", cwd, env, None, None).await?;
            let fixes = plugin.extract_quickfixes(&lint_result.output);

            if fixes.is_empty() || lint_result.exit_code == 0 {
                iterations.push(RepairIteration {
                    iteration: i + 1,
                    diagnostics: lint_result.output,
                    fixes_applied: 0,
                    clean: lint_result.exit_code == 0,
                });
                break;
            }

            let mut applied = 0u32;
            for fix in &fixes {
                match plugin.apply_quickfix(fix, cwd).await {
                    Ok(true) => applied += 1,
                    Ok(false) => {}
                    Err(e) => {
                        eprintln!("quickfix error: {e}");
                    }
                }
            }

            total_fixes += applied;
            iterations.push(RepairIteration {
                iteration: i + 1,
                diagnostics: lint_result.output,
                fixes_applied: applied,
                clean: false,
            });

            if applied == 0 {
                break; // no progress, stop
            }
        }

        let final_clean = iterations.last().map(|i| i.clean).unwrap_or(false);

        Ok(RepairResult {
            tool: tool_id.to_string(),
            total_iterations: iterations.len() as u32,
            iterations,
            total_fixes,
            final_clean,
        })
    }

    /// Run a pipeline of commands, short-circuiting on failure.
    pub async fn pipeline(
        &self,
        tool_id: &str,
        stages: &[String],
        cwd: &Path,
        env: &HashMap<String, String>,
    ) -> Result<PipelineResult, String> {
        let plugin = self
            .get(tool_id)
            .await
            .ok_or_else(|| format!("unknown tool: {tool_id}"))?;

        let mut results = Vec::new();
        let mut short_circuited_at = None;

        for stage in stages {
            let stage_name = stage.as_str();

            let result = if stage_name == "lint_fix" {
                let repair = self.repair(tool_id, cwd, env, 3).await?;
                ToolResult {
                    tool: tool_id.to_string(),
                    command: "lint_fix".to_string(),
                    exit_code: if repair.final_clean { 0 } else { 1 },
                    output: serde_json::to_value(&repair).unwrap(),
                    stderr: String::new(),
                }
            } else {
                plugin.run_command(stage_name, cwd, env, None, None).await?
            };

            let failed = result.exit_code != 0;
            results.push(result);

            if failed {
                short_circuited_at = Some(stage_name.to_string());
                break;
            }
        }

        let all_ok = short_circuited_at.is_none();

        Ok(PipelineResult {
            tool: tool_id.to_string(),
            stages: results,
            all_ok,
            short_circuited_at,
        })
    }
}

fn extract_json_patch_fixes(output: &serde_json::Value) -> Vec<QuickFix> {
    let mut fixes = Vec::new();
    if let Some(diagnostics) = output.get("diagnostics").and_then(|d| d.as_array()) {
        for diag in diagnostics {
            if let (Some(file), Some(patch)) = (
                diag.get("file").and_then(|f| f.as_str()),
                diag.get("quickfix"),
            ) {
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

fn extract_string_replace_fixes(output: &serde_json::Value) -> Vec<QuickFix> {
    let mut fixes = Vec::new();
    if let Some(diagnostics) = output.get("diagnostics").and_then(|d| d.as_array()) {
        for diag in diagnostics {
            if let (Some(file), Some(old), Some(new)) = (
                diag.get("file").and_then(|f| f.as_str()),
                diag.get("old").and_then(|o| o.as_str()),
                diag.get("new").and_then(|n| n.as_str()),
            ) {
                fixes.push(QuickFix::StringReplace {
                    file: file.to_string(),
                    old: old.to_string(),
                    new: new.to_string(),
                });
            }
        }
    }
    fixes
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn echo_descriptor() -> ToolDescriptor {
        let mut commands = HashMap::new();
        commands.insert(
            "run".into(),
            ToolCommand {
                bin: "echo".into(),
                args: vec!["hello".into()],
                output: "text".into(),
            },
        );
        commands.insert(
            "json_run".into(),
            ToolCommand {
                bin: "echo".into(),
                args: vec![r#"{"status":"ok"}"#.into()],
                output: "json".into(),
            },
        );
        ToolDescriptor {
            id: "echo_tool".into(),
            commands,
            source_pattern: None,
            manifest: None,
            diagnostics_format: "json".into(),
            supports_quickfix: false,
            quickfix_format: None,
        }
    }

    // --- ToolDescriptor serde tests ---

    #[test]
    fn descriptor_serialization_roundtrip() {
        let desc = echo_descriptor();
        let json = serde_json::to_string(&desc).unwrap();
        let parsed: ToolDescriptor = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "echo_tool");
        assert_eq!(parsed.commands.len(), 2);
    }

    #[test]
    fn descriptor_deserialize_defaults() {
        let json = r#"{"id": "minimal", "commands": {}}"#;
        let desc: ToolDescriptor = serde_json::from_str(json).unwrap();
        assert_eq!(desc.id, "minimal");
        assert_eq!(desc.diagnostics_format, "json");
        assert!(!desc.supports_quickfix);
        assert!(desc.source_pattern.is_none());
    }

    // --- QuickFix extraction tests ---

    #[test]
    fn extract_json_patch_fixes_valid() {
        let output = json!({
            "diagnostics": [
                {"file": "a.json", "quickfix": [{"op": "replace", "path": "/x", "value": 1}]},
                {"file": "b.json", "quickfix": null},
                {"file": "c.json"}
            ]
        });
        let fixes = extract_json_patch_fixes(&output);
        assert_eq!(fixes.len(), 1);
        match &fixes[0] {
            QuickFix::JsonPatch { file, .. } => assert_eq!(file, "a.json"),
            _ => panic!("expected JsonPatch"),
        }
    }

    #[test]
    fn extract_json_patch_fixes_empty() {
        let fixes = extract_json_patch_fixes(&json!({}));
        assert!(fixes.is_empty());
    }

    #[test]
    fn extract_string_replace_fixes_valid() {
        let output = json!({
            "diagnostics": [
                {"file": "main.rs", "old": "foo", "new": "bar"},
                {"file": "lib.rs"},
                {"file": "other.rs", "old": "x", "new": "y"}
            ]
        });
        let fixes = extract_string_replace_fixes(&output);
        assert_eq!(fixes.len(), 2);
        match &fixes[0] {
            QuickFix::StringReplace { file, old, new } => {
                assert_eq!(file, "main.rs");
                assert_eq!(old, "foo");
                assert_eq!(new, "bar");
            }
            _ => panic!("expected StringReplace"),
        }
    }

    #[test]
    fn extract_string_replace_fixes_empty() {
        let fixes = extract_string_replace_fixes(&json!({"nothing": true}));
        assert!(fixes.is_empty());
    }

    // --- ToolRegistry tests ---

    #[tokio::test]
    async fn registry_register_and_list() {
        let registry = ToolRegistry::new();
        assert!(registry.list().await.is_empty());

        let plugin = Arc::new(crate::plugins::generic_cli::GenericCliPlugin::new(
            echo_descriptor(),
        ));
        registry.register(plugin).await;

        let tools = registry.list().await;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].id, "echo_tool");
    }

    #[tokio::test]
    async fn registry_get_existing() {
        let registry = ToolRegistry::new();
        let plugin = Arc::new(crate::plugins::generic_cli::GenericCliPlugin::new(
            echo_descriptor(),
        ));
        registry.register(plugin).await;

        assert!(registry.get("echo_tool").await.is_some());
        assert!(registry.get("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn registry_run_unknown_tool() {
        let registry = ToolRegistry::new();
        let result = registry
            .run(
                "nope",
                "build",
                Path::new("/tmp"),
                &HashMap::new(),
                None,
                None,
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown tool"));
    }

    #[tokio::test]
    async fn registry_run_unknown_command() {
        let registry = ToolRegistry::new();
        let plugin = Arc::new(crate::plugins::generic_cli::GenericCliPlugin::new(
            echo_descriptor(),
        ));
        registry.register(plugin).await;

        let result = registry
            .run(
                "echo_tool",
                "nonexistent",
                Path::new("/tmp"),
                &HashMap::new(),
                None,
                None,
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown command"));
    }

    #[tokio::test]
    async fn registry_run_echo_text() {
        let registry = ToolRegistry::new();
        let plugin = Arc::new(crate::plugins::generic_cli::GenericCliPlugin::new(
            echo_descriptor(),
        ));
        registry.register(plugin).await;

        let result = registry
            .run(
                "echo_tool",
                "run",
                Path::new("/tmp"),
                &HashMap::new(),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.tool, "echo_tool");
        assert_eq!(result.command, "run");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.output["raw"].as_str().unwrap().trim(), "hello");
    }

    #[tokio::test]
    async fn registry_run_echo_json() {
        let registry = ToolRegistry::new();
        let plugin = Arc::new(crate::plugins::generic_cli::GenericCliPlugin::new(
            echo_descriptor(),
        ));
        registry.register(plugin).await;

        let result = registry
            .run(
                "echo_tool",
                "json_run",
                Path::new("/tmp"),
                &HashMap::new(),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.output["status"], "ok");
    }

    /// Mock plugin for driving the repair loop deterministically:
    /// each lint call returns N diagnostics from `fixes_per_iteration`,
    /// and `apply_quickfix` always succeeds without touching the filesystem.
    struct MockRepairPlugin {
        descriptor: ToolDescriptor,
        lint_call_count: std::sync::atomic::AtomicUsize,
        fixes_per_iteration: Vec<u32>,
    }

    #[async_trait::async_trait]
    impl ToolPlugin for MockRepairPlugin {
        fn descriptor(&self) -> &ToolDescriptor {
            &self.descriptor
        }

        async fn run_command(
            &self,
            command: &str,
            _cwd: &Path,
            _env: &HashMap<String, String>,
            _stdin_data: Option<&[u8]>,
            _args: Option<&serde_json::Value>,
        ) -> Result<ToolResult, String> {
            if command != "lint" {
                return Err(format!("mock: unknown command {command}"));
            }
            let i = self
                .lint_call_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let n = self.fixes_per_iteration.get(i).copied().unwrap_or(0);
            let diagnostics: Vec<serde_json::Value> = (0..n)
                .map(|j| json!({"file": format!("f{j}.txt"), "old": "x", "new": "y"}))
                .collect();
            Ok(ToolResult {
                tool: self.descriptor.id.clone(),
                command: command.to_string(),
                exit_code: if n == 0 { 0 } else { 1 },
                output: json!({"diagnostics": diagnostics}),
                stderr: String::new(),
            })
        }

        async fn apply_quickfix(&self, _fix: &QuickFix, _cwd: &Path) -> Result<bool, String> {
            Ok(true)
        }
    }

    fn mock_repair_descriptor() -> ToolDescriptor {
        let mut commands = HashMap::new();
        commands.insert(
            "lint".into(),
            ToolCommand {
                bin: "true".into(),
                args: vec![],
                output: "json".into(),
            },
        );
        ToolDescriptor {
            id: "mock_repair".into(),
            commands,
            source_pattern: None,
            manifest: None,
            diagnostics_format: "json".into(),
            supports_quickfix: true,
            quickfix_format: Some("string_replace".into()),
        }
    }

    /// Regression test for fix #1 (vikunja #243): RepairResult.total_iterations
    /// must reflect the number of loop iterations actually run, NOT the total
    /// number of fixes applied. The two were aliased due to a copy-paste bug.
    #[tokio::test]
    async fn repair_total_iterations_distinct_from_total_fixes() {
        let registry = ToolRegistry::new();
        // Sequence: iter 1 applies 3 fixes, iter 2 applies 1 fix, iter 3 sees
        // an empty diagnostic list and breaks. iterations.len() == 3, total_fixes == 4.
        let plugin = Arc::new(MockRepairPlugin {
            descriptor: mock_repair_descriptor(),
            lint_call_count: std::sync::atomic::AtomicUsize::new(0),
            fixes_per_iteration: vec![3, 1, 0],
        });
        registry.register(plugin).await;

        let result = registry
            .repair("mock_repair", Path::new("/tmp"), &HashMap::new(), 10)
            .await
            .unwrap();

        assert_eq!(
            result.iterations.len(),
            3,
            "loop should record one entry per iteration (incl. terminating clean iter)"
        );
        assert_eq!(result.total_fixes, 4, "expected 3 + 1 + 0 fixes applied");
        assert_eq!(
            result.total_iterations, 3,
            "total_iterations must equal iterations.len(), not total_fixes"
        );
        assert_ne!(
            result.total_iterations, result.total_fixes,
            "total_iterations and total_fixes must report different counts here"
        );
    }

    #[tokio::test]
    async fn registry_repair_no_lint_command() {
        let registry = ToolRegistry::new();
        let mut commands = HashMap::new();
        commands.insert(
            "build".into(),
            ToolCommand {
                bin: "echo".into(),
                args: vec![],
                output: "json".into(),
            },
        );
        let desc = ToolDescriptor {
            id: "no_lint".into(),
            commands,
            source_pattern: None,
            manifest: None,
            diagnostics_format: "json".into(),
            supports_quickfix: false,
            quickfix_format: None,
        };
        let plugin = Arc::new(crate::plugins::generic_cli::GenericCliPlugin::new(desc));
        registry.register(plugin).await;

        let result = registry
            .repair("no_lint", Path::new("/tmp"), &HashMap::new(), 3)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no 'lint' command"));
    }

    #[tokio::test]
    async fn pipeline_unknown_tool() {
        let registry = ToolRegistry::new();
        let result = registry
            .pipeline(
                "nope",
                &["build".into()],
                Path::new("/tmp"),
                &HashMap::new(),
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn pipeline_short_circuits_on_failure() {
        let registry = ToolRegistry::new();
        let mut commands = HashMap::new();
        commands.insert(
            "ok_stage".into(),
            ToolCommand {
                bin: "true".into(),
                args: vec![],
                output: "text".into(),
            },
        );
        commands.insert(
            "fail_stage".into(),
            ToolCommand {
                bin: "false".into(),
                args: vec![],
                output: "text".into(),
            },
        );
        commands.insert(
            "after_fail".into(),
            ToolCommand {
                bin: "echo".into(),
                args: vec!["should not run".into()],
                output: "text".into(),
            },
        );
        let desc = ToolDescriptor {
            id: "multi".into(),
            commands,
            source_pattern: None,
            manifest: None,
            diagnostics_format: "text".into(),
            supports_quickfix: false,
            quickfix_format: None,
        };
        let plugin = Arc::new(crate::plugins::generic_cli::GenericCliPlugin::new(desc));
        registry.register(plugin).await;

        let result = registry
            .pipeline(
                "multi",
                &["ok_stage".into(), "fail_stage".into(), "after_fail".into()],
                Path::new("/tmp"),
                &HashMap::new(),
            )
            .await
            .unwrap();
        assert!(!result.all_ok);
        assert_eq!(result.short_circuited_at, Some("fail_stage".into()));
        assert_eq!(result.stages.len(), 2); // after_fail never ran
    }

    #[tokio::test]
    async fn pipeline_all_ok() {
        let registry = ToolRegistry::new();
        let mut commands = HashMap::new();
        commands.insert(
            "s1".into(),
            ToolCommand {
                bin: "true".into(),
                args: vec![],
                output: "text".into(),
            },
        );
        commands.insert(
            "s2".into(),
            ToolCommand {
                bin: "true".into(),
                args: vec![],
                output: "text".into(),
            },
        );
        let desc = ToolDescriptor {
            id: "good".into(),
            commands,
            source_pattern: None,
            manifest: None,
            diagnostics_format: "text".into(),
            supports_quickfix: false,
            quickfix_format: None,
        };
        let plugin = Arc::new(crate::plugins::generic_cli::GenericCliPlugin::new(desc));
        registry.register(plugin).await;

        let result = registry
            .pipeline(
                "good",
                &["s1".into(), "s2".into()],
                Path::new("/tmp"),
                &HashMap::new(),
            )
            .await
            .unwrap();
        assert!(result.all_ok);
        assert!(result.short_circuited_at.is_none());
        assert_eq!(result.stages.len(), 2);
    }
}
