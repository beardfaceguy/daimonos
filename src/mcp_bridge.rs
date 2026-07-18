//! ACP MCP-server bridge (ADR-003, vikunja #990).
//!
//! Zed forwards every configured context server to the ACP agent on
//! `session/new` and `session/load` as `acp::McpServer` entries. This module
//! makes daimonos act as an MCP *client* to each forwarded server: it connects,
//! runs the `initialize` + `tools/list` handshake, exposes each remote tool to
//! the model under a collision-safe `mcp__{server}__{tool}` name, dispatches
//! `tools/call` on demand, and records usage in analytics.
//!
//! The bridge is per-session and owned by the ACP frontend (`acp_cmd.rs`); the
//! core `crate::session::Session` and the opcode tool facade stay MCP-free. See
//! `docs/adr/003-acp-mcp-server-bridge.md`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use rust_mcp_sdk::mcp_client::ClientRuntime;
use rust_mcp_sdk::mcp_client::{client_runtime, ClientHandler, McpClientOptions};
use rust_mcp_sdk::schema::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ContentBlock, Implementation,
    InitializeRequestParams, LATEST_PROTOCOL_VERSION,
};
use rust_mcp_sdk::{
    McpClient, RequestOptions, StdioTransport, StreamableTransportOptions, ToMcpClientHandler,
    TransportOptions,
};

use crate::analytics::{self, AnalyticsStore, ToolCallRecord};
use crate::config::AcpMcpConfig;
use crate::providers::ToolSchema;

/// Transport-neutral description of one forwarded MCP server. Decoupled from
/// the `agent_client_protocol` types so this module (and its tests) don't
/// depend on ACP; `acp_cmd.rs` converts `acp::McpServer` into this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerSpec {
    Stdio {
        name: String,
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
    Http {
        name: String,
        url: String,
        headers: HashMap<String, String>,
    },
}

impl ServerSpec {
    fn name(&self) -> &str {
        match self {
            ServerSpec::Stdio { name, .. } | ServerSpec::Http { name, .. } => name,
        }
    }
}

/// Result of dispatching a remote tool call, in the shape the agent loop needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteToolOutcome {
    pub content: String,
    pub is_error: bool,
}

/// Maps an exposed (namespaced) tool name to the client that serves it and the
/// original (un-namespaced) tool name to call on that server.
struct Route {
    client: usize,
    original: String,
}

/// A per-session set of MCP clients plus the tools they expose. Built once per
/// `session/new` (and per `session/load` rebuild); `shutdown` tears every
/// client down on `session/delete` / process exit.
pub struct McpBridge {
    clients: Vec<Arc<ClientRuntime>>,
    tools: Vec<ToolSchema>,
    routes: HashMap<String, Route>,
    analytics: Option<Arc<AnalyticsStore>>,
    external_session_id: Option<String>,
    call_timeout: Duration,
}

impl McpBridge {
    /// An empty bridge (bridge disabled, or no servers forwarded). Exposes no
    /// tools and never claims a tool name.
    #[cfg(test)]
    pub fn empty() -> Self {
        Self {
            clients: Vec::new(),
            tools: Vec::new(),
            routes: HashMap::new(),
            analytics: None,
            external_session_id: None,
            call_timeout: Duration::from_secs(0),
        }
    }

