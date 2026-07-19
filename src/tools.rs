use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;

use crate::protocol::{self, Op, Request};

/// Maps MCP args JSON to a protocol Request. Factored out of `ToolDef` so the
/// field type stays readable (clippy::type_complexity).
pub type ToRequestFn = fn(&Value) -> Result<Request, String>;

// --- Argument extraction helpers (used by to_request fns and mcp.rs) ---

pub fn get_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)?.as_str().map(String::from)
}

pub fn get_i64(args: &Value, key: &str) -> Option<i64> {
    args.get(key)?.as_i64()
}

pub fn get_str_array(args: &Value, key: &str) -> Option<Vec<String>> {
    args.get(key)?
        .as_array()?
        .iter()
        .map(|v| v.as_str().map(String::from))
        .collect()
}

pub fn get_str_map(args: &Value, key: &str) -> Option<std::collections::HashMap<String, String>> {
    let obj = args.get(key)?.as_object()?;
    let mut map = std::collections::HashMap::new();
    for (k, v) in obj {
        if let Some(s) = v.as_str() {
            map.insert(k.clone(), s.to_string());
        }
    }
    Some(map)
}

// --- Tool tier ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolTier {
    /// Always present with full JSON Schema in list_tools.
    Full,
    /// Name + description only in list_tools; full schema via get_tool_schema.
    Terse,
    /// Hidden until activated via list_all_tools.
    OnDemand,
}

// --- Tool definition ---

pub struct ToolDef {
    pub name: &'static str,
    pub tier: ToolTier,
    pub schema: Value,
    /// Maps MCP args JSON to a protocol Request. None for special tools
    /// that need session access or custom handling (git, set_cwd, etc.).
    pub to_request: Option<ToRequestFn>,
    /// If set, tool is omitted from list_tools when the check returns false.
    pub context_check: Option<fn(&Path) -> bool>,
}

// --- Registry ---

