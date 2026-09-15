//! Outbound MCP servers for non-ACP agent frontends (vikunja #1289).
//!
//! The ACP path gets its MCP servers from the client: Zed forwards them on
//! `session/new`/`session/load`. Every other frontend — the interactive TUI,
//! one-shot `agent`, the `chat` REPL, and the session daemon — has no client
//! to do that, so this module reads a Claude-style `mcpServers` JSON file
//! (`[agent.mcp] servers_file`, default `~/.config/daimonos/mcp_servers.json`)
//! and drives the same `McpBridge` (ADR-003) the forwarded servers use: same
//! handshake, `mcp__{server}__{tool}` naming, self-connection refusal
//! (#1116), per-call analytics, and bounded teardown (#1293).
//!
//! Fail-open everywhere: a missing file, unreadable file, or invalid JSON
//! yields no bridge (logged), and the agent runs with native tools only —
//! matching the bridge's own per-server fail-open policy.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value;

use crate::agent::{RemoteToolHook, RemoteToolResult};
use crate::analytics::AnalyticsStore;
use crate::config::Config;
use crate::mcp_bridge::{McpBridge, McpClientPool, ServerSpec};
use crate::providers::ToolSchema;

/// Parse either a Claude/Cursor-style `mcpServers` file or Zed's
/// `settings.json` (`context_servers`). This lets `[agent.mcp].servers_file`
/// point directly at the system's primary Zed configuration instead of
/// maintaining a translated copy. If both top-level keys exist,
/// `context_servers` takes precedence. Entries are sorted by name so bridge
/// order (and thus the `max_servers` cap) is deterministic.
pub fn parse_servers_json(content: &str) -> Result<Vec<ServerSpec>, String> {
    // Zed settings are JSONC. Reuse its reader's string-aware cleanup before
    // parsing; ordinary JSON passes through unchanged.
    let cleaned = crate::zed_config::clean_jsonc(content);
    let root: Value =
        serde_json::from_str(&cleaned).map_err(|error| format!("invalid JSON/JSONC: {error}"))?;
    if root.get("context_servers").is_some() {
        return Ok(crate::zed_config::specs_from_settings(&root));
    }
    let servers = root
        .get("mcpServers")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            "missing top-level \"mcpServers\" or \"context_servers\" object".to_string()
        })?;
    let mut names: Vec<&String> = servers.keys().collect();
    names.sort();
    names
        .into_iter()
        .map(|name| parse_entry(name, &servers[name]))
        .collect()
}

