use std::collections::HashMap;
use std::path::Path;

use serde_json::json;

use crate::tool_runner::{ToolCommand, ToolDescriptor, ToolPlugin, ToolResult};

pub struct GhPlugin {
    descriptor: ToolDescriptor,
}

impl GhPlugin {
    pub fn new() -> Self {
        let mut commands = HashMap::new();
        for name in [
            "pr_view",
            "pr_list",
            "pr_create",
            "pr_diff",
            "pr_checks",
            "pr_merge",
            "pr_checkout",
            "run_list",
            "run_view",
            "issue_list",
            "issue_view",
            "issue_create",
            "issue_comment",
            "api",
            "raw",
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

    async fn run_command_with_config(
        &self,
        command: &str,
        cwd: &Path,
        env: &HashMap<String, String>,
        _stdin_data: Option<&[u8]>,
        args: Option<&serde_json::Value>,
        process_cfg: &crate::config::ProcessConfig,
    ) -> Result<ToolResult, String> {
        let runner = GhRun {
            cwd,
            env,
            process_cfg,
        };
        let output = match command {
            "pr_view" => gh_pr_view(&runner, args).await?,
            "pr_list" => gh_pr_list(&runner, args).await?,
            "pr_create" => gh_pr_create(&runner, args).await?,
            "pr_diff" => gh_pr_diff(&runner, args).await?,
            "pr_checks" => gh_pr_checks(&runner, args).await?,
            "pr_merge" => gh_pr_merge(&runner, args).await?,
            "pr_checkout" => gh_pr_checkout(&runner, args).await?,
            "run_list" => gh_run_list(&runner, args).await?,
            "run_view" => gh_run_view(&runner, args).await?,
            "issue_list" => gh_issue_list(&runner, args).await?,
            "issue_view" => gh_issue_view(&runner, args).await?,
            "issue_create" => gh_issue_create(&runner, args).await?,
            "issue_comment" => gh_issue_comment(&runner, args).await?,
            "api" => gh_api(&runner, args).await?,
            "raw" => gh_raw(&runner, args).await?,
            _ => return Err(format!("unknown gh command: {command}")),
        };

        // `raw` surfaces the real gh exit code inside its payload; reflect it on
        // the ToolResult too so consumers reading `exit_code` aren't misled. Every
        // other command returns Err on a non-zero exit (via `?` above), so any
        // success path reached here is genuinely exit 0.
        let exit_code = if command == "raw" {
            output
                .get("exit_code")
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32
        } else {
            0
        };
        Ok(ToolResult {
            tool: "gh".into(),
            command: command.into(),
            exit_code,
            output,
            stderr: String::new(),
        })
    }
}

struct GhRun<'a> {
    cwd: &'a Path,
    env: &'a HashMap<String, String>,
    process_cfg: &'a crate::config::ProcessConfig,
}

impl GhRun<'_> {
    async fn output(
        &self,
        args: &[String],
    ) -> Result<crate::managed_process::ManagedOutput, String> {
        let mut env = self.env.clone();
        env.insert("GH_PROMPT_DISABLED".into(), "1".into());
        env.insert("NO_COLOR".into(), "1".into());
        crate::managed_process::run("gh", args, self.cwd, &env, self.process_cfg, None)
            .await
            .map_err(|e| format!("gh exec: {e}"))
    }

    async fn text(&self, args: &[String]) -> Result<String, String> {
        let output = self.output(args).await?;
        if !output.status.success() {
            return Err(format!("gh {}: {}", args[0], output.stderr.trim()));
        }
        if output.stdout_truncated || output.stderr_truncated {
            return Err("gh output exceeded process.output_memory_bytes".into());
        }
        Ok(output.stdout)
    }
}

async fn run_gh(runner: &GhRun<'_>, args: &[&str]) -> Result<String, String> {
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
    runner.text(&args).await
}

async fn gh_pr_view(
    runner: &GhRun<'_>,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let json_fields = "number,title,state,author,url,headRefName,baseRefName,additions,deletions,changedFiles,reviewDecision,checks,body";

    let number_str;
    let mut gh_args = vec!["pr", "view", "--json", json_fields];

    if let Some(n) = args.and_then(|a| a.get("number")).and_then(|v| v.as_i64()) {
        number_str = n.to_string();
        gh_args.insert(2, &number_str);
    }

    let out = run_gh(runner, &gh_args).await?;
    serde_json::from_str(&out).map_err(|e| format!("parse gh output: {e}"))
}