pub fn all_tools() -> Vec<ToolDef> {
    vec![
        // ===================== Tier 0: Full schema =====================
        ToolDef {
            name: "read_file",
            tier: ToolTier::Full,
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer"},
                    "limit": {"type": "integer"}
                },
                "required": ["path"]
            }),
            to_request: Some(|args| {
                Ok(Request::Single(Op {
                    c: protocol::op::READ,
                    p: get_str(args, "path"),
                    n: get_i64(args, "offset"),
                    n2: get_i64(args, "limit"),
                    ..Default::default()
                }))
            }),
            context_check: None,
        },
        ToolDef {
            name: "write_file",
            tier: ToolTier::Full,
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
            to_request: Some(|args| {
                Ok(Request::Single(Op {
                    c: protocol::op::WRITE,
                    p: get_str(args, "path"),
                    s: get_str(args, "content"),
                    ..Default::default()
                }))
            }),
            context_check: None,
        },
        ToolDef {
            name: "edit_file",
            tier: ToolTier::Full,
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "edits": {
                        "type": "array",
                        "items": {"type": "string"},
                    }
                },
                "required": ["path", "edits"]
            }),
            to_request: Some(|args| {
                Ok(Request::Single(Op {
                    c: protocol::op::PATCH,
                    p: get_str(args, "path"),
                    a: get_str_array(args, "edits"),
                    ..Default::default()
                }))
            }),
            context_check: None,
        },
        ToolDef {
            name: "search",
            tier: ToolTier::Full,
            schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "mode": {"type": "string", "enum": ["content", "files"]},
                    "path": {"type": "string"},
                    "glob": {"type": "string"},
                    "max_results": {"type": "integer"}
                },
                "required": ["pattern"]
            }),
            to_request: Some(|args| {
                let mode = get_str(args, "mode").unwrap_or_else(|| "content".into());
                if mode == "files" {
                    Ok(Request::Single(Op {
                        c: protocol::op::FIND,
                        p: get_str(args, "pattern"),
                        n: get_i64(args, "max_results"),
                        ..Default::default()
                    }))
                } else {
                    Ok(Request::Single(Op {
                        c: protocol::op::GREP,
                        p: get_str(args, "pattern"),
                        q: get_str(args, "path"),
                        g: get_str(args, "glob"),
                        n: get_i64(args, "max_results"),
                        ..Default::default()
                    }))
                }
            }),
            context_check: None,
        },
        ToolDef {
            name: "workspace_info",
            tier: ToolTier::Terse,
            schema: json!({"type": "object", "properties": {}}),
            to_request: None, // needs session.index access
            context_check: None,
        },
        ToolDef {
            name: "exec",
            tier: ToolTier::Full,
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "args": {"type": "array", "items": {"type": "string"}},
                    "cwd": {"type": "string"},
                    "env": {"type": "object", "additionalProperties": {"type": "string"}}
                },
                "required": ["command"]
            }),
            to_request: Some(|args| {
                Ok(Request::Single(Op {
                    c: protocol::op::EXEC,
                    s: get_str(args, "command"),
                    a: get_str_array(args, "args"),
                    q: get_str(args, "cwd"),
                    kv: get_str_map(args, "env"),
                    ..Default::default()
                }))
            }),
            context_check: None,
        },
        ToolDef {
            name: "batch",
            tier: ToolTier::Terse,
            schema: json!({
                "type": "object",
                "properties": {
                    "ops": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "tool": {"type": "string"},
                                "arguments": {"type": "object"}
                            },
                            "required": ["tool"]
                        }
                    }
                },
                "required": ["ops"]
            }),
            to_request: None, // meta-tool handled in mcp.rs
            context_check: None,
        },
        ToolDef {
            name: "get_tool_schema",
            tier: ToolTier::Full,
            schema: json!({
                "type": "object",
                "properties": {
                    "tools": {
                        "type": "array",
                        "items": {"type": "string"},
                    }
                },
                "required": ["tools"]
            }),
            to_request: None, // schema lookup handled in mcp.rs
            context_check: None,
        },
        ToolDef {
            name: "execute_script",
            tier: ToolTier::Full,
            schema: json!({
                "type": "object",
                "properties": {
                    "code": {"type": "string"},
                    "timeout": {"type": "integer"}
                },
                "required": ["code"]
            }),
            to_request: None, // Starlark runtime handled in mcp.rs
            context_check: None,
        },
        ToolDef {
            name: "kgl_query",
            tier: ToolTier::Full,
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "enum": ["index", "orient", "node", "neighbors", "find", "writers_of", "blast_radius", "open_questions", "check"]
                    },
                    "args": {
                        "type": "object",
                    }
                },
                "required": ["query"]
            }),
            to_request: None, // KGL store access handled in mcp.rs
            // #936 prefix diet: heaviest tool in the prefix and niche — only
            // list it where a KGL store exists. Still callable while hidden
            // (dispatch auto-activates), so `kgl_query index` bootstraps fine.
            context_check: Some(|ws| ws.join(".kgl").exists()),
        },
        ToolDef {
            name: "kgl_assert",
            tier: ToolTier::Full,
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["intent", "provenance", "declare_edge"]},
                    "args": {
                        "type": "object",
                    }
                },
                "required": ["action"]
            }),
            to_request: None, // KGL store access handled in mcp.rs
            context_check: Some(|ws| ws.join(".kgl").exists()),
        },
        ToolDef {
            name: "list_tool_signatures",
            tier: ToolTier::OnDemand,
            schema: json!({"type": "object", "properties": {}}),
            to_request: None, // returns static string
            context_check: None,
        },
        // ===================== Tier 1: Terse schema =====================
        ToolDef {
            name: "git",
            tier: ToolTier::Terse,
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["status", "log", "diff", "branch", "add", "commit", "push", "pull", "checkout"]},
                    "message": {"type": "string"},
                    "all": {"type": "boolean"},
                    "limit": {"type": "integer"},
                    "oneline": {"type": "boolean"},
                    "path": {"type": "string"},
                    "mode": {"type": "string", "enum": ["unstaged", "staged"]},
                    "paths": {"type": "array", "items": {"type": "string"}},
                    "branch": {"type": "string"},
                    "create": {"type": "boolean"},
                    "files": {"type": "array", "items": {"type": "string"}},
                    "remote": {"type": "string"},
                    "set_upstream": {"type": "boolean"},
                    "rebase": {"type": "boolean"}
                },
                "required": ["command"]
            }),
            to_request: None, // uses ToolRegistry plugin
            context_check: Some(|ws| ws.join(".git").exists()),
        },
        ToolDef {
            name: "cargo",
            tier: ToolTier::Terse,
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["test", "build", "check", "clippy", "fmt", "add"]},
                    "package": {"type": "string"},
                    "filter": {"type": "string"},
                    "lib": {"type": "boolean"},
                    "release": {"type": "boolean"},
                    "dev": {"type": "boolean"}
                },
                "required": ["command"]
            }),
            to_request: None, // uses ToolRegistry plugin
            context_check: Some(|ws| ws.join("Cargo.toml").exists()),
        },
        ToolDef {
            name: "pytest",
            tier: ToolTier::Terse,
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["run", "collect"]},
                    "path": {"type": "string"},
                    "filter": {"type": "string"},
                    "markers": {"type": "string"},
                    "verbose": {"type": "boolean"},
                    "failfast": {"type": "boolean"}
                },
                "required": ["command"]
            }),
            to_request: None, // uses ToolRegistry plugin
            context_check: Some(|ws| {
                ws.join("pytest.ini").exists()
                    || ws.join("pyproject.toml").exists()
                    || ws.join("setup.py").exists()
                    || ws.join("setup.cfg").exists()
                    || ws.join("tox.ini").exists()
                    || ws.join("conftest.py").exists()
                    || ws.join("tests").is_dir()
            }),
        },
        ToolDef {
            name: "gh",
            tier: ToolTier::Terse,
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["pr_view", "pr_list", "pr_create", "pr_diff", "pr_checks", "pr_merge", "pr_checkout", "run_list", "run_view", "issue_list", "issue_view", "issue_create", "issue_comment", "api", "raw"]},
                    "number": {"type": "integer"},
                    "state": {"type": "string", "enum": ["open", "closed", "merged", "all"]},
                    "limit": {"type": "integer"},
                    "author": {"type": "string"},
                    "title": {"type": "string"},
                    "body": {"type": "string"},
                    "base": {"type": "string"},
                    "draft": {"type": "boolean"},
                    "merge_method": {"type": "string", "enum": ["merge", "squash", "rebase"]},
                    "delete_branch": {"type": "boolean"},
                    "subject": {"type": "string"},
                    "branch": {"type": "string"},
                    "workflow": {"type": "string"},
                    "status": {"type": "string"},
                    "run_id": {"type": "integer"},
                    "label": {"type": "string"},
                    "args": {"type": "array", "items": {"type": "string"}},
                    "endpoint": {"type": "string"},
                    "method": {"type": "string"}
                },
                "required": ["command"]
            }),
            to_request: None,
            context_check: Some(|ws| ws.join(".git").exists()),
        },
        ToolDef {
            name: "docker",
            tier: ToolTier::Terse,
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["ps", "logs", "exec", "images", "inspect", "stop", "compose_up", "compose_down", "compose_ps"]},
                    "container": {"type": "string"},
                    "tail": {"type": "integer"},
                    "file": {"type": "string"},
                    "detach": {"type": "boolean"}
                },
                "required": ["command"]
            }),
            to_request: None,
            context_check: Some(|ws| {
                ws.join("Dockerfile").exists()
                    || ws.join("docker-compose.yml").exists()
                    || ws.join("docker-compose.yaml").exists()
            }),
        },
        ToolDef {
            name: "curl",
            tier: ToolTier::Terse,
            schema: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "method": {"type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"]},
                    "headers": {"type": "object"},
                    "body": {"type": "string"},
                    "timeout": {"type": "integer"}
                },
                "required": ["url"]
            }),
            to_request: None, // uses CurlPlugin via ToolRegistry
            context_check: None,
        },
        ToolDef {
            name: "shellcheck",
            tier: ToolTier::Terse,
            schema: json!({
                "type": "object",
                "properties": {
                    "file": {"type": "string"},
                    "files": {"type": "array", "items": {"type": "string"}},
                    "shell": {"type": "string", "enum": ["bash", "sh", "dash", "ksh"]}
                }
            }),
            to_request: None, // uses ShellcheckPlugin via ToolRegistry
            context_check: None,
        },
        ToolDef {
            name: "npm",
            tier: ToolTier::Terse,
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["install", "run", "test", "build", "audit"]},
                    "script": {"type": "string"}
                },
                "required": ["command"]
            }),
            to_request: None, // uses NpmPlugin via ToolRegistry
            context_check: None,
        },
        ToolDef {
            name: "discord",
            tier: ToolTier::Terse,
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["list_guilds", "list_channels", "read_messages", "search_messages"]},
                    "guild_id": {"type": "string"},
                    "channel_id": {"type": "string"},
                    "query": {"type": "string"},
                    "limit": {"type": "integer"},
                    "analytics_tag": {"type": "string"}
                },
                "required": ["command"]
            }),
            to_request: None,
            context_check: None,
        },
        ToolDef {
            name: "snapshot",
            tier: ToolTier::Terse,
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["create", "restore", "list", "delete"]},
                    "id": {"type": "string"},
                    "tag": {"type": "string"}
                },
                "required": ["action"]
            }),
            to_request: Some(|args| {
                let action = get_str(args, "action")
                    .ok_or_else(|| "snapshot requires 'action'".to_string())?;
                let opcode = match action.as_str() {
                    "create" => protocol::op::SNAP,
                    "restore" => protocol::op::RESTORE,
                    "list" => protocol::op::SNAP_LIST,
                    "delete" => protocol::op::SNAP_DELETE,
                    _ => return Err(format!("unknown snapshot action: {action}")),
                };
                let p = match action.as_str() {
                    "create" => get_str(args, "tag"),
                    "restore" | "delete" => get_str(args, "id"),
                    _ => None,
                };
                Ok(Request::Single(Op {
                    c: opcode,
                    p,
                    ..Default::default()
                }))
            }),
            context_check: None,
        },
        ToolDef {
            name: "set_cwd",
            tier: ToolTier::Terse,
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": ["path"]
            }),
            to_request: None, // mutates session directly
            context_check: None,
        },
        ToolDef {
            name: "ls",
            tier: ToolTier::Terse,
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "depth": {"type": "integer"},
                    "glob": {"type": "string"},
                    "type": {"type": "string", "enum": ["f", "d"]},
                    "all": {"type": "boolean"},
                    "stat": {"type": "boolean"}
                }
            }),
            to_request: Some(|args| {
                let flag = if args.get("stat").and_then(|v| v.as_bool()).unwrap_or(false) {
                    Some("stat".into())
                } else if args.get("all").and_then(|v| v.as_bool()).unwrap_or(false) {
                    Some("all".into())
                } else {
                    None
                };
                let type_filter = match args.get("type").and_then(|v| v.as_str()) {
                    Some("f") => Some(1),
                    Some("d") => Some(2),
                    _ => None,
                };
                Ok(Request::Single(Op {
                    c: protocol::op::LS,
                    p: get_str(args, "path"),
                    n: get_i64(args, "depth"),
                    q: get_str(args, "glob"),
                    n2: type_filter,
                    g: flag,
                    ..Default::default()
                }))
            }),
            context_check: None,
        },
        ToolDef {
            name: "session_stats",
            tier: ToolTier::Terse,
            schema: json!({
                "type": "object",
                "properties": {
                    "scope": {"type": "string", "enum": ["session", "history", "daily"]},
                    "days": {"type": "integer"},
                    "external_session_id": {"type": "string"}
                }
            }),
            to_request: None, // needs session.analytics access
            context_check: None,
        },
        ToolDef {
            name: "set_external_session_id",
            tier: ToolTier::Terse,
            schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"}
                },
                "required": ["id"]
            }),
            to_request: None, // mutates session directly
            context_check: None,
        },
        ToolDef {
            name: "set_verbosity",
            tier: ToolTier::Terse,
            schema: json!({
                "type": "object",
                "properties": {
                    "level": {"type": "string", "enum": ["full", "compact", "terse"]}
                },
                "required": ["level"]
            }),
            to_request: None, // mutates session directly
            context_check: None,
        },
        ToolDef {
            name: "list_all_tools",
            tier: ToolTier::Terse,
            schema: json!({"type": "object", "properties": {}}),
            to_request: None, // activates on-demand tools in session
            context_check: None,
        },
        // ===================== Tier 2: On-demand =====================
        ToolDef {
            name: "diff_files",
            tier: ToolTier::OnDemand,
            schema: json!({
                "type": "object",
                "properties": {
                    "path_a": {"type": "string"},
                    "path_b": {"type": "string"},
                    "content_b": {"type": "string"}
                },
                "required": ["path_a"]
            }),
            to_request: Some(|args| {
                Ok(Request::Single(Op {
                    c: protocol::op::DIFF,
                    p: get_str(args, "path_a"),
                    q: get_str(args, "path_b"),
                    s: get_str(args, "content_b"),
                    ..Default::default()
                }))
            }),
            context_check: None,
        },
        ToolDef {
            name: "tool_pipeline",
            tier: ToolTier::OnDemand,
            schema: json!({
                "type": "object",
                "properties": {
                    "tool_id": {"type": "string"},
                    "stages": {"type": "array", "items": {"type": "string"}},
                    "cwd": {"type": "string"}
                },
                "required": ["tool_id", "stages"]
            }),
            to_request: Some(|args| {
                Ok(Request::Single(Op {
                    c: protocol::op::TOOL_PIPELINE,
                    p: get_str(args, "tool_id"),
                    a: get_str_array(args, "stages"),
                    q: get_str(args, "cwd"),
                    ..Default::default()
                }))
            }),
            context_check: None,
        },
        ToolDef {
            name: "tool_repair",
            tier: ToolTier::OnDemand,
            schema: json!({
                "type": "object",
                "properties": {
                    "tool_id": {"type": "string"},
                    "max_iterations": {"type": "integer"},
                    "cwd": {"type": "string"}
                },
                "required": ["tool_id"]
            }),
            to_request: Some(|args| {
                Ok(Request::Single(Op {
                    c: protocol::op::TOOL_REPAIR,
                    p: get_str(args, "tool_id"),
                    n: get_i64(args, "max_iterations"),
                    q: get_str(args, "cwd"),
                    ..Default::default()
                }))
            }),
            context_check: None,
        },
    ]
}

