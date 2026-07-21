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

use futures_util::{future, stream, StreamExt};
use serde_json::Value;

use rust_mcp_sdk::mcp_client::ClientRuntime;
use rust_mcp_sdk::mcp_client::{client_runtime, ClientHandler, McpClientOptions};
use rust_mcp_sdk::schema::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ContentBlock, Implementation,
    InitializeRequestParams, Tool, LATEST_PROTOCOL_VERSION,
};
use rust_mcp_sdk::{
    McpClient, RequestOptions, StdioTransport, StreamableTransportOptions, ToMcpClientHandler,
    TransportOptions,
};

use crate::analytics::{self, AnalyticsStore, ToolCallRecord};
use crate::config::AcpMcpConfig;
use crate::providers::ToolSchema;

pub const REMOTE_TOOL_PREFIX: &str = "mcp__";

/// Canonical transport identity for pooling. Display names are deliberately
/// excluded: two sessions may alias the same server differently while sharing
/// one transport/client. Map fields are sorted so insertion order cannot
/// change identity.
#[derive(Clone, PartialEq, Eq, Hash)]
enum ServerKey {
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    Http {
        url: String,
        headers: Vec<(String, String)>,
    },
}

impl From<&ServerSpec> for ServerKey {
    fn from(spec: &ServerSpec) -> Self {
        match spec {
            ServerSpec::Stdio {
                command, args, env, ..
            } => Self::Stdio {
                command: command.clone(),
                args: args.clone(),
                env: sorted_pairs(env),
            },
            ServerSpec::Http { url, headers, .. } => Self::Http {
                url: url.clone(),
                headers: sorted_pairs(headers),
            },
        }
    }
}

fn sorted_pairs(values: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut pairs: Vec<_> = values
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    pairs.sort_unstable();
    pairs
}

struct PoolLease {
    slot: Arc<PoolSlot>,
    client: Arc<ClientRuntime>,
    tools: Arc<Vec<Tool>>,
    shutdown_timeout: Duration,
}

#[derive(Default)]
struct PoolSlot {
    state: tokio::sync::Mutex<PoolSlotState>,
    changed: tokio::sync::Notify,
}

#[derive(Default)]
enum PoolSlotState {
    #[default]
    Empty,
    ShuttingDown,
    Ready {
        client: Arc<ClientRuntime>,
        tools: Arc<Vec<Tool>>,
        leases: usize,
    },
}

#[derive(Default)]
struct PoolInner {
    slots: tokio::sync::Mutex<HashMap<ServerKey, std::sync::Weak<PoolSlot>>>,
}

/// Process-wide cache of initialized MCP clients. Clones share the same pool;
/// `McpBridge` instances retain explicit leases and release them asynchronously
/// on session deletion/process teardown.
#[derive(Clone, Default)]
pub struct McpClientPool {
    inner: Arc<PoolInner>,
}

impl McpClientPool {
    pub fn new() -> Self {
        Self::default()
    }

    async fn acquire(
        &self,
        spec: &ServerSpec,
        cfg: &AcpMcpConfig,
        init_timeout: Duration,
    ) -> Result<PoolLease, String> {
        let key = ServerKey::from(spec);
        let slot = {
            let mut slots = self.inner.slots.lock().await;
            // A Weak whose strong count reached zero cannot be resurrected, so
            // pruning under this map lock is race-free.
            slots.retain(|_, slot| slot.strong_count() > 0);
            match slots.get(&key).and_then(std::sync::Weak::upgrade) {
                Some(slot) => slot,
                None => {
                    let slot = Arc::new(PoolSlot::default());
                    slots.insert(key, Arc::downgrade(&slot));
                    slot
                }
            }
        };
        let mut state = loop {
            let mut state = slot.state.lock().await;
            if let PoolSlotState::Ready {
                client,
                tools,
                leases,
            } = &mut *state
            {
                *leases += 1;
                return Ok(PoolLease {
                    slot: Arc::clone(&slot),
                    client: Arc::clone(client),
                    tools: Arc::clone(tools),
                    shutdown_timeout: Duration::from_secs(cfg.shutdown_timeout_secs),
                });
            }
            if matches!(*state, PoolSlotState::ShuttingDown) {
                // Register before unlocking so completion cannot race between
                // observing ShuttingDown and beginning the wait.
                let changed = slot.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                drop(state);
                changed.await;
                continue;
            }
            break state;
        };

        let (client, tools) = connect(spec, cfg, init_timeout).await?;
        let tools = Arc::new(tools);
        *state = PoolSlotState::Ready {
            client: Arc::clone(&client),
            tools: Arc::clone(&tools),
            leases: 1,
        };
        Ok(PoolLease {
            slot: Arc::clone(&slot),
            client,
            tools,
            shutdown_timeout: Duration::from_secs(cfg.shutdown_timeout_secs),
        })
    }

