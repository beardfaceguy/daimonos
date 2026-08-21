use std::collections::HashMap;
use std::path::Path;

use crate::tool_runner::{ToolCommand, ToolDescriptor, ToolPlugin, ToolResult};
use serde_json::{json, Value};

pub fn is_available() -> bool {
    std::process::Command::new("npm")
        .arg("--version")
        // Never inherit our stdin; see build_command in ops/exec_ops.rs.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub struct NpmPlugin {
    descriptor: ToolDescriptor,
}

impl NpmPlugin {
    pub fn new() -> Self {
        let mut commands = HashMap::new();
        for cmd in &["install", "run", "test", "build", "audit"] {
            commands.insert(
                cmd.to_string(),
                ToolCommand {
                    bin: "npm".into(),
                    args: vec![],
                    output: "structured".into(),
                },
            );
        }
        Self {
            descriptor: ToolDescriptor {
                id: "npm".into(),
                commands,
                source_pattern: None,
                manifest: Some("package.json".into()),
                diagnostics_format: "none".into(),
                supports_quickfix: false,
                quickfix_format: None,
            },
        }
    }
}

#[async_trait::async_trait]
impl ToolPlugin for NpmPlugin {
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
        let runner = NpmRun {
            cwd,
            env,
            process_cfg,
        };
        let output = match command {
            "install" => npm_install(&runner, args).await?,
            "run" => npm_run(&runner, args).await?,
            "test" => npm_test(&runner, args).await?,
            "build" => npm_build(&runner, args).await?,
            "audit" => npm_audit(&runner, args).await?,
            _ => return Err(format!("unknown npm command: {command}")),
        };
        Ok(ToolResult {
            tool: "npm".into(),
            command: command.to_string(),
            exit_code: 0,
            output,
            stderr: String::new(),
        })
    }
}

struct NpmRun<'a> {
    cwd: &'a Path,
    env: &'a HashMap<String, String>,
    process_cfg: &'a crate::config::ProcessConfig,
}

impl NpmRun<'_> {
    async fn run(&self, npm_args: &[&str]) -> Result<(i32, String, String), String> {
        let args: Vec<String> = npm_args.iter().map(|arg| (*arg).to_string()).collect();
        let output =
            crate::managed_process::run("npm", &args, self.cwd, self.env, self.process_cfg, None)
                .await
                .map_err(|e| format!("npm exec: {e}"))?;
        if output.timed_out {
            return Err(format!(
                "npm timed out after {} seconds",
                self.process_cfg.default_timeout_secs
            ));
        }
        if output.stdout_truncated || output.stderr_truncated {
            return Err("npm output exceeded process.output_memory_bytes".into());
        }
        let exit = output.status.code().unwrap_or(-1);
        Ok((exit, output.stdout, output.stderr))
    }
}

async fn npm_install(runner: &NpmRun<'_>, _args: Option<&Value>) -> Result<Value, String> {
    let (exit, stdout, stderr) = runner.run(&["install", "--no-fund", "--no-audit"]).await?;
    Ok(json!({
        "exit": exit,
        "ok": exit == 0,
        "stdout": stdout,
        "stderr": stderr,
    }))
}

async fn npm_run(runner: &NpmRun<'_>, args: Option<&Value>) -> Result<Value, String> {
    let script = args
        .and_then(|v| v.get("script"))
        .and_then(|v| v.as_str())
        .ok_or("npm run: 'script' is required")?;

    let (exit, stdout, stderr) = runner.run(&["run", script]).await?;
    Ok(json!({
        "exit": exit,
        "ok": exit == 0,
        "script": script,
        "stdout": stdout,
        "stderr": stderr,
    }))
}

async fn npm_test(runner: &NpmRun<'_>, _args: Option<&Value>) -> Result<Value, String> {
    let (exit, stdout, stderr) = runner.run(&["test", "--", "--passWithNoTests"]).await?;
    Ok(json!({
        "exit": exit,
        "ok": exit == 0,
        "stdout": stdout,
        "stderr": stderr,
    }))
}

async fn npm_build(runner: &NpmRun<'_>, _args: Option<&Value>) -> Result<Value, String> {
    let (exit, stdout, stderr) = runner.run(&["run", "build"]).await?;
    Ok(json!({
        "exit": exit,
        "ok": exit == 0,
        "stdout": stdout,
        "stderr": stderr,
    }))
}

async fn npm_audit(runner: &NpmRun<'_>, _args: Option<&Value>) -> Result<Value, String> {
    let (exit, stdout, stderr) = runner.run(&["audit", "--json"]).await?;

    // npm audit exits non-zero when vulnerabilities are found — that's expected.
    // Fatal errors (no package.json, network failure) produce non-JSON output.
    match serde_json::from_str::<Value>(&stdout) {
        Ok(parsed) => Ok(compact_audit(parsed)),
        Err(_) => {
            let stderr = stderr.trim().to_string();
            Ok(json!({
                "error": if stderr.is_empty() { "npm audit produced no JSON output" } else { &stderr },
                "exit": exit,
            }))
        }
    }
}

