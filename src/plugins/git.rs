use std::collections::HashMap;
use std::path::Path;

use serde_json::json;
use tokio::process::Command;

use crate::tool_runner::{ToolCommand, ToolDescriptor, ToolPlugin, ToolResult};

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

    async fn run_command(
        &self,
        command: &str,
        cwd: &Path,
        _env: &HashMap<String, String>,
        _stdin_data: Option<&[u8]>,
        args: Option<&serde_json::Value>,
    ) -> Result<ToolResult, String> {
        let output = match command {
            "status" => git_status(cwd).await?,
            "log" => git_log(cwd, args).await?,
            "diff" => git_diff(cwd, args).await?,
            "branch" => git_branch(cwd).await?,
            "add" => git_add(cwd, args).await?,
            "commit" => git_commit(cwd, args).await?,
            "push" => git_push(cwd, args).await?,
            "pull" => git_pull(cwd, args).await?,
            "checkout" => git_checkout(cwd, args).await?,
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

async fn run_git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .map_err(|e| format!("git exec: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {}: {}", args[0], stderr.trim()));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

async fn git_status(cwd: &Path) -> Result<serde_json::Value, String> {
    let out = run_git(cwd, &["status", "--porcelain=v1", "-uall"]).await?;

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

    let mut result = json!({"clean": clean});
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
    cwd: &Path,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let limit = args
        .and_then(|a| a.get("limit").or_else(|| a.get("n")))
        .and_then(|v| v.as_i64())
        .unwrap_or(10)
        .clamp(1, 100);

    let path_filter = args
        .and_then(|a| a.get("path").or_else(|| a.get("g")))
        .and_then(|v| v.as_str())
        .map(String::from);

    let format = "%h%x00%an%x00%s%x00%aI";
    let limit_str = format!("-{limit}");
    let format_str = format!("--format={format}");
    let mut git_args = vec!["log", &limit_str, &format_str];

    let path_owned;
    if let Some(p) = &path_filter {
        path_owned = p.clone();
        git_args.push("--");
        git_args.push(&path_owned);
    }

    let out = run_git(cwd, &git_args).await?;

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
    cwd: &Path,
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

    let out = run_git(cwd, &numstat_args).await?;

    let mut full_args = vec!["diff", "-U3"];
    if staged {
        full_args.push("--cached");
    }

    let full_out = run_git(cwd, &full_args).await?;

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

async fn git_branch(cwd: &Path) -> Result<serde_json::Value, String> {
    let current = run_git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await?
        .trim()
        .to_string();

    let out = run_git(cwd, &["branch", "--format=%(refname:short)"]).await?;
    let branches: Vec<&str> = out.lines().collect();

    Ok(json!({
        "current": current,
        "branches": branches,
        "count": branches.len(),
    }))
}

async fn git_add(
    cwd: &Path,
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

    run_git(cwd, &git_args).await?;

    let status = git_status(cwd).await?;
    Ok(json!({
        "added_paths": paths,
        "status": status,
    }))
}

async fn git_commit(
    cwd: &Path,
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

    let output = Command::new("git")
        .args(&git_args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .map_err(|e| format!("git exec: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() {
        return Err(format!("git commit: {}", stderr.trim()));
    }

    let hash = run_git(cwd, &["rev-parse", "--short", "HEAD"])
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
    cwd: &Path,
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

    let output = Command::new("git")
        .args(&git_args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .map_err(|e| format!("git exec: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() {
        return Err(format!("git push: {}", stderr.trim()));
    }

    Ok(json!({
        "remote": remote,
        "output": format!("{}{}", stdout.trim(), stderr.trim()),
    }))
}

async fn git_pull(
    cwd: &Path,
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

    let output = Command::new("git")
        .args(&git_args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .map_err(|e| format!("git exec: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

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
    cwd: &Path,
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

    let output = Command::new("git")
        .args(&git_args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .map_err(|e| format!("git exec: {e}"))?;

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() {
        return Err(format!("git checkout: {}", stderr.trim()));
    }

    let current = run_git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
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
        assert!(commits[0].get("a").is_some(), "should have author field 'a'");
        assert!(commits[0].get("m").is_some(), "should have message field 'm'");
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

    #[tokio::test]
    async fn is_available_returns_true() {
        assert!(is_available());
    }
}