    async fn release(&self, lease: PoolLease) {
        let shutdown_timeout = lease.shutdown_timeout;
        let mut state = lease.slot.state.lock().await;
        let shutdown = match &mut *state {
            PoolSlotState::Ready { leases, .. } if *leases > 1 => {
                *leases -= 1;
                None
            }
            PoolSlotState::Ready { client, .. } => {
                let client = Arc::clone(client);
                *state = PoolSlotState::ShuttingDown;
                Some(client)
            }
            PoolSlotState::Empty | PoolSlotState::ShuttingDown => None,
        };
        if let Some(client) = shutdown {
            drop(state);
            let slot = Arc::clone(&lease.slot);
            let mut shutdown_task = tokio::spawn(async move {
                let _ = client.shut_down().await;
                let mut state = slot.state.lock().await;
                if matches!(*state, PoolSlotState::ShuttingDown) {
                    *state = PoolSlotState::Empty;
                }
                slot.changed.notify_waiters();
            });
            // Dropping a timed-out JoinHandle detaches rather than cancels its
            // task. The slot stays strongly held and ShuttingDown until the
            // original runtime actually exits, preventing overlapping clients.
            if tokio::time::timeout(shutdown_timeout, &mut shutdown_task)
                .await
                .is_err()
            {
                eprintln!(
                    "acp mcp bridge: client shutdown timed out after {}s",
                    shutdown_timeout.as_secs()
                );
            }
        }
    }

    #[cfg(test)]
    async fn entry_count(&self) -> usize {
        let slots: Vec<_> = self
            .inner
            .slots
            .lock()
            .await
            .values()
            .filter_map(std::sync::Weak::upgrade)
            .collect();
        let mut count = 0;
        for slot in slots {
            if matches!(*slot.state.lock().await, PoolSlotState::Ready { .. }) {
                count += 1;
            }
        }
        count
    }

    #[cfg(test)]
    async fn lease_count(&self) -> usize {
        let slots: Vec<_> = self
            .inner
            .slots
            .lock()
            .await
            .values()
            .filter_map(std::sync::Weak::upgrade)
            .collect();
        let mut count = 0;
        for slot in slots {
            if let PoolSlotState::Ready { leases, .. } = *slot.state.lock().await {
                count += leases;
            }
        }
        count
    }

    #[cfg(test)]
    async fn slot_count(&self) -> usize {
        self.inner.slots.lock().await.len()
    }
}

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
    lease: usize,
    original: String,
}

#[derive(Default)]
struct BridgeRuntime {
    leases: Vec<PoolLease>,
    routes: HashMap<String, Route>,
}

/// A per-session set of MCP client leases plus the tools they expose. Built
/// once per `session/new` (and per `session/load` rebuild); `shutdown` releases
/// every lease on `session/delete` / process exit. The pool tears the runtime
/// down when its final cross-session lease is released.
pub struct McpBridge {
    pool: McpClientPool,
    runtime: tokio::sync::RwLock<BridgeRuntime>,
    tools: Vec<ToolSchema>,
    diagnostics: Vec<String>,
    had_connection_failures: bool,
    analytics: Option<Arc<AnalyticsStore>>,
    external_session_id: Option<String>,
    call_timeout: Duration,
}

impl McpBridge {
    /// An empty bridge (bridge disabled, or no servers forwarded). Exposes no
    /// tools and never claims a tool name.
    #[cfg(test)]
    pub fn empty() -> Self {
        let pool = McpClientPool::new();
        Self {
            pool,
            runtime: tokio::sync::RwLock::new(BridgeRuntime::default()),
            tools: Vec::new(),
            diagnostics: Vec::new(),
            had_connection_failures: false,
            analytics: None,
            external_session_id: None,
            call_timeout: Duration::from_secs(0),
        }
    }