// --- Derived queries ---

/// Build the initial set of exposed tool names (Tier 0 + Tier 1).
pub fn initial_exposed_tools() -> HashSet<String> {
    all_tools()
        .iter()
        .filter(|t| t.tier == ToolTier::Full || t.tier == ToolTier::Terse)
        .map(|t| t.name.to_string())
        .collect()
}

/// Names of on-demand tools (activated by list_all_tools).
pub fn on_demand_names() -> Vec<&'static str> {
    all_tools()
        .iter()
        .filter(|t| t.tier == ToolTier::OnDemand)
        .map(|t| t.name)
        .collect()
}

/// Returns true if this tool is Full-tier (always gets schema in list_tools).
pub fn has_full_schema(name: &str) -> bool {
    all_tools()
        .iter()
        .any(|t| t.name == name && t.tier == ToolTier::Full)
}

/// Whether `list_tools` should include the full inputSchema for this tool.
pub fn expose_full_schema_in_list(name: &str, full_tool_schemas: bool, already_used: bool) -> bool {
    if full_tool_schemas {
        return all_tools()
            .iter()
            .any(|t| t.name == name && (t.tier == ToolTier::Full || t.tier == ToolTier::Terse));
    }
    has_full_schema(name) && !already_used
}

/// Render one tool definition for a list_tools response under the session's
/// verbosity + schema policy: swap in a terse description below `Full`, then
/// strip the inputSchema unless this tool should advertise it.
pub fn render_list_tool(
    mut t: rust_mcp_sdk::schema::Tool,
    descriptions: &crate::tool_descriptions::ToolDescriptions,
    verbosity: crate::verbosity::Verbosity,
    full_tool_schemas: bool,
    already_used: bool,
) -> rust_mcp_sdk::schema::Tool {
    if verbosity != crate::verbosity::Verbosity::Full {
        if let Some(d) = descriptions.terse(&t.name) {
            t.description = Some(d.to_string());
        }
    }
    if expose_full_schema_in_list(&t.name, full_tool_schemas, already_used) {
        t
    } else {
        rust_mcp_sdk::schema::Tool {
            input_schema: serde_json::from_value(json!({"type": "object"}))
                .unwrap_or(t.input_schema),
            ..t
        }
    }
}