fn parse_entry(name: &str, entry: &Value) -> Result<ServerSpec, String> {
    let obj = entry
        .as_object()
        .ok_or_else(|| format!("server {name:?}: entry is not an object"))?;
    let declared = obj.get("type").and_then(Value::as_str);
    if let Some(command) = obj.get("command").and_then(Value::as_str) {
        if let Some(kind) = declared {
            if kind != "stdio" {
                return Err(format!(
                    "server {name:?}: type {kind:?} conflicts with \"command\" (expected \"stdio\")"
                ));
            }
        }
        let args = obj
            .get("args")
            .and_then(Value::as_array)
            .map(|args| {
                args.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        return Ok(ServerSpec::Stdio {
            name: name.to_string(),
            command: command.to_string(),
            args,
            env: string_map(obj.get("env")),
        });
    }
    if let Some(url) = obj.get("url").and_then(Value::as_str) {
        // "sse" is accepted and attempted over the same HTTP transport; a
        // server that truly needs the legacy SSE handshake fails its bounded
        // init and is skipped (fail-open), which beats rejecting configs that
        // merely label streamable-HTTP endpoints "sse".
        match declared {
            None
            | Some("http")
            | Some("streamable-http")
            | Some("streamable_http")
            | Some("sse") => {}
            Some(kind) => {
                return Err(format!("server {name:?}: unsupported type {kind:?}"));
            }
        }
        return Ok(ServerSpec::Http {
            name: name.to_string(),
            url: url.to_string(),
            headers: string_map(obj.get("headers")),
        });
    }
    Err(format!(
        "server {name:?}: needs \"command\" (stdio) or \"url\" (http)"
    ))
}

fn string_map(value: Option<&Value>) -> HashMap<String, String> {
    value
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// A connected bridge plus the pieces a frontend wires into `AgentConfig`.
pub struct AgentMcp {
    pub bridge: Arc<McpBridge>,
}

impl AgentMcp {
    /// Remote tool schemas to append to the native tool list.
    pub fn tools(&self) -> Vec<ToolSchema> {
        self.bridge.tools().to_vec()
    }

    /// The agent-loop fallback dispatcher — same semantics as the ACP hook,
    /// including the #1116 legible refusal for self/nested-daimonos tools.
    pub fn dispatch_hook(&self) -> RemoteToolHook {
        let bridge = Arc::clone(&self.bridge);
        Box::new(move |name: &str, input: &Value| {
            let bridge = Arc::clone(&bridge);
            let name = name.to_string();
            let input = input.clone();
            Box::pin(async move {
                if let Some(reason) = bridge.self_dispatch_refused(&name).await {
                    return Some(RemoteToolResult {
                        content: reason,
                        is_error: true,
                    });
                }
                bridge
                    .call(&name, &input)
                    .await
                    .map(|outcome| RemoteToolResult {
                        content: outcome.content,
                        is_error: outcome.is_error,
                    })
            })
        })
    }

    /// Graceful teardown (bounded per `[acp.mcp]` shutdown budgets). Dropping
    /// without calling this still reaps stdio children via the client pool.
    pub async fn shutdown(&self) {
        self.bridge.shutdown().await;
    }
}

/// Load the system/user MCP configuration selected by `[agent.mcp]`.
/// Both Claude/Cursor and Zed settings formats are accepted.
pub async fn load_configured_specs(cfg: &Config) -> Vec<ServerSpec> {
    if !cfg.agent.mcp.enabled {
        return Vec::new();
    }
    let path = crate::paths::expand_tilde(&cfg.agent.mcp.servers_file);
    let content = match tokio::fs::read_to_string(&path).await {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            tracing::warn!(target: "daimonos::agent_mcp", path = %path.display(), %error,
                "mcp servers file unreadable; continuing without configured MCP servers");
            return Vec::new();
        }
    };
    match parse_servers_json(&content) {
        Ok(specs) => specs,
        Err(error) => {
            tracing::warn!(target: "daimonos::agent_mcp", path = %path.display(), %error,
                "mcp servers file invalid; continuing without configured MCP servers");
            Vec::new()
        }
    }
}

/// Read `[agent.mcp] servers_file` and connect. `None` when disabled, the
/// file is absent or empty, or it fails to read/parse (logged) — the agent
/// then runs with native tools only.
pub async fn connect(
    cfg: &Config,
    native_tool_names: &HashSet<String>,
    analytics: Option<Arc<AnalyticsStore>>,
) -> Option<AgentMcp> {
    let specs = load_configured_specs(cfg).await;
    if specs.is_empty() {
        return None;
    }
    let external_session_id = crate::analytics::read_agent_session_id_env();
    let bridge = McpBridge::build_with_pool(
        specs,
        &cfg.acp.mcp,
        native_tool_names,
        analytics,
        external_session_id,
        McpClientPool::new(),
    )
    .await;
    tracing::info!(
        target: "daimonos::agent_mcp",
        tools = bridge.tools().len(),
        "outbound MCP servers connected for agent mode"
    );
    Some(AgentMcp {
        bridge: Arc::new(bridge),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stdio_and_http_entries_sorted_by_name() {
        let json = r#"{
            "mcpServers": {
                "zeta": { "url": "https://api.example/mcp", "headers": {"Authorization": "Bearer x"} },
                "alpha": { "command": "npx", "args": ["-y", "some-server"], "env": {"K": "v"} }
            }
        }"#;
        let specs = parse_servers_json(json).unwrap();
        assert_eq!(specs.len(), 2);
        match &specs[0] {
            ServerSpec::Stdio {
                name,
                command,
                args,
                env,
            } => {
                assert_eq!(name, "alpha");
                assert_eq!(command, "npx");
                assert_eq!(args, &["-y".to_string(), "some-server".to_string()]);
                assert_eq!(env.get("K").map(String::as_str), Some("v"));
            }
            other => panic!("expected stdio spec, got {other:?}"),
        }
        match &specs[1] {
            ServerSpec::Http { name, url, headers } => {
                assert_eq!(name, "zeta");
                assert_eq!(url, "https://api.example/mcp");
                assert_eq!(
                    headers.get("Authorization").map(String::as_str),
                    Some("Bearer x")
                );
            }
            other => panic!("expected http spec, got {other:?}"),
        }
    }

    #[test]
    fn parses_zed_jsonc_context_servers() {
        let jsonc = r#"{
          // one primary config for Zed and Daimonos
          "context_servers": {
            "zed-http": { "url": "https://example.test/mcp", },
            "off": { "enabled": false, "command": "nope" },
          },
        }"#;
        let specs = parse_servers_json(jsonc).unwrap();
        assert_eq!(specs.len(), 1);
        assert!(matches!(&specs[0], ServerSpec::Http { name, .. } if name == "zed-http"));
    }

    #[test]
    fn type_field_is_validated_but_optional() {
        let ok = r#"{"mcpServers": {"a": {"type": "stdio", "command": "x"},
                                     "b": {"type": "sse", "url": "http://h/mcp"}}}"#;
        assert_eq!(parse_servers_json(ok).unwrap().len(), 2);
        let conflict = r#"{"mcpServers": {"a": {"type": "http", "command": "x"}}}"#;
        assert!(parse_servers_json(conflict)
            .unwrap_err()
            .contains("conflicts"));
        let unknown = r#"{"mcpServers": {"a": {"type": "websocket", "url": "wss://h"}}}"#;
        assert!(parse_servers_json(unknown)
            .unwrap_err()
            .contains("unsupported type"));
    }

    #[test]
    fn rejects_malformed_documents_legibly() {
        assert!(parse_servers_json("not json")
            .unwrap_err()
            .contains("invalid JSON/JSONC"));
        assert!(parse_servers_json("{}").unwrap_err().contains("mcpServers"));
        assert!(parse_servers_json(r#"{"mcpServers": {"a": {}}}"#)
            .unwrap_err()
            .contains("needs \"command\""));
    }

    #[tokio::test]
    async fn connect_is_none_when_file_missing_or_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.agent.mcp.servers_file = dir.path().join("absent.json").to_string_lossy().to_string();
        assert!(connect(&cfg, &HashSet::new(), None).await.is_none());
        // Disabled: even an existing file is never read.
        let present = dir.path().join("present.json");
        std::fs::write(&present, "not even json").unwrap();
        cfg.agent.mcp.servers_file = present.to_string_lossy().to_string();
        cfg.agent.mcp.enabled = false;
        assert!(connect(&cfg, &HashSet::new(), None).await.is_none());
    }
}