/// Reduce the verbose npm audit JSON to the fields agents actually need.
fn compact_audit(full: Value) -> Value {
    let vuln_summary = full
        .pointer("/metadata/vulnerabilities")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let total = vuln_summary
        .get("total")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let clean = total == 0;

    // Collect top-level vulnerability names + severity for a compact listing.
    let findings: Vec<Value> = full
        .get("vulnerabilities")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.values()
                .map(|entry| {
                    json!({
                        "name": entry.get("name").cloned().unwrap_or(json!("")),
                        "severity": entry.get("severity").cloned().unwrap_or(json!("unknown")),
                        "via": entry.get("via")
                            .and_then(|v| v.as_array())
                            .map(|a| a.iter()
                                .filter_map(|x| x.as_str().map(String::from)
                                    .or_else(|| x.get("title").and_then(|t| t.as_str()).map(String::from)))
                                .collect::<Vec<_>>())
                            .unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    json!({
        "clean": clean,
        "vulnerabilities": vuln_summary,
        "findings": findings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npm_is_available() {
        assert!(is_available(), "npm should be on PATH in this environment");
    }

    #[test]
    fn compact_audit_clean_project() {
        let full = json!({
            "auditReportVersion": 2,
            "vulnerabilities": {},
            "metadata": {
                "vulnerabilities": {
                    "info": 0, "low": 0, "moderate": 0,
                    "high": 0, "critical": 0, "total": 0
                }
            }
        });
        let result = compact_audit(full);
        assert_eq!(result["clean"], true);
        assert_eq!(result["findings"], json!([]));
        assert_eq!(result["vulnerabilities"]["total"], 0);
    }

    #[test]
    fn compact_audit_with_vulnerabilities() {
        let full = json!({
            "auditReportVersion": 2,
            "vulnerabilities": {
                "lodash": {
                    "name": "lodash",
                    "severity": "high",
                    "via": ["prototype-pollution"]
                }
            },
            "metadata": {
                "vulnerabilities": {
                    "info": 0, "low": 0, "moderate": 0,
                    "high": 1, "critical": 0, "total": 1
                }
            }
        });
        let result = compact_audit(full);
        assert_eq!(result["clean"], false);
        assert_eq!(result["vulnerabilities"]["high"], 1);
        assert_eq!(result["vulnerabilities"]["total"], 1);
        let findings = result["findings"].as_array().unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0]["name"], "lodash");
        assert_eq!(findings[0]["severity"], "high");
    }

    #[test]
    fn compact_audit_missing_metadata_defaults_clean() {
        let full = json!({"auditReportVersion": 2, "vulnerabilities": {}});
        let result = compact_audit(full);
        assert_eq!(result["clean"], true);
    }

    #[test]
    fn unknown_command_errors() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let plugin = NpmPlugin::new();
        let result = rt.block_on(async {
            plugin
                .run_command("publish", dir.path(), &HashMap::new(), None, None)
                .await
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown npm command"));
    }

    #[test]
    fn npm_run_without_script_errors() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let plugin = NpmPlugin::new();
        let args = json!({}); // missing "script"
        let result = rt.block_on(async {
            plugin
                .run_command("run", dir.path(), &HashMap::new(), None, Some(&args))
                .await
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("required"));
    }

    #[tokio::test]
    async fn npm_install_in_valid_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"test","version":"1.0.0","dependencies":{}}"#,
        )
        .unwrap();

        let plugin = NpmPlugin::new();
        let result = plugin
            .run_command("install", dir.path(), &HashMap::new(), None, None)
            .await
            .unwrap();

        assert!(
            result.output.get("exit").is_some(),
            "expected exit field; got: {}",
            result.output
        );
        assert!(
            result.output.get("ok").is_some(),
            "expected ok field; got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn npm_audit_in_valid_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"test","version":"1.0.0","dependencies":{}}"#,
        )
        .unwrap();
        // npm audit needs a lock file to work without error
        std::fs::write(
            dir.path().join("package-lock.json"),
            r#"{"name":"test","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{}}"#,
        )
        .unwrap();

        let plugin = NpmPlugin::new();
        let result = plugin
            .run_command("audit", dir.path(), &HashMap::new(), None, None)
            .await
            .unwrap();

        assert!(
            result.output.get("clean").is_some() || result.output.get("error").is_some(),
            "expected clean or error field; got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn npm_audit_no_package_json_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        // No package.json — npm audit should fail with non-JSON output
        let plugin = NpmPlugin::new();
        let result = plugin
            .run_command("audit", dir.path(), &HashMap::new(), None, None)
            .await
            .unwrap();

        // Should return {error, exit} not panic
        assert!(
            result.output.get("error").is_some() || result.output.get("clean").is_some(),
            "expected error or clean field; got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn npm_run_with_script_in_valid_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"test","version":"1.0.0","scripts":{"greet":"echo hello from npm"}}"#,
        )
        .unwrap();

        let plugin = NpmPlugin::new();
        let args = json!({"script": "greet"});
        let result = plugin
            .run_command("run", dir.path(), &HashMap::new(), None, Some(&args))
            .await
            .unwrap();

        assert_eq!(result.output["exit"], 0, "got: {}", result.output);
        assert_eq!(result.output["script"], "greet");
        assert!(
            result.output["stdout"]
                .as_str()
                .unwrap_or("")
                .contains("hello from npm"),
            "got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn npm_run_nonexistent_script_returns_nonzero() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"test","version":"1.0.0","scripts":{}}"#,
        )
        .unwrap();

        let plugin = NpmPlugin::new();
        let args = json!({"script": "nonexistent-script-xyz"});
        let result = plugin
            .run_command("run", dir.path(), &HashMap::new(), None, Some(&args))
            .await
            .unwrap();

        assert_ne!(
            result.output["exit"], 0,
            "expected nonzero exit for missing script; got: {}",
            result.output
        );
        assert_eq!(result.output["ok"], false);
    }
}
