use std::collections::HashMap;
use std::path::Path;

use crate::tool_runner::{ToolCommand, ToolDescriptor, ToolPlugin, ToolResult};
use serde_json::{json, Value};

pub fn is_available() -> bool {
    std::process::Command::new("shellcheck")
        .arg("--version")
        // Never inherit our stdin; see build_command in ops/exec_ops.rs.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub struct ShellcheckPlugin {
    descriptor: ToolDescriptor,
}

impl ShellcheckPlugin {
    pub fn new() -> Self {
        let mut commands = HashMap::new();
        commands.insert(
            "check".to_string(),
            ToolCommand {
                bin: "shellcheck".into(),
                args: vec![],
                output: "structured".into(),
            },
        );
        Self {
            descriptor: ToolDescriptor {
                id: "shellcheck".into(),
                commands,
                source_pattern: None,
                manifest: None,
                diagnostics_format: "none".into(),
                supports_quickfix: false,
                quickfix_format: None,
            },
        }
    }
}

#[async_trait::async_trait]
impl ToolPlugin for ShellcheckPlugin {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    async fn run_command_with_config(
        &self,
        command: &str,
        cwd: &Path,
        env: &HashMap<String, String>,
        _stdin_data: Option<&[u8]>,
        args: Option<&Value>,
        process_cfg: &crate::config::ProcessConfig,
    ) -> Result<ToolResult, String> {
        match command {
            "check" => {
                let output = shellcheck_check(cwd, env, process_cfg, args).await?;
                Ok(ToolResult {
                    tool: "shellcheck".into(),
                    command: "check".into(),
                    exit_code: 0,
                    output,
                    stderr: String::new(),
                })
            }
            _ => Err(format!("unknown shellcheck command: {command}")),
        }
    }
}