async fn gh_pr_list(
    runner: &GhRun<'_>,
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
        "pr",
        "list",
        "--state",
        state,
        "--limit",
        &limit_str,
        "--json",
        json_fields,
    ];

    let author_owned;
    if let Some(author) = args.and_then(|a| a.get("author")).and_then(|v| v.as_str()) {
        author_owned = author.to_string();
        gh_args.push("--author");
        gh_args.push(&author_owned);
    }

    let out = run_gh(runner, &gh_args).await?;
    let prs: serde_json::Value =
        serde_json::from_str(&out).map_err(|e| format!("parse gh output: {e}"))?;

    Ok(json!({
        "prs": prs,
        "count": prs.as_array().map(|a| a.len()).unwrap_or(0),
    }))
}

async fn gh_pr_create(
    runner: &GhRun<'_>,
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

    let out = run_gh(runner, &gh_args).await?;

    Ok(json!({
        "url": out.trim(),
    }))
}

async fn gh_pr_diff(
    runner: &GhRun<'_>,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let number_str;
    let mut gh_args = vec!["pr", "diff"];

    if let Some(n) = args.and_then(|a| a.get("number")).and_then(|v| v.as_i64()) {
        number_str = n.to_string();
        gh_args.push(&number_str);
    }

    let owned_args: Vec<String> = gh_args.iter().map(|arg| (*arg).to_string()).collect();
    let output = runner.output(&owned_args).await?;
    if !output.status.success() {
        return Err(format!("gh pr: {}", output.stderr.trim()));
    }
    let managed_truncated = output.stdout_truncated || output.stderr_truncated;
    let (diff, truncated) = cap_str(&output.stdout, MAX_GH_OUTPUT);

    Ok(json!({
        "diff": diff,
        "truncated": managed_truncated || truncated,
    }))
}

async fn gh_pr_checks(
    runner: &GhRun<'_>,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let json_fields = "name,state,conclusion,link";

    let number_str;
    let mut gh_args = vec!["pr", "checks", "--json", json_fields];

    if let Some(n) = args.and_then(|a| a.get("number")).and_then(|v| v.as_i64()) {
        number_str = n.to_string();
        gh_args.insert(2, &number_str);
    }

    let out = run_gh(runner, &gh_args).await?;
    let checks: serde_json::Value =
        serde_json::from_str(&out).map_err(|e| format!("parse gh output: {e}"))?;

    Ok(json!({
        "checks": checks,
        "count": checks.as_array().map(|a| a.len()).unwrap_or(0),
    }))
}

async fn gh_api(
    runner: &GhRun<'_>,
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

    let out = run_gh(runner, &gh_args).await?;

    serde_json::from_str(&out).or_else(|_| Ok(json!({"raw": out.trim()})))
}

// --- small typed arg accessors (the plugin receives a flat JSON args object) ---

fn arg_str<'a>(args: Option<&'a serde_json::Value>, key: &str) -> Option<&'a str> {
    args.and_then(|a| a.get(key)).and_then(|v| v.as_str())
}

fn arg_i64(args: Option<&serde_json::Value>, key: &str) -> Option<i64> {
    args.and_then(|a| a.get(key)).and_then(|v| v.as_i64())
}

fn arg_bool(args: Option<&serde_json::Value>, key: &str) -> bool {
    args.and_then(|a| a.get(key))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Run gh from an owned arg vector (the `*_argv` builders below produce these).
async fn run_gh_owned(runner: &GhRun<'_>, args: &[String]) -> Result<String, String> {
    runner.text(args).await
}

/// Max bytes of any single gh command's captured output retained before
/// truncation, bounding tool-output size and memory (vikunja #943, #944).
const MAX_GH_OUTPUT: usize = 100_000;

/// Truncate `s` to at most `max` bytes on a char boundary. Returns the possibly
/// truncated string and whether truncation happened; appends a marker when cut.
fn cap_str(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let end = crate::plugins::floor_char_boundary(s, max);
    (
        format!("{}\n[truncated {} bytes]", &s[..end], s.len() - end),
        true,
    )
}

// --- raw passthrough: any gh subcommand, present or future ---

/// Build the gh argv for `raw` from the `args` string array.
fn raw_argv(args: Option<&serde_json::Value>) -> Result<Vec<String>, String> {
    let arr = args
        .and_then(|a| a.get("args"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            "raw requires 'args' (array of strings), e.g. [\"pr\",\"merge\",\"6\",\"--squash\"]"
                .to_string()
        })?;
    if arr.is_empty() {
        return Err("raw 'args' must not be empty".to_string());
    }
    let mut out = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        let s = v
            .as_str()
            .ok_or_else(|| format!("raw args[{i}] must be a string"))?;
        out.push(s.to_string());
    }
    Ok(out)
}

