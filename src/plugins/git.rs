use std::collections::HashMap;
use std::path::Path;

use crate::tool_runner::{ToolCommand, ToolDescriptor, ToolPlugin, ToolResult};
use serde_json::json;
#[cfg(test)]
use tokio::process::Command;

pub struct GitPlugin {
    descriptor: ToolDescriptor,
}

impl GitPlugin {
    pub fn new() -> Self {
        let mut commands = HashMap::new();
        for name in [
            "status", "log", "diff", "branch", "add", "commit", "push", "pull", "checkout",
        ] {
            commands.insert(
                name.to_string(),
                ToolCommand {
                    bin: "git".into(),
                    args: vec![name.into()],
                    output: "structured".into(),
                },
            );
        }

        Self {
            descriptor: ToolDescriptor {
                id: "git".into(),
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
impl ToolPlugin for GitPlugin {
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
        let runner = GitRun {
            cwd,
            env,
            process_cfg,
        };
        let output = match command {
            "status" => git_status(&runner).await?,
            "log" => git_log(&runner, args).await?,
            "diff" => git_diff(&runner, args).await?,
            "branch" => git_branch(&runner).await?,
            "add" => git_add(&runner, args).await?,
            "commit" => git_commit(&runner, args).await?,
            "push" => git_push(&runner, args).await?,
            "pull" => git_pull(&runner, args).await?,
            "checkout" => git_checkout(&runner, args).await?,
            _ => return Err(format!("unknown git command: {command}")),
        };

        Ok(ToolResult {
            tool: "git".into(),
            command: command.into(),
            exit_code: 0,
            output,
            stderr: String::new(),
        })
    }
}

struct GitRun<'a> {
    cwd: &'a Path,
    env: &'a HashMap<String, String>,
    process_cfg: &'a crate::config::ProcessConfig,
}

impl GitRun<'_> {
    async fn output(&self, args: &[&str]) -> Result<crate::managed_process::ManagedOutput, String> {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
        let mut env = self.env.clone();
        env.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
        crate::managed_process::run("git", &args, self.cwd, &env, self.process_cfg, None)
            .await
            .map_err(|e| format!("git exec: {e}"))
    }

    async fn text(&self, args: &[&str]) -> Result<String, String> {
        let output = self.output(args).await?;
        if !output.status.success() {
            return Err(format!("git {}: {}", args[0], output.stderr.trim()));
        }
        if output.stdout_truncated || output.stderr_truncated {
            return Err(format!(
                "git {} output exceeded process.output_memory_bytes",
                args[0]
            ));
        }
        Ok(output.stdout)
    }
}

async fn git_status(runner: &GitRun<'_>) -> Result<serde_json::Value, String> {
    let out = runner.text(&["status", "--porcelain=v1", "-uall"]).await?;

    let mut modified = Vec::new();
    let mut added = Vec::new();
    let mut deleted = Vec::new();
    let mut untracked = Vec::new();
    let mut renamed = Vec::new();

    for line in out.lines() {
        if line.len() < 4 {
            continue;
        }
        let xy = &line[..2];
        let file = &line[3..];

        match xy.trim() {
            "M" | "MM" | "AM" => modified.push(file.to_string()),
            "A" => added.push(file.to_string()),
            "D" => deleted.push(file.to_string()),
            "??" => untracked.push(file.to_string()),
            s if s.starts_with('R') => {
                if let Some((from, to)) = file.split_once(" -> ") {
                    renamed.push(json!({"from": from, "to": to}));
                }
            }
            _ => modified.push(file.to_string()),
        }
    }

    let clean = modified.is_empty()
        && added.is_empty()
        && deleted.is_empty()
        && untracked.is_empty()
        && renamed.is_empty();

    // Include branch and HEAD info so a single status call gives full context
    let branch = runner
        .text(&["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .unwrap_or_default()
        .trim()
        .to_string();

    let head = runner
        .text(&["log", "-1", "--format=%h%x00%s"])
        .await
        .ok()
        .and_then(|out| {
            let parts: Vec<&str> = out.trim().splitn(2, '\0').collect();
            if parts.len() == 2 {
                Some(json!({"h": parts[0], "m": parts[1]}))
            } else {
                None
            }
        });

    let commit_count = runner
        .text(&["rev-list", "--count", "HEAD"])
        .await
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok());

    let mut result = json!({"clean": clean, "branch": branch});
    if let Some(h) = head {
        result["head"] = h;
    }
    if let Some(n) = commit_count {
        result["commits"] = json!(n);
    }
    if !modified.is_empty() {
        result["modified"] = json!(modified);
    }
    if !added.is_empty() {
        result["added"] = json!(added);
    }
    if !deleted.is_empty() {
        result["deleted"] = json!(deleted);
    }
    if !untracked.is_empty() {
        result["untracked"] = json!(untracked);
    }
    if !renamed.is_empty() {
        result["renamed"] = json!(renamed);
    }
    Ok(result)
}

async fn git_log(
    runner: &GitRun<'_>,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let limit = args
        .and_then(|a| a.get("limit").or_else(|| a.get("n")))
        .and_then(|v| v.as_i64())
        .unwrap_or(10)
        .clamp(1, 100);

    let oneline = args
        .and_then(|a| a.get("oneline"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let path_filter = args
        .and_then(|a| a.get("path").or_else(|| a.get("g")))
        .and_then(|v| v.as_str())
        .map(String::from);

    let limit_str = format!("-{limit}");

    if oneline {
        let format_str = "--format=%h %s".to_string();
        let mut git_args = vec!["log", &limit_str, &format_str];

        let path_owned;
        if let Some(p) = &path_filter {
            path_owned = p.clone();
            git_args.push("--");
            git_args.push(&path_owned);
        }

        let out = runner.text(&git_args).await?;
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        return Ok(json!({
            "log": lines,
            "count": lines.len(),
        }));
    }

    let format = "%h%x00%an%x00%s%x00%aI";
    let format_str = format!("--format={format}");
    let mut git_args = vec!["log", &limit_str, &format_str];

    let path_owned;
    if let Some(p) = &path_filter {
        path_owned = p.clone();
        git_args.push("--");
        git_args.push(&path_owned);
    }

    let out = runner.text(&git_args).await?;

    let commits: Vec<serde_json::Value> = out
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(4, '\0').collect();
            if parts.len() == 4 {
                Some(json!({
                    "h": parts[0],
                    "a": parts[1],
                    "m": parts[2],
                    "d": parts[3],
                }))
            } else {
                None
            }
        })
        .collect();

    Ok(json!({
        "commits": commits,
        "count": commits.len(),
    }))
}

async fn git_diff(
    runner: &GitRun<'_>,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let mode = args
        .and_then(|a| a.get("mode").or_else(|| a.get("g")))
        .and_then(|v| v.as_str())
        .unwrap_or("unstaged");
    let staged = mode == "staged";

    let mut numstat_args = vec!["diff", "--numstat"];
    if staged {
        numstat_args.push("--cached");
    }

    let out = runner.text(&numstat_args).await?;

    let mut full_args = vec!["diff", "-U3"];
    if staged {
        full_args.push("--cached");
    }

    let full_out = runner.text(&full_args).await?;

    let files = parse_numstat(&out);
    let hunks = parse_unified_diff(&full_out);

    Ok(json!({
        "staged": staged,
        "files": files,
        "hunks": hunks,
        "file_count": files.len(),
    }))
}

fn parse_numstat(output: &str) -> Vec<serde_json::Value> {
    output
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() == 3 {
                let added = parts[0].parse::<i64>().unwrap_or(-1);
                let removed = parts[1].parse::<i64>().unwrap_or(-1);
                Some(json!({
                    "f": parts[2],
                    "+": added,
                    "-": removed,
                }))
            } else {
                None
            }
        })
        .collect()
}