    /// Connect to forwarded servers with bounded concurrency (fail-open: a
    /// server that can't be started, initialized, or listed is logged and
    /// skipped). Results are registered in forwarded order so network timing
    /// cannot change collision suffixes. `native_tool_names` are the daimonos
    /// tool names that must always win a name collision.
    #[cfg(test)]
    pub async fn build(
        specs: Vec<ServerSpec>,
        cfg: &AcpMcpConfig,
        native_tool_names: &HashSet<String>,
        analytics: Option<Arc<AnalyticsStore>>,
        external_session_id: Option<String>,
    ) -> Self {
        Self::build_with_pool(
            specs,
            cfg,
            native_tool_names,
            analytics,
            external_session_id,
            McpClientPool::new(),
        )
        .await
    }

    /// Build using a process-shared client pool. When pooling is disabled in
    /// config, a private pool preserves the original per-session isolation.
    pub async fn build_with_pool(
        specs: Vec<ServerSpec>,
        cfg: &AcpMcpConfig,
        native_tool_names: &HashSet<String>,
        analytics: Option<Arc<AnalyticsStore>>,
        external_session_id: Option<String>,
        shared_pool: McpClientPool,
    ) -> Self {
        let pool = if cfg.shared_pool_enabled {
            shared_pool
        } else {
            McpClientPool::new()
        };
        let mut bridge = Self {
            pool: pool.clone(),
            runtime: tokio::sync::RwLock::new(BridgeRuntime::default()),
            tools: Vec::new(),
            diagnostics: Vec::new(),
            had_connection_failures: false,
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
        let concurrency = cfg.max_concurrent_connects.min(cfg.max_servers).max(1);
        if specs.len() > cfg.max_servers {
            bridge.diagnostics.push(format!(
                "{} forwarded MCP server(s) were skipped by max_servers={}",
                specs.len() - cfg.max_servers,
                cfg.max_servers
            ));
        }
        let connect_futures =
            specs
                .into_iter()
                .take(cfg.max_servers)
                .enumerate()
                .map(|(index, spec)| {
                    let pool = pool.clone();
                    async move {
                        let server_name = spec.name().to_string();
                        let outcome = pool.acquire(&spec, cfg, init_timeout).await;
                        (index, server_name, outcome)
                    }
                });
        let mut outcomes = stream::iter(connect_futures)
            .buffer_unordered(concurrency)
            .collect::<Vec<_>>()
            .await;
        outcomes.sort_unstable_by_key(|(index, _, _)| *index);

        for (_, server_name, outcome) in outcomes {
            match outcome {
                Ok(lease) => {
                    let lease_idx = bridge.runtime.get_mut().leases.len();
                    let mut registered_any = false;
                    if lease.tools.len() > cfg.max_tools_per_server {
                        bridge.diagnostics.push(format!(
                            "MCP server '{server_name}': {} tool(s) were skipped by max_tools_per_server={}",
                            lease.tools.len() - cfg.max_tools_per_server,
                            cfg.max_tools_per_server
                        ));
                    }
                    for tool in lease.tools.iter().take(cfg.max_tools_per_server) {
                        let Some(exposed) =
                            resolve_name(&server_name, &tool.name, native_tool_names, &mut used)
                        else {
                            let message = format!(
                                "MCP server '{server_name}': tool '{}' was skipped because its name collides and cannot be de-duplicated",
                                tool.name
                            );
                            eprintln!("acp mcp bridge: {message}");
                            bridge.diagnostics.push(message);
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
                        bridge.runtime.get_mut().routes.insert(
                            exposed,
                            Route {
                                lease: lease_idx,
                                original: tool.name.clone(),
                            },
                        );
                        registered_any = true;
                    }
                    if registered_any {
                        bridge.runtime.get_mut().leases.push(lease);
                    } else {
                        // Contributed no tools — don't keep an idle client.
                        bridge.pool.release(lease).await;
                    }
                }
                Err(e) => {
                    let message = format!("MCP server '{server_name}' failed to connect: {e}");
                    eprintln!("acp mcp bridge: {message}");
                    bridge.diagnostics.push(message);
                    bridge.had_connection_failures = true;
                }
            }
        }
        bridge
    }

    /// The tool schemas to append to the model's tool list.
    pub fn tools(&self) -> &[ToolSchema] {
        &self.tools
    }

    /// Connection/tool-registration problems to surface through ACP instead
    /// of leaving them only on the child process's stderr pipe.
    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    /// Whether at least one server failed before tool discovery completed.
    /// Unlike limit/collision diagnostics, these failures are worth retrying
    /// when Zed reloads a live session.
    pub fn had_connection_failures(&self) -> bool {
        self.had_connection_failures
    }

    /// Number of connected servers (clients that contributed ≥1 tool).
    #[cfg(test)]
    pub fn server_count(&self) -> usize {
        self.runtime
            .try_read()
            .expect("test must not inspect bridge during a call/shutdown")
            .leases
            .len()
    }

    /// Whether `name` is a remote tool this bridge serves.
    #[cfg(test)]
    pub fn handles(&self, name: &str) -> bool {
        self.runtime
            .try_read()
            .expect("test must not inspect bridge during a call/shutdown")
            .routes
            .contains_key(name)
    }

    /// Dispatch a remote tool call. Returns `None` if `name` is not a remote
    /// tool (so the caller falls through to native handling); `Some` with an
    /// error outcome when the call itself fails or times out.
    pub async fn call(&self, name: &str, input: &Value) -> Option<RemoteToolOutcome> {
        // Hold a read lease through the request. Shutdown takes the write lock,
        // so it waits for in-flight calls, clears routes, then releases clients.
        let runtime = self.runtime.read().await;
        let route = runtime.routes.get(name)?;
        let client = &runtime.leases.get(route.lease)?.client;
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

    /// Release every pooled-client lease. The last release shuts down the MCP
    /// runtime and (for stdio) reaps the child process. Idempotent.
    pub async fn shutdown(&self) {
        let leases = {
            let mut runtime = self.runtime.write().await;
            runtime.routes.clear();
            std::mem::take(&mut runtime.leases)
        };
        future::join_all(leases.into_iter().map(|lease| self.pool.release(lease))).await;
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

/// Connect to one server and list its tools. `init_timeout` is a single
/// handshake deadline covering `start()` (initialize) *and* `tools/list`, so N
/// slow/non-responsive servers cannot add up to `N * 2 * init_timeout` and
/// wedge `session/new` (ADR-003 D3 fail-open). On any failure *after* the
/// client is created, the client is shut down before returning so a spawned
/// stdio child and its runtime tasks cannot leak.
async fn connect(
    spec: &ServerSpec,
    cfg: &AcpMcpConfig,
    init_timeout: Duration,
) -> Result<(Arc<ClientRuntime>, Vec<rust_mcp_sdk::schema::Tool>), String> {
    let client = create_client(spec, cfg)?;
    let deadline = tokio::time::Instant::now() + init_timeout;
    match handshake(&client, deadline).await {
        Ok(tools) => Ok((client, tools)),
        Err(e) => {
            // Fail-open must not leak: tear down whatever start() spawned.
            let shutdown_timeout = Duration::from_secs(cfg.shutdown_timeout_secs);
            let mut shutdown_task = tokio::spawn(async move {
                let _ = client.shut_down().await;
            });
            // Timeout bounds the caller, not cleanup: dropping JoinHandle
            // detaches the task so it continues reaping a slow stdio child.
            let _ = tokio::time::timeout(shutdown_timeout, &mut shutdown_task).await;
            Err(e)
        }
    }
}

/// Initialize then list tools against an already-created client, both bounded
/// by a single shared `deadline`.
async fn handshake(
    client: &Arc<ClientRuntime>,
    deadline: tokio::time::Instant,
) -> Result<Vec<rust_mcp_sdk::schema::Tool>, String> {
    tokio::time::timeout_at(deadline, client.clone().start())
        .await
        .map_err(|_| "initialize timed out".to_string())?
        .map_err(|e| format!("initialize failed: {e}"))?;
    let result = tokio::time::timeout_at(deadline, client.request_tool_list(None))
        .await
        .map_err(|_| "tools/list timed out".to_string())?
        .map_err(|e| format!("tools/list failed: {e}"))?;
    Ok(result.tools)
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
    let raw = format!("{REMOTE_TOOL_PREFIX}{server}__{tool}");
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

    #[test]
    fn server_key_ignores_name_and_map_insertion_order() {
        let first = ServerSpec::Stdio {
            name: "linear-primary".to_string(),
            command: "server".to_string(),
            args: vec!["--stdio".to_string()],
            env: HashMap::from([
                ("TOKEN".to_string(), "secret".to_string()),
                ("MODE".to_string(), "prod".to_string()),
            ]),
        };
        let second = ServerSpec::Stdio {
            name: "linear-alias".to_string(),
            command: "server".to_string(),
            args: vec!["--stdio".to_string()],
            env: HashMap::from([
                ("MODE".to_string(), "prod".to_string()),
                ("TOKEN".to_string(), "secret".to_string()),
            ]),
        };
        assert!(ServerKey::from(&first) == ServerKey::from(&second));

        let different_credentials = ServerSpec::Stdio {
            name: "linear-primary".to_string(),
            command: "server".to_string(),
            args: vec!["--stdio".to_string()],
            env: HashMap::from([
                ("TOKEN".to_string(), "different".to_string()),
                ("MODE".to_string(), "prod".to_string()),
            ]),
        };
        assert!(ServerKey::from(&first) != ServerKey::from(&different_credentials));
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
        assert!(bridge.had_connection_failures());
        assert!(
            bridge
                .diagnostics()
                .iter()
                .any(|message| message.contains("broken") && message.contains("failed")),
            "failed server must remain visible to the ACP frontend"
        );
    }

    #[tokio::test]
    async fn limit_diagnostic_is_not_a_retryable_connection_failure() {
        let Some(bin) = daimonos_binary() else {
            eprintln!("skipping: daimonos binary not found next to test exe");
            return;
        };
        let workspace = tempfile::tempdir().unwrap();
        let cfg = AcpMcpConfig {
            max_servers: 1,
            ..AcpMcpConfig::default()
        };
        let spec = |name: &str| ServerSpec::Stdio {
            name: name.to_string(),
            command: bin.to_string_lossy().into_owned(),
            args: vec![
                "--mcp".to_string(),
                "-w".to_string(),
                workspace.path().to_string_lossy().into_owned(),
            ],
            env: HashMap::from([("SERVER_NAME".to_string(), name.to_string())]),
        };
        let bridge = McpBridge::build(
            vec![spec("connected"), spec("skipped")],
            &cfg,
            &native_set(&[]),
            None,
            None,
        )
        .await;
        assert!(!bridge.diagnostics().is_empty());
        assert!(!bridge.had_connection_failures());
        bridge.shutdown().await;
    }

    #[tokio::test]
    async fn build_times_out_on_unresponsive_server_within_single_deadline() {
        // A process that launches but never speaks MCP (sleep) must time out on
        // the initialize handshake, be cleaned up, and be skipped — without the
        // whole build taking multiple init_timeouts (#990 review: single
        // handshake deadline covering initialize + tools/list).
        if which_sleep().is_none() {
            eprintln!("skipping: no `sleep` binary");
            return;
        }
        let cfg = AcpMcpConfig {
            init_timeout_secs: 1,
            ..AcpMcpConfig::default()
        };
        let spec = ServerSpec::Stdio {
            name: "asleep".to_string(),
            command: "sleep".to_string(),
            args: vec!["30".to_string()],
            env: HashMap::new(),
        };
        let start = std::time::Instant::now();
        let bridge = McpBridge::build(vec![spec], &cfg, &native_set(&[]), None, None).await;
        let elapsed = start.elapsed();
        assert!(bridge.tools().is_empty());
        assert_eq!(bridge.server_count(), 0);
        // One server, single ~1s deadline: comfortably under 5s. A regression
        // to per-call timeouts (initialize + list) would still pass here, but a
        // regression that hangs on the child (no cleanup) would blow this.
        assert!(
            elapsed < Duration::from_secs(5),
            "build should be bounded by the handshake deadline, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn build_connects_unresponsive_servers_with_bounded_concurrency() {
        if which_sleep().is_none() {
            eprintln!("skipping: no `sleep` binary");
            return;
        }
        let cfg = AcpMcpConfig {
            init_timeout_secs: 2,
            max_concurrent_connects: 3,
            ..AcpMcpConfig::default()
        };
        let specs = (0..3)
            .map(|index| ServerSpec::Stdio {
                name: format!("asleep-{index}"),
                command: "sleep".to_string(),
                args: vec!["30".to_string()],
                // Distinct inert env values make these distinct transport
                // configs. Identical configs are intentionally serialized and
                // deduplicated by the shared pool (#1008).
                env: HashMap::from([("TEST_SERVER_INDEX".to_string(), index.to_string())]),
            })
            .collect();

        let start = std::time::Instant::now();
        let bridge = McpBridge::build(specs, &cfg, &native_set(&[]), None, None).await;
        let elapsed = start.elapsed();

        assert!(bridge.tools().is_empty());
        assert_eq!(bridge.server_count(), 0);
        // Three sequential two-second handshakes take at least six seconds.
        // With fan-out three they share one timeout window. A five-second bound
        // leaves roughly three seconds of CI scheduling headroom over the
        // expected concurrent runtime while still rejecting the old behavior.
        assert!(
            elapsed < Duration::from_secs(5),
            "three handshakes should run concurrently, took {elapsed:?}"
        );
    }

    fn which_sleep() -> Option<()> {
        std::process::Command::new("sleep")
            .arg("0")
            .status()
            .ok()
            .filter(|s| s.success())
            .map(|_| ())
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

    #[tokio::test]
    async fn partial_connection_failure_keeps_healthy_server_and_diagnostic() {
        let Some(bin) = daimonos_binary() else {
            eprintln!("skipping: daimonos binary not found next to test exe");
            return;
        };
        let workspace = tempfile::tempdir().unwrap();
        let broken = ServerSpec::Stdio {
            name: "broken".to_string(),
            command: "daimonos-nonexistent-binary-xyz".to_string(),
            args: vec![],
            env: HashMap::new(),
        };
        let healthy = ServerSpec::Stdio {
            name: "healthy".to_string(),
            command: bin.to_string_lossy().into_owned(),
            args: vec![
                "--mcp".to_string(),
                "-w".to_string(),
                workspace.path().to_string_lossy().into_owned(),
            ],
            env: HashMap::new(),
        };

        let bridge = McpBridge::build(
            vec![broken, healthy],
            &AcpMcpConfig::default(),
            &native_set(&[]),
            None,
            None,
        )
        .await;

        assert!(bridge.handles("mcp__healthy__read_file"));
        assert!(bridge
            .diagnostics()
            .iter()
            .any(|message| message.contains("broken")));
        bridge.shutdown().await;
    }

    #[tokio::test]
    async fn shared_pool_deduplicates_concurrent_bridges_and_releases_last_lease() {
        let Some(bin) = daimonos_binary() else {
            eprintln!("skipping: daimonos binary not found next to test exe");
            return;
        };
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("pooled-marker.txt"), "shared").unwrap();
        let spec = |name: &str| ServerSpec::Stdio {
            name: name.to_string(),
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
            shared_pool_enabled: true,
            ..AcpMcpConfig::default()
        };
        let pool = McpClientPool::new();
        let native = native_set(&[]);

        let (first, second) = tokio::join!(
            McpBridge::build_with_pool(
                vec![spec("first")],
                &cfg,
                &native,
                None,
                Some("session-first".to_string()),
                pool.clone(),
            ),
            McpBridge::build_with_pool(
                vec![spec("second")],
                &cfg,
                &native,
                None,
                Some("session-second".to_string()),
                pool.clone(),
            )
        );

        assert_eq!(pool.entry_count().await, 1);
        assert_eq!(pool.lease_count().await, 2);
        assert_eq!(first.server_count(), 1);
        assert_eq!(second.server_count(), 1);
        assert!(first.handles("mcp__first__ls"));
        assert!(second.handles("mcp__second__ls"));

        first.shutdown().await;
        assert_eq!(pool.entry_count().await, 1);
        assert_eq!(pool.lease_count().await, 1);
        let outcome = second
            .call("mcp__second__ls", &serde_json::json!({}))
            .await
            .expect("second bridge should still route");
        assert!(
            !outcome.is_error && outcome.content.contains("pooled-marker.txt"),
            "remaining lease should keep shared client alive: {}",
            outcome.content
        );

        second.shutdown().await;
        assert_eq!(pool.entry_count().await, 0);
        assert_eq!(pool.lease_count().await, 0);
        assert!(second
            .call("mcp__second__ls", &serde_json::json!({}))
            .await
            .is_none());
        // Idempotent repeated teardown must not underflow lease counts.
        second.shutdown().await;
        assert_eq!(pool.entry_count().await, 0);
    }

    #[tokio::test]
    async fn shared_pool_does_not_cache_failed_connections() {
        let cfg = AcpMcpConfig {
            init_timeout_secs: 1,
            shared_pool_enabled: true,
            ..AcpMcpConfig::default()
        };
        let pool = McpClientPool::new();
        let bad = ServerSpec::Stdio {
            name: "broken".to_string(),
            command: "daimonos-nonexistent-binary-xyz".to_string(),
            args: vec![],
            env: HashMap::new(),
        };

        let bridge =
            McpBridge::build_with_pool(vec![bad], &cfg, &native_set(&[]), None, None, pool.clone())
                .await;

        assert_eq!(bridge.server_count(), 0);
        assert_eq!(pool.entry_count().await, 0);
        assert_eq!(pool.lease_count().await, 0);

        // A later acquisition prunes stale Weak slots before inserting its
        // own, so repeated failures across distinct configs remain bounded.
        let other_bad = ServerSpec::Stdio {
            name: "also-broken".to_string(),
            command: "another-nonexistent-binary-xyz".to_string(),
            args: vec![],
            env: HashMap::new(),
        };
        let other_bridge = McpBridge::build_with_pool(
            vec![other_bad],
            &cfg,
            &native_set(&[]),
            None,
            None,
            pool.clone(),
        )
        .await;
        assert_eq!(other_bridge.server_count(), 0);
        assert!(pool.slot_count().await <= 1);
    }

    #[tokio::test]
    async fn shared_pool_config_opt_out_uses_private_pool() {
        let Some(bin) = daimonos_binary() else {
            eprintln!("skipping: daimonos binary not found next to test exe");
            return;
        };
        let workspace = tempfile::tempdir().unwrap();
        let spec = ServerSpec::Stdio {
            name: "isolated".to_string(),
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
            shared_pool_enabled: false,
            ..AcpMcpConfig::default()
        };
        let process_pool = McpClientPool::new();

        let bridge = McpBridge::build_with_pool(
            vec![spec],
            &cfg,
            &native_set(&[]),
            None,
            None,
            process_pool.clone(),
        )
        .await;

        assert_eq!(bridge.server_count(), 1);
        assert_eq!(process_pool.entry_count().await, 0);
        assert_eq!(bridge.pool.entry_count().await, 1);
        bridge.shutdown().await;
        assert_eq!(bridge.pool.entry_count().await, 0);
    }

    #[tokio::test]
    async fn concurrent_build_registers_tools_in_forwarded_order() {
        let Some(bin) = daimonos_binary() else {
            eprintln!("skipping: daimonos binary not found next to test exe");
            return;
        };
        if std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .status()
            .map_or(true, |status| !status.success())
        {
            eprintln!("skipping: no `sh` binary");
            return;
        }
        let first_workspace = tempfile::tempdir().unwrap();
        let second_workspace = tempfile::tempdir().unwrap();
        std::fs::write(first_workspace.path().join("marker.txt"), "first").unwrap();
        std::fs::write(second_workspace.path().join("marker.txt"), "second").unwrap();

        // Delay the first forwarded server so the second finishes connecting
        // first. Registration must still follow vector order.
        let first = ServerSpec::Stdio {
            name: "duplicate".to_string(),
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "sleep 1; exec \"$1\" --mcp -w \"$2\"".to_string(),
                "sh".to_string(),
                bin.to_string_lossy().into_owned(),
                first_workspace.path().to_string_lossy().into_owned(),
            ],
            env: HashMap::new(),
        };
        let second = ServerSpec::Stdio {
            name: "duplicate".to_string(),
            command: bin.to_string_lossy().into_owned(),
            args: vec![
                "--mcp".to_string(),
                "-w".to_string(),
                second_workspace.path().to_string_lossy().into_owned(),
            ],
            env: HashMap::new(),
        };
        let cfg = AcpMcpConfig {
            init_timeout_secs: 30,
            call_timeout_secs: 30,
            max_concurrent_connects: 2,
            ..AcpMcpConfig::default()
        };
        let bridge =
            McpBridge::build(vec![first, second], &cfg, &native_set(&[]), None, None).await;

        assert_eq!(bridge.server_count(), 2);
        let first_outcome = bridge
            .call(
                "mcp__duplicate__read_file",
                &serde_json::json!({"path": "marker.txt"}),
            )
            .await
            .expect("base route should exist");
        let second_outcome = bridge
            .call(
                "mcp__duplicate__read_file__2",
                &serde_json::json!({"path": "marker.txt"}),
            )
            .await
            .expect("suffixed route should exist");
        assert!(
            first_outcome.content.contains("first"),
            "base route must remain assigned to first forwarded server: {}",
            first_outcome.content
        );
        assert!(
            second_outcome.content.contains("second"),
            "suffix route must remain assigned to second forwarded server: {}",
            second_outcome.content
        );

        bridge.shutdown().await;
    }
}