/// Run an arbitrary gh invocation, surfacing exit code + stdout + stderr rather
/// than erroring on a non-zero exit (the caller chose the command; let them see
/// the full result). stdout is JSON-parsed when possible, else returned verbatim.
async fn gh_raw(
    runner: &GhRun<'_>,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let argv = raw_argv(args)?;
    let output = runner.output(&argv).await?;
    let managed_truncated = output.stdout_truncated || output.stderr_truncated;
    let (stdout, stdout_truncated) = cap_str(&output.stdout, MAX_GH_OUTPUT);
    let (stderr, stderr_truncated) = cap_str(&output.stderr, MAX_GH_OUTPUT);
    // Parse stdout as JSON only when returned intact — a truncated buffer isn't
    // valid JSON, so surface it as a string in that case.
    let stdout_val = if managed_truncated || stdout_truncated {
        json!(stdout)
    } else {
        serde_json::from_str::<serde_json::Value>(stdout.trim()).unwrap_or_else(|_| json!(stdout))
    };
    Ok(json!({
        "exit_code": output.status.code().unwrap_or(-1),
        "stdout": stdout_val,
        "stderr": stderr.trim(),
        "truncated": managed_truncated || stdout_truncated || stderr_truncated,
    }))
}

// --- pr_merge / pr_checkout ---

fn pr_merge_argv(args: Option<&serde_json::Value>) -> Result<Vec<String>, String> {
    let mut v = vec!["pr".to_string(), "merge".to_string()];
    if let Some(n) = arg_i64(args, "number") {
        v.push(n.to_string());
    }
    match arg_str(args, "merge_method").unwrap_or("merge") {
        "merge" => v.push("--merge".to_string()),
        "squash" => v.push("--squash".to_string()),
        "rebase" => v.push("--rebase".to_string()),
        other => {
            return Err(format!(
                "pr_merge: invalid merge_method {other:?}; use merge|squash|rebase"
            ))
        }
    }
    if arg_bool(args, "delete_branch") {
        v.push("--delete-branch".to_string());
    }
    if let Some(s) = arg_str(args, "subject") {
        v.push("--subject".to_string());
        v.push(s.to_string());
    }
    if let Some(b) = arg_str(args, "body") {
        v.push("--body".to_string());
        v.push(b.to_string());
    }
    Ok(v)
}

async fn gh_pr_merge(
    runner: &GhRun<'_>,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let out = run_gh_owned(runner, &pr_merge_argv(args)?).await?;
    // Return gh's own output rather than a synthesized boolean; gh exits non-zero
    // (surfaced as an error above) when it cannot merge, so success == the merge
    // message it printed.
    Ok(json!({ "output": out.trim() }))
}

fn pr_checkout_argv(args: Option<&serde_json::Value>) -> Result<Vec<String>, String> {
    let target = arg_i64(args, "number")
        .map(|n| n.to_string())
        .or_else(|| arg_str(args, "branch").map(|s| s.to_string()))
        .ok_or_else(|| "pr_checkout requires 'number' or 'branch'".to_string())?;
    Ok(vec!["pr".to_string(), "checkout".to_string(), target])
}

async fn gh_pr_checkout(
    runner: &GhRun<'_>,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let out = run_gh_owned(runner, &pr_checkout_argv(args)?).await?;
    Ok(json!({ "output": out.trim() }))
}

// --- run_list / run_view ---