fn parse_unified_diff(output: &str) -> Vec<serde_json::Value> {
    let mut hunks = Vec::new();
    let mut current_file = String::new();
    let mut current_changes: Vec<serde_json::Value> = Vec::new();
    let mut hunk_header = String::new();

    for line in output.lines() {
        if line.starts_with("diff --git") {
            continue;
        } else if let Some(f) = line
            .strip_prefix("--- a/")
            .or_else(|| line.strip_prefix("+++ b/"))
        {
            current_file = f.to_string();
        } else if line.starts_with("@@") {
            if !current_changes.is_empty() {
                hunks.push(json!({
                    "f": current_file,
                    "h": hunk_header,
                    "c": current_changes,
                }));
                current_changes = Vec::new();
            }
            hunk_header = line.to_string();
        } else if let Some(v) = line.strip_prefix('+') {
            current_changes.push(json!({"t": "+", "v": v}));
        } else if let Some(v) = line.strip_prefix('-') {
            current_changes.push(json!({"t": "-", "v": v}));
        }
    }

    if !current_changes.is_empty() {
        hunks.push(json!({
            "f": current_file,
            "h": hunk_header,
            "c": current_changes,
        }));
    }

    hunks
}

async fn git_branch(runner: &GitRun<'_>) -> Result<serde_json::Value, String> {
    let current = runner
        .text(&["rev-parse", "--abbrev-ref", "HEAD"])
        .await?
        .trim()
        .to_string();

    let out = runner
        .text(&["branch", "--format=%(refname:short)"])
        .await?;
    let branches: Vec<&str> = out.lines().collect();

    Ok(json!({
        "current": current,
        "branches": branches,
        "count": branches.len(),
    }))
}

