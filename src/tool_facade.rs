use serde_json::Value;
use std::path::Path;

use crate::ops;
use crate::protocol::Response;
use crate::session::Session;
use crate::tools::{self, ToolTier};

/// Provider-neutral tool schema — what the agent loop hands to the LLM.
#[allow(dead_code)]
pub struct NeutralToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Tool schemas visible to the agent loop for a given workspace.
///
/// Returns Full + Terse tools whose context_check passes. OnDemand tools are
/// excluded (they must be activated explicitly, same as in the MCP path).
#[allow(dead_code)]
pub fn active_schemas(workspace: &Path) -> Vec<NeutralToolSchema> {
    tools::all_tools()
        .into_iter()
        .filter(|t| t.tier != ToolTier::OnDemand)
        .filter(|t| tools::passes_context_check(t.name, workspace))
        .map(|t| NeutralToolSchema {
            name: t.name.to_string(),
            description: t.description.to_string(),
            input_schema: t.schema.clone(),
        })
        .collect()
}

/// Dispatch an ops-backed tool call directly (no MCP layer).
///
/// Returns `Some(Response)` for tools that have an opcode mapping
/// (`to_request` is set). Returns `None` for plugin/special tools — the
/// caller (MCP adapter or agent loop) must handle those itself.
pub async fn invoke(session: &mut Session, name: &str, args: &Value) -> Option<Response> {
    let req = tools::build_request(name, args)?;
    let response = match req {
        Ok(r) => ops::dispatch(session, r).await,
        Err(e) => Response::err(3, &e),
    };
    Some(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use serde_json::json;
    use std::sync::Arc;

    fn session_in(dir: &std::path::Path) -> Session {
        Session::new(dir.to_path_buf(), Arc::new(Config::default()))
    }

    // --- active_schemas ---

    #[test]
    fn active_schemas_is_non_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!active_schemas(dir.path()).is_empty());
    }

    #[test]
    fn active_schemas_excludes_on_demand_tools() {
        let dir = tempfile::tempdir().unwrap();
        let on_demand: Vec<_> = tools::all_tools()
            .into_iter()
            .filter(|t| t.tier == ToolTier::OnDemand)
            .map(|t| t.name)
            .collect();
        let names: Vec<_> = active_schemas(dir.path())
            .iter()
            .map(|s| s.name.clone())
            .collect();
        for od in on_demand {
            assert!(!names.contains(&od.to_string()), "OnDemand tool {od} leaked into active_schemas");
        }
    }

    #[test]
    fn active_schemas_fields_populated() {
        let dir = tempfile::tempdir().unwrap();
        for schema in active_schemas(dir.path()) {
            assert!(!schema.name.is_empty(), "empty name");
            assert!(!schema.description.is_empty(), "empty description for {}", schema.name);
            assert!(schema.input_schema.is_object(), "non-object schema for {}", schema.name);
        }
    }

    #[test]
    fn active_schemas_no_duplicate_names() {
        let dir = tempfile::tempdir().unwrap();
        let schemas = active_schemas(dir.path());
        let mut seen = std::collections::HashSet::new();
        for s in &schemas {
            assert!(seen.insert(s.name.clone()), "duplicate tool name: {}", s.name);
        }
    }

    #[test]
    fn active_schemas_excludes_context_filtered_tools_without_git() {
        let dir = tempfile::tempdir().unwrap();
        // No .git dir — git tool should not appear
        let names: Vec<_> = active_schemas(dir.path())
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert!(!names.contains(&"git".to_string()));
    }

    #[test]
    fn active_schemas_includes_git_when_git_dir_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let names: Vec<_> = active_schemas(dir.path())
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert!(names.contains(&"git".to_string()));
    }

    #[test]
    fn active_schemas_excludes_cargo_without_cargo_toml() {
        let dir = tempfile::tempdir().unwrap();
        let names: Vec<_> = active_schemas(dir.path())
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert!(!names.contains(&"cargo".to_string()));
    }

    #[test]
    fn active_schemas_includes_cargo_when_cargo_toml_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        let names: Vec<_> = active_schemas(dir.path())
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert!(names.contains(&"cargo".to_string()));
    }

    #[test]
    fn active_schemas_excludes_docker_without_dockerfile() {
        let dir = tempfile::tempdir().unwrap();
        let names: Vec<_> = active_schemas(dir.path())
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert!(!names.contains(&"docker".to_string()));
    }

    #[test]
    fn active_schemas_includes_docker_when_dockerfile_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Dockerfile"), "FROM ubuntu").unwrap();
        let names: Vec<_> = active_schemas(dir.path())
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert!(names.contains(&"docker".to_string()));
    }

    #[test]
    fn active_schemas_includes_docker_when_compose_yml_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("docker-compose.yml"), "version: '3'").unwrap();
        let names: Vec<_> = active_schemas(dir.path())
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert!(names.contains(&"docker".to_string()));
    }

    #[test]
    fn active_schemas_includes_docker_when_compose_yaml_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("docker-compose.yaml"), "version: '3'").unwrap();
        let names: Vec<_> = active_schemas(dir.path())
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert!(names.contains(&"docker".to_string()));
    }

    // --- invoke ---

    #[tokio::test]
    async fn invoke_returns_none_for_special_tools() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_in(dir.path());
        // git, docker, set_cwd have no to_request mapping → None
        assert!(invoke(&mut session, "git", &json!({})).await.is_none());
        assert!(invoke(&mut session, "docker", &json!({})).await.is_none());
        assert!(invoke(&mut session, "set_cwd", &json!({})).await.is_none());
    }

    #[tokio::test]
    async fn invoke_returns_none_for_unknown_tool() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_in(dir.path());
        assert!(invoke(&mut session, "does_not_exist", &json!({})).await.is_none());
    }

    #[tokio::test]
    async fn invoke_dispatches_opcode_tool() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "hi").unwrap();
        let mut session = session_in(dir.path());
        let result = invoke(&mut session, "read_file", &json!({"path": "hello.txt"})).await;
        assert!(result.is_some());
        assert!(result.unwrap().ok);
    }

    #[tokio::test]
    async fn invoke_returns_error_response_for_bad_args() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_in(dir.path());
        // snapshot with invalid action → build_request returns Err → Response::err
        let result = invoke(&mut session, "snapshot", &json!({"action": "badaction"})).await;
        assert!(result.is_some());
        assert!(!result.unwrap().ok);
    }
}