fn run_list_argv(args: Option<&serde_json::Value>) -> Vec<String> {
    let fields =
        "databaseId,displayTitle,status,conclusion,workflowName,headBranch,event,createdAt";
    let limit = arg_i64(args, "limit").unwrap_or(10).clamp(1, 100);
    let mut v = vec![
        "run".to_string(),
        "list".to_string(),
        "--json".to_string(),
        fields.to_string(),
        "--limit".to_string(),
        limit.to_string(),
    ];
    if let Some(b) = arg_str(args, "branch") {
        v.push("--branch".to_string());
        v.push(b.to_string());
    }
    if let Some(w) = arg_str(args, "workflow") {
        v.push("--workflow".to_string());
        v.push(w.to_string());
    }
    if let Some(s) = arg_str(args, "status") {
        v.push("--status".to_string());
        v.push(s.to_string());
    }
    v
}

async fn gh_run_list(
    runner: &GhRun<'_>,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let out = run_gh_owned(runner, &run_list_argv(args)).await?;
    let runs: serde_json::Value =
        serde_json::from_str(&out).map_err(|e| format!("parse gh output: {e}"))?;
    Ok(json!({
        "runs": runs,
        "count": runs.as_array().map(|a| a.len()).unwrap_or(0),
    }))
}

fn run_view_argv(args: Option<&serde_json::Value>) -> Result<Vec<String>, String> {
    let id = arg_i64(args, "run_id")
        .ok_or_else(|| "run_view requires 'run_id' (from run_list)".to_string())?;
    let fields = "databaseId,status,conclusion,displayTitle,workflowName,headBranch,event,jobs";
    Ok(vec![
        "run".to_string(),
        "view".to_string(),
        id.to_string(),
        "--json".to_string(),
        fields.to_string(),
    ])
}

async fn gh_run_view(
    runner: &GhRun<'_>,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let out = run_gh_owned(runner, &run_view_argv(args)?).await?;
    serde_json::from_str(&out).map_err(|e| format!("parse gh output: {e}"))
}

// --- issue_list / issue_view / issue_create / issue_comment ---

fn issue_list_argv(args: Option<&serde_json::Value>) -> Vec<String> {
    let fields = "number,title,state,author,url,labels";
    let state = arg_str(args, "state").unwrap_or("open");
    let limit = arg_i64(args, "limit").unwrap_or(20).clamp(1, 100);
    let mut v = vec![
        "issue".to_string(),
        "list".to_string(),
        "--json".to_string(),
        fields.to_string(),
        "--state".to_string(),
        state.to_string(),
        "--limit".to_string(),
        limit.to_string(),
    ];
    if let Some(a) = arg_str(args, "author") {
        v.push("--author".to_string());
        v.push(a.to_string());
    }
    if let Some(l) = arg_str(args, "label") {
        v.push("--label".to_string());
        v.push(l.to_string());
    }
    v
}

async fn gh_issue_list(
    runner: &GhRun<'_>,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let out = run_gh_owned(runner, &issue_list_argv(args)).await?;
    let issues: serde_json::Value =
        serde_json::from_str(&out).map_err(|e| format!("parse gh output: {e}"))?;
    Ok(json!({
        "issues": issues,
        "count": issues.as_array().map(|a| a.len()).unwrap_or(0),
    }))
}

fn issue_view_argv(args: Option<&serde_json::Value>) -> Result<Vec<String>, String> {
    let n = arg_i64(args, "number").ok_or_else(|| "issue_view requires 'number'".to_string())?;
    let fields = "number,title,state,author,url,body,labels,comments";
    Ok(vec![
        "issue".to_string(),
        "view".to_string(),
        n.to_string(),
        "--json".to_string(),
        fields.to_string(),
    ])
}

async fn gh_issue_view(
    runner: &GhRun<'_>,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let out = run_gh_owned(runner, &issue_view_argv(args)?).await?;
    serde_json::from_str(&out).map_err(|e| format!("parse gh output: {e}"))
}

fn issue_create_argv(args: Option<&serde_json::Value>) -> Result<Vec<String>, String> {
    let title =
        arg_str(args, "title").ok_or_else(|| "issue_create requires 'title'".to_string())?;
    let mut v = vec![
        "issue".to_string(),
        "create".to_string(),
        "--title".to_string(),
        title.to_string(),
    ];
    if let Some(b) = arg_str(args, "body") {
        v.push("--body".to_string());
        v.push(b.to_string());
    }
    if let Some(l) = arg_str(args, "label") {
        v.push("--label".to_string());
        v.push(l.to_string());
    }
    Ok(v)
}

async fn gh_issue_create(
    runner: &GhRun<'_>,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let out = run_gh_owned(runner, &issue_create_argv(args)?).await?;
    Ok(json!({ "url": out.trim() }))
}