async fn git_add(
    runner: &GitRun<'_>,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let paths: Vec<String> = args
        .and_then(|a| a.get("paths"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_else(|| vec![".".to_string()]);

    let mut git_args: Vec<&str> = vec!["add"];
    for p in &paths {
        git_args.push(p.as_str());
    }

    runner.text(&git_args).await?;

    let status = git_status(runner).await?;
    Ok(json!({
        "added_paths": paths,
        "status": status,
    }))
}

async fn git_commit(
    runner: &GitRun<'_>,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let message = args
        .and_then(|a| a.get("message").or_else(|| a.get("m")))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "commit requires 'message' argument".to_string())?;

    let mut git_args = vec!["commit", "-m", message];

    if args
        .and_then(|a| a.get("all"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        git_args.insert(1, "-a");
    }

    let output = runner.output(&git_args).await?;
    let stdout = output.stdout;
    let stderr = output.stderr;

    if !output.status.success() {
        return Err(format!("git commit: {}", stderr.trim()));
    }

    let hash = runner
        .text(&["rev-parse", "--short", "HEAD"])
        .await
        .unwrap_or_default()
        .trim()
        .to_string();

    Ok(json!({
        "hash": hash,
        "message": message,
        "output": stdout.trim(),
    }))
}

async fn git_push(
    runner: &GitRun<'_>,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let remote = args
        .and_then(|a| a.get("remote"))
        .and_then(|v| v.as_str())
        .unwrap_or("origin");

    let branch = args.and_then(|a| a.get("branch")).and_then(|v| v.as_str());

    let mut git_args = vec!["push", remote];

    let branch_owned;
    if let Some(b) = branch {
        branch_owned = b.to_string();
        git_args.push(&branch_owned);
    }

    if args
        .and_then(|a| a.get("set_upstream"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        git_args.insert(1, "-u");
    }

    let output = runner.output(&git_args).await?;
    let stdout = output.stdout;
    let stderr = output.stderr;

    if !output.status.success() {
        return Err(format!("git push: {}", stderr.trim()));
    }

    Ok(json!({
        "remote": remote,
        "output": format!("{}{}", stdout.trim(), stderr.trim()),
    }))
}

async fn git_pull(
    runner: &GitRun<'_>,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let remote = args
        .and_then(|a| a.get("remote"))
        .and_then(|v| v.as_str())
        .unwrap_or("origin");

    let branch = args.and_then(|a| a.get("branch")).and_then(|v| v.as_str());

    let mut git_args = vec!["pull", remote];

    let branch_owned;
    if let Some(b) = branch {
        branch_owned = b.to_string();
        git_args.push(&branch_owned);
    }

    if args
        .and_then(|a| a.get("rebase"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        git_args.insert(1, "--rebase");
    }

    let output = runner.output(&git_args).await?;
    let stdout = output.stdout;
    let stderr = output.stderr;

    if !output.status.success() {
        return Err(format!("git pull: {}", stderr.trim()));
    }

    Ok(json!({
        "remote": remote,
        "output": stdout.trim(),
        "stderr": stderr.trim(),
    }))
}

async fn git_checkout(
    runner: &GitRun<'_>,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let target = args
        .and_then(|a| a.get("branch").or_else(|| a.get("ref")))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "checkout requires 'branch' argument".to_string())?;

    let create = args
        .and_then(|a| a.get("create"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut git_args = if create {
        vec!["checkout", "-b", target]
    } else {
        vec!["checkout", target]
    };

    // Allow checking out specific files
    let files: Option<Vec<String>> = args
        .and_then(|a| a.get("files"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        });

    let file_refs: Vec<&str>;
    if let Some(ref f) = files {
        git_args.push("--");
        file_refs = f.iter().map(|s| s.as_str()).collect();
        for fr in &file_refs {
            git_args.push(fr);
        }
    }

    let output = runner.output(&git_args).await?;
    let stderr = output.stderr;

    if !output.status.success() {
        return Err(format!("git checkout: {}", stderr.trim()));
    }

    let current = runner
        .text(&["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .unwrap_or_default()
        .trim()
        .to_string();

    Ok(json!({
        "branch": current,
        "created": create,
    }))
}

/// Check if git is available on PATH.
pub fn is_available() -> bool {
    std::process::Command::new("git")
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

    async fn setup_git_repo(dir: &Path) {
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(dir)
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir)
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir)
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(dir)
            .output()
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn plugin_status_clean() {
        let dir = tempfile::tempdir().unwrap();
        setup_git_repo(dir.path()).await;
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();

        let plugin = GitPlugin::new();
        let env = HashMap::new();
        let result = plugin
            .run_command("status", dir.path(), &env, None, None)
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.output["clean"], true);
        assert_eq!(result.output["branch"], "main");
        assert_eq!(result.output["head"]["m"], "init");
        assert!(result.output["head"]["h"].as_str().unwrap().len() <= 12);
        assert_eq!(result.output["commits"], 1);
    }

    #[tokio::test]
    async fn plugin_status_dirty() {
        let dir = tempfile::tempdir().unwrap();
        setup_git_repo(dir.path()).await;
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        std::fs::write(dir.path().join("f.txt"), "changed").unwrap();

        let plugin = GitPlugin::new();
        let env = HashMap::new();
        let result = plugin
            .run_command("status", dir.path(), &env, None, None)
            .await
            .unwrap();
        assert_eq!(result.output["clean"], false);
        assert!(result.output["modified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f.as_str().unwrap().contains("f.txt")));
    }

    #[tokio::test]
    async fn plugin_log_with_limit() {
        let dir = tempfile::tempdir().unwrap();
        setup_git_repo(dir.path()).await;
        for i in 0..5 {
            std::fs::write(dir.path().join("f.txt"), format!("v{i}")).unwrap();
            Command::new("git")
                .args(["add", "."])
                .current_dir(dir.path())
                .output()
                .await
                .unwrap();
            Command::new("git")
                .args(["commit", "-m", &format!("commit {i}")])
                .current_dir(dir.path())
                .output()
                .await
                .unwrap();
        }

        let plugin = GitPlugin::new();
        let env = HashMap::new();
        let args = json!({"limit": 2});
        let result = plugin
            .run_command("log", dir.path(), &env, None, Some(&args))
            .await
            .unwrap();
        assert_eq!(result.output["count"], 2);
    }

    #[tokio::test]
    async fn plugin_log_oneline() {
        let dir = tempfile::tempdir().unwrap();
        setup_git_repo(dir.path()).await;
        for i in 0..3 {
            std::fs::write(dir.path().join("f.txt"), format!("v{i}")).unwrap();
            Command::new("git")
                .args(["add", "."])
                .current_dir(dir.path())
                .output()
                .await
                .unwrap();
            Command::new("git")
                .args(["commit", "-m", &format!("commit {i}")])
                .current_dir(dir.path())
                .output()
                .await
                .unwrap();
        }

        let plugin = GitPlugin::new();
        let env = HashMap::new();
        let args = json!({"oneline": true});
        let result = plugin
            .run_command("log", dir.path(), &env, None, Some(&args))
            .await
            .unwrap();
        let log = result.output["log"].as_array().unwrap();
        assert_eq!(result.output["count"], 3);
        assert!(log[0].as_str().unwrap().contains("commit 2"));
        assert!(log[2].as_str().unwrap().contains("commit 0"));
    }

    #[tokio::test]
    async fn plugin_branch_current() {
        let dir = tempfile::tempdir().unwrap();
        setup_git_repo(dir.path()).await;
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();

        let plugin = GitPlugin::new();
        let env = HashMap::new();
        let result = plugin
            .run_command("branch", dir.path(), &env, None, None)
            .await
            .unwrap();
        assert_eq!(result.output["current"], "main");
    }

    #[tokio::test]
    async fn plugin_via_registry() {
        let dir = tempfile::tempdir().unwrap();
        setup_git_repo(dir.path()).await;
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();

        let registry = ToolRegistry::new();
        registry.register(Arc::new(GitPlugin::new())).await;

        let env = HashMap::new();
        let result = registry
            .run("git", "status", dir.path(), &env, None, None)
            .await
            .unwrap();
        assert_eq!(result.output["clean"], true);
        assert_eq!(result.tool, "git");
    }

    #[tokio::test]
    async fn plugin_not_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = GitPlugin::new();
        let env = HashMap::new();
        let result = plugin
            .run_command("status", dir.path(), &env, None, None)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn plugin_add_stages_files() {
        let dir = tempfile::tempdir().unwrap();
        setup_git_repo(dir.path()).await;
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        std::fs::write(dir.path().join("b.txt"), "world").unwrap();

        let plugin = GitPlugin::new();
        let env = HashMap::new();
        let args = json!({"paths": ["a.txt"]});
        let result = plugin
            .run_command("add", dir.path(), &env, None, Some(&args))
            .await
            .unwrap();

        let status = &result.output["status"];
        assert!(status["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f.as_str().unwrap() == "a.txt"));
        assert!(status["untracked"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f.as_str().unwrap() == "b.txt"));
    }

    #[tokio::test]
    async fn plugin_add_defaults_to_all() {
        let dir = tempfile::tempdir().unwrap();
        setup_git_repo(dir.path()).await;
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();

        let plugin = GitPlugin::new();
        let env = HashMap::new();
        let result = plugin
            .run_command("add", dir.path(), &env, None, None)
            .await
            .unwrap();

        let status = &result.output["status"];
        assert!(
            status.get("untracked").is_none(),
            "untracked should be omitted when empty"
        );
        assert!(status["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f.as_str().unwrap() == "a.txt"));
    }

    #[tokio::test]
    async fn plugin_commit_creates_commit() {
        let dir = tempfile::tempdir().unwrap();
        setup_git_repo(dir.path()).await;
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();

        let plugin = GitPlugin::new();
        let env = HashMap::new();
        let args = json!({"message": "test commit"});
        let result = plugin
            .run_command("commit", dir.path(), &env, None, Some(&args))
            .await
            .unwrap();

        assert!(!result.output["hash"].as_str().unwrap().is_empty());
        assert_eq!(result.output["message"], "test commit");
    }

    #[tokio::test]
    async fn plugin_log_uses_short_hash() {
        let dir = tempfile::tempdir().unwrap();
        setup_git_repo(dir.path()).await;
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();

        let plugin = GitPlugin::new();
        let env = HashMap::new();
        let result = plugin
            .run_command("log", dir.path(), &env, None, None)
            .await
            .unwrap();

        let commits = result.output["commits"].as_array().unwrap();
        assert!(!commits.is_empty());
        let hash = commits[0]["h"].as_str().unwrap();
        assert!(hash.len() <= 12, "should be a short hash, got: {hash}");
        assert!(
            commits[0].get("a").is_some(),
            "should have author field 'a'"
        );
        assert!(
            commits[0].get("m").is_some(),
            "should have message field 'm'"
        );
        assert!(commits[0].get("d").is_some(), "should have date field 'd'");
    }

    #[tokio::test]
    async fn plugin_status_clean_omits_empty_arrays() {
        let dir = tempfile::tempdir().unwrap();
        setup_git_repo(dir.path()).await;
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();

        let plugin = GitPlugin::new();
        let env = HashMap::new();
        let result = plugin
            .run_command("status", dir.path(), &env, None, None)
            .await
            .unwrap();
        assert_eq!(result.output["clean"], true);
        assert!(
            result.output.get("modified").is_none(),
            "empty arrays should be omitted"
        );
        assert!(
            result.output.get("added").is_none(),
            "empty arrays should be omitted"
        );
        // branch and head should always be present
        assert!(result.output.get("branch").is_some());
        assert!(result.output.get("head").is_some());
        assert!(result.output.get("commits").is_some());
    }

    #[tokio::test]
    async fn plugin_commit_requires_message() {
        let dir = tempfile::tempdir().unwrap();
        setup_git_repo(dir.path()).await;
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();

        let plugin = GitPlugin::new();
        let env = HashMap::new();
        let result = plugin
            .run_command("commit", dir.path(), &env, None, None)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn plugin_commit_all_flag() {
        let dir = tempfile::tempdir().unwrap();
        setup_git_repo(dir.path()).await;
        std::fs::write(dir.path().join("f.txt"), "v1").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();

        std::fs::write(dir.path().join("f.txt"), "v2").unwrap();

        let plugin = GitPlugin::new();
        let env = HashMap::new();
        let args = json!({"message": "auto-stage", "all": true});
        let result = plugin
            .run_command("commit", dir.path(), &env, None, Some(&args))
            .await
            .unwrap();

        assert!(!result.output["hash"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn plugin_checkout_creates_branch() {
        let dir = tempfile::tempdir().unwrap();
        setup_git_repo(dir.path()).await;
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();

        let plugin = GitPlugin::new();
        let env = HashMap::new();
        let args = json!({"branch": "feature-x", "create": true});
        let result = plugin
            .run_command("checkout", dir.path(), &env, None, Some(&args))
            .await
            .unwrap();

        assert_eq!(result.output["branch"], "feature-x");
        assert_eq!(result.output["created"], true);
    }

    #[tokio::test]
    async fn plugin_checkout_switches_branch() {
        let dir = tempfile::tempdir().unwrap();
        setup_git_repo(dir.path()).await;
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["checkout", "-b", "other"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["checkout", "main"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();

        let plugin = GitPlugin::new();
        let env = HashMap::new();
        let args = json!({"branch": "other"});
        let result = plugin
            .run_command("checkout", dir.path(), &env, None, Some(&args))
            .await
            .unwrap();

        assert_eq!(result.output["branch"], "other");
        assert_eq!(result.output["created"], false);
    }

    #[tokio::test]
    async fn plugin_checkout_requires_branch() {
        let dir = tempfile::tempdir().unwrap();
        setup_git_repo(dir.path()).await;

        let plugin = GitPlugin::new();
        let env = HashMap::new();
        let result = plugin
            .run_command("checkout", dir.path(), &env, None, None)
            .await;
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn structured_git_rejects_managed_capture_truncation() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("git");
        std::fs::write(
            &fake,
            "#!/bin/sh\n/usr/bin/python3 -c 'print(\"x\" * 1000)'\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut env = HashMap::new();
        env.insert("PATH".into(), dir.path().display().to_string());
        let cfg = crate::config::ProcessConfig {
            output_memory_bytes: 32,
            ..crate::config::ProcessConfig::default()
        };
        let error = GitPlugin::new()
            .run_command_with_config("status", dir.path(), &env, None, None, &cfg)
            .await
            .unwrap_err();
        assert!(error.contains("output exceeded"));
    }

    #[tokio::test]
    async fn is_available_returns_true() {
        assert!(is_available());
    }
}