async fn shellcheck_check(
    cwd: &Path,
    env: &HashMap<String, String>,
    process_cfg: &crate::config::ProcessConfig,
    args: Option<&Value>,
) -> Result<Value, String> {
    let args = args
        .and_then(|v| v.as_object())
        .ok_or("shellcheck: args must be a JSON object")?;

    // Accept either "file" (single path) or "files" (array)
    let files: Vec<String> = if let Some(f) = args.get("file").and_then(|v| v.as_str()) {
        vec![f.to_string()]
    } else if let Some(arr) = args.get("files").and_then(|v| v.as_array()) {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    } else {
        return Err("shellcheck: 'file' or 'files' is required".into());
    };

    if files.is_empty() {
        return Err("shellcheck: no files provided".into());
    }

    let shell = args.get("shell").and_then(|v| v.as_str()).unwrap_or("bash");

    let mut cmd_args: Vec<String> = vec!["--format=json".into(), format!("--shell={shell}")];
    cmd_args.extend(files.iter().cloned());

    let output = crate::managed_process::run("shellcheck", &cmd_args, cwd, env, process_cfg, None)
        .await
        .map_err(|e| format!("shellcheck exec: {e}"))?;

    let stdout = output.stdout;
    let stderr = output.stderr.trim().to_string();

    // shellcheck exits 0 = no issues, 1 = issues found, 2+ = fatal error
    let exit_code = output.status.code().unwrap_or(-1);
    if output.stdout_truncated || output.stderr_truncated {
        return Ok(json!({
            "error": "shellcheck output exceeded process.output_memory_bytes",
            "exit": exit_code,
        }));
    }

    if exit_code >= 2 || (!output.status.success() && stdout.trim().is_empty()) {
        return Ok(json!({
            "error": if stderr.is_empty() { "shellcheck failed" } else { &stderr },
            "exit": exit_code,
        }));
    }

    // Parse JSON diagnostics array from stdout
    let diagnostics: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|_| json!([]));

    let clean = diagnostics.as_array().map(|a| a.is_empty()).unwrap_or(true);

    Ok(json!({
        "clean": clean,
        "diagnostics": diagnostics,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shellcheck_is_available() {
        assert!(
            is_available(),
            "shellcheck should be on PATH in this environment"
        );
    }

    #[tokio::test]
    async fn clean_script_returns_clean_true() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("ok.sh");
        std::fs::write(&script, "#!/bin/bash\necho hello\n").unwrap();

        let plugin = ShellcheckPlugin::new();
        let args = json!({"file": script.to_str().unwrap()});
        let result = plugin
            .run_command("check", dir.path(), &HashMap::new(), None, Some(&args))
            .await
            .unwrap();

        assert_eq!(result.output["clean"], true, "got: {}", result.output);
        assert_eq!(
            result.output["diagnostics"],
            json!([]),
            "got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn script_with_issues_returns_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("bad.sh");
        // SC2086: Double quote to prevent globbing and word splitting
        std::fs::write(&script, "#!/bin/bash\nfoo=$1\necho $foo\n").unwrap();

        let plugin = ShellcheckPlugin::new();
        let args = json!({"file": script.to_str().unwrap()});
        let result = plugin
            .run_command("check", dir.path(), &HashMap::new(), None, Some(&args))
            .await
            .unwrap();

        assert_eq!(result.output["clean"], false, "got: {}", result.output);
        let diags = result.output["diagnostics"].as_array().unwrap();
        assert!(!diags.is_empty(), "expected diagnostics; got empty array");
        // Each diagnostic should have the standard shellcheck JSON fields
        let first = &diags[0];
        assert!(first.get("code").is_some(), "missing 'code' field");
        assert!(first.get("message").is_some(), "missing 'message' field");
        assert!(first.get("level").is_some(), "missing 'level' field");
        assert!(first.get("line").is_some(), "missing 'line' field");
    }

    #[tokio::test]
    async fn multiple_files_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let s1 = dir.path().join("a.sh");
        let s2 = dir.path().join("b.sh");
        std::fs::write(&s1, "#!/bin/bash\necho hi\n").unwrap();
        std::fs::write(&s2, "#!/bin/bash\necho bye\n").unwrap();

        let plugin = ShellcheckPlugin::new();
        let args = json!({"files": [s1.to_str().unwrap(), s2.to_str().unwrap()]});
        let result = plugin
            .run_command("check", dir.path(), &HashMap::new(), None, Some(&args))
            .await
            .unwrap();

        assert_eq!(result.output["clean"], true, "got: {}", result.output);
    }

    #[tokio::test]
    async fn missing_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();

        let plugin = ShellcheckPlugin::new();
        let args = json!({"file": "/definitely/nonexistent/script.sh"});
        let result = plugin
            .run_command("check", dir.path(), &HashMap::new(), None, Some(&args))
            .await
            .unwrap();

        assert!(
            result.output.get("error").is_some(),
            "missing file should produce an error field; got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn shell_override_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("ok.sh");
        std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();

        let plugin = ShellcheckPlugin::new();
        let args = json!({"file": script.to_str().unwrap(), "shell": "sh"});
        let result = plugin
            .run_command("check", dir.path(), &HashMap::new(), None, Some(&args))
            .await
            .unwrap();

        assert_eq!(result.output["clean"], true, "got: {}", result.output);
    }

    #[tokio::test]
    async fn unknown_command_errors() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = ShellcheckPlugin::new();
        let result = plugin
            .run_command("lint", dir.path(), &HashMap::new(), None, None)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown shellcheck command"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn truncated_json_is_never_reported_clean() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("shellcheck");
        std::fs::write(
            &fake,
            "#!/bin/sh\n/usr/bin/python3 -c 'print(\"[\" + \" \" * 1000 + \"]\")'\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.path().join("test.sh"), "echo ok\n").unwrap();
        let mut env = HashMap::new();
        env.insert("PATH".into(), dir.path().display().to_string());
        let cfg = crate::config::ProcessConfig {
            output_memory_bytes: 32,
            ..crate::config::ProcessConfig::default()
        };
        let result = ShellcheckPlugin::new()
            .run_command_with_config(
                "check",
                dir.path(),
                &env,
                None,
                Some(&json!({"file": "test.sh"})),
                &cfg,
            )
            .await
            .unwrap();
        assert!(result.output.get("error").is_some());
        assert!(result.output.get("clean").is_none());
    }

    #[tokio::test]
    async fn missing_args_errors() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = ShellcheckPlugin::new();
        let args = json!({"shell": "bash"}); // no file or files key
        let result = plugin
            .run_command("check", dir.path(), &HashMap::new(), None, Some(&args))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("required"));
    }
}