fn issue_comment_argv(args: Option<&serde_json::Value>) -> Result<Vec<String>, String> {
    let n = arg_i64(args, "number").ok_or_else(|| "issue_comment requires 'number'".to_string())?;
    let body = arg_str(args, "body").ok_or_else(|| "issue_comment requires 'body'".to_string())?;
    Ok(vec![
        "issue".to_string(),
        "comment".to_string(),
        n.to_string(),
        "--body".to_string(),
        body.to_string(),
    ])
}

async fn gh_issue_comment(
    runner: &GhRun<'_>,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let out = run_gh_owned(runner, &issue_comment_argv(args)?).await?;
    Ok(json!({ "url": out.trim() }))
}

/// Check if gh CLI is available on PATH.
pub fn is_available() -> bool {
    std::process::Command::new("gh")
        .arg("--version")
        // Never inherit our stdin; see build_command in ops/exec_ops.rs.
        .stdin(std::process::Stdio::null())
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

    #[cfg(unix)]
    fn fake_gh(dir: &Path, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("gh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        dir.display().to_string()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn configured_runner_forwards_explicit_environment() {
        let dir = tempfile::tempdir().unwrap();
        let path = fake_gh(
            dir.path(),
            "printf '{\"sentinel\":\"%s\"}' \"$PLUGIN_SENTINEL\"",
        );
        let mut env = HashMap::new();
        env.insert("PATH".into(), path);
        env.insert("PLUGIN_SENTINEL".into(), "visible".into());
        let plugin = GhPlugin::new();
        let result = plugin
            .run_command_with_config(
                "raw",
                dir.path(),
                &env,
                None,
                Some(&json!({"args": ["test"]})),
                &crate::config::ProcessConfig::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.output["stdout"]["sentinel"], "visible");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn raw_reports_managed_capture_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = fake_gh(dir.path(), "/usr/bin/python3 -c 'print(\"x\" * 1000)'");
        let mut env = HashMap::new();
        env.insert("PATH".into(), path);
        let cfg = crate::config::ProcessConfig {
            output_memory_bytes: 32,
            ..crate::config::ProcessConfig::default()
        };
        let result = GhPlugin::new()
            .run_command_with_config(
                "raw",
                dir.path(),
                &env,
                None,
                Some(&json!({"args": ["test"]})),
                &cfg,
            )
            .await
            .unwrap();
        assert_eq!(result.output["truncated"], true);
        assert!(result.output["stdout"]
            .as_str()
            .unwrap()
            .contains("truncated"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pr_diff_preserves_managed_capture_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = fake_gh(
            dir.path(),
            "/usr/bin/python3 -c 'print(\"diff\" + \"x\" * 1000)'",
        );
        let mut env = HashMap::new();
        env.insert("PATH".into(), path);
        let cfg = crate::config::ProcessConfig {
            output_memory_bytes: 32,
            ..crate::config::ProcessConfig::default()
        };
        let result = GhPlugin::new()
            .run_command_with_config("pr_diff", dir.path(), &env, None, None, &cfg)
            .await
            .unwrap();
        assert_eq!(result.output["truncated"], true);
        assert!(result.output["diff"]
            .as_str()
            .unwrap()
            .contains("truncated"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn configured_runner_honors_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = fake_gh(dir.path(), "/bin/sleep 30");
        let mut env = HashMap::new();
        env.insert("PATH".into(), path);
        let cfg = crate::config::ProcessConfig {
            default_timeout_secs: 1,
            termination_grace_ms: 10,
            ..crate::config::ProcessConfig::default()
        };
        let error = GhPlugin::new()
            .run_command_with_config(
                "raw",
                dir.path(),
                &env,
                None,
                Some(&json!({"args": ["test"]})),
                &cfg,
            )
            .await
            .unwrap_err();
        assert!(error.contains("timed out"));
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
        assert!(desc.commands.contains_key("pr_merge"));
        assert!(desc.commands.contains_key("pr_checkout"));
        assert!(desc.commands.contains_key("run_list"));
        assert!(desc.commands.contains_key("run_view"));
        assert!(desc.commands.contains_key("issue_list"));
        assert!(desc.commands.contains_key("issue_view"));
        assert!(desc.commands.contains_key("issue_create"));
        assert!(desc.commands.contains_key("issue_comment"));
        assert!(desc.commands.contains_key("api"));
        assert!(desc.commands.contains_key("raw"));
    }

    // --- pure argv builders (no live gh required) ---

    #[test]
    fn raw_argv_extracts_string_array() {
        let args = json!({"args": ["pr", "merge", "6", "--squash"]});
        assert_eq!(
            raw_argv(Some(&args)).unwrap(),
            vec!["pr", "merge", "6", "--squash"]
        );
    }

    #[test]
    fn raw_argv_rejects_missing_empty_or_non_string() {
        assert!(raw_argv(Some(&json!({}))).is_err());
        assert!(raw_argv(Some(&json!({"args": []}))).is_err());
        assert!(raw_argv(Some(&json!({"args": ["ok", 3]}))).is_err());
    }

    #[test]
    fn cap_str_truncates_only_when_over_limit() {
        let (s, cut) = cap_str("hello", 100);
        assert_eq!(s, "hello");
        assert!(!cut);
        let (s, cut) = cap_str("abcdefghij", 4);
        assert!(cut);
        assert!(s.starts_with("abcd"));
        assert!(s.contains("[truncated 6 bytes]"));
    }

    #[test]
    fn cap_str_respects_char_boundaries() {
        // 'é' is 2 bytes; cutting at byte 1 must back off to a boundary, not panic.
        let (_s, cut) = cap_str("é", 1);
        assert!(cut);
    }

    #[test]
    fn pr_merge_argv_maps_method_and_flags() {
        let args = json!({"number": 6, "merge_method": "squash", "delete_branch": true});
        assert_eq!(
            pr_merge_argv(Some(&args)).unwrap(),
            vec!["pr", "merge", "6", "--squash", "--delete-branch"]
        );
        // default method is merge; number optional (current branch)
        assert_eq!(
            pr_merge_argv(Some(&json!({}))).unwrap(),
            vec!["pr", "merge", "--merge"]
        );
        assert!(pr_merge_argv(Some(&json!({"merge_method": "bogus"}))).is_err());
    }

    #[test]
    fn pr_checkout_argv_accepts_number_or_branch() {
        assert_eq!(
            pr_checkout_argv(Some(&json!({"number": 6}))).unwrap(),
            vec!["pr", "checkout", "6"]
        );
        assert_eq!(
            pr_checkout_argv(Some(&json!({"branch": "feat/x"}))).unwrap(),
            vec!["pr", "checkout", "feat/x"]
        );
        assert!(pr_checkout_argv(Some(&json!({}))).is_err());
    }

    #[test]
    fn run_list_argv_defaults_and_filters() {
        let base = run_list_argv(Some(&json!({})));
        assert_eq!(base[0], "run");
        assert_eq!(base[1], "list");
        assert!(base.contains(&"--limit".to_string()));
        let filtered = run_list_argv(Some(&json!({"branch": "master", "limit": 3})));
        assert!(filtered.windows(2).any(|w| w == ["--branch", "master"]));
        assert!(filtered.windows(2).any(|w| w == ["--limit", "3"]));
    }

    #[test]
    fn run_view_argv_requires_run_id() {
        assert!(run_view_argv(Some(&json!({}))).is_err());
        let v = run_view_argv(Some(&json!({"run_id": 12345}))).unwrap();
        assert_eq!(v[0..3], ["run", "view", "12345"]);
    }

    #[test]
    fn issue_argv_builders() {
        assert!(issue_list_argv(Some(&json!({})))
            .windows(2)
            .any(|w| w == ["--state", "open"]));
        assert!(issue_view_argv(Some(&json!({}))).is_err());
        assert_eq!(
            issue_view_argv(Some(&json!({"number": 42}))).unwrap()[0..3],
            ["issue", "view", "42"]
        );
        assert!(issue_create_argv(Some(&json!({}))).is_err());
        assert!(issue_create_argv(Some(&json!({"title": "T"})))
            .unwrap()
            .windows(2)
            .any(|w| w == ["--title", "T"]));
        assert!(issue_comment_argv(Some(&json!({"number": 1}))).is_err());
        assert_eq!(
            issue_comment_argv(Some(&json!({"number": 1, "body": "hi"}))).unwrap(),
            vec!["issue", "comment", "1", "--body", "hi"]
        );
    }
}
