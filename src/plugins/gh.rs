use std::collections::HashMap;
use std::path::Path;

use serde_json::json;
use tokio::process::Command;

use crate::tool_runner::{ToolCommand, ToolDescriptor, ToolPlugin, ToolResult};

pub struct GhPlugin {
    descriptor: ToolDescriptor,
}

impl GhPlugin {
    pub fn new() -> Self {
        let mut commands = HashMap::new();
        for name in [
            "pr_view", "pr_list", "pr_create", "pr_diff", "pr_checks", "api",
        ] {
            commands.insert(
                name.to_string(),
                ToolCommand {
                    bin: "gh".into(),
                    args: vec![name.into()],
                    output: "structured".into(),
                },
            );
        }

        Self {
            descriptor: ToolDescriptor {
                id: "gh".into(),
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
impl ToolPlugin for GhPlugin {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    async fn run_command(
        &self,
        command: &str,
        cwd: &Path,
        _env: &HashMap<String, String>,
        _stdin_data: Option<&[u8]>,
        args: Option<&serde_json::Value>,
    ) -> Result<ToolResult, String> {
        let output = match command {
            "pr_view" => gh_pr_view(cwd, args).await?,
            "pr_list" => gh_pr_list(cwd, args).await?,
            "pr_create" => gh_pr_create(cwd, args).await?,
            "pr_diff" => gh_pr_diff(cwd, args).await?,
            "pr_checks" => gh_pr_checks(cwd, args).await?,
            "api" => gh_api(cwd, args).await?,
            _ => return Err(format!("unknown gh command: {command}")),
        };

        Ok(ToolResult {
            tool: "gh".into(),
            command: command.into(),
            exit_code: 0,
            output,
            stderr: String::new(),
        })
    }
}

async fn run_gh(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("gh")
        .args(args)
        .current_dir(cwd)
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .output()
        .await
        .map_err(|e| format!("gh exec: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh {}: {}", args[0], stderr.trim()));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

async fn gh_pr_view(
    cwd: &Path,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let json_fields = "number,title,state,author,url,headRefName,baseRefName,additions,deletions,changedFiles,reviewDecision,checks,body";

    let number_str;
    let mut gh_args = vec!["pr", "view", "--json", json_fields];

    if let Some(n) = args.and_then(|a| a.get("number")).and_then(|v| v.as_i64()) {
        number_str = n.to_string();
        gh_args.insert(2, &number_str);
    }

    let out = run_gh(cwd, &gh_args).await?;
    serde_json::from_str(&out).map_err(|e| format!("parse gh output: {e}"))
}

async fn gh_pr_list(
    cwd: &Path,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let state = args
        .and_then(|a| a.get("state"))
        .and_then(|v| v.as_str())
        .unwrap_or("open");

    let limit = args
        .and_then(|a| a.get("limit"))
        .and_then(|v| v.as_i64())
        .unwrap_or(10)
        .clamp(1, 100);

    let json_fields = "number,title,state,author,url,headRefName";
    let limit_str = limit.to_string();
    let mut gh_args = vec![
        "pr", "list", "--state", state, "--limit", &limit_str, "--json", json_fields,
    ];

    let author_owned;
    if let Some(author) = args.and_then(|a| a.get("author")).and_then(|v| v.as_str()) {
        author_owned = author.to_string();
        gh_args.push("--author");
        gh_args.push(&author_owned);
    }

    let out = run_gh(cwd, &gh_args).await?;
    let prs: serde_json::Value =
        serde_json::from_str(&out).map_err(|e| format!("parse gh output: {e}"))?;

    Ok(json!({
        "prs": prs,
        "count": prs.as_array().map(|a| a.len()).unwrap_or(0),
    }))
}

async fn gh_pr_create(
    cwd: &Path,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let title = args
        .and_then(|a| a.get("title"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "pr_create requires 'title' argument".to_string())?;

    let mut gh_args = vec!["pr", "create", "--title", title];

    let body_owned;
    if let Some(body) = args.and_then(|a| a.get("body")).and_then(|v| v.as_str()) {
        body_owned = body.to_string();
        gh_args.push("--body");
        gh_args.push(&body_owned);
    }

    let base_owned;
    if let Some(base) = args.and_then(|a| a.get("base")).and_then(|v| v.as_str()) {
        base_owned = base.to_string();
        gh_args.push("--base");
        gh_args.push(&base_owned);
    }

    if args
        .and_then(|a| a.get("draft"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        gh_args.push("--draft");
    }

    let out = run_gh(cwd, &gh_args).await?;

    Ok(json!({
        "url": out.trim(),
    }))
}

async fn gh_pr_diff(
    cwd: &Path,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let number_str;
    let mut gh_args = vec!["pr", "diff"];

    if let Some(n) = args.and_then(|a| a.get("number")).and_then(|v| v.as_i64()) {
        number_str = n.to_string();
        gh_args.push(&number_str);
    }

    let out = run_gh(cwd, &gh_args).await?;

    Ok(json!({
        "diff": out,
    }))
}

async fn gh_pr_checks(
    cwd: &Path,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let json_fields = "name,state,conclusion,link";

    let number_str;
    let mut gh_args = vec!["pr", "checks", "--json", json_fields];

    if let Some(n) = args.and_then(|a| a.get("number")).and_then(|v| v.as_i64()) {
        number_str = n.to_string();
        gh_args.insert(2, &number_str);
    }

    let out = run_gh(cwd, &gh_args).await?;
    let checks: serde_json::Value =
        serde_json::from_str(&out).map_err(|e| format!("parse gh output: {e}"))?;

    Ok(json!({
        "checks": checks,
        "count": checks.as_array().map(|a| a.len()).unwrap_or(0),
    }))
}

async fn gh_api(
    cwd: &Path,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let endpoint = args
        .and_then(|a| a.get("endpoint"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "api requires 'endpoint' argument".to_string())?;

    let method = args
        .and_then(|a| a.get("method"))
        .and_then(|v| v.as_str())
        .unwrap_or("GET");

    let gh_args = vec!["api", "-X", method, endpoint];

    let out = run_gh(cwd, &gh_args).await?;

    serde_json::from_str(&out).or_else(|_| Ok(json!({"raw": out.trim()})))
}

/// Check if gh CLI is available on PATH.
pub fn is_available() -> bool {
    std::process::Command::new("gh")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_runner::ToolRegistry;
    use std::sync::Arc;

    #[test]
    fn is_available_does_not_panic() {
        let _ = is_available();
    }

    #[tokio::test]
    async fn unknown_command_returns_error() {
        let plugin = GhPlugin::new();
        let env = HashMap::new();
        let dir = tempfile::tempdir().unwrap();
        let result = plugin
            .run_command("nonexistent", dir.path(), &env, None, None)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown gh command"));
    }

    #[tokio::test]
    async fn plugin_registers_in_registry() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(GhPlugin::new())).await;

        let dir = tempfile::tempdir().unwrap();
        let env = HashMap::new();
        let result = registry
            .run("gh", "nonexistent", dir.path(), &env, None, None)
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn descriptor_has_all_commands() {
        let plugin = GhPlugin::new();
        let desc = plugin.descriptor();
        assert_eq!(desc.id, "gh");
        assert!(desc.commands.contains_key("pr_view"));
        assert!(desc.commands.contains_key("pr_list"));
        assert!(desc.commands.contains_key("pr_create"));
        assert!(desc.commands.contains_key("pr_diff"));
        assert!(desc.commands.contains_key("pr_checks"));
        assert!(desc.commands.contains_key("api"));
    }
}
