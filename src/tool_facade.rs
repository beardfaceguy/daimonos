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
/// Returns Full + Terse + AgentOnly tools whose context_check passes.
/// OnDemand tools are excluded (they must be activated explicitly).
#[allow(dead_code)]
pub fn active_schemas(
    workspace: &Path,
    descriptions: &crate::tool_descriptions::ToolDescriptions,
) -> Vec<NeutralToolSchema> {
    tools::all_tools()
        .into_iter()
        .filter(|t| {
            matches!(
                t.tier,
                ToolTier::Full | ToolTier::Terse | ToolTier::AgentOnly
            )
        })
        .filter(|t| tools::passes_context_check(t.name, workspace))
        .map(|t| NeutralToolSchema {
            name: t.name.to_string(),
            description: descriptions.full_or_name(t.name).to_string(),
            input_schema: descriptions.schema_with_parameters(t.name, &t.schema),
        })
        .collect()
}

/// Dispatch an ops-backed tool call directly (no MCP layer).
///
/// Returns `Some(Response)` for tools that have an opcode mapping
/// (`to_request` is set). Returns `None` for plugin/special tools — the
/// caller (MCP adapter or agent loop) must handle those itself.
pub async fn invoke(session: &mut Session, name: &str, args: &Value) -> Option<Response> {
    invoke_with_progress(session, name, args, None).await
}