    /// Connect to every forwarded server (fail-open: a server that can't be
    /// started, initialized, or listed is logged and skipped). `native_tool_names`
    /// are the daimonos tool names that must always win a name collision.
    pub async fn build(
        specs: Vec<ServerSpec>,
        cfg: &AcpMcpConfig,
        native_tool_names: &HashSet<String>,
        analytics: Option<Arc<AnalyticsStore>>,
        external_session_id: Option<String>,
    ) -> Self {
        let mut bridge = Self {
            clients: Vec::new(),
            tools: Vec::new(),
            routes: HashMap::new(),
            analytics,
            external_session_id,
            call_timeout: Duration::from_secs(cfg.call_timeout_secs),
        };
        if !cfg.enabled {
            return bridge;
        }

        // Names already taken: native tools win, then earlier remote tools.
        let mut used: HashSet<String> = native_tool_names.clone();
        let init_timeout = Duration::from_secs(cfg.init_timeout_secs);

        for spec in specs.into_iter().take(cfg.max_servers) {
            let server_name = spec.name().to_string();
            match connect(&spec, cfg, init_timeout).await {
                Ok((client, tools)) => {
                    let client_idx = bridge.clients.len();
                    let mut registered_any = false;
                    for tool in tools.into_iter().take(cfg.max_tools_per_server) {
                        let Some(exposed) =
                            resolve_name(&server_name, &tool.name, native_tool_names, &mut used)
                        else {
                            eprintln!(
                                "acp mcp bridge: skipping remote tool '{}' from '{server_name}': name collides and cannot be de-duped",
                                tool.name
                            );
                            continue;
                        };
                        let input_schema = serde_json::to_value(&tool.input_schema)
                            .unwrap_or_else(|_| serde_json::json!({"type": "object"}));
                        let description = tool
                            .description
                            .clone()
                            .unwrap_or_else(|| format!("Remote MCP tool from {server_name}"));
                        bridge.tools.push(ToolSchema {
                            name: exposed.clone(),
                            description,
                            input_schema,
                        });
                        bridge.routes.insert(
                            exposed,
                            Route {
                                client: client_idx,
                                original: tool.name,
                            },
                        );
                        registered_any = true;
                    }
                    if registered_any {
                        bridge.clients.push(client);
                    } else {
                        // Contributed no tools — don't keep an idle client.
                        let _ = client.shut_down().await;
                    }
                }
                Err(e) => {
                    eprintln!("acp mcp bridge: skipping MCP server '{server_name}': {e}");
                }
            }
        }
        bridge
    }

    /// The tool schemas to append to the model's tool list.
    pub fn tools(&self) -> &[ToolSchema] {
        &self.tools
    }

    /// Number of connected servers (clients that contributed ≥1 tool).
    #[cfg(test)]
    pub fn server_count(&self) -> usize {
        self.clients.len()
    }

    /// Whether `name` is a remote tool this bridge serves.
    #[cfg(test)]
    pub fn handles(&self, name: &str) -> bool {
        self.routes.contains_key(name)
    }

    /// Dispatch a remote tool call. Returns `None` if `name` is not a remote
    /// tool (so the caller falls through to native handling); `Some` with an
    /// error outcome when the call itself fails or times out.
    pub async fn call(&self, name: &str, input: &Value) -> Option<RemoteToolOutcome> {
        let route = self.routes.get(name)?;
        let client = self.clients.get(route.client)?;
        let params = CallToolRequestParams {
            name: route.original.clone(),
            arguments: input.as_object().cloned(),
            meta: None,
            task: None,
        };
        let request_chars = input.to_string().len();
        let started = Instant::now();
        let outcome =
            match tokio::time::timeout(self.call_timeout, client.request_tool_call(params)).await {
                Ok(Ok(result)) => result_to_outcome(result),
                Ok(Err(e)) => RemoteToolOutcome {
                    content: format!("remote MCP tool '{name}' failed: {e}"),
                    is_error: true,
                },
                Err(_) => RemoteToolOutcome {
                    content: format!(
                        "remote MCP tool '{name}' timed out after {}s",
                        self.call_timeout.as_secs()
                    ),
                    is_error: true,
                },
            };
        self.record(
            name,
            request_chars,
            outcome.content.len(),
            started.elapsed(),
        );
        Some(outcome)
    }

    /// Shut down every client (and, for stdio, reap the child process). Called
    /// on `session/delete` and process exit.
    pub async fn shutdown(&self) {
        for client in &self.clients {
            let _ = client.shut_down().await;
        }
    }

    fn record(&self, name: &str, request_chars: usize, response_chars: usize, elapsed: Duration) {
        let Some(analytics) = &self.analytics else {
            return;
        };
        let record = ToolCallRecord {
            tool_name: name.to_string(),
            command: None,
            request_tokens: analytics::estimate_tokens(request_chars),
            response_tokens: analytics::estimate_tokens(response_chars),
            saved_tokens: 0,
            savings_pct: 0.0,
            exec_time_ms: elapsed.as_millis() as u64,
            was_redirect: false,
            was_filtered: false,
            read_dedup: false,
            batch_size: 1,
            external_session_id: self.external_session_id.clone(),
        };
        analytics.record_async(record);
    }
}