/// Returns false if the tool has a context check that fails for this workspace.
pub fn passes_context_check(name: &str, workspace: &Path) -> bool {
    all_tools()
        .iter()
        .find(|t| t.name == name)
        .and_then(|t| t.context_check.map(|f| f(workspace)))
        .unwrap_or(true)
}

/// Try to build a protocol Request from MCP tool args.
/// Returns None if the tool is unknown or has no opcode mapping (special tool).
/// Returns Some(Err) if args are invalid.
pub fn build_request(name: &str, args: &Value) -> Option<Result<Request, String>> {
    let tools = all_tools();
    let tool = tools.iter().find(|t| t.name == name)?;
    let to_request = tool.to_request?;
    Some(to_request(args))
}

/// Build MCP Tool objects from the registry for list_tools responses.
pub fn tool_definitions(
    descriptions: &crate::tool_descriptions::ToolDescriptions,
) -> Vec<rust_mcp_sdk::schema::Tool> {
    all_tools()
        .into_iter()
        .map(|td| {
            let input_schema = descriptions.schema_with_parameters(td.name, &td.schema);
            serde_json::from_value(json!({
                "name": td.name,
                "description": descriptions.full_or_name(td.name),
                "inputSchema": input_schema,
            }))
            .expect("valid tool definition")
        })
        .collect()
}