pub async fn invoke_with_progress(
    session: &mut Session,
    name: &str,
    args: &Value,
    on_exec_progress: Option<&crate::ops::ExecProgressCallback<'_>>,
) -> Option<Response> {
    let req = tools::build_request(name, args)?;
    let response = match req {
        Ok(r) => ops::dispatch_with_progress(session, r, on_exec_progress).await,
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

    fn default_schemas(dir: &std::path::Path) -> Vec<NeutralToolSchema> {
        active_schemas(dir, &crate::tool_descriptions::ToolDescriptions::default())
    }

    // --- active_schemas ---

    /// The defect behind vikunja 1112: `active_schemas` is the catalog handed to
    /// the model, but the agent loop could only dispatch opcode-backed tools plus
    /// two special cases. Nineteen advertised tools therefore failed every call
    /// with "not available in agent mode".
    ///
    /// Advertising a capability and then refusing it is worse than not
    /// advertising it: the model cannot plan around it and burns a turn per
    /// discovery. This asserts every advertised tool is reachable by *some*
    /// agent-loop path, so the catalog and the dispatcher cannot drift apart
    /// again.
    #[tokio::test]
    async fn every_advertised_tool_is_dispatchable_by_the_agent_loop() {
        let dir = tempfile::tempdir().unwrap();
        let mut undispatchable = Vec::new();

        for schema in default_schemas(dir.path()) {
            let name = schema.name.as_str();

            // Path 1: opcode-backed, handled by this facade.
            if tools::has_opcode_mapping(name) {
                continue;
            }
            // Path 2: special-cased directly in the agent loop.
            if name == "execute_script" || name == crate::agent::UPDATE_PLAN_TOOL {
                continue;
            }
            // Path 3: plugin/meta tool shared with the MCP adapter. Arguments are
            // deliberately empty — a tool that rejects them still proves it is
            // wired, and `Some` vs `None` is exactly the reachability question.
            let mut session = session_in(dir.path());
            if crate::mcp::dispatch_local_tool(&mut session, name, &json!({}))
                .await
                .is_some()
            {
                continue;
            }

            undispatchable.push(schema.name.clone());
        }

        assert!(
            undispatchable.is_empty(),
            "tools advertised to the model that the agent loop cannot dispatch: {undispatchable:?}"
        );
    }

    /// The agent catalog must withhold `McpOnly` tools. Asserted explicitly
    /// because the invariant lives in a filter in this file while the tier is set
    /// in `tools.rs` — reviewers reading only the `tools.rs` side of a diff have
    /// twice flagged this as possibly broken, and nothing stated it outright.
    #[test]
    fn active_schemas_withholds_mcp_only_tools() {
        let dir = tempfile::tempdir().unwrap();
        let advertised: Vec<String> = default_schemas(dir.path())
            .iter()
            .map(|s| s.name.clone())
            .collect();

        let mcp_only: Vec<&str> = tools::all_tools()
            .into_iter()
            .filter(|t| t.tier == ToolTier::McpOnly)
            .map(|t| t.name)
            .collect();
        assert!(
            !mcp_only.is_empty(),
            "no McpOnly tools left; this test is now vacuous and should be removed"
        );

        for name in mcp_only {
            assert!(
                !advertised.contains(&name.to_string()),
                "{name} is McpOnly but was advertised to the agent"
            );
        }
    }

    /// The MCP side of the same tier: `McpOnly` must stay exposed there. Moving
    /// `batch` to the new tier initially dropped it out of `initial_exposed_tools`
    /// and so out of MCP `list_tools` entirely, which the Rust suite did not
    /// catch — only the Python MCP integration suite did.
    #[test]
    fn mcp_only_tools_remain_exposed_over_mcp() {
        let exposed = tools::initial_exposed_tools();
        for tool in tools::all_tools()
            .into_iter()
            .filter(|t| t.tier == ToolTier::McpOnly)
        {
            assert!(
                exposed.contains(tool.name),
                "{} is McpOnly but missing from the default MCP-exposed set",
                tool.name
            );
        }
    }

    #[test]
    fn active_schemas_is_non_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!default_schemas(dir.path()).is_empty());
    }

    #[test]
    fn active_schemas_includes_execute_script() {
        // execute_script is Full-tier, so it is always exposed to the model.
        // The agent loop now dispatches it (vikunja #1050), so it must stay in
        // the catalog the frontends build from active_schemas.
        let dir = tempfile::tempdir().unwrap();
        assert!(default_schemas(dir.path())
            .iter()
            .any(|schema| schema.name == "execute_script"));
    }

    #[test]
    fn active_schemas_include_agent_only_plan_tool() {
        let dir = tempfile::tempdir().unwrap();
        assert!(default_schemas(dir.path())
            .iter()
            .any(|schema| schema.name == crate::agent::UPDATE_PLAN_TOOL));
    }

    #[test]
    fn active_schemas_excludes_on_demand_tools() {
        let dir = tempfile::tempdir().unwrap();
        let on_demand: Vec<_> = tools::all_tools()
            .into_iter()
            .filter(|t| t.tier == ToolTier::OnDemand)
            .map(|t| t.name)
            .collect();
        let names: Vec<_> = default_schemas(dir.path())
            .iter()
            .map(|s| s.name.clone())
            .collect();
        for od in on_demand {
            assert!(
                !names.contains(&od.to_string()),
                "OnDemand tool {od} leaked into active_schemas"
            );
        }
    }

    #[test]
    fn active_schemas_fields_populated() {
        let dir = tempfile::tempdir().unwrap();
        for schema in default_schemas(dir.path()) {
            assert!(!schema.name.is_empty(), "empty name");
            assert!(
                !schema.description.is_empty(),
                "empty description for {}",
                schema.name
            );
            assert!(
                schema.input_schema.is_object(),
                "non-object schema for {}",
                schema.name
            );
        }
    }

    #[tokio::test]
    async fn active_schemas_use_runtime_description_override() {
        let dir = tempfile::tempdir().unwrap();
        let catalog_path = dir.path().join("tools.toml");
        tokio::fs::write(
            &catalog_path,
            "[read_file]\nfull = \"CUSTOM AGENT READ\"\n\n[read_file.parameters]\npath = \"CUSTOM AGENT PATH\"\n",
        )
        .await
        .unwrap();
        let descriptions = crate::tool_descriptions::ToolDescriptions::load(Some(
            catalog_path.to_string_lossy().as_ref(),
        ))
        .await;
        let read = active_schemas(dir.path(), &descriptions)
            .into_iter()
            .find(|schema| schema.name == "read_file")
            .unwrap();
        assert_eq!(read.description, "CUSTOM AGENT READ");
        assert_eq!(
            read.input_schema["properties"]["path"]["description"],
            "CUSTOM AGENT PATH"
        );
    }

    #[test]
    fn active_schemas_no_duplicate_names() {
        let dir = tempfile::tempdir().unwrap();
        let schemas = default_schemas(dir.path());
        let mut seen = std::collections::HashSet::new();
        for s in &schemas {
            assert!(
                seen.insert(s.name.clone()),
                "duplicate tool name: {}",
                s.name
            );
        }
    }

    #[test]
    fn active_schemas_excludes_context_filtered_tools_without_git() {
        let dir = tempfile::tempdir().unwrap();
        // No .git dir — git tool should not appear
        let names: Vec<_> = default_schemas(dir.path())
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert!(!names.contains(&"git".to_string()));
    }

    #[test]
    fn active_schemas_includes_git_when_git_dir_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let names: Vec<_> = default_schemas(dir.path())
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert!(names.contains(&"git".to_string()));
    }

    #[test]
    fn active_schemas_excludes_cargo_without_cargo_toml() {
        let dir = tempfile::tempdir().unwrap();
        let names: Vec<_> = default_schemas(dir.path())
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert!(!names.contains(&"cargo".to_string()));
    }

    #[test]
    fn active_schemas_includes_cargo_when_cargo_toml_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        let names: Vec<_> = default_schemas(dir.path())
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert!(names.contains(&"cargo".to_string()));
    }

    #[test]
    fn active_schemas_excludes_docker_without_dockerfile() {
        let dir = tempfile::tempdir().unwrap();
        let names: Vec<_> = default_schemas(dir.path())
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert!(!names.contains(&"docker".to_string()));
    }

    #[test]
    fn active_schemas_includes_docker_when_dockerfile_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Dockerfile"), "FROM ubuntu").unwrap();
        let names: Vec<_> = default_schemas(dir.path())
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert!(names.contains(&"docker".to_string()));
    }

    #[test]
    fn active_schemas_includes_docker_when_compose_yml_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("docker-compose.yml"), "version: '3'").unwrap();
        let names: Vec<_> = default_schemas(dir.path())
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert!(names.contains(&"docker".to_string()));
    }

    #[test]
    fn active_schemas_includes_docker_when_compose_yaml_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("docker-compose.yaml"), "version: '3'").unwrap();
        let names: Vec<_> = default_schemas(dir.path())
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
        assert!(invoke(&mut session, "does_not_exist", &json!({}))
            .await
            .is_none());
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
    async fn invoke_dispatches_coordination_tools_from_agent_loop() {
        // ADR-009 D5 / #1050: coordination tools must be reachable from the
        // internal agent loop via tool_facade::invoke (i.e. opcode-backed,
        // returning Some), NOT MCP-only special tools that return None.
        let dir = tempfile::tempdir().unwrap();
        let db_dir = dir.path().join("coorddb");
        let mut cfg = Config::default();
        cfg.coordination.db_dir = Some(db_dir.to_string_lossy().to_string());
        let mut session = Session::new(dir.path().to_path_buf(), Arc::new(cfg));

        let reg = invoke(
            &mut session,
            "register_agent",
            &json!({"name": "BlueLake", "program": "codex-cli"}),
        )
        .await;
        assert!(reg.is_some(), "register_agent must dispatch via the facade");
        assert!(reg.unwrap().ok);

        let list = invoke(&mut session, "list_agents", &json!({})).await;
        assert!(list.is_some(), "list_agents must dispatch via the facade");
        let resp = list.unwrap();
        assert!(resp.ok);
        assert_eq!(resp.d.unwrap()["agents"][0]["name"], "BlueLake");
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