/// Connect to one server and list its tools, bounded by `init_timeout`.
async fn connect(
    spec: &ServerSpec,
    cfg: &AcpMcpConfig,
    init_timeout: Duration,
) -> Result<(Arc<ClientRuntime>, Vec<rust_mcp_sdk::schema::Tool>), String> {
    let client = create_client(spec, cfg)?;
    tokio::time::timeout(init_timeout, client.clone().start())
        .await
        .map_err(|_| "initialize timed out".to_string())?
        .map_err(|e| format!("initialize failed: {e}"))?;
    let listed = tokio::time::timeout(init_timeout, client.request_tool_list(None)).await;
    match listed {
        Ok(Ok(result)) => Ok((client, result.tools)),
        Ok(Err(e)) => {
            let _ = client.shut_down().await;
            Err(format!("tools/list failed: {e}"))
        }
        Err(_) => {
            let _ = client.shut_down().await;
            Err("tools/list timed out".to_string())
        }
    }
}

fn client_details() -> InitializeRequestParams {
    InitializeRequestParams {
        capabilities: ClientCapabilities::default(),
        client_info: Implementation {
            name: "daimonos".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            title: None,
            description: None,
            icons: vec![],
            website_url: None,
        },
        protocol_version: LATEST_PROTOCOL_VERSION.into(),
        meta: None,
    }
}

fn create_client(spec: &ServerSpec, cfg: &AcpMcpConfig) -> Result<Arc<ClientRuntime>, String> {
    match spec {
        ServerSpec::Stdio {
            command, args, env, ..
        } => {
            if !cfg.allow_stdio {
                return Err("stdio transport disabled by config".to_string());
            }
            let env = if env.is_empty() {
                None
            } else {
                Some(env.clone())
            };
            let transport = StdioTransport::create_with_server_launch(
                command.clone(),
                args.clone(),
                env,
                TransportOptions::default(),
            )
            .map_err(|e| format!("stdio transport: {e}"))?;
            Ok(client_runtime::create_client(McpClientOptions {
                client_details: client_details(),
                transport,
                handler: BridgeClientHandler.to_mcp_client_handler(),
                task_store: None,
                server_task_store: None,
                message_observer: None,
            }))
        }
        ServerSpec::Http { url, headers, .. } => {
            if !cfg.allow_http {
                return Err("http transport disabled by config".to_string());
            }
            let options = StreamableTransportOptions {
                mcp_url: url.clone(),
                request_options: RequestOptions {
                    custom_headers: if headers.is_empty() {
                        None
                    } else {
                        Some(headers.clone())
                    },
                    ..RequestOptions::default()
                },
            };
            Ok(client_runtime::with_transport_options(
                client_details(),
                options,
                BridgeClientHandler,
                None,
                None,
                None,
            ))
        }
    }
}

/// Build the exposed, collision-safe tool name for a remote tool, or `None` if
/// it cannot be placed (collides with a native tool, or exhausts de-dup
/// suffixes against other remote tools). Mutates `used` to reserve the name.
fn resolve_name(
    server: &str,
    tool: &str,
    native: &HashSet<String>,
    used: &mut HashSet<String>,
) -> Option<String> {
    let base = namespaced_name(server, tool);
    // Native tools always win: never rename around them, drop the remote tool.
    if native.contains(&base) {
        return None;
    }
    if !used.contains(&base) {
        used.insert(base.clone());
        return Some(base);
    }
    // Remote-vs-remote collision: append a deterministic numeric suffix.
    for n in 2..=99u32 {
        let suffix = format!("__{n}");
        let mut candidate = base.clone();
        if candidate.len() + suffix.len() > MAX_TOOL_NAME_LEN {
            candidate.truncate(MAX_TOOL_NAME_LEN - suffix.len());
        }
        candidate.push_str(&suffix);
        if !native.contains(&candidate) && !used.contains(&candidate) {
            used.insert(candidate.clone());
            return Some(candidate);
        }
    }
    None
}

const MAX_TOOL_NAME_LEN: usize = 64;

/// `mcp__{server}__{tool}`, sanitized to the provider tool-name constraint
/// (`^[a-zA-Z0-9_-]{1,64}$`) and truncated to 64 bytes. Non-conforming chars
/// become `_`.
fn namespaced_name(server: &str, tool: &str) -> String {
    let raw = format!("mcp__{server}__{tool}");
    let mut s: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.len() > MAX_TOOL_NAME_LEN {
        s.truncate(MAX_TOOL_NAME_LEN);
    }
    s
}