/// All tool names in the registry.
pub fn all_tool_names() -> Vec<&'static str> {
    all_tools().iter().map(|t| t.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptions() -> crate::tool_descriptions::ToolDescriptions {
        crate::tool_descriptions::ToolDescriptions::default()
    }

    #[test]
    fn all_tools_has_entries() {
        let tools = all_tools();
        assert!(!tools.is_empty());
        let names: Vec<&str> = tools.iter().map(|t| t.name).collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"edit_file"));
        assert!(names.contains(&"exec"));
        assert!(names.contains(&"search"));
        assert!(names.contains(&"batch"));
        assert!(names.contains(&"get_tool_schema"));
        assert!(names.contains(&"execute_script"));
        assert!(names.contains(&"git"));
        assert!(names.contains(&"discord"));
        assert!(names.contains(&"snapshot"));
    }

    #[test]
    fn all_tools_no_duplicates() {
        let tools = all_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name).collect();
        let unique: HashSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len(), "duplicate tool names");
    }

    #[test]
    fn all_tools_have_descriptions() {
        let descriptions = descriptions();
        for tool in all_tools() {
            assert!(
                descriptions
                    .full(tool.name)
                    .is_some_and(|description| !description.is_empty()),
                "tool '{}' has no description",
                tool.name
            );
        }
    }

    #[test]
    fn parameter_descriptions_live_only_in_runtime_catalog() {
        let raw_count: usize = all_tools()
            .iter()
            .filter_map(|tool| tool.schema.get("properties")?.as_object())
            .map(|properties| {
                properties
                    .values()
                    .filter(|property| property.get("description").is_some())
                    .count()
            })
            .sum();
        assert_eq!(
            raw_count, 0,
            "raw structural schemas must contain no model text"
        );

        let rendered_count: usize = tool_definitions(&descriptions())
            .into_iter()
            .filter_map(|tool| serde_json::to_value(tool.input_schema).ok())
            .filter_map(|schema| schema.get("properties").cloned())
            .filter_map(|properties| properties.as_object().cloned())
            .map(|properties| {
                properties
                    .values()
                    .filter(|property| property.get("description").is_some())
                    .count()
            })
            .sum();
        assert_eq!(
            rendered_count,
            crate::tool_descriptions::DEFAULT_PARAMETER_DESCRIPTION_COUNT,
            "rendered schemas must restore every migrated description"
        );
    }

    #[test]
    fn tier_classification() {
        let tools = all_tools();
        let full: Vec<&str> = tools
            .iter()
            .filter(|t| t.tier == ToolTier::Full)
            .map(|t| t.name)
            .collect();
        let terse: Vec<&str> = tools
            .iter()
            .filter(|t| t.tier == ToolTier::Terse)
            .map(|t| t.name)
            .collect();
        let on_demand: Vec<&str> = tools
            .iter()
            .filter(|t| t.tier == ToolTier::OnDemand)
            .map(|t| t.name)
            .collect();

        assert!(full.contains(&"read_file"));
        assert!(full.contains(&"exec"));
        assert!(full.contains(&"execute_script"));
        assert!(terse.contains(&"git"));
        assert!(terse.contains(&"snapshot"));
        assert!(on_demand.contains(&"diff_files"));
        assert!(on_demand.contains(&"tool_pipeline"));
    }

    #[test]
    fn initial_exposed_excludes_on_demand() {
        let exposed = initial_exposed_tools();
        assert!(exposed.contains("read_file"));
        assert!(exposed.contains("git"));
        assert!(!exposed.contains("diff_files"));
        assert!(!exposed.contains("tool_pipeline"));
    }

    #[test]
    fn has_full_schema_correct() {
        assert!(has_full_schema("read_file"));
        assert!(has_full_schema("exec"));
        assert!(has_full_schema("get_tool_schema"));
        assert!(!has_full_schema("git"));
        assert!(!has_full_schema("snapshot"));
        assert!(!has_full_schema("ls"));
    }

    #[test]
    fn expose_full_schema_in_list_respects_mode() {
        assert!(expose_full_schema_in_list("read_file", false, false));
        assert!(!expose_full_schema_in_list("read_file", false, true));
        assert!(!expose_full_schema_in_list("git", false, false));
        assert!(expose_full_schema_in_list("git", true, false));
        assert!(expose_full_schema_in_list("git", true, true));
        assert!(!expose_full_schema_in_list("diff_files", true, false));
    }

    #[test]
    fn build_request_for_opcode_tools() {
        let args = json!({"path": "foo.txt"});
        let req = build_request("read_file", &args);
        assert!(req.is_some());
        assert!(req.unwrap().is_ok());
    }

    #[test]
    fn build_request_none_for_special_tools() {
        let args = json!({});
        assert!(build_request("git", &args).is_none());
        assert!(build_request("docker", &args).is_none());
        assert!(build_request("set_cwd", &args).is_none());
        assert!(build_request("batch", &args).is_none());
    }

    #[test]
    fn build_request_none_for_unknown() {
        assert!(build_request("nonexistent", &json!({})).is_none());
    }

    #[test]
    fn snapshot_request_validates_action() {
        let ok = build_request("snapshot", &json!({"action": "create"}));
        assert!(ok.unwrap().is_ok());

        let bad = build_request("snapshot", &json!({"action": "nope"}));
        assert!(bad.unwrap().is_err());

        let missing = build_request("snapshot", &json!({}));
        assert!(missing.unwrap().is_err());
    }

    #[test]
    fn context_check_git() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!passes_context_check("git", dir.path()));
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        assert!(passes_context_check("git", dir.path()));
    }

    #[test]
    fn context_check_no_check_always_passes() {
        let dir = tempfile::tempdir().unwrap();
        assert!(passes_context_check("read_file", dir.path()));
        assert!(passes_context_check("nonexistent_tool", dir.path()));
    }

    #[test]
    fn context_check_kgl_gated_on_store_dir() {
        // #936 prefix diet: the kgl pair is the heaviest prefix content
        // (1.6k chars) and niche — only list it where a store exists.
        let dir = tempfile::tempdir().unwrap();
        assert!(!passes_context_check("kgl_query", dir.path()));
        assert!(!passes_context_check("kgl_assert", dir.path()));
        std::fs::create_dir(dir.path().join(".kgl")).unwrap();
        assert!(passes_context_check("kgl_query", dir.path()));
        assert!(passes_context_check("kgl_assert", dir.path()));
    }

    #[test]
    fn tool_definitions_roundtrip() {
        let defs = tool_definitions(&descriptions());
        assert!(!defs.is_empty());
        for tool in &defs {
            assert!(!tool.name.is_empty());
            assert!(tool.description.is_some());
        }
    }

    // --- verbosity-parameterized descriptions (#936 lever 3) ---

    #[test]
    fn terse_descriptions_are_shorter_and_curated_only() {
        let descriptions = descriptions();
        for t in all_tools() {
            if let Some(terse) = descriptions.terse(t.name) {
                let full = descriptions.full(t.name).unwrap();
                assert!(
                    terse.len() < full.len(),
                    "terse desc for {} not shorter ({} >= {})",
                    t.name,
                    terse.len(),
                    full.len()
                );
            }
        }
        assert!(descriptions.terse("does_not_exist").is_none());
        // Already-short descriptions need no terse variant.
        assert!(descriptions.terse("write_file").is_none());
    }

    #[test]
    fn terse_multiplexer_descriptions_keep_subcommands() {
        let descriptions = descriptions();
        // Dropping command names at low verbosity would force extra
        // get_tool_schema round-trips; guard against over-trimming.
        assert!(descriptions.terse("git").unwrap().contains("commit"));
        assert!(descriptions.terse("git").unwrap().contains("push"));
        assert!(descriptions.terse("cargo").unwrap().contains("clippy"));
        assert!(descriptions.terse("docker").unwrap().contains("compose"));
        let gh = descriptions.terse("gh").unwrap();
        assert!(gh.contains("merge") && gh.contains("raw"));
    }

    #[test]
    fn render_list_tool_swaps_description_below_full() {
        use crate::verbosity::Verbosity;
        let descriptions = descriptions();
        let git = || {
            tool_definitions(&descriptions)
                .into_iter()
                .find(|t| t.name == "git")
                .unwrap()
        };

        let orig = git().description.clone();
        let full = render_list_tool(git(), &descriptions, Verbosity::Full, false, false);
        assert_eq!(
            full.description, orig,
            "Full verbosity must keep the full description"
        );

        let terse = render_list_tool(git(), &descriptions, Verbosity::Terse, false, false);
        assert_eq!(terse.description.as_deref(), descriptions.terse("git"));
        assert!(terse.description.unwrap().len() < full.description.unwrap().len());

        // A tool without a terse variant is unchanged even below Full.
        let wf = || {
            tool_definitions(&descriptions)
                .into_iter()
                .find(|t| t.name == "write_file")
                .unwrap()
        };
        let wf_full = render_list_tool(wf(), &descriptions, Verbosity::Full, false, false);
        let wf_terse = render_list_tool(wf(), &descriptions, Verbosity::Terse, false, false);
        assert_eq!(wf_full.description, wf_terse.description);
    }

    #[test]
    fn description_block_shrinks_meaningfully_at_terse() {
        let descriptions = descriptions();
        let full_bytes: usize = tool_definitions(&descriptions)
            .iter()
            .map(|t| t.description.as_deref().unwrap_or("").len())
            .sum();
        let terse_bytes: usize = tool_definitions(&descriptions)
            .iter()
            .map(|t| {
                descriptions
                    .terse(&t.name)
                    .map(|s| s.len())
                    .unwrap_or_else(|| t.description.as_deref().unwrap_or("").len())
            })
            .sum();
        let reduction = 100.0 * (1.0 - terse_bytes as f64 / full_bytes as f64);
        eprintln!(
            "description block: full {full_bytes} B, terse {terse_bytes} B ({reduction:.1}% smaller)"
        );
        assert!(
            reduction > 15.0,
            "terse description block should be >15% smaller, got {reduction:.1}%"
        );
    }
}
