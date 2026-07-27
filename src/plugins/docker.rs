use std::collections::HashMap;
use std::path::Path;

use serde_json::json;
use tokio::process::Command;

use crate::tool_runner::{ToolCommand, ToolDescriptor, ToolPlugin, ToolResult};

pub struct DockerPlugin {
    descriptor: ToolDescriptor,
}

impl DockerPlugin {
    pub fn new() -> Self {
        let mut commands = HashMap::new();
        for name in [
            "ps",
            "logs",
            "exec",
            "images",
            "inspect",
            "stop",
            "compose_up",
            "compose_down",
            "compose_ps",
        ] {
            commands.insert(
                name.to_string(),
                ToolCommand {
                    bin: "docker".into(),
                    args: vec![name.into()],
                    output: "structured".into(),
                },
            );
        }

        Self {
            descriptor: ToolDescriptor {
                id: "docker".into(),
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
impl ToolPlugin for DockerPlugin {
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
            "ps" => docker_ps(cwd).await?,
            "logs" => docker_logs(cwd, args).await?,
            "exec" => docker_exec(cwd, args).await?,
            "images" => docker_images(cwd).await?,
            "inspect" => docker_inspect(cwd, args).await?,
            "stop" => docker_stop(cwd, args).await?,
            "compose_up" => docker_compose_up(cwd, args).await?,
            "compose_down" => docker_compose_down(cwd, args).await?,
            "compose_ps" => docker_compose_ps(cwd, args).await?,
            _ => return Err(format!("unknown docker command: {command}")),
        };

        Ok(ToolResult {
            tool: "docker".into(),
            command: command.into(),
            exit_code: 0,
            output,
            stderr: String::new(),
        })
    }
}

async fn run_docker(cwd: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("docker")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| format!("docker exec: {e}"))
}

/// Parse docker's line-delimited JSON output into an array.
fn parse_json_lines(stdout: &str) -> serde_json::Value {
    let items: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    json!(items)
}

async fn docker_ps(cwd: &Path) -> Result<serde_json::Value, String> {
    let output = run_docker(cwd, &["ps", "--format", "{{json .}}"]).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("docker ps: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let containers = parse_json_lines(&stdout);
    Ok(json!({"containers": containers}))
}

async fn docker_logs(
    cwd: &Path,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let container = args
        .and_then(|a| a.get("container"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "logs requires 'container' argument".to_string())?;

    let tail = args
        .and_then(|a| a.get("tail"))
        .and_then(|v| v.as_i64())
        .unwrap_or(50);

    let tail_str = tail.to_string();
    let output = run_docker(cwd, &["logs", "--tail", &tail_str, container]).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("docker logs: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Docker often writes logs to stderr (for attached containers)
    let combined = if stdout.is_empty() {
        stderr.to_string()
    } else {
        stdout.to_string()
    };

    Ok(json!({"logs": combined.trim_end()}))
}

async fn docker_exec(
    cwd: &Path,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let container = args
        .and_then(|a| a.get("container"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "exec requires 'container' argument".to_string())?;

    let command = args
        .and_then(|a| a.get("command"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "exec requires 'command' argument".to_string())?;

    let output = Command::new("docker")
        .args(["exec", container, "sh", "-c", command])
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| format!("docker exec: {e}"))?;

    let exit = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut result = json!({
        "exit": exit,
        "out": stdout.trim_end(),
    });
    if !stderr.is_empty() {
        result["err"] = json!(stderr.trim_end());
    }
    Ok(result)
}

async fn docker_images(cwd: &Path) -> Result<serde_json::Value, String> {
    let output = run_docker(cwd, &["images", "--format", "{{json .}}"]).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("docker images: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let images = parse_json_lines(&stdout);
    Ok(json!({"images": images}))
}

async fn docker_inspect(
    cwd: &Path,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let container = args
        .and_then(|a| a.get("container"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "inspect requires 'container' argument".to_string())?;

    let output = run_docker(cwd, &["inspect", "--format", "{{json .}}", container]).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("docker inspect: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).unwrap_or_else(|_| json!({"raw": stdout.trim()}));

    Ok(parsed)
}

async fn docker_stop(
    cwd: &Path,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let container = args
        .and_then(|a| a.get("container"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "stop requires 'container' argument".to_string())?;

    let output = run_docker(cwd, &["stop", container]).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("docker stop: {}", stderr.trim()));
    }

    Ok(json!({"stopped": container}))
}

async fn docker_compose_up(
    cwd: &Path,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let file = args.and_then(|a| a.get("file")).and_then(|v| v.as_str());

    let detach = args
        .and_then(|a| a.get("detach"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let mut docker_args: Vec<&str> = vec!["compose"];

    let file_owned;
    if let Some(f) = file {
        file_owned = f.to_string();
        docker_args.push("-f");
        docker_args.push(&file_owned);
    }

    docker_args.push("up");
    if detach {
        docker_args.push("-d");
    }

    let output = Command::new("docker")
        .args(&docker_args)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| format!("docker compose up: {e}"))?;

    let exit = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut result = json!({
        "exit": exit,
        "out": stdout.trim_end(),
    });
    if !stderr.is_empty() {
        result["err"] = json!(stderr.trim_end());
    }
    Ok(result)
}

async fn docker_compose_down(
    cwd: &Path,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let file = args.and_then(|a| a.get("file")).and_then(|v| v.as_str());

    let mut docker_args: Vec<&str> = vec!["compose"];

    let file_owned;
    if let Some(f) = file {
        file_owned = f.to_string();
        docker_args.push("-f");
        docker_args.push(&file_owned);
    }

    docker_args.push("down");

    let output = Command::new("docker")
        .args(&docker_args)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| format!("docker compose down: {e}"))?;

    let exit = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut result = json!({
        "exit": exit,
        "out": stdout.trim_end(),
    });
    if !stderr.is_empty() {
        result["err"] = json!(stderr.trim_end());
    }
    Ok(result)
}

async fn docker_compose_ps(
    cwd: &Path,
    args: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let file = args.and_then(|a| a.get("file")).and_then(|v| v.as_str());

    let mut docker_args: Vec<&str> = vec!["compose"];

    let file_owned;
    if let Some(f) = file {
        file_owned = f.to_string();
        docker_args.push("-f");
        docker_args.push(&file_owned);
    }

    docker_args.push("ps");
    docker_args.push("--format");
    docker_args.push("json");

    let output = Command::new("docker")
        .args(&docker_args)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| format!("docker compose ps: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("docker compose ps: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let services = parse_json_lines(&stdout);
    Ok(json!({"services": services}))
}

/// Check if docker is available on PATH.
pub fn is_available() -> bool {
    std::process::Command::new("docker")
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
    fn is_available_returns_bool() {
        // docker may or may not be installed — just verify it doesn't panic
        let _ = is_available();
    }

    #[tokio::test]
    async fn unknown_command_returns_error() {
        let plugin = DockerPlugin::new();
        let env = HashMap::new();
        let result = plugin
            .run_command("nonexistent", Path::new("/tmp"), &env, None, None)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown docker command"));
    }

    #[tokio::test]
    async fn plugin_registers_in_registry() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(DockerPlugin::new())).await;

        let tools = registry.list().await;
        assert!(tools.iter().any(|t| t.id == "docker"));

        let plugin = registry.get("docker").await;
        assert!(plugin.is_some());
    }

    #[test]
    fn descriptor_has_all_commands() {
        let plugin = DockerPlugin::new();
        let desc = plugin.descriptor();
        assert_eq!(desc.id, "docker");
        let expected = [
            "ps",
            "logs",
            "exec",
            "images",
            "inspect",
            "stop",
            "compose_up",
            "compose_down",
            "compose_ps",
        ];
        for cmd in &expected {
            assert!(desc.commands.contains_key(*cmd), "missing command: {cmd}");
        }
    }

    #[test]
    fn parse_json_lines_empty() {
        let result = parse_json_lines("");
        assert_eq!(result, json!([]));
    }

    #[test]
    fn parse_json_lines_multiple() {
        let input = r#"{"name":"a","status":"running"}
{"name":"b","status":"exited"}
"#;
        let result = parse_json_lines(input);
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["name"], "a");
        assert_eq!(arr[1]["name"], "b");
    }

    #[test]
    fn parse_json_lines_skips_invalid() {
        let input = r#"{"name":"a"}
not json
{"name":"b"}
"#;
        let result = parse_json_lines(input);
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[tokio::test]
    async fn logs_requires_container() {
        let plugin = DockerPlugin::new();
        let env = HashMap::new();
        let args = json!({});
        let result = plugin
            .run_command("logs", Path::new("/tmp"), &env, None, Some(&args))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("container"));
    }

    #[tokio::test]
    async fn exec_requires_container_and_command() {
        let plugin = DockerPlugin::new();
        let env = HashMap::new();

        let result = plugin
            .run_command("exec", Path::new("/tmp"), &env, None, Some(&json!({})))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("container"));

        let result = plugin
            .run_command(
                "exec",
                Path::new("/tmp"),
                &env,
                None,
                Some(&json!({"container": "foo"})),
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("command"));
    }

    #[tokio::test]
    async fn inspect_requires_container() {
        let plugin = DockerPlugin::new();
        let env = HashMap::new();
        let result = plugin
            .run_command("inspect", Path::new("/tmp"), &env, None, Some(&json!({})))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("container"));
    }

    #[tokio::test]
    async fn stop_requires_container() {
        let plugin = DockerPlugin::new();
        let env = HashMap::new();
        let result = plugin
            .run_command("stop", Path::new("/tmp"), &env, None, Some(&json!({})))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("container"));
    }
}