fn result_to_outcome(result: CallToolResult) -> RemoteToolOutcome {
    let is_error = result.is_error.unwrap_or(false);
    let texts: Vec<String> = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::TextContent(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect();
    let content = if !texts.is_empty() {
        texts.join("\n")
    } else if let Some(structured) = &result.structured_content {
        serde_json::to_string(structured).unwrap_or_default()
    } else {
        serde_json::to_string(&result.content).unwrap_or_default()
    };
    RemoteToolOutcome { content, is_error }
}

/// No-op client handler: daimonos is a pure tool consumer, so it declines
/// server-initiated requests (sampling, elicitation, roots) via the trait
/// defaults and ignores server notifications.
struct BridgeClientHandler;
impl ClientHandler for BridgeClientHandler {}

#[cfg(test)]
mod tests {
    use super::*;

    fn native_set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    // --- namespaced_name ---

    #[test]
    fn namespaced_name_basic() {
        assert_eq!(
            namespaced_name("linear", "create_issue"),
            "mcp__linear__create_issue"
        );
    }

    #[test]
    fn namespaced_name_sanitizes_disallowed_chars() {
        let n = namespaced_name("my server.io", "do/it");
        assert_eq!(n, "mcp__my_server_io__do_it");
        assert!(n
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
    }

    #[test]
    fn namespaced_name_truncates_to_64() {
        let long_tool = "t".repeat(200);
        let n = namespaced_name("srv", &long_tool);
        assert_eq!(n.len(), 64);
    }

    // --- resolve_name / collisions ---

    #[test]
    fn resolve_name_registers_unique() {
        let native = native_set(&["read_file"]);
        let mut used = native.clone();
        let name = resolve_name("linear", "create_issue", &native, &mut used).unwrap();
        assert_eq!(name, "mcp__linear__create_issue");
        assert!(used.contains("mcp__linear__create_issue"));
    }

    #[test]
    fn resolve_name_native_wins_collision_drops_remote() {
        // A remote tool that namespaces onto a native name is dropped.
        let native = native_set(&["mcp__x__read_file"]);
        let mut used = native.clone();
        assert!(resolve_name("x", "read_file", &native, &mut used).is_none());
    }

    #[test]
    fn resolve_name_remote_vs_remote_suffixes() {
        let native = native_set(&["read_file"]);
        let mut used = native.clone();
        let first = resolve_name("srv", "tool", &native, &mut used).unwrap();
        let second = resolve_name("srv", "tool", &native, &mut used).unwrap();
        assert_eq!(first, "mcp__srv__tool");
        assert_eq!(second, "mcp__srv__tool__2");
        assert_ne!(first, second);
    }

    // --- result_to_outcome ---

    #[test]
    fn result_to_outcome_joins_text_blocks() {
        use rust_mcp_sdk::schema::TextContent;
        let result = CallToolResult {
            content: vec![
                ContentBlock::TextContent(TextContent::new("line1".to_string(), None, None)),
                ContentBlock::TextContent(TextContent::new("line2".to_string(), None, None)),
            ],
            is_error: None,
            meta: None,
            structured_content: None,
        };
        let outcome = result_to_outcome(result);
        assert_eq!(outcome.content, "line1\nline2");
        assert!(!outcome.is_error);
    }

    #[test]
    fn result_to_outcome_marks_error() {
        let result = CallToolResult {
            content: vec![],
            is_error: Some(true),
            meta: None,
            structured_content: None,
        };
        let outcome = result_to_outcome(result);
        assert!(outcome.is_error);
    }

    // --- build: fail-open + disabled ---

    #[tokio::test]
    async fn build_disabled_returns_empty() {
        let cfg = AcpMcpConfig {
            enabled: false,
            ..AcpMcpConfig::default()
        };
        let spec = ServerSpec::Stdio {
            name: "x".to_string(),
            command: "true".to_string(),
            args: vec![],
            env: HashMap::new(),
        };
        let bridge = McpBridge::build(vec![spec], &cfg, &native_set(&[]), None, None).await;
        assert!(bridge.tools().is_empty());
        assert_eq!(bridge.server_count(), 0);
    }

    #[tokio::test]
    async fn build_fail_open_skips_bad_server() {
        // A command that doesn't exist / isn't an MCP server must be skipped
        // without failing the whole bridge.
        let cfg = AcpMcpConfig {
            init_timeout_secs: 2,
            ..AcpMcpConfig::default()
        };
        let spec = ServerSpec::Stdio {
            name: "broken".to_string(),
            command: "daimonos-nonexistent-binary-xyz".to_string(),
            args: vec![],
            env: HashMap::new(),
        };
        let bridge = McpBridge::build(vec![spec], &cfg, &native_set(&[]), None, None).await;
        assert!(bridge.tools().is_empty());
        assert_eq!(bridge.server_count(), 0);
    }

    #[tokio::test]
    async fn build_respects_http_disabled() {
        let cfg = AcpMcpConfig {
            allow_http: false,
            init_timeout_secs: 2,
            ..AcpMcpConfig::default()
        };
        let spec = ServerSpec::Http {
            name: "remote".to_string(),
            url: "http://127.0.0.1:1/mcp".to_string(),
            headers: HashMap::new(),
        };
        let bridge = McpBridge::build(vec![spec], &cfg, &native_set(&[]), None, None).await;
        assert!(bridge.tools().is_empty());
    }

    #[tokio::test]
    async fn call_unknown_tool_returns_none() {
        let bridge = McpBridge::empty();
        assert!(bridge
            .call("mcp__x__y", &serde_json::json!({}))
            .await
            .is_none());
        assert!(!bridge.handles("mcp__x__y"));
    }

    // --- live round-trip against a real MCP server (daimonos --mcp) ---

    /// Locate the built `daimonos` binary next to the test executable. Returns
    /// `None` when it can't be found, so the live test skips instead of failing
    /// in environments where only the test harness was built.
    fn daimonos_binary() -> Option<std::path::PathBuf> {
        let test_exe = std::env::current_exe().ok()?;
        // target/debug/deps/daimonos-<hash> → target/debug/daimonos
        let deps_dir = test_exe.parent()?;
        let target_dir = deps_dir.parent()?;
        let candidate = target_dir.join(if cfg!(windows) {
            "daimonos.exe"
        } else {
            "daimonos"
        });
        candidate.exists().then_some(candidate)
    }

    #[tokio::test]
    async fn live_round_trip_against_daimonos_mcp() {
        let Some(bin) = daimonos_binary() else {
            eprintln!("skipping: daimonos binary not found next to test exe");
            return;
        };
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("hello.txt"), "hi there").unwrap();

        let spec = ServerSpec::Stdio {
            name: "self".to_string(),
            command: bin.to_string_lossy().into_owned(),
            args: vec![
                "--mcp".to_string(),
                "-w".to_string(),
                workspace.path().to_string_lossy().into_owned(),
            ],
            env: HashMap::new(),
        };
        let cfg = AcpMcpConfig {
            init_timeout_secs: 30,
            call_timeout_secs: 30,
            ..AcpMcpConfig::default()
        };
        let native = native_set(&["read_file", "write_file"]);
        let bridge = McpBridge::build(vec![spec], &cfg, &native, None, None).await;

        assert_eq!(
            bridge.server_count(),
            1,
            "expected the self MCP server to connect"
        );
        // Every remote tool is namespaced, so the remote read_file is exposed as
        // mcp__self__read_file and does NOT collide with the native read_file —
        // that's the point of namespacing (native-wins only fires on an exact
        // namespaced-name clash, covered by resolve_name_native_wins_*).
        assert!(
            bridge
                .tools()
                .iter()
                .all(|t| t.name.starts_with("mcp__self__")),
            "all remote tools must be namespaced"
        );
        assert!(
            bridge.handles("mcp__self__read_file"),
            "remote read_file should be exposed under its namespaced name"
        );

        // Call a remote tool that exists on daimonos --mcp: ls the workspace.
        let ls_name = bridge
            .tools()
            .iter()
            .map(|t| t.name.clone())
            .find(|n| n == "mcp__self__ls")
            .expect("daimonos --mcp should expose an ls tool");
        let outcome = bridge
            .call(&ls_name, &serde_json::json!({}))
            .await
            .expect("ls should dispatch");
        assert!(!outcome.is_error, "ls call errored: {}", outcome.content);
        assert!(
            outcome.content.contains("hello.txt"),
            "ls output should list the workspace file, got: {}",
            outcome.content
        );

        bridge.shutdown().await;
    }
}
