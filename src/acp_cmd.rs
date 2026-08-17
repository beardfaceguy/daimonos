//! `daimonos acp` — a native Agent Client Protocol engine (vikunja #954),
//! so Zed (and other ACP editors) can drive daimonos directly over stdio
//! instead of through the MCP adapter.
//!
//! Scope (v1): one active session at a time, text-only prompts, tool-call
//! lifecycle + permission requests + live usage reporting via
//! `session/update`. Cancellable via `session/cancel`. Multiple concurrent
//! sessions (Zed keeps one process across chat threads) and `session/load`
//! (thread refocus / reopen) are supported — the latter replays history from
//! memory when the session is still live, or from on-disk persistence after
//! a process restart (mirroring how Zed's native providers restore history).
//! Out of scope: the `fs/*`/`terminal/*` client-proxy methods — daimonos has
//! its own file/exec tools and doesn't need to shell out through the client.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context as TaskContext, Poll};

use agent_client_protocol::schema::v1::{
    AgentCapabilities, AvailableCommand, AvailableCommandsUpdate, CancelNotification,
    ContentBlock as AcpContentBlock, ContentChunk, Cost as AcpCost, DeleteSessionRequest,
    DeleteSessionResponse, Diff as AcpDiff, EmbeddedResourceResource, ImageContent,
    InitializeRequest, InitializeResponse, ListSessionsRequest, ListSessionsResponse,
    LoadSessionRequest, LoadSessionResponse, McpCapabilities, McpServer, Meta, NewSessionRequest,
    NewSessionResponse, PermissionOption, PermissionOptionKind, Plan as AcpPlan,
    PlanEntry as AcpPlanEntry, PlanEntryPriority as AcpPlanPriority,
    PlanEntryStatus as AcpPlanStatus, PromptCapabilities, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, SessionCapabilities, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelectOption, SessionDeleteCapabilities, SessionId,
    SessionInfo, SessionListCapabilities, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason as AcpStopReason,
    TextContent, ToolCall, ToolCallContent as AcpToolCallContent, ToolCallLocation, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind, UsageUpdate,
};
use agent_client_protocol::{
    Agent as AcpAgentRole, ByteStreams, Client as AcpClientRole, ConnectTo, ConnectionTo, Dispatch,
    JsonRpcRequest, JsonRpcResponse,
};
use futures_util::io::AsyncRead;
use serde::{Deserialize, Serialize};
use tracing::Instrument;

use crate::agent::{
    parse_plan_entries, AfterHook, AfterHookResult, AgentConfig, AgentSession, BeforeHook,
    BeforeHookResult, PlanEntry as AgentPlanEntry, PlanHook, PlanPriority, PlanStatus,
    RemoteToolHook, RemoteToolResult, TokenLogConfig, ToolCallInfo, ToolProgressHook, TurnResult,
    UPDATE_PLAN_TOOL,
};
use crate::analytics::AnalyticsStore;
use crate::config::Config;
use crate::mcp_bridge::{McpBridge, McpClientPool, ServerSpec};
use crate::observability::{PromptMetadata, PromptSpan};
use crate::providers::{
    CompleteOpts, ContentBlock as CoreBlock, LlmProvider, Message as CoreMessage, Role as CoreRole,
    StreamEvent, ToolSchema, Usage,
};
use crate::session::Session;
use crate::session_core::{
    align_client_user_message_ids, canonical_assistant_outcome_with_logging, tool_call_title,
    ApprovalBroker, CanonicalToolLifecycle, SessionCompaction, SessionCore, SessionEventHandler,
    SessionEventRouter, SessionPersistence, SessionPromptError, SessionPromptOutcome, TurnError,
};
#[cfg(test)]
use crate::session_core::{
    canonical_assistant_outcome, log_raw_provider_error, safe_provider_error_message,
    sanitize_provider_error, RAW_ERROR_LOG_CAP,
};
use crate::session_protocol::{
    ApprovalDecision as CoreApprovalDecision, ApprovalRequest as CoreApprovalRequest,
    AssistantOutcome as CoreAssistantOutcome, ClientCapability, RuntimeChoice as CoreRuntimeChoice,
    RuntimeOption as CoreRuntimeOption, RuntimeValue as CoreRuntimeValue,
    SessionEvent as CoreSessionEvent,
};
use crate::session_store::{SessionStore, SessionSummary};
use crate::tool_facade;

/// Tool-call ids announced to the ACP client and not yet completed. Every
/// `ToolCallUpdate` path consults this set so cancellation, replay anomalies,
/// or a late progress callback cannot update an id the client does not know
/// (or resurrect a call that already reached a terminal state).
#[derive(Clone)]
struct ActiveToolCall {
    terminal_output: bool,
    raw_input: Option<serde_json::Value>,
}

impl ActiveToolCall {
    fn new(info: &ToolCallInfo, terminal_output: bool, retain_raw_input: bool) -> Self {
        Self {
            terminal_output,
            raw_input: retain_raw_input.then(|| info.input.clone()),
        }
    }
}

type ActiveToolCalls = Arc<StdMutex<HashMap<String, ActiveToolCall>>>;

/// ACP presentation hooks read the live client connection for rich
/// notifications. In agent-client-protocol 1.2 every handler receives a clone
/// of the same underlying connection; request UUIDs and oneshot channels route
/// responses independently of the handler dispatch that supplied the clone.
/// Canonical tool and approval lifecycles do not depend on this adapter handle.
///
/// **Invariant: the `Option` is never `None` in practice.** It is written
/// `Some(..)` when the session is built and re-armed `Some(..)` at the top of
/// every prompt dispatch; no code path clears it. #1107 fix #2 kept it that way
/// on purpose when it coupled prompt lifetime to transport lifetime — teardown
/// cancels turns and retires approvals/tool calls but leaves this slot armed,
/// so notifications to the dead pipe just fail and are discarded.
///
/// Do not clear it to `None` without first reading the `current_cx` early
/// return in `build_after_tool_call_hook`: that branch is unreachable only
/// because of this invariant, and it is not safe if the invariant is dropped.
type CurrentConnection = Arc<StdMutex<Option<ConnectionTo<AcpClientRole>>>>;

/// The ACP JSON-RPC input descriptor. Named rather than inlined because it is
/// both read from and repaired (see `clear_fd_nonblocking`).
const ACP_STDIN_FD: i32 = 0;

/// Blocking stdout adapter for ACP that treats `WouldBlock`/EAGAIN as
/// transient pipe backpressure instead of a fatal transport error. Zed tears
/// down the entire agent when an output transport error escapes. Retry on the
/// blocking worker (never the async executor), with a small sleep to avoid
/// spinning. Other errors retain their kind and gain directional context.
///
/// The stdin counterpart is `ResilientReader`, which shares this backoff but
/// retries without a bound. Stdin must remain an indefinite blocking read while
/// the agent is idle, so a *bounded* retry there would incorrectly time out a
/// healthy idle session — which is why stdin was originally left unprotected.
struct ResilientWriter<T> {
    inner: T,
    direction: &'static str,
    /// With capped exponential backoff, 100 attempts is roughly five seconds.
    /// Prevents a permanently blocked fd from hanging ACP forever.
    max_would_block_attempts: u64,
}

impl<T> ResilientWriter<T> {
    fn new(inner: T, direction: &'static str) -> Self {
        Self {
            inner,
            direction,
            max_would_block_attempts: 100,
        }
    }

    #[cfg(test)]
    fn with_retry_limit(inner: T, direction: &'static str, limit: u64) -> Self {
        Self {
            inner,
            direction,
            max_would_block_attempts: limit.max(1),
        }
    }

    fn contextual_error(&self, operation: &str, error: std::io::Error) -> std::io::Error {
        std::io::Error::new(
            error.kind(),
            format!("ACP {} {operation} failed: {error}", self.direction),
        )
    }

    fn note_would_block(&self, operation: &str, attempts: u64) -> std::io::Result<()> {
        note_stdio_would_block(
            self.direction,
            operation,
            attempts,
            Some(self.max_would_block_attempts),
        )
    }
}

/// Shared `WouldBlock` backoff for the blocking ACP stdio adapters. Sleeps on
/// the calling thread, which is always a `blocking::Unblock` worker and never
/// the async executor. `max_attempts: None` retries indefinitely.
fn note_stdio_would_block(
    direction: &'static str,
    operation: &str,
    attempts: u64,
    max_attempts: Option<u64>,
) -> std::io::Result<()> {
    if attempts == 1 {
        // A single pipe-backpressure retry is expected under bursts; keep
        // it out of warn-level logs.
        tracing::debug!(
            target: "daimonos::acp",
            event = "stdio_would_block_retry",
            direction,
            operation,
            attempts,
        );
    } else if attempts.is_multiple_of(25) && max_attempts.is_none_or(|max| attempts < max) {
        tracing::warn!(
            target: "daimonos::acp",
            event = "stdio_would_block_sustained",
            direction,
            operation,
            attempts,
        );
    }
    if max_attempts.is_some_and(|max| attempts >= max) {
        tracing::error!(
            target: "daimonos::acp",
            event = "stdio_would_block_timeout",
            direction,
            operation,
            attempts,
        );
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("ACP {direction} {operation} remained blocked after {attempts} attempts"),
        ));
    }
    // Exponential backoff (1,2,4,…), capped at 50ms. This absorbs short
    // bursts cheaply without tight polling during sustained backpressure.
    let shift = attempts.saturating_sub(1).min(6) as u32;
    let delay_ms = (1u64 << shift).min(50);
    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
    Ok(())
}

impl<T: std::io::Write> std::io::Write for ResilientWriter<T> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let mut attempts = 0u64;
        loop {
            match self.inner.write(buffer) {
                // Empty writes are required to succeed immediately.
                Ok(0) if buffer.is_empty() => return Ok(0),
                // A zero-length write for a nonempty buffer is transient
                // backpressure for this pipe transport. Letting it escape makes
                // write_all convert it to fatal WriteZero.
                Ok(0) => {
                    attempts = attempts.saturating_add(1);
                    self.note_would_block("write_zero", attempts)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    attempts = attempts.saturating_add(1);
                    self.note_would_block("write", attempts)?;
                }
                Err(error) => return Err(self.contextual_error("write", error)),
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut attempts = 0u64;
        loop {
            match self.inner.flush() {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    attempts = attempts.saturating_add(1);
                    self.note_would_block("flush", attempts)?;
                }
                Err(error) => return Err(self.contextual_error("flush", error)),
                result => return result,
            }
        }
    }
}

/// Blocking stdin adapter for ACP. Mirrors `ResilientWriter` with two
/// deliberate differences: retries are unbounded, because an idle session
/// legitimately has nothing to read and any bound would kill healthy sessions;
/// and every `WouldBlock` first tries to clear `O_NONBLOCK` from the underlying
/// descriptor.
///
/// A blocking read only reports EAGAIN when fd 0 is in non-blocking mode, which
/// happens when a child process inherits it and flips the flag on the shared
/// open file description without restoring it. Child stdin is now nulled at
/// every spawn site, so that should no longer occur; recovering here keeps one
/// stray child from taking down the transport anyway.
struct ResilientReader<T> {
    inner: T,
    direction: &'static str,
    /// Descriptor to un-poison on `WouldBlock`. `None` disables recovery, which
    /// is what tests over in-memory readers want.
    recover_fd: Option<i32>,
}

impl<T> ResilientReader<T> {
    fn new(inner: T, direction: &'static str, recover_fd: Option<i32>) -> Self {
        Self {
            inner,
            direction,
            recover_fd,
        }
    }
}

impl<T: std::io::Read> std::io::Read for ResilientReader<T> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let mut attempts = 0u64;
        loop {
            match self.inner.read(buffer) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    attempts = attempts.saturating_add(1);
                    if let Some(fd) = self.recover_fd {
                        if clear_fd_nonblocking(fd).unwrap_or(false) {
                            tracing::warn!(
                                target: "daimonos::acp",
                                event = "stdio_nonblocking_cleared",
                                direction = self.direction,
                                fd,
                                attempts,
                            );
                        }
                    }
                    note_stdio_would_block(self.direction, "read", attempts, None)?;
                }
                Err(error) => {
                    return Err(std::io::Error::new(
                        error.kind(),
                        format!("ACP {} read failed: {error}", self.direction),
                    ));
                }
                result => return result,
            }
        }
    }
}

/// Clear `O_NONBLOCK` from `fd`, reporting whether the flag had been set.
/// Blocking reads on a non-blocking descriptor fail with EAGAIN, so a child that
/// poisoned an inherited descriptor would otherwise break the ACP transport.
#[cfg(unix)]
fn clear_fd_nonblocking(fd: i32) -> std::io::Result<bool> {
    // SAFETY: F_GETFL/F_SETFL take an int argument and touch no memory we own;
    // `fd` is a descriptor this process already holds open.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if flags & libc::O_NONBLOCK == 0 {
        return Ok(false);
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(true)
}

#[cfg(not(unix))]
fn clear_fd_nonblocking(_fd: i32) -> std::io::Result<bool> {
    Ok(false)
}

struct EofAwareReader<R> {
    inner: R,
    eof: Arc<tokio::sync::Notify>,
    input_error: Arc<StdMutex<Option<String>>>,
    notified: bool,
}

impl<R> EofAwareReader<R> {
    fn new(
        inner: R,
        eof: Arc<tokio::sync::Notify>,
        input_error: Arc<StdMutex<Option<String>>>,
    ) -> Self {
        Self {
            inner,
            eof,
            input_error,
            notified: false,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for EofAwareReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buffer: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_read(cx, buffer);
        if matches!(result, Poll::Ready(Ok(0) | Err(_))) && !self.notified {
            self.notified = true;
            if let Poll::Ready(Err(error)) = &result {
                *self
                    .input_error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error.to_string());
            }
            // `Notify::notify_one` stores a permit even if input closes before
            // the select begins polling its wait branch.
            self.eof.notify_one();
        }
        result
    }
}

/// Builds a fresh `LlmProvider` for a new session. `LlmProvider` isn't
/// `Clone`, and Zed keeps one `daimonos acp` process alive across multiple
/// sessions (new chat threads), so we can't move a single provider into one
/// session — each `session/new` constructs its own.
pub type ProviderFactory = Arc<dyn Fn() -> Result<Box<dyn LlmProvider>, String> + Send + Sync>;

/// Per-session state. Each session gets its own session lock, cancel slot,
/// connection cell, and current-model cell, so concurrent sessions (Zed can
/// run several chat threads against one process) never block or cross-talk
/// with each other. Shared via `Arc` so a long prompt turn holds only this
/// handle — not the sessions-map lock.
struct SessionHandle {
    core: SessionCore,
    active_tool_calls: ActiveToolCalls,
    connection: CurrentConnection,
    /// Per-session bridge to Zed-forwarded MCP servers (ADR-003). Empty when
    /// the bridge is disabled or no servers were forwarded. Shut down on
    /// session/delete.
    bridge: BridgeSlot,
    mcp_specs: tokio::sync::Mutex<Vec<ServerSpec>>,
}

type BridgeSlot = Arc<tokio::sync::RwLock<Arc<McpBridge>>>;

/// Shared engine state across all sessions on one process.
struct AcpState {
    /// Active sessions keyed by id. The map lock is held only briefly to
    /// look up / insert a handle — never across a prompt turn.
    sessions: tokio::sync::Mutex<HashMap<SessionId, Arc<SessionHandle>>>,
    /// Per-id single-flight locks for load/delete, including persisted
    /// sessions that do not yet have a live `SessionHandle`.
    session_operations:
        tokio::sync::Mutex<HashMap<SessionId, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    /// Builds a provider per new session (see [`ProviderFactory`]).
    make_provider: ProviderFactory,
    /// Candidate models for the picker (from `DAIMONOS_AGENT_MODELS`);
    /// always non-empty (includes the active model).
    models: Vec<String>,
    /// The model a new session starts on.
    default_model: String,
    /// On-disk session persistence for cross-process `session/load` resume.
    /// `None` disables persistence (used by tests that don't exercise it).
    store: Option<SessionStore>,
    /// Whether the configured provider adapter can serialize image prompts.
    supports_images: bool,
    /// Zed's `_meta.terminal_output` extension, negotiated at initialize.
    supports_terminal_output: AtomicBool,
    /// Maximum sessions returned by one session/list response.
    session_list_page_size: usize,
    /// Context/window compaction configuration cloned into each session.
    compaction: SessionCompaction,
    /// Analytics store for attributing remote MCP tool calls (ADR-003).
    /// `None` when analytics is disabled.
    analytics: Option<Arc<AnalyticsStore>>,
    /// When true, each prompt turn is prefixed with a real system-clock
    /// timestamp line in the thread (#1070, `DAIMONOS_AGENT_TIMESTAMP_TURNS`).
    timestamp_turns: bool,
    /// Reasoning effort for every prompt turn (#1122,
    /// `DAIMONOS_AGENT_THINKING`; default Medium). Cloned into each session's
    /// `CompleteOpts.thinking` via `build_agent_config`.
    thinking: crate::providers::ThinkingLevel,
    /// Process-wide pool for deduplicating identical forwarded MCP server
    /// configs across ACP sessions (#1008). Bridges retain explicit leases.
    mcp_pool: McpClientPool,
    /// Overall deadline for concurrently draining every session's MCP bridge
    /// after the ACP transport disappears (#1293).
    mcp_teardown_timeout: std::time::Duration,
}

async fn session_operation_lock(
    state: &AcpState,
    session_id: &SessionId,
) -> Arc<tokio::sync::Mutex<()>> {
    let mut operations = state.session_operations.lock().await;
    operations.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = operations
        .get(session_id)
        .and_then(std::sync::Weak::upgrade)
    {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    operations.insert(session_id.clone(), Arc::downgrade(&lock));
    lock
}

fn request_session_cancel(handle: &SessionHandle) {
    // `cancel_turn` emits `Cancelling` before signalling, so it is ordered ahead
    // of the terminal event the unwinding turn's `Drop` produces.
    let _ = handle.core.cancel_turn();
}

/// The `SessionConfigId` for the model picker option.
const MODEL_CONFIG_ID: &str = "model";
const SESSION_END_REASON_DELETED: &str = "deleted";
const SESSION_END_REASON_ENGINE_SHUTDOWN: &str = "engine_shutdown";
const REFUSAL_DIAGNOSTIC: &str = "Provider refused the request based on content policy.";
const CLIENT_USER_MESSAGE_IDS_META_KEY: &str = "zed.dev/clientUserMessageIds";
const SESSION_RETRY_META_KEY: &str = "zed.dev/sessionRetry";
const SESSION_TRUNCATE_META_KEY: &str = "zed.dev/sessionTruncate";
const CLIENT_USER_MESSAGE_ID_META_KEY: &str = "zed.dev/clientUserMessageId";

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[serde(rename_all = "camelCase")]
#[request(method = "_zed/session/retry", response = PromptResponse)]
struct RetrySessionRequest {
    session_id: SessionId,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[serde(rename_all = "camelCase")]
#[request(method = "_zed/session/truncate", response = TruncateSessionResponse)]
struct TruncateSessionRequest {
    session_id: SessionId,
    client_user_message_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
struct TruncateSessionResponse {}

const ACP_HELP_TEXT: &str = "\
Commands:
  /clear   reset conversation history (cumulative usage is kept)
  /usage   show cumulative token usage for this session
  /help    show this message";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcpCommand {
    Clear,
    Usage,
    Help,
}

fn parse_acp_command(text: &str) -> Option<AcpCommand> {
    match text.trim() {
        "/clear" => Some(AcpCommand::Clear),
        "/usage" => Some(AcpCommand::Usage),
        "/help" => Some(AcpCommand::Help),
        _ => None,
    }
}

fn available_commands() -> Vec<AvailableCommand> {
    vec![
        AvailableCommand::new(
            "clear",
            "Reset conversation history; cumulative usage is kept",
        ),
        AvailableCommand::new("usage", "Show cumulative token usage and cost"),
        AvailableCommand::new("help", "Show available Daimonos commands"),
    ]
}

fn send_available_commands(cx: &ConnectionTo<AcpClientRole>, session_id: &SessionId) {
    send_notification(
        cx,
        session_id,
        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(available_commands())),
    );
}

fn send_mcp_diagnostics(
    cx: &ConnectionTo<AcpClientRole>,
    session_id: &SessionId,
    bridge: &McpBridge,
) {
    for diagnostic in bridge.diagnostics() {
        let chunk = ContentChunk::new(AcpContentBlock::Text(TextContent::new(format!(
            "[MCP bridge: {diagnostic}]"
        ))));
        send_notification(cx, session_id, SessionUpdate::AgentThoughtChunk(chunk));
    }
}

fn run_acp_command(command: AcpCommand, session: &mut AgentSession) -> String {
    match command {
        AcpCommand::Clear => {
            session.clear();
            "[history cleared]".to_string()
        }
        AcpCommand::Usage => {
            let usage = session.total_usage();
            format!(
                "input={} output={} cache_read={} cache_write={} cost=${:.4}",
                usage.input,
                usage.output,
                usage.cache_read,
                usage.cache_write,
                usage.cost.total_usd
            )
        }
        AcpCommand::Help => ACP_HELP_TEXT.to_string(),
    }
}

fn session_info(summary: SessionSummary, fallback_cwd: &Path) -> SessionInfo {
    let cwd = summary.cwd.unwrap_or_else(|| fallback_cwd.to_path_buf());
    let title = summary.first_user_line.filter(|line| !line.is_empty());
    let updated_at = summary
        .updated_at
        .map(|time| chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339());
    SessionInfo::new(summary.id, cwd)
        .title(title)
        .updated_at(updated_at)
}

#[derive(Debug, PartialEq, Eq)]
struct SessionListCursor {
    updated_nanos: u128,
    id: String,
}

fn session_updated_nanos(summary: &SessionSummary) -> u128 {
    summary
        .updated_at
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos())
}

fn encode_session_list_cursor(summary: &SessionSummary) -> String {
    format!("v1:{}:{}", session_updated_nanos(summary), summary.id)
}

fn decode_session_list_cursor(cursor: &str) -> Result<SessionListCursor, String> {
    let mut parts = cursor.splitn(3, ':');
    let version = parts.next();
    let updated_nanos = parts.next();
    let id = parts.next();
    if version != Some("v1") || id.is_none_or(str::is_empty) {
        return Err("invalid session list cursor".to_string());
    }
    let updated_nanos = updated_nanos
        .and_then(|value| value.parse::<u128>().ok())
        .ok_or_else(|| "invalid session list cursor".to_string())?;
    Ok(SessionListCursor {
        updated_nanos,
        id: id.unwrap_or_default().to_string(),
    })
}

fn paginate_session_summaries(
    summaries: Vec<SessionSummary>,
    requested_cwd: Option<&Path>,
    cursor: Option<&str>,
    page_size: usize,
    fallback_cwd: &Path,
) -> Result<(Vec<SessionInfo>, Option<String>), String> {
    if page_size == 0 {
        return Err("ACP session list page size must be greater than zero".to_string());
    }
    let summaries: Vec<_> = summaries
        .into_iter()
        .filter(|summary| {
            requested_cwd
                .is_none_or(|requested| summary.cwd.as_deref().unwrap_or(fallback_cwd) == requested)
        })
        .collect();
    let start = if let Some(cursor) = cursor {
        let cursor = decode_session_list_cursor(cursor)?;
        summaries
            .iter()
            .position(|summary| {
                summary.id == cursor.id && session_updated_nanos(summary) == cursor.updated_nanos
            })
            .map(|index| index + 1)
            .ok_or_else(|| "session list cursor is stale".to_string())?
    } else {
        0
    };
    let end = start.saturating_add(page_size).min(summaries.len());
    let next_cursor =
        (end < summaries.len()).then(|| encode_session_list_cursor(&summaries[end - 1]));
    let sessions = summaries[start..end]
        .iter()
        .cloned()
        .map(|summary| session_info(summary, fallback_cwd))
        .collect();
    Ok((sessions, next_cursor))
}

/// Pre-edit file text captured by the before hook (keyed by tool-call id)
/// and consumed by the after hook to render write_file/edit_file
/// completions as ACP `Diff` content (vikunja #983). A `None` value means
/// the file did not exist (or could not be read) before the call.
type DiffStash = Arc<StdMutex<HashMap<String, Option<String>>>>;

/// Tools whose successful completion is rendered as an ACP `Diff` instead
/// of the raw JSON tool output (vikunja #983).
fn is_file_edit_tool(name: &str) -> bool {
    matches!(name, "write_file" | "edit_file")
}

/// Resolve a tool call's `path` argument the way `Session::resolve_path`
/// does for a session whose cwd is the workspace. Best-effort: if the
/// agent moved its cwd with `set_cwd`, a relative path may resolve
/// differently than the tool saw it — consumers degrade gracefully (diff
/// rendering falls back to plain text, follow-the-agent locations just
/// don't resolve to a buffer).
fn tool_target_path(workspace: &Path, input: &serde_json::Value) -> Option<PathBuf> {
    let path = PathBuf::from(input.get("path")?.as_str()?);
    Some(if path.is_absolute() {
        path
    } else {
        workspace.join(path)
    })
}

/// Locations for the client's "follow the agent" mode (vikunja #986):
/// file-oriented tool calls advertise the file (and line, when known) they
/// touch, so Zed can move its agent-location indicator there. Non-file
/// tools (and calls without a usable `path`) advertise none.
fn tool_call_locations(
    workspace: &Path,
    name: &str,
    input: &serde_json::Value,
) -> Vec<ToolCallLocation> {
    if !matches!(
        name,
        "read_file" | "write_file" | "edit_file" | "append_file" | "search"
    ) {
        return Vec::new();
    }
    let Some(path) = tool_target_path(workspace, input) else {
        return Vec::new();
    };
    // read_file's `offset` is a 0-based start line — the same base Zed
    // uses for `line` (it builds `Point::new(line, ...)` directly).
    let line = if name == "read_file" {
        input
            .get("offset")
            .and_then(|v| v.as_u64())
            .and_then(|line| u32::try_from(line).ok())
    } else {
        None
    };
    vec![ToolCallLocation::new(path).line(line)]
}

/// Reconstruct the full post-edit file text by replaying `edit_file`'s
/// applied `[old, new]` pairs on the pre-edit text — the same sequential
/// `replacen(old, new, 1)` the patch op performs. Returns `None` when a
/// pair doesn't match (the pre-edit capture raced another write or
/// resolved a different path than the tool did), signalling the caller to
/// fall back to plain-text rendering rather than show a wrong diff.
fn replay_edit_pairs(old_text: &str, pairs: &[serde_json::Value]) -> Option<String> {
    let mut text = old_text.to_string();
    for pair in pairs {
        let old = pair.get(0)?.as_str()?;
        let new = pair.get(1)?.as_str()?;
        if !text.contains(old) {
            return None;
        }
        text = text.replacen(old, new, 1);
    }
    Some(text)
}

/// Build the ACP `Diff` for a successfully completed write_file/edit_file
/// call, or `None` to keep the plain-text rendering. `old_text` is the
/// pre-call file content captured by the before hook.
fn diff_for_completed_edit(
    info: &ToolCallInfo,
    output: &str,
    workspace: &Path,
    old_text: Option<String>,
) -> Option<AcpDiff> {
    let path = tool_target_path(workspace, &info.input)?;
    let new_text = match info.name.as_str() {
        "write_file" => info.input.get("content")?.as_str()?.to_string(),
        "edit_file" => {
            let parsed: serde_json::Value = serde_json::from_str(output).ok()?;
            // No `diffs` key means nothing was applied — no diff to show.
            let pairs = parsed.get("diffs")?.as_array()?;
            replay_edit_pairs(old_text.as_deref()?, pairs)?
        }
        _ => return None,
    };
    Some(AcpDiff::new(path, new_text).old_text(old_text))
}

/// Map a daimonos tool name to the closest ACP [`ToolKind`] for client UI
/// (icon/treatment) purposes. Best-effort — unmapped tools fall back to
/// `ToolKind::Other`, which is a harmless default, not an error.
fn tool_kind_for(name: &str) -> ToolKind {
    match name {
        "read_file" | "search" | "kgl_query" | "ls" | "get_tool_schema" | "list_all_tools"
        | "workspace_info" | "session_stats" => ToolKind::Read,
        "write_file" | "edit_file" | "append_file" => ToolKind::Edit,
        "exec" | "execute_script" | "cargo" | "npm" | "pytest" | "docker" | "git" | "gh"
        | "batch" | "kgl_assert" => ToolKind::Execute,
        "curl" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

fn current_cx(connection: &CurrentConnection) -> Option<ConnectionTo<AcpClientRole>> {
    connection.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

fn try_send_notification(
    cx: &ConnectionTo<AcpClientRole>,
    session_id: &SessionId,
    update: SessionUpdate,
) -> bool {
    cx.send_notification(SessionNotification::new(session_id.clone(), update))
        .is_ok()
}

fn send_notification(
    cx: &ConnectionTo<AcpClientRole>,
    session_id: &SessionId,
    update: SessionUpdate,
) {
    let _ = try_send_notification(cx, session_id, update);
}

fn to_acp_plan(entries: &[AgentPlanEntry]) -> AcpPlan {
    AcpPlan::new(
        entries
            .iter()
            .map(|entry| {
                let priority = match entry.priority {
                    PlanPriority::High => AcpPlanPriority::High,
                    PlanPriority::Medium => AcpPlanPriority::Medium,
                    PlanPriority::Low => AcpPlanPriority::Low,
                };
                let status = match entry.status {
                    PlanStatus::Pending => AcpPlanStatus::Pending,
                    PlanStatus::InProgress => AcpPlanStatus::InProgress,
                    PlanStatus::Completed => AcpPlanStatus::Completed,
                };
                AcpPlanEntry::new(entry.content.clone(), priority, status)
            })
            .collect(),
    )
}

fn build_plan_hook(connection: CurrentConnection, session_id: SessionId) -> PlanHook {
    Box::new(move |entries: &[AgentPlanEntry]| {
        let Some(cx) = current_cx(&connection) else {
            return;
        };
        send_notification(&cx, &session_id, SessionUpdate::Plan(to_acp_plan(entries)));
    })
}

fn client_supports_terminal_output(req: &InitializeRequest) -> bool {
    req.client_capabilities
        .meta
        .as_ref()
        .and_then(|meta| meta.get("terminal_output"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn terminal_cwd(workspace: &Path, input: &serde_json::Value) -> PathBuf {
    input
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .map(|cwd| {
            if cwd.is_absolute() {
                cwd
            } else {
                workspace.join(cwd)
            }
        })
        .unwrap_or_else(|| workspace.to_path_buf())
}

fn terminal_info_meta(workspace: &Path, info: &ToolCallInfo) -> Option<Meta> {
    (info.name == "exec").then(|| {
        Meta::from_iter([(
            "terminal_info".to_string(),
            serde_json::json!({
                "terminal_id": info.id,
                "cwd": terminal_cwd(workspace, &info.input),
            }),
        )])
    })
}

fn terminal_exit_meta(info: &ToolCallInfo, code: Option<i32>, signal: Option<String>) -> Meta {
    Meta::from_iter([(
        "terminal_exit".to_string(),
        serde_json::json!({
            "terminal_id": info.id,
            "exit_code": code.and_then(|code| u32::try_from(code).ok()),
            "signal": signal,
        }),
    )])
}

/// Send an update only while its tool call is live. For terminal updates,
/// removal happens atomically before enqueueing the final update, so any
/// concurrently arriving progress callback observes the call as closed and
/// cannot emit terminal output after completion.
fn send_active_tool_call_update(
    cx: &ConnectionTo<AcpClientRole>,
    session_id: &SessionId,
    active_tool_calls: &ActiveToolCalls,
    tool_call_id: &str,
    update: ToolCallUpdate,
    terminal: bool,
) -> bool {
    let should_send = tool_call_update_is_live(active_tool_calls, tool_call_id, terminal);
    if should_send {
        send_notification(cx, session_id, SessionUpdate::ToolCallUpdate(update));
    }
    should_send
}

fn tool_call_update_is_live(
    active_tool_calls: &ActiveToolCalls,
    tool_call_id: &str,
    terminal: bool,
) -> bool {
    let mut active = active_tool_calls
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if terminal {
        active.remove(tool_call_id).is_some()
    } else {
        active.contains_key(tool_call_id)
    }
}

fn cancel_active_tool_calls(
    cx: &ConnectionTo<AcpClientRole>,
    session_id: &SessionId,
    active_tool_calls: &ActiveToolCalls,
) -> Vec<String> {
    let active = {
        let mut calls = active_tool_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        calls.drain().collect::<Vec<_>>()
    };
    let cancelled_ids = active
        .iter()
        .map(|(tool_call_id, _)| tool_call_id.clone())
        .collect::<Vec<_>>();
    for (tool_call_id, active_call) in active {
        let fields = ToolCallUpdateFields::new()
            .status(Some(ToolCallStatus::Failed))
            .content(Some(vec![AcpContentBlock::Text(TextContent::new(
                "cancelled".to_string(),
            ))
            .into()]))
            .raw_output(Some(serde_json::json!({ "cancelled": true })));
        let mut update = ToolCallUpdate::new(tool_call_id.clone(), fields);
        if active_call.terminal_output {
            update = update.meta(Meta::from_iter([(
                "terminal_exit".to_string(),
                serde_json::json!({
                    "terminal_id": tool_call_id,
                    "exit_code": null,
                    "signal": "cancelled",
                }),
            )]));
        }
        send_notification(cx, session_id, SessionUpdate::ToolCallUpdate(update));
    }
    cancelled_ids
}

/// Resolve reliable approval waiters and close every ACP/canonical tool call
/// whose normal completion path was dropped with a cancelled turn.
fn cleanup_cancelled_turn(
    handle: &SessionHandle,
    cx: &ConnectionTo<AcpClientRole>,
    session_id: &SessionId,
) {
    handle.core.cleanup_cancelled_turn();
    cancel_active_tool_calls(cx, session_id, &handle.active_tool_calls);
}

fn build_tool_progress_hook(
    connection: CurrentConnection,
    session_id: SessionId,
    active_tool_calls: ActiveToolCalls,
    enabled: bool,
) -> Option<ToolProgressHook> {
    enabled.then(|| {
        Box::new(
            move |info: &ToolCallInfo, event: crate::ops::ExecProgress| {
                if info.name != "exec" {
                    return;
                }
                let Some(cx) = current_cx(&connection) else {
                    return;
                };
                let (key, value) = match event {
                    crate::ops::ExecProgress::Output(data) => (
                        "terminal_output",
                        serde_json::json!({
                            "terminal_id": info.id,
                            "data": data,
                        }),
                    ),
                    crate::ops::ExecProgress::Exit { code, signal } => {
                        let update =
                            ToolCallUpdate::new(info.id.clone(), ToolCallUpdateFields::new())
                                .meta(terminal_exit_meta(info, code, signal));
                        send_active_tool_call_update(
                            &cx,
                            &session_id,
                            &active_tool_calls,
                            &info.id,
                            update,
                            false,
                        );
                        return;
                    }
                };
                let update = ToolCallUpdate::new(info.id.clone(), ToolCallUpdateFields::new())
                    .meta(Meta::from_iter([(key.to_string(), value)]));
                send_active_tool_call_update(
                    &cx,
                    &session_id,
                    &active_tool_calls,
                    &info.id,
                    update,
                    false,
                );
            },
        ) as ToolProgressHook
    })
}

#[allow(clippy::too_many_arguments)]
fn build_before_tool_call_hook(
    connection: CurrentConnection,
    session_id: SessionId,
    active_tool_calls: ActiveToolCalls,
    tool_lifecycle: Arc<CanonicalToolLifecycle>,
    diff_stash: DiffStash,
    workspace: PathBuf,
    terminal_output: bool,
) -> BeforeHook {
    Box::new(move |info: &ToolCallInfo| {
        let connection = Arc::clone(&connection);
        let session_id = session_id.clone();
        let active_tool_calls = Arc::clone(&active_tool_calls);
        let tool_lifecycle = Arc::clone(&tool_lifecycle);
        let diff_stash = Arc::clone(&diff_stash);
        let workspace = workspace.clone();
        Box::pin(async move {
            // update_plan has native ACP presentation via SessionUpdate::Plan;
            // suppress redundant generic tool-call chrome and permissions.
            if info.name == UPDATE_PLAN_TOOL && parse_plan_entries(&info.input).is_ok() {
                return BeforeHookResult::Allow;
            }
            if let Err(reason) = tool_lifecycle.reserve(info) {
                return BeforeHookResult::Block(reason);
            }
            let cx = current_cx(&connection);
            let retain_raw_input = tool_lifecycle.approval_required(&info.name);

            let title = if terminal_output {
                tool_call_title(info)
            } else {
                info.name.clone()
            };
            if let Some(cx) = cx.as_ref() {
                let mut tool_call = ToolCall::new(info.id.clone(), title.clone())
                    .kind(tool_kind_for(&info.name))
                    .status(ToolCallStatus::Pending)
                    .locations(tool_call_locations(&workspace, &info.name, &info.input))
                    .raw_input(Some(info.input.clone()));
                if terminal_output {
                    tool_call = tool_call.meta(terminal_info_meta(&workspace, info));
                }
                if !try_send_notification(cx, &session_id, SessionUpdate::ToolCall(tool_call)) {
                    tool_lifecycle.finish(info, "failed to announce ACP tool call", true);
                    return BeforeHookResult::Block("failed to announce ACP tool call".to_string());
                }
                active_tool_calls
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(
                        info.id.clone(),
                        ActiveToolCall::new(
                            info,
                            terminal_output && info.name == "exec",
                            retain_raw_input,
                        ),
                    );
            }

            let decision = tool_lifecycle.authorize_reserved(info).await;

            // Capture the pre-edit file text so the after hook can render
            // the completion as a Diff (vikunja #983).
            if matches!(decision, BeforeHookResult::Allow) && is_file_edit_tool(&info.name) {
                if let Some(path) = tool_target_path(&workspace, &info.input) {
                    let old_text = tokio::fs::read_to_string(&path).await.ok();
                    diff_stash
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .insert(info.id.clone(), old_text);
                }
            }

            let fields = match &decision {
                BeforeHookResult::Allow => {
                    ToolCallUpdateFields::new().status(Some(ToolCallStatus::InProgress))
                }
                BeforeHookResult::Block(reason) => {
                    let message = format!("blocked: {reason}");
                    ToolCallUpdateFields::new()
                        .status(Some(ToolCallStatus::Failed))
                        .content(Some(vec![
                            AcpContentBlock::Text(TextContent::new(message)).into()
                        ]))
                        .raw_output(Some(serde_json::json!({
                            "blocked": true,
                            "reason": reason,
                        })))
                }
            };
            let mut update = ToolCallUpdate::new(info.id.clone(), fields);
            if terminal_output
                && info.name == "exec"
                && matches!(decision, BeforeHookResult::Block(_))
            {
                update = update.meta(terminal_exit_meta(info, None, Some("blocked".to_string())));
            }
            let blocked = matches!(decision, BeforeHookResult::Block(_));
            if let Some(cx) = cx.as_ref() {
                send_active_tool_call_update(
                    cx,
                    &session_id,
                    &active_tool_calls,
                    &info.id,
                    update,
                    blocked,
                );
            }

            decision
        })
    })
}

fn build_after_tool_call_hook(
    connection: CurrentConnection,
    session_id: SessionId,
    active_tool_calls: ActiveToolCalls,
    tool_lifecycle: Arc<CanonicalToolLifecycle>,
    diff_stash: DiffStash,
    workspace: PathBuf,
) -> AfterHook {
    Box::new(move |info: &ToolCallInfo, content: &str, is_error: bool| {
        if info.name == UPDATE_PLAN_TOOL && !is_error {
            return AfterHookResult::Continue;
        }
        // Always drain this call's stash entry (even on failure) so blocked
        // or failed edits don't leak entries.
        let old_text = diff_stash
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&info.id)
            .flatten();
        let canonical_finished = tool_lifecycle.finish(info, content, is_error);
        let acp_finished = tool_call_update_is_live(&active_tool_calls, &info.id, true);
        if !canonical_finished {
            return AfterHookResult::Continue;
        }
        // Unreachable while `CurrentConnection` holds its never-`None`
        // invariant (see the type's docs). Left as a total match rather than an
        // unwrap because making it reachable is an open design question, not an
        // impossibility — but it is NOT safe to reach as written:
        //
        // `acp_finished` above has already removed the id from
        // `active_tool_calls` (the terminal claim) and the canonical
        // `ToolCallFinished` has already been emitted. With the id gone, no
        // later path can deliver the terminal ACP update —
        // `cancel_active_tool_calls` drains that same map — so a still-connected
        // client would sit on Pending/InProgress chrome for that tool call
        // forever. Whether to keep the id claimable or skip the canonical event
        // depends on how canonical events are consumed on reconnect (#1097).
        // Full analysis: vikunja #1107, first comment.
        let Some(cx) = current_cx(&connection) else {
            return AfterHookResult::Continue;
        };
        if !acp_finished {
            return AfterHookResult::Continue;
        }
        let status = if is_error {
            ToolCallStatus::Failed
        } else {
            ToolCallStatus::Completed
        };
        let block: AcpToolCallContent = match (!is_error)
            .then(|| diff_for_completed_edit(info, content, &workspace, old_text))
            .flatten()
        {
            Some(diff) => diff.into(),
            None => AcpContentBlock::Text(TextContent::new(content.to_string())).into(),
        };
        // Machine-readable result for the client's tool-call inspector
        // (vikunja #991). Tool output is normally compact JSON; plain-text
        // messages (e.g. "tool not available") become a JSON string.
        let raw_output: serde_json::Value = serde_json::from_str(content)
            .unwrap_or_else(|_| serde_json::Value::String(content.to_string()));
        let update = ToolCallUpdate::new(
            info.id.clone(),
            ToolCallUpdateFields::new()
                .status(Some(status))
                .content(Some(vec![block]))
                .raw_output(Some(raw_output)),
        );
        send_notification(&cx, &session_id, SessionUpdate::ToolCallUpdate(update));
        AfterHookResult::Continue
    })
}

fn build_stream_hook(events: Arc<SessionEventRouter>) -> crate::agent::StreamHook {
    Box::new(move |event: StreamEvent| {
        let event = match event {
            StreamEvent::TextDelta(text) => CoreSessionEvent::AssistantDelta { text },
            StreamEvent::ThinkingDelta(text) => CoreSessionEvent::ThoughtDelta { text },
        };
        let _ = events.emit(event);
    })
}

fn route_acp_approval_request(
    cx: Option<ConnectionTo<AcpClientRole>>,
    session_id: SessionId,
    approvals: Arc<ApprovalBroker>,
    active_tool_calls: ActiveToolCalls,
    request: CoreApprovalRequest,
) {
    let local_capabilities = [
        ClientCapability::ApproveOnce,
        ClientCapability::ApproveAlways,
    ];
    let Some(cx) = cx else {
        let _ = approvals.resolve(
            &request.id,
            "acp_unavailable",
            &local_capabilities,
            CoreApprovalDecision::Deny,
        );
        return;
    };

    let mut options = vec![PermissionOption::new(
        "allow_once",
        "Allow",
        PermissionOptionKind::AllowOnce,
    )];
    if request.allow_always_available {
        options.push(PermissionOption::new(
            "allow_always",
            "Always Allow",
            PermissionOptionKind::AllowAlways,
        ));
    }
    options.push(PermissionOption::new(
        "reject",
        "Reject",
        PermissionOptionKind::RejectOnce,
    ));
    let raw_input = active_tool_calls
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_mut(&request.tool_call_id)
        .and_then(|active| active.raw_input.take());
    let acp_request = RequestPermissionRequest::new(
        session_id,
        ToolCallUpdate::new(
            request.tool_call_id.clone(),
            ToolCallUpdateFields::new().raw_input(raw_input),
        ),
        options,
    );
    let approval_id = request.id;
    let task_approval_id = approval_id.clone();
    let request_connection = cx.clone();
    let approvals_for_task = Arc::clone(&approvals);
    let spawn = cx.spawn(async move {
        let (decision, resolved_by) = match request_connection
            .send_request(acp_request)
            .block_task()
            .await
        {
            Ok(response) => match response.outcome {
                RequestPermissionOutcome::Selected(selected) => {
                    match selected.option_id.to_string().as_str() {
                        "allow_once" => (CoreApprovalDecision::AllowOnce, "acp_local"),
                        "allow_always" => (CoreApprovalDecision::AllowAlways, "acp_local"),
                        _ => (CoreApprovalDecision::Deny, "acp_unrecognized"),
                    }
                }
                RequestPermissionOutcome::Cancelled => {
                    (CoreApprovalDecision::Deny, "acp_cancelled")
                }
                _ => (CoreApprovalDecision::Deny, "acp_unrecognized"),
            },
            Err(_) => (CoreApprovalDecision::Deny, "acp_request_failed"),
        };
        let _ = approvals_for_task.resolve(
            &task_approval_id,
            resolved_by,
            &local_capabilities,
            decision,
        );
        Ok(())
    });
    if spawn.is_err() {
        let _ = approvals.resolve(
            &approval_id,
            "acp_spawn_failed",
            &local_capabilities,
            CoreApprovalDecision::Deny,
        );
    }
}

fn build_acp_event_handler(
    connection: CurrentConnection,
    session_id: SessionId,
    approvals: Arc<ApprovalBroker>,
    active_tool_calls: ActiveToolCalls,
) -> SessionEventHandler {
    Arc::new(move |_sequence, event| {
        if let CoreSessionEvent::ApprovalRequested { request } = event {
            route_acp_approval_request(
                current_cx(&connection),
                session_id.clone(),
                Arc::clone(&approvals),
                Arc::clone(&active_tool_calls),
                request,
            );
            return;
        }
        let Some(cx) = current_cx(&connection) else {
            return;
        };
        let update = match event {
            CoreSessionEvent::AssistantDelta { text } => Some(SessionUpdate::AgentMessageChunk(
                ContentChunk::new(AcpContentBlock::Text(TextContent::new(text))),
            )),
            CoreSessionEvent::ThoughtDelta { text } => Some(SessionUpdate::AgentThoughtChunk(
                ContentChunk::new(AcpContentBlock::Text(TextContent::new(text))),
            )),
            CoreSessionEvent::AssistantDone {
                outcome: CoreAssistantOutcome::Errored { message, .. },
            } => {
                send_provider_error_diagnostic(&cx, &session_id, &message);
                None
            }
            CoreSessionEvent::AssistantDone {
                outcome: CoreAssistantOutcome::Refused,
            } => {
                send_refusal_diagnostic(&cx, &session_id);
                None
            }
            // Rich ACP tool/diff/terminal projections remain adapter-specific;
            // the canonical events still feed daemon snapshots/replay.
            _ => None,
        };
        if let Some(update) = update {
            send_notification(&cx, &session_id, update);
        }
    })
}

fn emit_usage_update(
    cx: &ConnectionTo<AcpClientRole>,
    session_id: &SessionId,
    context_window: Option<u64>,
    last_call_usage: &Usage,
    cumulative_cost_usd: f64,
) {
    let Some(update) = usage_update(context_window, last_call_usage, cumulative_cost_usd) else {
        return;
    };
    send_notification(cx, session_id, SessionUpdate::UsageUpdate(update));
}

fn usage_update(
    context_window: Option<u64>,
    last_call_usage: &Usage,
    cumulative_cost_usd: f64,
) -> Option<UsageUpdate> {
    let context_window = context_window?;
    let used = last_call_usage
        .prompt_tokens()
        .saturating_add(last_call_usage.output);
    Some(UsageUpdate::new(used, context_window).cost(AcpCost::new(cumulative_cost_usd, "USD")))
}

/// Build the single model-picker config option (vikunja #960): a `Select`
/// of `models`, `category: Model` (the UX hint that makes Zed render it as
/// the model dropdown), with `current` marked selected. Model id == display
/// name for v1. Returns the full `config_options` list to advertise.
fn model_config_options(models: &[String], current: &str) -> Vec<SessionConfigOption> {
    let options: Vec<SessionConfigSelectOption> = models
        .iter()
        .map(|m| SessionConfigSelectOption::new(m.clone(), m.clone()))
        .collect();
    let option =
        SessionConfigOption::select(MODEL_CONFIG_ID, "Model", current.to_string(), options)
            .category(Some(SessionConfigOptionCategory::Model));
    vec![option]
}

fn canonical_model_options(models: &[String], current: &str) -> Vec<CoreRuntimeOption> {
    vec![CoreRuntimeOption::select(
        MODEL_CONFIG_ID,
        "Model",
        CoreRuntimeValue::String(current.to_string()),
        models
            .iter()
            .map(|model| CoreRuntimeChoice::new(model, model))
            .collect(),
    )]
}

/// Surface a compaction as a subtle thought chunk in Zed's thread view
/// (ADR-002 Q6): no dedicated compaction SessionUpdate exists in schema
/// 1.4.0, and a thought renders collapsed/greyed — honest without faking
/// agent output.
fn build_compaction_hook(events: Arc<SessionEventRouter>) -> crate::agent::CompactionHook {
    Box::new(move |event: &crate::compaction::CompactionEvent| {
        let _ = events.emit(CoreSessionEvent::ThoughtDelta {
            text: format!(
                "[context compacted: {} older turn(s) summarized]",
                event.evicted_turns
            ),
        });
    })
}

/// Build the [`AgentConfig`] for one ACP session — mirrors
/// `chat_cmd::build_agent_config`, but every hook reports through
/// `session/update`/`session/request_permission` instead of the terminal.
#[allow(clippy::too_many_arguments)]
fn build_agent_config(
    workspace: &Path,
    model: String,
    thinking: crate::providers::ThinkingLevel,
    connection: CurrentConnection,
    session_id: SessionId,
    active_tool_calls: ActiveToolCalls,
    events: Arc<SessionEventRouter>,
    tool_lifecycle: Arc<CanonicalToolLifecycle>,
    token_log: Option<PathBuf>,
    compaction: Option<crate::compaction::CompactionPolicy>,
    system_prompt: String,
    terminal_output: bool,
    descriptions: &crate::tool_descriptions::ToolDescriptions,
    bridge: Arc<McpBridge>,
    bridge_slot: BridgeSlot,
    // Ordered failover chain (#1240). This is `AgentEnv::models`, which is
    // `[active_model]` when `DAIMONOS_AGENT_MODELS` is unset — so failover is
    // naturally opt-in: a single-entry chain has no successor to advance to.
    failover_models: Vec<String>,
) -> AgentConfig {
    let tools = agent_tools(workspace, descriptions, &bridge);
    let diff_stash: DiffStash = Arc::new(StdMutex::new(HashMap::new()));
    AgentConfig {
        system: Some(system_prompt),
        tools,
        opts: CompleteOpts {
            model,
            thinking,
            ..CompleteOpts::default()
        },
        // Item 3: leave unset so `run()` resolves it from
        // `DAIMONOS_AGENT_AUTO_CONTINUE` (off unless the operator opts in).
        auto_continue_budget: None,
        // #1240: bounded retry of transient provider failures, then failover
        // along the configured chain. An ACP client (Zed) surfaces a provider
        // blip as a failed turn the user must redo, so absorbing it is exactly
        // where this pays off.
        provider_retry: crate::agent::ProviderRetryConfig {
            failover_models,
            ..crate::agent::ProviderRetryConfig::default()
        },
        before_tool_call: Some(build_before_tool_call_hook(
            Arc::clone(&connection),
            session_id.clone(),
            Arc::clone(&active_tool_calls),
            Arc::clone(&tool_lifecycle),
            Arc::clone(&diff_stash),
            workspace.to_path_buf(),
            terminal_output,
        )),
        after_tool_call: Some(build_after_tool_call_hook(
            Arc::clone(&connection),
            session_id.clone(),
            Arc::clone(&active_tool_calls),
            tool_lifecycle,
            diff_stash,
            workspace.to_path_buf(),
        )),
        on_compaction: Some(build_compaction_hook(Arc::clone(&events))),
        on_stream_event: Some(build_stream_hook(Arc::clone(&events))),
        on_tool_progress: build_tool_progress_hook(
            Arc::clone(&connection),
            session_id.clone(),
            active_tool_calls,
            terminal_output,
        ),
        on_plan_update: Some(build_plan_hook(Arc::clone(&connection), session_id.clone())),
        token_log: token_log.map(|path| TokenLogConfig {
            path,
            label: "acp".to_string(),
        }),
        compaction,
        remote_tool_dispatch: Some(build_remote_dispatch_hook(bridge_slot)),
        subcall_provider: None,
        generation_ordinal: Default::default(),
    }
}

/// Native tools first (OnDemand exclusion + per-tool context checks intact),
/// then the currently connected bridge's remote tools (ADR-003, D6).
fn agent_tools(
    workspace: &Path,
    descriptions: &crate::tool_descriptions::ToolDescriptions,
    bridge: &McpBridge,
) -> Vec<ToolSchema> {
    let mut tools: Vec<ToolSchema> = tool_facade::active_schemas(workspace, descriptions)
        .into_iter()
        .map(|s| ToolSchema {
            name: s.name,
            description: s.description,
            input_schema: s.input_schema,
        })
        .collect();
    tools.extend(bridge.tools().iter().cloned());
    tools
}

/// Convert Zed-forwarded `McpServer` entries into transport-neutral
/// [`ServerSpec`]s. Stdio and HTTP are bridged; SSE and the unstable ACP
/// transport are dropped (ADR-003, out of scope).
fn to_server_specs(servers: Vec<McpServer>) -> Vec<ServerSpec> {
    servers
        .into_iter()
        .filter_map(|server| match server {
            McpServer::Stdio(s) => Some(ServerSpec::Stdio {
                name: s.name,
                command: s.command.to_string_lossy().into_owned(),
                args: s.args,
                env: s.env.into_iter().map(|e| (e.name, e.value)).collect(),
            }),
            McpServer::Http(s) => Some(ServerSpec::Http {
                name: s.name,
                url: s.url,
                headers: s.headers.into_iter().map(|h| (h.name, h.value)).collect(),
            }),
            _ => None,
        })
        .collect()
}

/// Resolve the MCP servers to bridge for a session. Normally these are the
/// list Zed forwards in `session/new`/`session/load`. But unpatched Zed has a
/// cold-start race where it forwards an EMPTY list (its context-server store
/// isn't populated when it issues a restored session) and never re-forwards to
/// a live session — leaving the session with no MCP tools. So when the
/// forwarded list is empty (and the fallback is enabled), read Zed's own
/// `context_servers` settings directly and bridge those instead (see
/// [`crate::zed_config`]). Only the empty case triggers the fallback; servers
/// Zed did forward are never overridden.
fn resolve_mcp_specs(forwarded: Vec<McpServer>, cfg: &Config) -> Vec<ServerSpec> {
    let specs = to_server_specs(forwarded);
    if !specs.is_empty() || !cfg.acp.mcp.enabled || !cfg.acp.mcp.zed_config_fallback {
        return specs;
    }
    match crate::zed_config::context_server_specs(cfg.acp.mcp.zed_settings_path.as_deref()) {
        Ok(fallback) if !fallback.is_empty() => {
            tracing::warn!(
                target: "daimonos::acp",
                event = "mcp_forward_empty_fallback",
                recovered = fallback.len(),
                "Zed forwarded no MCP servers; recovered them from Zed settings \
                 (unpatched-Zed cold-start race)"
            );
            fallback
        }
        Ok(_) => specs,
        Err(e) => {
            tracing::warn!(
                target: "daimonos::acp",
                event = "mcp_fallback_failed",
                error = %e,
                "Zed forwarded no MCP servers and reading Zed settings failed"
            );
            specs
        }
    }
}

fn should_refresh_mcp_bridge(
    current: &[ServerSpec],
    requested: &[ServerSpec],
    had_connection_failures: bool,
) -> bool {
    had_connection_failures || current != requested
}

/// Dispatch hook consulted by the agent loop when the opcode facade doesn't
/// serve a tool: routes `mcp__*` calls to the session's bridge (ADR-003, D5).
fn build_remote_dispatch_hook(bridge_slot: BridgeSlot) -> RemoteToolHook {
    Box::new(move |name: &str, input: &serde_json::Value| {
        let bridge_slot = Arc::clone(&bridge_slot);
        let name = name.to_string();
        let input = input.clone();
        Box::pin(async move {
            let bridge = Arc::clone(&*bridge_slot.read().await);
            // Provenance guard (task #1116): a name under a refused self /
            // nested-daimonos server never reached `routes`, so `call` would
            // return `None` and the agent loop would surface a generic "not
            // available." Answer with an explicit prohibition instead, so a
            // self-server tool that leaks in (stale catalog, replayed history,
            // hand-built call) fails legibly rather than silently missing.
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

fn map_stop_reason(stop_reason: crate::providers::StopReason) -> AcpStopReason {
    use crate::providers::StopReason;
    match stop_reason {
        StopReason::EndTurn | StopReason::ToolUse => AcpStopReason::EndTurn,
        StopReason::MaxTokens => AcpStopReason::MaxTokens,
        StopReason::Aborted => AcpStopReason::Cancelled,
        StopReason::Refusal => AcpStopReason::Refusal,
        // ACP has no infrastructure-error stop reason. The prompt path emits a
        // safe diagnostic chunk and ends normally so Zed does not mislabel a
        // provider/network failure as a content-policy refusal.
        StopReason::Error => AcpStopReason::EndTurn,
    }
}

fn acp_stop_reason_name(stop_reason: &AcpStopReason) -> &'static str {
    match stop_reason {
        AcpStopReason::EndTurn => "end_turn",
        AcpStopReason::MaxTokens => "max_tokens",
        AcpStopReason::Refusal => "refusal",
        AcpStopReason::Cancelled => "cancelled",
        _ => "unknown",
    }
}

/// Format the per-turn timestamp line shown at the top of an agent turn when
/// `DAIMONOS_AGENT_TIMESTAMP_TURNS` is on (#1070). Sourced from the OS clock
/// by the caller (never the model). Generic over the timezone so tests can
/// pin a fixed instant. Kept as its own line so it renders distinctly and
/// persists/replays like any other assistant text.
fn turn_timestamp_line<Tz: chrono::TimeZone>(now: chrono::DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    format!("[{}]\n", now.format("%Y-%m-%d %H:%M:%S %Z"))
}

fn send_provider_error_diagnostic(
    cx: &ConnectionTo<AcpClientRole>,
    session_id: &SessionId,
    message: &str,
) {
    send_notification(
        cx,
        session_id,
        SessionUpdate::AgentThoughtChunk(ContentChunk::new(AcpContentBlock::Text(
            TextContent::new(message),
        ))),
    );
}

fn emit_assistant_done(events: &SessionEventRouter, session_id: &SessionId, turn: &TurnResult) {
    let outcome = canonical_assistant_outcome_with_logging(session_id, turn);
    let _ = events.emit(CoreSessionEvent::AssistantDone { outcome });
}

fn send_refusal_diagnostic(cx: &ConnectionTo<AcpClientRole>, session_id: &SessionId) {
    send_notification(
        cx,
        session_id,
        SessionUpdate::AgentThoughtChunk(ContentChunk::new(AcpContentBlock::Text(
            TextContent::new(REFUSAL_DIAGNOSTIC),
        ))),
    );
}

/// Convert ACP prompt content into the provider-neutral message shape without
/// silently dropping blocks. Embedded text is bounded by visible URI markers;
/// image payloads stay structured for provider-specific serialization.
fn prompt_message(blocks: Vec<AcpContentBlock>) -> CoreMessage {
    let mut content = Vec::new();
    for block in blocks {
        match block {
            AcpContentBlock::Text(text) => content.push(CoreBlock::Text(text.text)),
            AcpContentBlock::Image(image) => content.push(CoreBlock::Image {
                data: image.data,
                media_type: image.mime_type,
                uri: image.uri,
            }),
            AcpContentBlock::Resource(resource) => match resource.resource {
                EmbeddedResourceResource::TextResourceContents(resource) => {
                    content.push(CoreBlock::Text(format!(
                        "[Embedded resource: {}]\n{}\n[End embedded resource]",
                        resource.uri, resource.text
                    )));
                }
                EmbeddedResourceResource::BlobResourceContents(resource) => {
                    let media_type = resource
                        .mime_type
                        .unwrap_or_else(|| "application/octet-stream".to_string());
                    if media_type.starts_with("image/") {
                        content.push(CoreBlock::Image {
                            data: resource.blob,
                            media_type,
                            uri: Some(resource.uri),
                        });
                    } else {
                        content.push(CoreBlock::Text(format!(
                            "[Unsupported embedded binary resource: {} ({media_type})]",
                            resource.uri
                        )));
                    }
                }
                _ => content.push(CoreBlock::Text(
                    "[Unsupported ACP embedded resource]".to_string(),
                )),
            },
            AcpContentBlock::ResourceLink(link) => {
                let description = link
                    .description
                    .map(|description| format!(" — {description}"))
                    .unwrap_or_default();
                content.push(CoreBlock::Text(format!(
                    "[Resource link: {} ({}){description}]",
                    link.name, link.uri
                )));
            }
            AcpContentBlock::Audio(audio) => {
                content.push(CoreBlock::Text(format!(
                    "[Unsupported ACP audio block ({})]",
                    audio.mime_type
                )));
            }
            _ => content.push(CoreBlock::Text(
                "[Unsupported ACP prompt content block]".to_string(),
            )),
        }
    }
    if content.is_empty() {
        content.push(CoreBlock::Text(String::new()));
    }
    CoreMessage {
        role: CoreRole::User,
        content,
    }
}

fn direct_command_text(message: &CoreMessage) -> Option<&str> {
    match message.content.as_slice() {
        [CoreBlock::Text(text)] => Some(text),
        _ => None,
    }
}

fn canonical_user_message_text(message: &CoreMessage) -> String {
    let mut parts = Vec::new();
    for block in &message.content {
        match block {
            CoreBlock::Text(text) => parts.push(text.clone()),
            CoreBlock::Image {
                data, media_type, ..
            } => parts.push(format!(
                "[image: media_type={media_type}, encoded_bytes={}]",
                data.len()
            )),
            _ => parts.push("[unsupported user content]".to_string()),
        }
    }
    parts.join("\n")
}

fn message_has_images(message: &CoreMessage) -> bool {
    message
        .content
        .iter()
        .any(|block| matches!(block, CoreBlock::Image { .. }))
}

async fn run_acp_direct_command(
    handle: &SessionHandle,
    cx: &ConnectionTo<AcpClientRole>,
    session_id: &SessionId,
    command: AcpCommand,
    client_user_message_id: Option<&str>,
) -> Result<AcpStopReason, TurnError> {
    let active_turn = handle.core.begin_turn()?;
    let mut agent_session = handle.core.session.lock().await;
    {
        let mut client_ids = handle.core.client_user_message_ids.lock().await;
        align_client_user_message_ids(&mut client_ids, agent_session.user_turn_count());
        if let Some(id) = client_user_message_id {
            if client_ids.iter().any(|existing| existing == id) {
                send_notification(
                    cx,
                    session_id,
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(AcpContentBlock::Text(
                        TextContent::new(format!("duplicate client user message id '{id}'")),
                    ))),
                );
                return Ok(AcpStopReason::EndTurn);
            }
        }
    }
    *handle
        .connection
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(cx.clone());
    let model = handle
        .core
        .current_model
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if let Err(error) = handle.core.prepare_model(&mut agent_session, &model).await {
        send_notification(
            cx,
            session_id,
            SessionUpdate::AgentMessageChunk(ContentChunk::new(AcpContentBlock::Text(
                TextContent::new(error),
            ))),
        );
        return Ok(AcpStopReason::EndTurn);
    }

    let response = run_acp_command(command, &mut agent_session);
    let cleared_history = (command == AcpCommand::Clear).then(|| agent_session.history().to_vec());
    if command == AcpCommand::Clear {
        handle.core.client_user_message_ids.lock().await.clear();
    }
    drop(agent_session);

    if let Some(messages) = cleared_history {
        handle.core.persist(&model, &messages, &[]);
    }
    send_notification(
        cx,
        session_id,
        SessionUpdate::AgentMessageChunk(ContentChunk::new(AcpContentBlock::Text(
            TextContent::new(response),
        ))),
    );
    drop(active_turn);
    Ok(AcpStopReason::EndTurn)
}

/// Run one prompt turn against `handle`, racing it against `session/cancel`.
/// Returns the ACP stop reason. Holds only this session's own lock, so other
/// sessions run concurrently unaffected.
async fn run_prompt_turn(
    handle: &Arc<SessionHandle>,
    cx: &ConnectionTo<AcpClientRole>,
    session_id: &SessionId,
    user_message: CoreMessage,
    client_user_message_id: Option<String>,
    assistant_prefix: Option<String>,
) -> Result<AcpStopReason, TurnError> {
    if let Some(command) = direct_command_text(&user_message).and_then(parse_acp_command) {
        return run_acp_direct_command(
            handle,
            cx,
            session_id,
            command,
            client_user_message_id.as_deref(),
        )
        .await;
    }

    *handle
        .connection
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(cx.clone());
    let canonical_user_text = canonical_user_message_text(&user_message);

    let execution = handle
        .core
        .prompt(
            user_message,
            canonical_user_text,
            client_user_message_id,
            assistant_prefix,
            || {
                cancel_active_tool_calls(cx, session_id, &handle.active_tool_calls);
            },
            |turn| canonical_assistant_outcome_with_logging(session_id, turn),
        )
        .await;
    let execution = match execution {
        Ok(execution) => execution,
        Err(SessionPromptError::Busy | SessionPromptError::Stopped) => {
            return Err(TurnError::Busy);
        }
        Err(SessionPromptError::DuplicateRequest(id)) => {
            send_notification(
                cx,
                session_id,
                SessionUpdate::AgentMessageChunk(ContentChunk::new(AcpContentBlock::Text(
                    TextContent::new(format!("duplicate client user message id '{id}'")),
                ))),
            );
            return Ok(AcpStopReason::EndTurn);
        }
        Err(SessionPromptError::Model(error)) => {
            send_notification(
                cx,
                session_id,
                SessionUpdate::AgentMessageChunk(ContentChunk::new(AcpContentBlock::Text(
                    TextContent::new(error),
                ))),
            );
            return Ok(AcpStopReason::EndTurn);
        }
    };

    Ok(match execution.outcome {
        SessionPromptOutcome::Completed(turn) => {
            emit_usage_update(
                cx,
                session_id,
                execution.context_window,
                &turn.last_call_usage,
                execution.cumulative_cost_usd,
            );
            // A turn that completes with `Aborted` was terminated by the
            // after-tool-call policy hook, not the client (ADR-006 D5).
            if matches!(turn.stop_reason, crate::providers::StopReason::Aborted) {
                tracing::Span::current().record("daimonos.cancel.reason", "policy");
            }
            map_stop_reason(turn.stop_reason)
        }
        SessionPromptOutcome::Cancelled => {
            // The prompt future lost the select to the `session/cancel`
            // notify: a client-initiated cancellation.
            tracing::Span::current().record("error.type", "client_cancelled");
            tracing::Span::current().record("daimonos.cancel.reason", "client");
            AcpStopReason::Cancelled
        }
    })
}

async fn run_retry_turn(
    handle: &Arc<SessionHandle>,
    cx: &ConnectionTo<AcpClientRole>,
    session_id: &SessionId,
) -> Result<AcpStopReason, String> {
    let active_turn = handle
        .core
        .begin_turn()
        .map_err(|error| error.to_string())?;
    let mut agent_session = handle.core.session.lock().await;
    *handle.connection.lock().unwrap_or_else(|p| p.into_inner()) = Some(cx.clone());
    let model = handle
        .core
        .current_model
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let context_window = handle
        .core
        .prepare_model(&mut agent_session, &model)
        .await?;

    let outcome = tokio::select! {
        turn = agent_session.retry_last_turn() => Some(turn),
        _ = active_turn.cancelled() => None,
    };
    if outcome.is_some() {
        // Claim completion immediately; see run_prompt_turn for why.
        active_turn.mark_completed();
    }
    let cumulative_cost_usd = agent_session.total_usage().cost.total_usd;
    if outcome.is_none() {
        // The retry path runs the same provider/tool loop as a prompt and must
        // perform the same pre-terminal cleanup when that future is dropped.
        cleanup_cancelled_turn(handle, cx, session_id);
    }

    let (stop_reason, history_snapshot) = match outcome {
        Some(Ok(turn)) => {
            let used_tokens = turn
                .last_call_usage
                .prompt_tokens()
                .saturating_add(turn.last_call_usage.output);
            let _ = handle
                .core
                .events
                .emit(CoreSessionEvent::ContextUsageChanged {
                    usage: handle.core.context_usage(used_tokens, context_window),
                });
            emit_assistant_done(&handle.core.events, session_id, &turn);
            emit_usage_update(
                cx,
                session_id,
                context_window,
                &turn.last_call_usage,
                cumulative_cost_usd,
            );
            (
                map_stop_reason(turn.stop_reason),
                Some(agent_session.history().to_vec()),
            )
        }
        Some(Err(error)) => return Err(error),
        None => (AcpStopReason::Cancelled, None),
    };
    let client_ids = handle.core.client_user_message_ids.lock().await.clone();
    drop(agent_session);
    drop(active_turn);

    if let Some(messages) = history_snapshot {
        handle.core.persist(&model, &messages, &client_ids);
    }
    Ok(stop_reason)
}

async fn truncate_session(
    handle: &Arc<SessionHandle>,
    session_id: &SessionId,
    client_user_message_id: &str,
    store: Option<&SessionStore>,
) -> Result<(), String> {
    let mut agent_session = handle.core.session.lock().await;
    let mut client_ids = handle.core.client_user_message_ids.lock().await;
    let user_turn_count = agent_session.user_turn_count();
    align_client_user_message_ids(&mut client_ids, user_turn_count);
    let Some(turn_index) = client_ids
        .iter()
        .position(|id| id == client_user_message_id)
    else {
        return Err(format!(
            "client user message id '{client_user_message_id}' not found"
        ));
    };
    agent_session.truncate_from_user_turn(turn_index)?;
    client_ids.truncate(turn_index);
    let mut outcomes = handle
        .core
        .assistant_outcomes
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    outcomes.truncate(turn_index);

    if let Some(store) = store {
        let model = handle
            .core
            .current_model
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        store.save_acp(
            &session_id.to_string(),
            &model,
            agent_session.thinking().as_str(),
            agent_session.history(),
            &handle.core.cwd,
            &client_ids,
            &outcomes,
        );
    }
    Ok(())
}

/// Render the human-facing idle notification as a normal visible agent message
/// block. The explicit "Daimonos agent mail" label distinguishes this
/// daemon-authored UI event from model-authored prose; it is not persisted into
/// provider history.
fn coordination_ui_update(text: String) -> SessionUpdate {
    SessionUpdate::AgentMessageChunk(ContentChunk::new(AcpContentBlock::Text(TextContent::new(
        text,
    ))))
}

/// Poll a human-visible coordination notice only while the AgentSession is
/// idle. `run_prompt_turn`/retry holds this mutex across provider streaming and
/// tool execution; `try_lock` therefore skips (never queues) a mid-turn tick.
async fn poll_coordination_ui_notice_if_idle(
    agent_session: &tokio::sync::Mutex<AgentSession>,
    tool_session: &Arc<tokio::sync::Mutex<Session>>,
) -> Option<(String, i64)> {
    let _idle_guard = agent_session.try_lock().ok()?;
    AgentSession::poll_coordination_ui_notice(tool_session).await
}

/// Build the native tool session used by an ACP chat thread. The MCP bridge is
/// deliberately separate: native tools are provisioned from the same canonical
/// registry as agent and MCP, while forwarded MCP servers retain their
/// per-session collision and self-connection guards.
async fn build_acp_tool_session(
    session_workspace: &Path,
    cfg: &Arc<Config>,
    analytics: Option<Arc<AnalyticsStore>>,
    acp_session_key: &str,
) -> Session {
    let services =
        crate::provisioning::build_tool_services(session_workspace, cfg, true, true, analytics)
            .await;
    let mut tool_session = Session::new(session_workspace.to_path_buf(), Arc::clone(cfg));
    crate::provisioning::provision_session(&mut tool_session, &services);
    // ACP session ids are stable across process restarts and intentionally
    // override the launch-process id installed by generic provisioning.
    tool_session.external_session_id = Some(acp_session_key.to_string());

    // Recover a previously registered identity for this persisted ACP session.
    // Fail-open: missing/corrupt coordination storage leaves notifications off.
    if cfg.coordination.enabled {
        let db_path = crate::coordination::workspace_db_path(
            &cfg.coordination.resolved_db_dir(),
            session_workspace,
        );
        if let Ok(store) = crate::coordination::CoordinationStore::open_with(
            &db_path,
            cfg.coordination.effective_busy_timeout_ms(),
        ) {
            tool_session.coordination_agent_name =
                store.agent_name_for_session(acp_session_key).ok().flatten();
        }
    }
    tool_session
}

/// Build a fresh session handle (provider + agent session + per-session
/// cells) without inserting it into the map. Shared by `session/new` and the
/// unknown-id branch of `session/load` (a respawned process has no in-memory
/// state for the requested id).
#[allow(clippy::too_many_arguments)]
async fn build_session_handle(
    state: &AcpState,
    cfg: &Arc<Config>,
    safety: Arc<crate::safety::SafetyPolicy>,
    token_log: Option<PathBuf>,
    session_id: SessionId,
    session_workspace: PathBuf,
    mcp_specs: Vec<ServerSpec>,
    cx: ConnectionTo<AcpClientRole>,
) -> Result<Arc<SessionHandle>, String> {
    // Build a fresh provider for this session — Zed keeps one process across
    // multiple chat threads, so we can't reuse a single moved-once provider.
    let provider = (state.make_provider)()?;
    let connection: CurrentConnection = Arc::new(StdMutex::new(Some(cx)));
    // Build the MCP bridge from the forwarded servers (fail-open). Native tool
    // names are gathered first so they always win a name collision (ADR-003).
    let descriptions = &cfg.prompts.resolved_tool_descriptions;
    let native_tool_names: std::collections::HashSet<String> =
        tool_facade::active_schemas(&session_workspace, descriptions)
            .into_iter()
            .map(|s| s.name)
            .collect();
    let bridge = Arc::new(
        McpBridge::build_with_pool(
            mcp_specs.clone(),
            &cfg.acp.mcp,
            &native_tool_names,
            state.analytics.clone(),
            crate::analytics::read_agent_session_id_env(),
            state.mcp_pool.clone(),
        )
        .await,
    );
    let bridge_slot = Arc::new(tokio::sync::RwLock::new(Arc::clone(&bridge)));
    let active_tool_calls: ActiveToolCalls = Arc::new(StdMutex::new(HashMap::new()));
    // ACP local clients may retain the existing AllowAlways behavior. Remote
    // clients are capability-gated independently when they are attached.
    let approvals = Arc::new(ApprovalBroker::new(true));
    let events = Arc::new(SessionEventRouter::new(Some(build_acp_event_handler(
        Arc::clone(&connection),
        session_id.clone(),
        Arc::clone(&approvals),
        Arc::clone(&active_tool_calls),
    ))));
    let tool_lifecycle = Arc::new(CanonicalToolLifecycle::new_with_output_limit(
        Arc::clone(&events),
        Arc::clone(&approvals),
        safety,
        cfg.session.max_active_tool_calls,
        cfg.session.max_tool_event_output_bytes,
    ));
    let config = build_agent_config(
        &session_workspace,
        state.default_model.clone(),
        state.thinking.clone(),
        Arc::clone(&connection),
        session_id.clone(),
        Arc::clone(&active_tool_calls),
        Arc::clone(&events),
        Arc::clone(&tool_lifecycle),
        token_log,
        state.compaction.policy.clone(),
        crate::prompts::agent_system(cfg).await,
        state.supports_terminal_output.load(Ordering::Acquire),
        &cfg.prompts.resolved_tool_descriptions,
        Arc::clone(&bridge),
        Arc::clone(&bridge_slot),
        state.models.clone(),
    );
    let acp_session_key = session_id.to_string();
    let tool_session = build_acp_tool_session(
        &session_workspace,
        cfg,
        state.analytics.clone(),
        &acp_session_key,
    )
    .await;
    let mut context_windows = HashMap::new();
    if state.compaction.follows_model_window {
        if let Some(policy) = &state.compaction.policy {
            context_windows.insert(state.default_model.clone(), policy.context_window);
        }
    }
    let persistence = state
        .store
        .clone()
        .map(|store| SessionPersistence::new(session_id.to_string(), store));
    let core = SessionCore::new(
        AgentSession::new(provider, tool_session, config),
        state.default_model.clone(),
        session_workspace,
        state.compaction.clone(),
        context_windows,
        Arc::clone(&approvals),
        persistence,
        Arc::clone(&events),
        Arc::clone(&tool_lifecycle),
    );
    let handle = Arc::new(SessionHandle {
        core,
        active_tool_calls,
        connection,
        bridge: bridge_slot,
        mcp_specs: tokio::sync::Mutex::new(mcp_specs),
    });

    // Idle human-visible agent-mail notification poller (#1063). A Weak handle
    // prevents a task cycle; when the session is deleted/dropped the task exits.
    // Locking AgentSession means polling waits through active prompts/tools and
    // can never mutate or notify mid-stream.
    if cfg.coordination.enabled
        && cfg.coordination.notifications.enabled
        && cfg.coordination.notifications.ui_notice
    {
        let weak = Arc::downgrade(&handle);
        let notification_tool_session = {
            let session = handle.core.session.lock().await;
            session.coordination_tool_session()
        };
        let base_ms = cfg.coordination.notifications.effective_poll_interval_ms();
        // Stable per-session jitter (up to 20% of the already-clamped effective
        // interval) prevents many sessions from opening SQLite connections in
        // lockstep while keeping deterministic behavior and no RNG dependency.
        let jitter = session_id
            .to_string()
            .bytes()
            .fold(0u64, |acc, b| acc.wrapping_mul(131).wrapping_add(b as u64))
            % (base_ms / 5 + 1);
        let interval = std::time::Duration::from_millis(base_ms + jitter);
        let poll_session_id = session_id.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let Some(handle) = weak.upgrade() else { break };
                // Non-blocking idle gate: run_prompt_turn/retry holds this lock
                // for the entire provider stream + tool loop. If busy, skip this
                // tick so no UI notification can appear mid-stream/tool call.
                let notice = poll_coordination_ui_notice_if_idle(
                    &handle.core.session,
                    &notification_tool_session,
                )
                .await;
                if let Some((text, newest_message_id)) = notice {
                    match current_cx(&handle.connection) {
                        Some(cx) => {
                            let delivery = cx.send_notification(SessionNotification::new(
                                poll_session_id.clone(),
                                coordination_ui_update(text),
                            ));
                            match delivery {
                                Ok(()) => {
                                    // Advance only after successful delivery.
                                    AgentSession::acknowledge_coordination_ui_notice(
                                        &notification_tool_session,
                                        newest_message_id,
                                    )
                                    .await;
                                }
                                Err(error) => tracing::warn!(
                                    target: "daimonos::coordination",
                                    event = "ui_notification_delivery_failed",
                                    session_id = %poll_session_id,
                                    error = %error,
                                ),
                            }
                        }
                        None => tracing::debug!(
                            target: "daimonos::coordination",
                            event = "ui_notification_no_connection",
                            session_id = %poll_session_id,
                        ),
                    }
                }
            }
        });
    }
    Ok(handle)
}

/// Refresh a live session's forwarded MCP bridge without replacing its agent
/// session. Holding the session lock prevents prompt dispatch while schemas
/// and routing switch together, preserving provider/history/usage state.
async fn refresh_live_mcp_bridge(
    handle: &SessionHandle,
    state: &AcpState,
    cfg: &Config,
    mcp_specs: Vec<ServerSpec>,
) {
    let mut agent_session = handle.core.session.lock().await;
    let descriptions = &cfg.prompts.resolved_tool_descriptions;
    let native_tool_names: std::collections::HashSet<String> =
        tool_facade::active_schemas(&handle.core.cwd, descriptions)
            .into_iter()
            .map(|schema| schema.name)
            .collect();
    let new_bridge = Arc::new(
        McpBridge::build_with_pool(
            mcp_specs.clone(),
            &cfg.acp.mcp,
            &native_tool_names,
            state.analytics.clone(),
            crate::analytics::read_agent_session_id_env(),
            state.mcp_pool.clone(),
        )
        .await,
    );
    agent_session.set_tools(agent_tools(&handle.core.cwd, descriptions, &new_bridge));
    let old_bridge = {
        let mut bridge = handle.bridge.write().await;
        std::mem::replace(&mut *bridge, new_bridge)
    };
    *handle.mcp_specs.lock().await = mcp_specs;
    drop(agent_session);
    old_bridge.shutdown().await;
}

async fn session_bridge(handle: &SessionHandle) -> Arc<McpBridge> {
    Arc::clone(&*handle.bridge.read().await)
}

async fn shutdown_session_bridge(handle: &SessionHandle) {
    session_bridge(handle).await.shutdown().await;
}

async fn send_session_mcp_diagnostics(
    cx: &ConnectionTo<AcpClientRole>,
    session_id: &SessionId,
    handle: &SessionHandle,
) {
    let bridge = session_bridge(handle).await;
    send_mcp_diagnostics(cx, session_id, &bridge);
}

/// Replay a loaded session's in-memory history back to the client as
/// `session/update` notifications, so Zed rebuilds the reopened thread's
/// view on `session/load`. User text/images → `UserMessageChunk`, assistant
/// text/images → `AgentMessageChunk`, assistant thinking →
/// `AgentThoughtChunk`, and tool calls/results → the
/// `ToolCall`/`ToolCallUpdate` lifecycle.
fn replay_history(
    cx: &ConnectionTo<AcpClientRole>,
    session_id: &SessionId,
    history: &[crate::providers::Message],
) {
    use crate::providers::{ContentBlock as CoreBlock, Role};
    let mut plan_tool_ids = HashSet::new();
    let mut announced_tool_ids = HashSet::new();
    for message in history {
        for block in &message.content {
            match block {
                CoreBlock::Text(text) => {
                    let chunk =
                        ContentChunk::new(AcpContentBlock::Text(TextContent::new(text.clone())));
                    let update = match message.role {
                        Role::User => SessionUpdate::UserMessageChunk(chunk),
                        Role::Assistant => SessionUpdate::AgentMessageChunk(chunk),
                    };
                    send_notification(cx, session_id, update);
                }
                CoreBlock::Image {
                    data,
                    media_type,
                    uri,
                } => {
                    let chunk = ContentChunk::new(AcpContentBlock::Image(
                        ImageContent::new(data.clone(), media_type.clone()).uri(uri.clone()),
                    ));
                    let update = match message.role {
                        Role::User => SessionUpdate::UserMessageChunk(chunk),
                        Role::Assistant => SessionUpdate::AgentMessageChunk(chunk),
                    };
                    send_notification(cx, session_id, update);
                }
                CoreBlock::ToolCall { id, name, input } => {
                    if name == UPDATE_PLAN_TOOL {
                        match parse_plan_entries(input) {
                            Ok(entries) => {
                                plan_tool_ids.insert(id.clone());
                                send_notification(
                                    cx,
                                    session_id,
                                    SessionUpdate::Plan(to_acp_plan(&entries)),
                                );
                            }
                            Err(_) => {
                                let tool_call = ToolCall::new(id.clone(), name.clone())
                                    .kind(tool_kind_for(name))
                                    .status(ToolCallStatus::InProgress)
                                    .raw_input(Some(input.clone()));
                                if try_send_notification(
                                    cx,
                                    session_id,
                                    SessionUpdate::ToolCall(tool_call),
                                ) {
                                    announced_tool_ids.insert(id.clone());
                                }
                            }
                        }
                    } else {
                        let tool_call = ToolCall::new(id.clone(), name.clone())
                            .kind(tool_kind_for(name))
                            .status(ToolCallStatus::InProgress)
                            .raw_input(Some(input.clone()));
                        if try_send_notification(cx, session_id, SessionUpdate::ToolCall(tool_call))
                        {
                            announced_tool_ids.insert(id.clone());
                        }
                    }
                }
                CoreBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    if plan_tool_ids.contains(tool_use_id) {
                        continue;
                    }
                    if !announced_tool_ids.remove(tool_use_id) {
                        tracing::warn!(
                            target: "daimonos::acp",
                            event = "unmatched_tool_result_dropped_during_replay",
                            tool_call_id = %tool_use_id,
                        );
                        continue;
                    }
                    let status = if *is_error {
                        ToolCallStatus::Failed
                    } else {
                        ToolCallStatus::Completed
                    };
                    let update = ToolCallUpdate::new(
                        tool_use_id.clone(),
                        ToolCallUpdateFields::new()
                            .status(Some(status))
                            .content(Some(vec![AcpContentBlock::Text(TextContent::new(
                                content.clone(),
                            ))
                            .into()])),
                    );
                    send_notification(cx, session_id, SessionUpdate::ToolCallUpdate(update));
                }
                CoreBlock::Thinking(text) if message.role == Role::Assistant => {
                    let chunk =
                        ContentChunk::new(AcpContentBlock::Text(TextContent::new(text.clone())));
                    send_notification(cx, session_id, SessionUpdate::AgentThoughtChunk(chunk));
                }
                CoreBlock::Thinking(_) | CoreBlock::ProviderState { .. } => {}
            }
        }
    }
}

/// Build the fully-configured ACP agent, ready to `.connect_to(transport)`.
/// Split out from [`run_acp`] so tests can connect it to an in-process
/// [`AcpClientRole`] builder instead of real stdio. Production goes through
/// [`build_agent_with_state`] so it can drain MCP bridges on exit.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn build_agent(
    make_provider: ProviderFactory,
    workspace: &Path,
    cfg: Arc<Config>,
    model: String,
    models: Vec<String>,
    safety: Arc<crate::safety::SafetyPolicy>,
    token_log: Option<PathBuf>,
    sessions_dir: Option<PathBuf>,
    compaction: Option<crate::compaction::CompactionPolicy>,
    analytics: Option<Arc<AnalyticsStore>>,
) -> impl ConnectTo<AcpClientRole> {
    // Tests don't need the state handle; production ([`run_acp`]) uses
    // [`build_agent_with_state`] so it can drain MCP bridges on exit.
    build_agent_with_state(
        make_provider,
        workspace,
        cfg,
        model,
        models,
        safety,
        token_log,
        sessions_dir,
        SessionCompaction::new(compaction, false),
        analytics,
        false,
        crate::providers::ThinkingLevel::default(),
        &mut None,
    )
}

/// Like [`build_agent`], but also hands the caller an `Arc<AcpState>` via
/// `state_out`. [`run_acp`] needs it to tear down every session's MCP bridge
/// when ACP stdin closes (ADR-003 D7/D8): `SessionHandle`s are otherwise just
/// dropped, and a dropped bridge does not `await` `shut_down()`, so stdio
/// grandchildren that don't exit on their own stdin EOF could outlive daimonos.
#[allow(clippy::too_many_arguments)]
fn build_agent_with_state(
    make_provider: ProviderFactory,
    workspace: &Path,
    cfg: Arc<Config>,
    model: String,
    models: Vec<String>,
    safety: Arc<crate::safety::SafetyPolicy>,
    token_log: Option<PathBuf>,
    sessions_dir: Option<PathBuf>,
    compaction: SessionCompaction,
    analytics: Option<Arc<AnalyticsStore>>,
    timestamp_turns: bool,
    thinking: crate::providers::ThinkingLevel,
    state_out: &mut Option<Arc<AcpState>>,
) -> impl ConnectTo<AcpClientRole> {
    let workspace = workspace.to_path_buf();
    let supports_images = make_provider()
        .map(|provider| provider.supports_images())
        .unwrap_or(false);
    // Advertise MCP transports so Zed forwards the matching server kinds
    // (ADR-003, D8). stdio needs no capability flag; http is gated on it.
    let mcp_enabled = cfg.acp.mcp.enabled;
    let mcp_http = mcp_enabled && cfg.acp.mcp.allow_http;
    let state = Arc::new(AcpState {
        sessions: tokio::sync::Mutex::new(HashMap::new()),
        session_operations: tokio::sync::Mutex::new(HashMap::new()),
        make_provider,
        models,
        default_model: model,
        store: sessions_dir.map(SessionStore::new),
        supports_images,
        supports_terminal_output: AtomicBool::new(false),
        session_list_page_size: cfg.acp.session_list_page_size,
        compaction,
        analytics,
        timestamp_turns,
        thinking,
        mcp_pool: McpClientPool::new(),
        mcp_teardown_timeout: std::time::Duration::from_secs(cfg.acp.mcp.teardown_timeout_secs),
    });
    *state_out = Some(Arc::clone(&state));

    AcpAgentRole
        .builder()
        .name("daimonos")
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                move |req: InitializeRequest,
                      responder: agent_client_protocol::Responder<InitializeResponse>,
                      _cx: ConnectionTo<AcpClientRole>| {
                    let persistence_enabled = state.store.is_some();
                    let supports_images = state.supports_images;
                    let state = Arc::clone(&state);
                    async move {
                        state
                            .supports_terminal_output
                            .store(client_supports_terminal_output(&req), Ordering::Release);
                        // load_session(true): Zed calls session/load to reopen
                        // a thread on window refocus.
                        let mut capabilities = AgentCapabilities::new()
                            .load_session(true)
                            .prompt_capabilities(
                                PromptCapabilities::new()
                                    .embedded_context(true)
                                    .image(supports_images),
                            );
                        if persistence_enabled {
                            capabilities = capabilities.session_capabilities(
                                SessionCapabilities::new()
                                    .list(SessionListCapabilities::new())
                                    .delete(SessionDeleteCapabilities::new()),
                            );
                        }
                        if mcp_enabled {
                            // stdio is always supported; advertise http so Zed
                            // forwards HTTP servers too (ADR-003, D8).
                            capabilities = capabilities
                                .mcp_capabilities(McpCapabilities::new().http(mcp_http));
                        }
                        capabilities = capabilities.meta(Meta::from_iter([
                            (CLIENT_USER_MESSAGE_IDS_META_KEY.to_string(), true.into()),
                            (SESSION_RETRY_META_KEY.to_string(), true.into()),
                            (SESSION_TRUNCATE_META_KEY.to_string(), true.into()),
                        ]));
                        responder.respond(
                            InitializeResponse::new(req.protocol_version)
                                .agent_capabilities(capabilities),
                        )
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                move |req: RetrySessionRequest,
                      responder: agent_client_protocol::Responder<PromptResponse>,
                      cx: ConnectionTo<AcpClientRole>| {
                    let state = Arc::clone(&state);
                    async move {
                        let spawn_cx = cx.clone();
                        let _ = cx.spawn(async move {
                            let handle =
                                state.sessions.lock().await.get(&req.session_id).cloned();
                            match handle {
                                Some(handle) => match run_retry_turn(
                                    &handle,
                                    &spawn_cx,
                                    &req.session_id,
                                )
                                .await
                                {
                                    Ok(stop_reason) => {
                                        responder.respond(PromptResponse::new(stop_reason))?;
                                    }
                                    Err(error) => {
                                        responder.respond_with_error(
                                            agent_client_protocol::util::internal_error(error),
                                        )?;
                                    }
                                },
                                None => {
                                    responder.respond_with_error(
                                        agent_client_protocol::util::internal_error(
                                            "session not found",
                                        ),
                                    )?;
                                }
                            }
                            Ok(())
                        });
                        Ok(())
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                move |req: TruncateSessionRequest,
                      responder: agent_client_protocol::Responder<TruncateSessionResponse>,
                      cx: ConnectionTo<AcpClientRole>| {
                    let state = Arc::clone(&state);
                    async move {
                        let _ = cx.spawn(async move {
                            let handle =
                                state.sessions.lock().await.get(&req.session_id).cloned();
                            match handle {
                                Some(handle) => match truncate_session(
                                    &handle,
                                    &req.session_id,
                                    &req.client_user_message_id,
                                    state.store.as_ref(),
                                )
                                .await
                                {
                                    Ok(()) => {
                                        responder.respond(TruncateSessionResponse {})?;
                                    }
                                    Err(error) => {
                                        responder.respond_with_error(
                                            agent_client_protocol::util::internal_error(error),
                                        )?;
                                    }
                                },
                                None => {
                                    responder.respond_with_error(
                                        agent_client_protocol::util::internal_error(
                                            "session not found",
                                        ),
                                    )?;
                                }
                            }
                            Ok(())
                        });
                        Ok(())
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                let workspace = workspace.clone();
                move |req: ListSessionsRequest,
                      responder: agent_client_protocol::Responder<ListSessionsResponse>,
                      _cx: ConnectionTo<AcpClientRole>| {
                    let store = state.store.clone();
                    let page_size = state.session_list_page_size;
                    let fallback_cwd = workspace.clone();
                    async move {
                        let Some(store) = store else {
                            return responder.respond_with_error(
                                agent_client_protocol::util::internal_error(
                                    "session persistence is disabled",
                                ),
                            );
                        };
                        match paginate_session_summaries(
                            store.list(),
                            req.cwd.as_deref(),
                            req.cursor.as_deref(),
                            page_size,
                            &fallback_cwd,
                        ) {
                            Ok((sessions, next_cursor)) => responder.respond(
                                ListSessionsResponse::new(sessions).next_cursor(next_cursor),
                            ),
                            Err(error) => responder.respond_with_error(
                                agent_client_protocol::Error::invalid_params().data(error),
                            ),
                        }
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                move |req: DeleteSessionRequest,
                      responder: agent_client_protocol::Responder<DeleteSessionResponse>,
                      _cx: ConnectionTo<AcpClientRole>| {
                    let state = Arc::clone(&state);
                    async move {
                        let started = std::time::Instant::now();
                        tracing::info!(
                            target: "daimonos::acp",
                            event = "session_delete_started",
                            session_id = %req.session_id,
                        );
                        let Some(store) = state.store.clone() else {
                            return responder.respond_with_error(
                                agent_client_protocol::util::internal_error(
                                    "session persistence is disabled",
                                ),
                            );
                        };
                        // Signal cancellation before waiting on lifecycle: a
                        // concurrent load may own lifecycle while waiting for
                        // the active prompt's session lock.
                        if let Some(handle) =
                            state.sessions.lock().await.get(&req.session_id).cloned()
                        {
                            request_session_cancel(&handle);
                        }
                        let operation = session_operation_lock(&state, &req.session_id).await;
                        let _operation = operation.lock().await;
                        let existing_handle =
                            state.sessions.lock().await.get(&req.session_id).cloned();
                        if let Some(handle) = &existing_handle {
                            request_session_cancel(handle);
                        }
                        let _lifecycle = match &existing_handle {
                            Some(handle) => Some(handle.core.lifecycle.lock().await),
                            None => None,
                        };
                        let removed_handle = state.sessions.lock().await.remove(&req.session_id);
                        if let Some(handle) = &removed_handle {
                            // Wait for a cancelled turn (or short direct command)
                            // to release the session before removing its file, so
                            // it cannot save itself again after deletion.
                            let session = handle.core.session.lock().await;
                            drop(session);
                        }
                        if let Err(error) = store.delete(&req.session_id.to_string()) {
                            if let Some(handle) = removed_handle {
                                state
                                    .sessions
                                    .lock()
                                    .await
                                    .insert(req.session_id.clone(), handle);
                            }
                            return responder.respond_with_error(
                                agent_client_protocol::util::internal_error(format!(
                                    "failed to delete session: {error}"
                                )),
                            );
                        }
                        if let Some(handle) = &removed_handle {
                            let _ = handle.core.events.emit(CoreSessionEvent::SessionEnding {
                                reason: SESSION_END_REASON_DELETED.to_string(),
                            });
                        }
                        // Session file is gone; tear down its MCP clients so
                        // Zed-spawned stdio servers don't linger (ADR-003, D7).
                        if let Some(handle) = removed_handle {
                            shutdown_session_bridge(&handle).await;
                        }
                        tracing::info!(
                            target: "daimonos::acp",
                            event = "session_delete_completed",
                            session_id = %req.session_id,
                            duration_ms = started.elapsed().as_millis() as u64,
                        );
                        responder.respond(DeleteSessionResponse::new())
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                let workspace = workspace.clone();
                let cfg = Arc::clone(&cfg);
                let safety = Arc::clone(&safety);
                let token_log = token_log.clone();
                move |req: NewSessionRequest,
                      responder: agent_client_protocol::Responder<NewSessionResponse>,
                      cx: ConnectionTo<AcpClientRole>| {
                    let state = Arc::clone(&state);
                    let workspace_fallback = workspace.clone();
                    let cfg = Arc::clone(&cfg);
                    let safety = Arc::clone(&safety);
                    let token_log = token_log.clone();
                    async move {
                        let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
                        // Zed forwards the user's configured MCP servers here
                        // (ADR-003); bridge them into this session. Falls back
                        // to Zed's settings when the forwarded list is empty
                        // (unpatched-Zed cold-start race).
                        let mcp_specs = resolve_mcp_specs(req.mcp_servers, &cfg);
                        let mcp_server_count = mcp_specs.len();
                        // Use the client-provided project root, not the CLI's
                        // own cwd — Zed passes the actual project it wants this
                        // session to operate on.
                        let session_workspace = if req.cwd.as_os_str().is_empty() {
                            workspace_fallback
                        } else {
                            req.cwd
                        };
                        tracing::info!(
                            target: "daimonos::acp",
                            event = "session_new_started",
                            session_id = %session_id,
                            workspace = %session_workspace.display(),
                            mcp_servers = mcp_server_count,
                        );
                        let started = std::time::Instant::now();
                        let handle = match build_session_handle(
                            &state,
                            &cfg,
                            safety,
                            token_log,
                            session_id.clone(),
                            session_workspace,
                            mcp_specs,
                            cx.clone(),
                        )
                        .await
                        {
                            Ok(handle) => handle,
                            Err(e) => {
                                return responder.respond_with_error(
                                    agent_client_protocol::util::internal_error(format!(
                                        "provider init: {e}"
                                    )),
                                );
                            }
                        };
                        state
                            .sessions
                            .lock()
                            .await
                            .insert(session_id.clone(), Arc::clone(&handle));

                        // Persist immediately with empty history so a thread the
                        // user opens but never prompts survives a process restart
                        // and can be resumed by session/load. Without this,
                        // session/new is in-memory only and a cold session/load
                        // for that id fails with "no session found" (vikunja
                        // #1046). Best-effort and fire-and-forget like every
                        // other save_acp call site: save_acp returns unit and
                        // logs internally on a write error, so a persist failure
                        // never fails session creation.
                        if let Some(store) = state.store.as_ref() {
                            store.save_acp(
                                &session_id.to_string(),
                                &state.default_model,
                                state.thinking.as_str(),
                                &[],
                                &handle.core.cwd,
                                &[],
                                &[],
                            );
                        }

                        // Advertise the model picker (vikunja #960); new sessions
                        // start on the default model.
                        let config_options =
                            model_config_options(&state.models, &state.default_model);
                        responder.respond(
                            NewSessionResponse::new(session_id.clone())
                                .config_options(Some(config_options)),
                        )?;
                        send_available_commands(&cx, &session_id);
                        send_session_mcp_diagnostics(&cx, &session_id, &handle).await;
                        tracing::info!(
                            target: "daimonos::acp",
                            event = "session_new_completed",
                            session_id = %session_id,
                            duration_ms = started.elapsed().as_millis() as u64,
                        );
                        Ok(())
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                // session/load (vikunja #961): Zed reopens a thread by calling
                // this. We resolve the session in the same three states Zed's own
                // native providers do (see agent/src/agent.rs open_thread):
                //   1. still live in memory (window-switch)  → replay in-memory
                //   2. not in memory but persisted on disk (process restarted) →
                //      restore history from disk, then replay
                //   3. neither  → error, exactly as native's load_thread does for
                //      an id not in its store ("no thread found"). We do NOT
                //      fabricate an empty "resumed" thread.
                // Either way, replayed `session/update`s rebuild the thread; Zed
                // registers it before this RPC returns so the notifications land.
                let state = Arc::clone(&state);
                let workspace = workspace.clone();
                let cfg = Arc::clone(&cfg);
                let safety = Arc::clone(&safety);
                let token_log = token_log.clone();
                move |req: LoadSessionRequest,
                      responder: agent_client_protocol::Responder<LoadSessionResponse>,
                      cx: ConnectionTo<AcpClientRole>| {
                    let state = Arc::clone(&state);
                    let workspace_fallback = workspace.clone();
                    let cfg = Arc::clone(&cfg);
                    let safety = Arc::clone(&safety);
                    let token_log = token_log.clone();
                    async move {
                        let session_id = req.session_id.clone();
                        // Same empty-forward fallback as session/new: a
                        // reloaded thread must also recover its MCP servers.
                        let mcp_specs = resolve_mcp_specs(req.mcp_servers, &cfg);
                        let session_workspace = if req.cwd.as_os_str().is_empty() {
                            workspace_fallback
                        } else {
                            req.cwd
                        };
                        let operation = session_operation_lock(&state, &session_id).await;
                        let _operation = operation.lock().await;
                        let existing = state.sessions.lock().await.get(&session_id).cloned();
                        let was_live = existing.is_some();
                        let started = std::time::Instant::now();
                        tracing::info!(
                            target: "daimonos::acp",
                            event = "session_load_started",
                            session_id = %session_id,
                            live = was_live,
                            workspace = %session_workspace.display(),
                        );
                        // Keep this guard through replay and response enqueue.
                        // session/delete takes the same guard before removing
                        // the handle, giving load/delete a single linear order.
                        let _lifecycle = match &existing {
                            Some(handle) => Some(handle.core.lifecycle.lock().await),
                            None => None,
                        };
                        if let Some(handle) = &existing {
                            let still_current = state
                                .sessions
                                .lock()
                                .await
                                .get(&session_id)
                                .is_some_and(|current| Arc::ptr_eq(current, handle));
                            if !still_current {
                                return responder.respond_with_error(
                                    agent_client_protocol::util::internal_error(format!(
                                        "session '{session_id}' was deleted while loading"
                                    )),
                                );
                            }
                        }
                        let (current_model, active_handle, send_diagnostics) =
                            if let Some(handle) = &existing {
                            let current_specs = handle.mcp_specs.lock().await.clone();
                            let bridge = session_bridge(handle).await;
                            let refresh_bridge = should_refresh_mcp_bridge(
                                &current_specs,
                                &mcp_specs,
                                bridge.had_connection_failures(),
                            );
                            drop(bridge);
                            if refresh_bridge {
                                // Zed re-sends MCP configuration on every load.
                                // Refresh routing and schemas in place when it
                                // changed, or retry servers that failed earlier.
                                refresh_live_mcp_bridge(
                                    handle,
                                    &state,
                                    &cfg,
                                    mcp_specs,
                                )
                                .await;
                            }
                            // Live sessions always preserve provider, history,
                            // usage, compaction cache, and tool-session state.
                            let agent_session = handle.core.session.lock().await;
                            replay_history(&cx, &session_id, agent_session.history());
                            drop(agent_session);
                            let model = handle
                                .core
                                .current_model
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .clone();
                            (model, Arc::clone(handle), refresh_bridge)
                        } else if let Some(record) = state
                            .store
                            .as_ref()
                            .and_then(|s| s.load(&session_id.to_string()))
                        {
                            // 2. Persisted on disk (process was restarted):
                            // rebuild the session, seed its history + model, then
                            // replay so Zed reconstructs the thread.
                            let handle = match build_session_handle(
                                &state,
                                &cfg,
                                safety,
                                token_log,
                                session_id.clone(),
                                session_workspace,
                                mcp_specs,
                                cx.clone(),
                            )
                            .await
                            {
                                Ok(handle) => handle,
                                Err(e) => {
                                    return responder.respond_with_error(
                                        agent_client_protocol::util::internal_error(format!(
                                            "provider init: {e}"
                                        )),
                                    );
                                }
                            };
                            let model = record.model.clone();
                            {
                                let mut agent_session = handle.core.session.lock().await;
                                agent_session.set_history(record.messages);
                                agent_session.set_model(model.clone());
                                let mut client_ids = record.client_user_message_ids;
                                align_client_user_message_ids(
                                    &mut client_ids,
                                    agent_session.user_turn_count(),
                                );
                                *handle.core.client_user_message_ids.lock().await = client_ids;
                                replay_history(&cx, &session_id, agent_session.history());
                            }
                            *handle
                                .core
                                .current_model
                                .lock()
                                .unwrap_or_else(|p| p.into_inner()) = model.clone();
                            state
                                .sessions
                                .lock()
                                .await
                                .insert(session_id.clone(), Arc::clone(&handle));
                            (model, handle, true)
                        } else {
                            // 3. Unknown: not live, nothing persisted. Match
                            // native providers — error rather than fake a resume.
                            return responder.respond_with_error(
                                agent_client_protocol::util::internal_error(format!(
                                    "no session found with id '{session_id}'"
                                )),
                            );
                        };
                        // Echo the model picker (vikunja #960) with the session's
                        // current model, as session/new does.
                        let config_options = model_config_options(&state.models, &current_model);
                        responder.respond(
                            LoadSessionResponse::new().config_options(Some(config_options)),
                        )?;
                        send_available_commands(&cx, &session_id);
                        // Notifications must follow a successfully queued load
                        // response: Zed registers the session while handling
                        // that response, then accepts its session updates.
                        if send_diagnostics {
                            send_session_mcp_diagnostics(&cx, &session_id, &active_handle).await;
                        }
                        tracing::info!(
                            target: "daimonos::acp",
                            event = "session_load_completed",
                            session_id = %session_id,
                            live = was_live,
                            duration_ms = started.elapsed().as_millis() as u64,
                        );
                        Ok(())
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                move |req: PromptRequest,
                      responder: agent_client_protocol::Responder<PromptResponse>,
                      cx: ConnectionTo<AcpClientRole>| {
                    let state = Arc::clone(&state);
                    async move {
                        // Must use `cx.spawn`, not a bare `tokio::spawn`: requests
                        // sent via `.block_task()` from within this task (e.g. the
                        // permission request in `before_tool_call`) only get their
                        // response routed correctly when the task is registered
                        // with the connection this way (see `SentRequest::block_task`'s
                        // docs — "use this when you're in a spawned task via
                        // ConnectionTo::spawn").
                        let spawn_cx = cx.clone();
                        let _ = cx.spawn(async move {
                            let client_user_message_id = req
                                .meta
                                .as_ref()
                                .and_then(|meta| meta.get(CLIENT_USER_MESSAGE_ID_META_KEY))
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string);
                            let session_id = req.session_id;
                            let started = std::time::Instant::now();
                            tracing::info!(
                                target: "daimonos::acp",
                                event = "prompt_started",
                                session_id = %session_id,
                            );
                            let handle = state.sessions.lock().await.get(&session_id).cloned();
                            let turn_result = match handle {
                                Some(handle) => {
                                    let model = handle
                                        .core
                                        .current_model
                                        .lock()
                                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                                        .clone();
                                    // Observability must not wait behind the
                                    // active turn's long-held session lock: the
                                    // prompt path owns the atomic Busy check.
                                    let (turn_index, tools_exposed) = handle
                                        .core
                                        .session
                                        .try_lock()
                                        .map(|session| {
                                            (session.user_turn_count(), session.tool_count())
                                        })
                                        .unwrap_or((0, 0));
                                    let session_key = session_id.to_string();
                                    let prompt_span = PromptSpan::new(PromptMetadata {
                                        mode: "acp",
                                        session_id: Some(&session_key),
                                        model: &model,
                                        workspace: &handle.core.cwd,
                                        turn_index,
                                        tools_exposed,
                                    });
                                    let user_message = prompt_message(req.prompt);
                                    let turn_result = if message_has_images(&user_message)
                                        && !state.supports_images
                                    {
                                        send_notification(
                                            &spawn_cx,
                                            &session_id,
                                            SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                                AcpContentBlock::Text(TextContent::new(
                                                    "The configured provider does not support image prompts.",
                                                )),
                                            )),
                                        );
                                        Ok(AcpStopReason::EndTurn)
                                    } else {
                                        let assistant_prefix = state
                                            .timestamp_turns
                                            .then(|| turn_timestamp_line(chrono::Local::now()));
                                        run_prompt_turn(
                                            &handle,
                                            &spawn_cx,
                                            &session_id,
                                            user_message,
                                            client_user_message_id,
                                            assistant_prefix,
                                        )
                                        .instrument(prompt_span.span().clone())
                                        .await
                                    };
                                    match turn_result {
                                        Ok(stop_reason) => {
                                            let error_type = match stop_reason {
                                                AcpStopReason::Refusal => Some("refusal"),
                                                AcpStopReason::Cancelled => {
                                                    Some("client_cancelled")
                                                }
                                                _ => None,
                                            };
                                            prompt_span.finish(
                                                acp_stop_reason_name(&stop_reason),
                                                error_type,
                                            );
                                            Ok(stop_reason)
                                        }
                                        Err(error) => {
                                            prompt_span.finish("error", Some("busy"));
                                            Err(error)
                                        }
                                    }
                                }
                                None => {
                                    send_notification(
                                        &spawn_cx,
                                        &session_id,
                                        SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                            AcpContentBlock::Text(TextContent::new(
                                                "ACP session is not available.",
                                            )),
                                        )),
                                    );
                                    Ok(AcpStopReason::EndTurn)
                                }
                            };
                            match turn_result {
                                Ok(stop_reason) => {
                                    tracing::info!(
                                        target: "daimonos::acp",
                                        event = "prompt_completed",
                                        session_id = %session_id,
                                        stop_reason = ?stop_reason,
                                        duration_ms = started.elapsed().as_millis() as u64,
                                    );
                                    let _ = responder.respond(PromptResponse::new(stop_reason));
                                }
                                Err(error) => {
                                    tracing::info!(
                                        target: "daimonos::acp",
                                        event = "prompt_rejected",
                                        session_id = %session_id,
                                        error = %error,
                                        duration_ms = started.elapsed().as_millis() as u64,
                                    );
                                    let _ = responder.respond_with_error(
                                        agent_client_protocol::util::internal_error(
                                            error.to_string(),
                                        ),
                                    );
                                }
                            }
                            Ok(())
                        });
                        Ok(())
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                // Model picker (vikunja #960): the user picked a model in Zed's
                // dropdown. Update this session's current-model cell (cheap, no
                // dispatch-loop stall, no wait on an in-flight prompt's session
                // lock); it's applied to the session on the next prompt turn.
                let state = Arc::clone(&state);
                move |req: SetSessionConfigOptionRequest,
                      responder: agent_client_protocol::Responder<
                    SetSessionConfigOptionResponse,
                >,
                      _cx: ConnectionTo<AcpClientRole>| {
                    let state = Arc::clone(&state);
                    async move {
                        let handle = state.sessions.lock().await.get(&req.session_id).cloned();
                        // Current value defaults to the session's start model if
                        // the session is unknown (shouldn't happen from a real
                        // client, but keep the echo sensible).
                        let mut current = state.default_model.clone();
                        if let Some(handle) = handle {
                            let mut changed = false;
                            if req.config_id.to_string() == MODEL_CONFIG_ID {
                                if let Some(value) = req.value.as_value_id() {
                                    let picked = value.to_string();
                                    // Only honor a value we actually advertised.
                                    if state.models.iter().any(|m| m == &picked) {
                                        let mut selected = handle
                                            .core
                                            .current_model
                                            .lock()
                                            .unwrap_or_else(|p| p.into_inner());
                                        if *selected != picked {
                                            *selected = picked;
                                            changed = true;
                                        }
                                    }
                                }
                            }
                            current = handle
                                .core
                                .current_model
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .clone();
                            if changed {
                                let _ = handle.core.events.emit(
                                    CoreSessionEvent::RuntimeOptionsChanged {
                                        options: canonical_model_options(&state.models, &current),
                                    },
                                );
                            }
                        }
                        let options = model_config_options(&state.models, &current);
                        responder.respond(SetSessionConfigOptionResponse::new(options))
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let state = Arc::clone(&state);
                move |notif: CancelNotification, _cx| {
                    let state = Arc::clone(&state);
                    async move {
                        let handle = state.sessions.lock().await.get(&notif.session_id).cloned();
                        if let Some(handle) = handle {
                            request_session_cancel(&handle);
                        }
                        Ok(())
                    }
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_dispatch(
            async move |message: Dispatch, cx: ConnectionTo<AcpClientRole>| {
                // `Dispatch::Response` here is a legitimate correlated
                // response to a request *we* sent (e.g. session/request_
                // permission) — it must pass through unclaimed so the
                // framework's own internal routing can deliver it to the
                // waiting `SentRequest`. Only genuinely unhandled incoming
                // requests/notifications should be rejected.
                if matches!(message, Dispatch::Response(..)) {
                    return Ok(agent_client_protocol::Handled::No {
                        message,
                        retry: false,
                    });
                }
                message.respond_with_error(
                    agent_client_protocol::util::internal_error("unhandled ACP method"),
                    cx,
                )?;
                Ok(agent_client_protocol::Handled::Yes)
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
}

/// Run the `daimonos acp` engine to completion (until stdin closes).
/// `make_provider` builds a fresh provider per session (Zed keeps one
/// process across chat threads); it's validated once up front so a
/// misconfigured provider fails fast rather than only on the first session.
#[allow(clippy::too_many_arguments)]
pub async fn run_acp(
    make_provider: ProviderFactory,
    workspace: &Path,
    cfg: Arc<Config>,
    model: String,
    models: Vec<String>,
    safety: crate::safety::SafetyPolicy,
    token_log: Option<PathBuf>,
    sessions_dir: Option<PathBuf>,
    compaction: SessionCompaction,
    analytics: Option<Arc<AnalyticsStore>>,
    timestamp_turns: bool,
    thinking: crate::providers::ThinkingLevel,
) -> anyhow::Result<()> {
    tracing::info!(
        target: "daimonos::acp",
        event = "acp_starting",
        workspace = %workspace.display(),
        configured_models = models.len(),
        persistence_enabled = sessions_dir.is_some(),
    );
    if let Err(e) = (make_provider)() {
        tracing::error!(target: "daimonos::acp", event = "provider_init_failed", error = %e);
        anyhow::bail!("provider init: {e}");
    }
    tracing::info!(target: "daimonos::acp", event = "provider_validated");
    let mut state_out: Option<Arc<AcpState>> = None;
    let agent = build_agent_with_state(
        make_provider,
        workspace,
        cfg,
        model,
        models,
        Arc::new(safety),
        token_log,
        sessions_dir,
        compaction,
        analytics,
        timestamp_turns,
        thinking,
        &mut state_out,
    );
    // The upstream ACP stdio transport waits for both its incoming and
    // outgoing actors. After stdin EOF, the outgoing actor can remain parked
    // on its internal channel forever, orphaning this process. Race the
    // connection against an EOF signal emitted by the reader itself.
    let eof = Arc::new(tokio::sync::Notify::new());
    let input_error = Arc::new(StdMutex::new(None));
    let eof_wait = {
        let eof = Arc::clone(&eof);
        let input_error = Arc::clone(&input_error);
        async move {
            eof.notified().await;
            input_error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
        }
    };
    // A child spawned before this build may have inherited fd 0 and left it
    // non-blocking, which would make every blocking read fail with EAGAIN.
    match clear_fd_nonblocking(ACP_STDIN_FD) {
        Ok(true) => tracing::warn!(
            target: "daimonos::acp",
            event = "stdin_nonblocking_cleared_at_startup",
        ),
        Ok(false) => {}
        Err(error) => tracing::warn!(
            target: "daimonos::acp",
            event = "stdin_flag_probe_failed",
            error = %error,
        ),
    }
    let stdin = EofAwareReader::new(
        blocking::Unblock::new(ResilientReader::new(
            std::io::stdin(),
            "stdin",
            Some(ACP_STDIN_FD),
        )),
        Arc::clone(&eof),
        input_error,
    );
    let stdout = blocking::Unblock::new(ResilientWriter::new(std::io::stdout(), "stdout"));
    let connection = agent.connect_to(ByteStreams::new(stdout, stdin));
    tokio::pin!(connection);
    tokio::pin!(eof_wait);
    let (result, input_error) = tokio::select! {
        result = &mut connection => (Some(result), None),
        input_error = &mut eof_wait => (None, input_error),
    };
    tracing::info!(
        target: "daimonos::acp",
        event = "acp_transport_stopped",
        reason = if input_error.is_some() { "stdin_error" } else if result.is_some() { "connection_completed" } else { "stdin_eof" },
    );
    // stdin closed (or the connection errored): drain every live session's MCP
    // bridge so Zed-spawned stdio servers are reaped instead of leaking (D7/D8).
    if let Some(state) = state_out {
        tracing::info!(target: "daimonos::acp", event = "session_shutdown_started");
        shutdown_all_bridges(&state).await;
        tracing::info!(target: "daimonos::acp", event = "session_shutdown_completed");
    }
    if let Some(error) = input_error {
        anyhow::bail!("ACP stdin read failed: {error}");
    }
    if let Some(result) = result {
        result?;
    }
    Ok(())
}

/// Shut down the MCP bridge of every live session. Called on ACP engine
/// teardown; `session/delete` already tears down a single session's bridge, so
/// this handles the process-exit path where handles are otherwise just dropped.
#[derive(Debug, PartialEq, Eq)]
struct BridgeShutdownSummary {
    sessions: usize,
    completed: usize,
    remaining: usize,
    timed_out: bool,
    duration_ms: u64,
}

async fn shutdown_session_handles_with<F, Fut>(
    handles: Vec<Arc<SessionHandle>>,
    timeout: std::time::Duration,
    shutdown: F,
) -> BridgeShutdownSummary
where
    F: Fn(Arc<SessionHandle>) -> Fut,
    Fut: Future<Output = ()>,
{
    let started = std::time::Instant::now();
    let sessions = handles.len();
    for handle in &handles {
        // vikunja #1107 fix #2: retire the work this transport owned before
        // tearing its bridge down. Signalling cancel here is what stops an
        // in-flight prompt from running detached from the client that asked
        // for it.
        request_session_cancel(handle);
        // A turn parked on an approval never observes cancellation. Resolve
        // approvals and close canonical tool calls before awaiting teardown.
        handle.core.cleanup_cancelled_turn();
        let _ = handle.core.events.emit(CoreSessionEvent::SessionEnding {
            reason: SESSION_END_REASON_ENGINE_SHUTDOWN.to_string(),
        });
    }

    let completed = Arc::new(AtomicUsize::new(0));
    let shutdowns = handles.into_iter().map(|handle| {
        let completed = Arc::clone(&completed);
        let shutdown = shutdown(handle);
        async move {
            shutdown.await;
            completed.fetch_add(1, Ordering::Relaxed);
        }
    });
    let timed_out = tokio::time::timeout(timeout, futures_util::future::join_all(shutdowns))
        .await
        .is_err();
    let completed = completed.load(Ordering::Relaxed);

    BridgeShutdownSummary {
        sessions,
        completed,
        remaining: sessions.saturating_sub(completed),
        timed_out,
        duration_ms: started.elapsed().as_millis() as u64,
    }
}

async fn shutdown_all_bridges(state: &AcpState) -> BridgeShutdownSummary {
    let handles: Vec<Arc<SessionHandle>> = {
        let mut map = state.sessions.lock().await;
        map.drain().map(|(_, handle)| handle).collect()
    };
    let timeout = state.mcp_teardown_timeout;
    let summary = shutdown_session_handles_with(handles, timeout, |handle| async move {
        shutdown_session_bridge(&handle).await;
    })
    .await;
    if summary.timed_out {
        tracing::warn!(
            target: "daimonos::acp",
            event = "bridge_shutdown_deadline_expired",
            sessions = summary.sessions,
            completed = summary.completed,
            remaining = summary.remaining,
            timeout_ms = timeout.as_millis() as u64,
            duration_ms = summary.duration_ms,
        );
    } else {
        tracing::info!(
            target: "daimonos::acp",
            event = "bridge_shutdown_batch_completed",
            sessions = summary.sessions,
            duration_ms = summary.duration_ms,
        );
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction::CompactionPolicy;
    use agent_client_protocol::schema::ProtocolVersion;
    use async_trait::async_trait;
    use futures_util::io::AsyncReadExt;
    use std::collections::VecDeque;

    #[derive(Default)]
    struct TransientWriter {
        write_interrupted_remaining: usize,
        write_zero_remaining: usize,
        write_would_block_remaining: usize,
        flush_interrupted_remaining: usize,
        flush_would_block_remaining: usize,
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl std::io::Write for TransientWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            if self.write_interrupted_remaining > 0 {
                self.write_interrupted_remaining -= 1;
                return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
            }
            if self.write_would_block_remaining > 0 {
                self.write_would_block_remaining -= 1;
                return Err(std::io::Error::from(std::io::ErrorKind::WouldBlock));
            }
            if self.write_zero_remaining > 0 {
                self.write_zero_remaining -= 1;
                return Ok(0);
            }
            self.bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            if self.flush_interrupted_remaining > 0 {
                self.flush_interrupted_remaining -= 1;
                return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
            }
            if self.flush_would_block_remaining > 0 {
                self.flush_would_block_remaining -= 1;
                return Err(std::io::Error::from(std::io::ErrorKind::WouldBlock));
            }
            self.flushes += 1;
            Ok(())
        }
    }

    struct AlwaysWouldBlockWriter;

    impl std::io::Write for AlwaysWouldBlockWriter {
        fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct FatalWriter;

    impl std::io::Write for FatalWriter {
        fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "denied",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn resilient_writer_retries_transient_write_and_flush_would_block() {
        let inner = TransientWriter {
            write_interrupted_remaining: 1,
            write_zero_remaining: 1,
            write_would_block_remaining: 2,
            flush_interrupted_remaining: 1,
            flush_would_block_remaining: 1,
            ..Default::default()
        };
        let mut writer = ResilientWriter::new(inner, "stdout");
        assert_eq!(std::io::Write::write(&mut writer, b"hello").unwrap(), 5);
        std::io::Write::flush(&mut writer).unwrap();
        assert_eq!(writer.inner.bytes, b"hello");
        assert_eq!(writer.inner.flushes, 1);
    }

    #[test]
    fn resilient_writer_times_out_persistent_would_block() {
        let mut writer = ResilientWriter::with_retry_limit(AlwaysWouldBlockWriter, "stdout", 3);
        let error = std::io::Write::write(&mut writer, b"x").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(error
            .to_string()
            .contains("ACP stdout write remained blocked"));
        assert!(error.to_string().contains("3 attempts"));
    }

    #[test]
    fn resilient_writer_preserves_real_error_kind_and_adds_direction() {
        let mut writer = ResilientWriter::new(FatalWriter, "stdout");
        let error = std::io::Write::write(&mut writer, b"x").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("ACP stdout write failed"));
        assert!(error.to_string().contains("denied"));
    }

    #[derive(Default)]
    struct TransientReader {
        interrupted_remaining: usize,
        would_block_remaining: usize,
        payload: Vec<u8>,
    }

    impl std::io::Read for TransientReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.interrupted_remaining > 0 {
                self.interrupted_remaining -= 1;
                return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
            }
            if self.would_block_remaining > 0 {
                self.would_block_remaining -= 1;
                return Err(std::io::Error::from(std::io::ErrorKind::WouldBlock));
            }
            let count = self.payload.len().min(buffer.len());
            buffer[..count].copy_from_slice(&self.payload[..count]);
            self.payload.drain(..count);
            Ok(count)
        }
    }

    struct FatalReader;

    impl std::io::Read for FatalReader {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "denied",
            ))
        }
    }

    #[test]
    fn resilient_reader_retries_transient_interrupted_and_would_block() {
        let inner = TransientReader {
            interrupted_remaining: 1,
            would_block_remaining: 3,
            payload: b"hello".to_vec(),
        };
        let mut reader = ResilientReader::new(inner, "stdin", None);
        let mut buffer = [0u8; 8];
        let read = std::io::Read::read(&mut reader, &mut buffer).unwrap();
        assert_eq!(&buffer[..read], b"hello");
    }

    #[test]
    fn resilient_reader_reports_eof_without_retrying() {
        // Ok(0) is a closed peer, not backpressure. Retrying it would hang the
        // shutdown path that waits on stdin EOF.
        let mut reader = ResilientReader::new(TransientReader::default(), "stdin", None);
        let mut buffer = [0u8; 8];
        assert_eq!(std::io::Read::read(&mut reader, &mut buffer).unwrap(), 0);
    }

    #[test]
    fn resilient_reader_preserves_real_error_kind_and_adds_direction() {
        let mut reader = ResilientReader::new(FatalReader, "stdin", None);
        let mut buffer = [0u8; 8];
        let error = std::io::Read::read(&mut reader, &mut buffer).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("ACP stdin read failed"));
        assert!(error.to_string().contains("denied"));
    }

    #[test]
    fn stdio_would_block_backoff_is_bounded_for_writes_and_unbounded_for_reads() {
        // The write side must eventually give up so a permanently blocked pipe
        // cannot hang ACP forever.
        let error = note_stdio_would_block("stdout", "write", 3, Some(3)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(error
            .to_string()
            .contains("ACP stdout write remained blocked"));
        // The read side must not: an idle session has nothing to read, and any
        // bound would eventually kill a perfectly healthy connection.
        note_stdio_would_block("stdin", "read", 10_000, None).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn clear_fd_nonblocking_reports_and_clears_the_flag() {
        let mut fds = [0i32; 2];
        // SAFETY: `fds` is the two-element array pipe(2) writes its ends into.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (read_fd, write_fd) = (fds[0], fds[1]);

        // A fresh pipe is blocking, so there is nothing to clear.
        assert!(!clear_fd_nonblocking(read_fd).unwrap());

        // SAFETY: `read_fd` is the live read end of the pipe above.
        let flags = unsafe { libc::fcntl(read_fd, libc::F_GETFL) };
        assert_eq!(
            unsafe { libc::fcntl(read_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );

        assert!(clear_fd_nonblocking(read_fd).unwrap());
        assert_eq!(
            unsafe { libc::fcntl(read_fd, libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );
        // Idempotent: a second call has nothing left to do.
        assert!(!clear_fd_nonblocking(read_fd).unwrap());

        // SAFETY: both ends are still open and owned by this test.
        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
        }
    }

    #[cfg(unix)]
    #[test]
    fn resilient_reader_recovers_a_child_poisoned_descriptor() {
        // The regression this whole change exists for: a child process left
        // O_NONBLOCK on an inherited descriptor, so a blocking read returns
        // EAGAIN. The reader must repair the flag and keep reading instead of
        // failing the transport.
        use std::os::fd::FromRawFd;

        let mut fds = [0i32; 2];
        // SAFETY: `fds` is the two-element array pipe(2) writes its ends into.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (read_fd, write_fd) = (fds[0], fds[1]);
        // SAFETY: `read_fd` is the live read end of the pipe above.
        let flags = unsafe { libc::fcntl(read_fd, libc::F_GETFL) };
        assert_eq!(
            unsafe { libc::fcntl(read_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );

        // Nothing is buffered yet, so the first attempt sees EAGAIN. The write
        // lands well after the flag has been repaired, so the retry blocks
        // normally and returns data.
        let writer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            // SAFETY: `write_fd` is the live write end of the pipe above.
            unsafe {
                assert_eq!(libc::write(write_fd, b"ok".as_ptr().cast(), 2), 2);
                libc::close(write_fd);
            }
        });

        // SAFETY: takes ownership of the read end, closed when `file` drops.
        let file = unsafe { std::fs::File::from_raw_fd(read_fd) };
        let mut reader = ResilientReader::new(file, "stdin", Some(read_fd));
        let mut buffer = [0u8; 8];
        let read = std::io::Read::read(&mut reader, &mut buffer).unwrap();
        assert_eq!(&buffer[..read], b"ok");
        // SAFETY: `reader` still owns the descriptor at this point.
        assert_eq!(
            unsafe { libc::fcntl(read_fd, libc::F_GETFL) } & libc::O_NONBLOCK,
            0,
            "the retry path should have cleared O_NONBLOCK"
        );
        writer.join().unwrap();
    }

    #[tokio::test]
    async fn acp_tool_session_execute_script_can_call_builtin_git_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(init.success());

        let cfg = Arc::new(Config::default());
        let tool_session = build_acp_tool_session(dir.path(), &cfg, None, "acp-test-session").await;
        let result = crate::script::execute(
            "result = git(command=\"status\")",
            Arc::new(tokio::sync::Mutex::new(tool_session)),
            std::time::Duration::from_secs(10),
        )
        .await
        .expect("ACP execute_script should dispatch the built-in git plugin");
        assert_eq!(result.value["clean"], true);
    }

    #[test]
    fn coordination_ui_update_is_visible_agent_message_and_metadata_only() {
        let text = "Daimonos agent mail: 1 new unread message(s) for NotifyB (highest importance: high). The agent will be notified at its next safe action boundary.";
        let update = coordination_ui_update(text.to_string());
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => match chunk.content {
                AcpContentBlock::Text(content) => {
                    assert_eq!(content.text, text);
                    assert!(content.text.starts_with("Daimonos agent mail:"));
                    assert!(!content.text.contains("SUBJECT_SECRET"));
                    assert!(!content.text.contains("BODY_SECRET"));
                }
                other => panic!("expected text content, got {other:?}"),
            },
            other => panic!("expected visible AgentMessageChunk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn coordination_ui_poll_skips_while_agent_session_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let db_dir = dir.path().join("coord-db");
        let mut cfg = Config::default();
        cfg.coordination.db_dir = Some(db_dir.to_string_lossy().to_string());
        let cfg = Arc::new(cfg);
        let mut tool_state = Session::new(dir.path().to_path_buf(), Arc::clone(&cfg));
        tool_state.coordination_agent_name = Some("BlueLake".to_string());
        let db = crate::coordination::workspace_db_path(&db_dir, dir.path());
        let store = crate::coordination::CoordinationStore::open_with(&db, 5_000).unwrap();
        store
            .send_message(
                "RedStone",
                &["BlueLake".into()],
                &[],
                "secret subject",
                "secret body",
                crate::coordination::Importance::High,
                false,
                None,
                "2026-07-25T00:00:00Z",
            )
            .unwrap();
        let agent_session = tokio::sync::Mutex::new(AgentSession::new(
            Box::new(MockProvider::new(vec![])),
            tool_state,
            AgentConfig::default(),
        ));
        let notification_tool_session = {
            let guard = agent_session.lock().await;
            guard.coordination_tool_session()
        };

        // Simulate an active prompt/tool loop holding AgentSession. The poll is
        // non-blocking and must skip without advancing the UI watermark.
        let busy_guard = agent_session.lock().await;
        assert!(
            poll_coordination_ui_notice_if_idle(&agent_session, &notification_tool_session)
                .await
                .is_none()
        );
        assert_eq!(
            notification_tool_session
                .lock()
                .await
                .coordination_ui_watermark,
            0
        );
        drop(busy_guard);

        // Once idle, the same pending message is surfaced.
        assert!(
            poll_coordination_ui_notice_if_idle(&agent_session, &notification_tool_session)
                .await
                .is_some()
        );
    }

    #[test]
    fn resolve_mcp_specs_fallback_gating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{ "context_servers": { "x": { "url": "http://127.0.0.1:9/mcp/" } } }"#,
        )
        .unwrap();
        let mut cfg = Config::default();
        cfg.acp.mcp.zed_settings_path = Some(path.to_string_lossy().into_owned());

        // Enabled + empty forward -> read Zed config fallback.
        cfg.acp.mcp.zed_config_fallback = true;
        assert_eq!(resolve_mcp_specs(vec![], &cfg).len(), 1);

        // Disabled (the default) -> no fallback, even with an empty forward.
        cfg.acp.mcp.zed_config_fallback = false;
        assert!(resolve_mcp_specs(vec![], &cfg).is_empty());

        // A non-empty forward is never overridden by the fallback.
        cfg.acp.mcp.zed_config_fallback = true;
        let forwarded = vec![McpServer::Http(
            agent_client_protocol::schema::v1::McpServerHttp::new(
                "fwd".to_string(),
                "http://127.0.0.1:10/".to_string(),
            ),
        )];
        let specs = resolve_mcp_specs(forwarded, &cfg);
        assert_eq!(specs.len(), 1);
        assert!(matches!(&specs[0], ServerSpec::Http { name, .. } if name == "fwd"));
    }
    use std::time::Duration;

    // --- MockProvider (mirrors agent.rs/agent_cmd.rs test doubles) ---

    struct MockProvider {
        responses: StdMutex<VecDeque<crate::providers::LlmResponse>>,
    }

    struct GatedToolProvider {
        calls: StdMutex<usize>,
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    struct ErrorReader;

    impl AsyncRead for ErrorReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
            _buffer: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(std::io::Error::other("input failed")))
        }
    }

    impl MockProvider {
        fn new(responses: Vec<crate::providers::LlmResponse>) -> Self {
            MockProvider {
                responses: StdMutex::new(VecDeque::from(responses)),
            }
        }
    }

    /// A `ProviderFactory` that yields a fresh `MockProvider` (with a fresh
    /// copy of `responses`) per session — matches how the real engine builds
    /// one provider per `session/new`.
    fn mock_factory(responses: Vec<crate::providers::LlmResponse>) -> ProviderFactory {
        Arc::new(move || Ok(Box::new(MockProvider::new(responses.clone())) as Box<dyn LlmProvider>))
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        async fn complete(
            &self,
            _ctx: &crate::providers::Context,
            _opts: &CompleteOpts,
        ) -> crate::providers::LlmResponse {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| crate::providers::LlmResponse::error("MockProvider exhausted"))
        }

        // Real providers stream text deltas; mirror that here (instead of
        // inheriting the no-op default) so tests exercise the same
        // AgentMessageChunk path production traffic does.
        async fn stream(
            &self,
            ctx: &crate::providers::Context,
            opts: &CompleteOpts,
            on_event: &mut (dyn FnMut(StreamEvent) + Send),
        ) -> crate::providers::LlmResponse {
            let response = self.complete(ctx, opts).await;
            for block in &response.content {
                match block {
                    crate::providers::ContentBlock::Text(text) => {
                        on_event(StreamEvent::TextDelta(text.clone()));
                    }
                    crate::providers::ContentBlock::Thinking(text) => {
                        on_event(StreamEvent::ThinkingDelta(text.clone()));
                    }
                    _ => {}
                }
            }
            response
        }

        async fn context_window(&self, _model: &str) -> Option<u64> {
            Some(200_000)
        }
    }

    #[async_trait]
    impl LlmProvider for GatedToolProvider {
        async fn complete(
            &self,
            _ctx: &crate::providers::Context,
            _opts: &CompleteOpts,
        ) -> crate::providers::LlmResponse {
            let call = {
                let mut calls = self
                    .calls
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let call = *calls;
                *calls += 1;
                call
            };
            if call == 0 {
                self.started.notify_one();
                self.release.notified().await;
                tool_call_resp(
                    "gated-tool",
                    "exec",
                    serde_json::json!({"command": "printf approved"}),
                )
            } else {
                end_turn_resp("done")
            }
        }

        async fn context_window(&self, _model: &str) -> Option<u64> {
            Some(200_000)
        }
    }

    struct ImageCapableProvider;

    #[async_trait]
    impl LlmProvider for ImageCapableProvider {
        async fn complete(
            &self,
            _ctx: &crate::providers::Context,
            _opts: &CompleteOpts,
        ) -> crate::providers::LlmResponse {
            end_turn_resp("done")
        }

        fn supports_images(&self) -> bool {
            true
        }
    }

    fn image_capable_factory() -> ProviderFactory {
        Arc::new(|| Ok(Box::new(ImageCapableProvider)))
    }

    fn end_turn_resp(text: &str) -> crate::providers::LlmResponse {
        crate::providers::LlmResponse {
            retryable: false,
            content: vec![crate::providers::ContentBlock::Text(text.to_string())],
            stop_reason: crate::providers::StopReason::EndTurn,
            error_message: None,
            context_overflow: false,
            usage: Usage {
                input: 10,
                output: 5,
                ..Usage::default()
            },
        }
    }

    fn thinking_resp(thinking: &str, text: &str) -> crate::providers::LlmResponse {
        crate::providers::LlmResponse {
            retryable: false,
            content: vec![
                crate::providers::ContentBlock::Thinking(thinking.to_string()),
                crate::providers::ContentBlock::Text(text.to_string()),
            ],
            stop_reason: crate::providers::StopReason::EndTurn,
            error_message: None,
            context_overflow: false,
            usage: Usage::default(),
        }
    }

    fn tool_call_resp(
        id: &str,
        name: &str,
        input: serde_json::Value,
    ) -> crate::providers::LlmResponse {
        crate::providers::LlmResponse {
            retryable: false,
            content: vec![crate::providers::ContentBlock::ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                input,
            }],
            stop_reason: crate::providers::StopReason::ToolUse,
            error_message: None,
            context_overflow: false,
            usage: Usage::default(),
        }
    }

    /// Never resolves inside any reasonable test timeout — used to prove
    /// `session/cancel` actually short-circuits an in-flight prompt rather
    /// than the test just happening to finish naturally.
    struct SlowProvider;

    #[async_trait]
    impl LlmProvider for SlowProvider {
        async fn complete(
            &self,
            _ctx: &crate::providers::Context,
            _opts: &CompleteOpts,
        ) -> crate::providers::LlmResponse {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            end_turn_resp("too slow")
        }
    }

    // --- full protocol flow, in-process (no subprocess/stdio needed) ---

    #[tokio::test]
    async fn acp_initialize_session_new_prompt_flow() {
        let dir = tempfile::tempdir().unwrap();
        let make_provider = mock_factory(vec![end_turn_resp("hello from daimonos")]);
        let agent = build_agent(
            make_provider,
            dir.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            None,
            None,
            None,
        );

        let updates: Arc<StdMutex<Vec<SessionUpdate>>> = Arc::new(StdMutex::new(Vec::new()));
        let updates_for_handler = Arc::clone(&updates);

        let stop_reason = AcpClientRole
            .builder()
            .on_receive_notification(
                move |notif: SessionNotification, _cx| {
                    let updates = Arc::clone(&updates_for_handler);
                    async move {
                        updates.lock().unwrap().push(notif.update);
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;

                let new_session = connection
                    .send_request(NewSessionRequest::new(dir.path()))
                    .block_task()
                    .await?;

                let prompt_response = connection
                    .send_request(PromptRequest::new(
                        new_session.session_id,
                        vec![AcpContentBlock::Text(TextContent::new("hi"))],
                    ))
                    .block_task()
                    .await?;

                Ok(prompt_response.stop_reason)
            })
            .await
            .unwrap();

        assert_eq!(stop_reason, AcpStopReason::EndTurn);

        let updates = updates.lock().unwrap();
        assert!(
            updates
                .iter()
                .any(|u| matches!(u, SessionUpdate::AgentMessageChunk(_))),
            "expected an AgentMessageChunk update, got: {updates:?}"
        );
        assert!(
            updates
                .iter()
                .any(|u| matches!(u, SessionUpdate::UsageUpdate(_))),
            "expected a UsageUpdate, got: {updates:?}"
        );
        let commands = updates.iter().find_map(|update| match update {
            SessionUpdate::AvailableCommandsUpdate(commands) => Some(&commands.available_commands),
            _ => None,
        });
        let command_names: Vec<&str> = commands
            .expect("session/new must advertise slash commands")
            .iter()
            .map(|command| command.name.as_str())
            .collect();
        assert_eq!(command_names, vec!["clear", "usage", "help"]);
    }

    #[tokio::test]
    async fn acp_zed_retry_and_truncate_extension_round_trips() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let make_provider = mock_factory(vec![
            end_turn_resp("zero"),
            end_turn_resp("first"),
            end_turn_resp("second"),
            end_turn_resp("second retried"),
        ]);
        let mut state_out = None;
        let agent = build_agent_with_state(
            make_provider,
            workspace.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            Some(sessions.path().to_path_buf()),
            SessionCompaction::new(None, false),
            None,
            false,
            crate::providers::ThinkingLevel::default(),
            &mut state_out,
        );
        let state = state_out.expect("ACP state");
        let workspace_path = workspace.path().to_path_buf();

        let session_id = AcpClientRole
            .builder()
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                let initialized = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let meta = initialized
                    .agent_capabilities
                    .meta
                    .expect("extension capabilities");
                assert_eq!(
                    meta.get(SESSION_RETRY_META_KEY),
                    Some(&serde_json::json!(true))
                );
                assert_eq!(
                    meta.get(SESSION_TRUNCATE_META_KEY),
                    Some(&serde_json::json!(true))
                );

                let session_id = connection
                    .send_request(NewSessionRequest::new(&workspace_path))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(PromptRequest::new(
                        session_id.clone(),
                        vec![AcpContentBlock::Text(TextContent::new("zero"))],
                    ))
                    .block_task()
                    .await?;
                for (id, text) in [("user-1", "one"), ("user-2", "two")] {
                    let mut request = PromptRequest::new(
                        session_id.clone(),
                        vec![AcpContentBlock::Text(TextContent::new(text))],
                    );
                    request.meta = Some(Meta::from_iter([(
                        CLIENT_USER_MESSAGE_ID_META_KEY.to_string(),
                        id.into(),
                    )]));
                    connection.send_request(request).block_task().await?;
                    if id == "user-1" {
                        let mut duplicate = PromptRequest::new(
                            session_id.clone(),
                            vec![AcpContentBlock::Text(TextContent::new("duplicate"))],
                        );
                        duplicate.meta = Some(Meta::from_iter([(
                            CLIENT_USER_MESSAGE_ID_META_KEY.to_string(),
                            id.into(),
                        )]));
                        let response = connection.send_request(duplicate).block_task().await?;
                        assert_eq!(response.stop_reason, AcpStopReason::EndTurn);
                    }
                }
                connection
                    .send_request(RetrySessionRequest {
                        session_id: session_id.clone(),
                    })
                    .block_task()
                    .await?;
                assert!(connection
                    .send_request(TruncateSessionRequest {
                        session_id: session_id.clone(),
                        client_user_message_id: "unknown".to_string(),
                    })
                    .block_task()
                    .await
                    .is_err());
                connection
                    .send_request(TruncateSessionRequest {
                        session_id: session_id.clone(),
                        client_user_message_id: "user-2".to_string(),
                    })
                    .block_task()
                    .await?;
                Ok(session_id)
            })
            .await
            .unwrap();

        let handle = state
            .sessions
            .lock()
            .await
            .get(&session_id)
            .cloned()
            .expect("live session");
        assert_eq!(handle.core.session.lock().await.user_turn_count(), 2);
        assert_eq!(
            *handle.core.client_user_message_ids.lock().await,
            vec!["", "user-1"]
        );
        let persisted = SessionStore::new(sessions.path().to_path_buf())
            .load(&session_id.to_string())
            .expect("persisted session");
        assert_eq!(persisted.client_user_message_ids, vec!["", "user-1"]);
    }

    #[test]
    fn turn_timestamp_line_is_a_bracketed_dated_line() {
        use chrono::TimeZone;
        let fixed = chrono::Utc
            .with_ymd_and_hms(2026, 7, 25, 16, 45, 3)
            .unwrap();
        let line = turn_timestamp_line(fixed);
        assert_eq!(line, "[2026-07-25 16:45:03 UTC]\n");
        assert!(line.starts_with('['));
        assert!(line.ends_with("]\n"));
    }

    fn assistant_texts(history: &[crate::providers::Message]) -> Vec<String> {
        history
            .iter()
            .filter(|m| m.role == CoreRole::Assistant)
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                CoreBlock::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    async fn first_turn_assistant_texts(timestamp_turns: bool) -> (Vec<String>, Vec<String>) {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let mut state_out = None;
        let agent = build_agent_with_state(
            mock_factory(vec![end_turn_resp("hello from daimonos")]),
            workspace.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            Some(sessions.path().to_path_buf()),
            SessionCompaction::new(None, false),
            None,
            timestamp_turns,
            crate::providers::ThinkingLevel::default(),
            &mut state_out,
        );
        let state = state_out.expect("ACP state");
        let workspace_path = workspace.path().to_path_buf();
        let session_id = AcpClientRole
            .builder()
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session_id = connection
                    .send_request(NewSessionRequest::new(&workspace_path))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(PromptRequest::new(
                        session_id.clone(),
                        vec![AcpContentBlock::Text(TextContent::new("go"))],
                    ))
                    .block_task()
                    .await?;
                Ok(session_id)
            })
            .await
            .unwrap();
        let handle = state
            .sessions
            .lock()
            .await
            .get(&session_id)
            .cloned()
            .expect("live session");
        let history = handle.core.session.lock().await.history().to_vec();
        let persisted = state
            .store
            .as_ref()
            .expect("session store")
            .load(&session_id.to_string())
            .expect("persisted session");
        (
            assistant_texts(&history),
            assistant_texts(&persisted.messages),
        )
    }

    #[test]
    fn assistant_prefix_flows_only_through_canonical_delta_event() {
        let prefix_path = include_str!("session_core.rs")
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .expect("session core production source");
        assert!(
            prefix_path.contains("emit(SessionEvent::AssistantDelta"),
            "assistant prefix must enter the canonical stream"
        );
        assert!(
            !prefix_path.contains("send_notification"),
            "assistant prefix must not bypass the canonical event adapter"
        );
    }

    #[tokio::test]
    async fn timestamp_turn_on_prefixes_assistant_history() {
        let (texts, persisted_texts) = first_turn_assistant_texts(true).await;
        let re = regex::Regex::new(r"^\[\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2} .+\]\n$").unwrap();
        let stamps = texts.iter().filter(|t| re.is_match(t)).count();
        assert_eq!(
            stamps, 1,
            "expected exactly one timestamp line, got {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("hello from daimonos")),
            "model text still present: {texts:?}"
        );
        assert_eq!(persisted_texts, texts, "timestamp must survive persistence");
    }

    #[tokio::test]
    async fn timestamp_turn_off_by_default_emits_no_timestamp() {
        let (texts, persisted_texts) = first_turn_assistant_texts(false).await;
        let re = regex::Regex::new(r"^\[\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2} .+\]\n$").unwrap();
        assert!(
            !texts.iter().any(|t| re.is_match(t)),
            "no timestamp line expected: {texts:?}"
        );
        assert_eq!(persisted_texts, texts);
    }

    #[tokio::test]
    async fn session_delete_waits_for_live_load_lifecycle() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let cfg = Arc::new(Config::default());
        let mut state_out = None;
        let agent = build_agent_with_state(
            mock_factory(vec![]),
            workspace.path(),
            Arc::clone(&cfg),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            Some(sessions.path().to_path_buf()),
            SessionCompaction::new(None, false),
            None,
            false,
            crate::providers::ThinkingLevel::default(),
            &mut state_out,
        );
        let state = state_out.expect("ACP state");
        let state_for_client = Arc::clone(&state);
        let workspace_path = workspace.path().to_path_buf();

        AcpClientRole
            .builder()
            .connect_with(
                agent,
                move |connection: ConnectionTo<AcpAgentRole>| async move {
                    connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    let session_id = connection
                        .send_request(NewSessionRequest::new(workspace_path))
                        .block_task()
                        .await?
                        .session_id;
                    let handle = state_for_client
                        .sessions
                        .lock()
                        .await
                        .get(&session_id)
                        .cloned()
                        .expect("new session handle");
                    let lifecycle = handle.core.lifecycle.lock().await;
                    let delete = connection
                        .send_request(DeleteSessionRequest::new(session_id.clone()))
                        .block_task();
                    tokio::pin!(delete);
                    assert!(
                        tokio::time::timeout(Duration::from_millis(50), &mut delete)
                            .await
                            .is_err(),
                        "delete must wait while load owns the lifecycle guard"
                    );
                    drop(lifecycle);
                    tokio::time::timeout(Duration::from_secs(1), &mut delete)
                        .await
                        .expect("delete should continue after load releases lifecycle")?;
                    assert!(!state_for_client
                        .sessions
                        .lock()
                        .await
                        .contains_key(&session_id));
                    Ok(())
                },
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn concurrent_cold_load_builds_persisted_session_once() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let session_id = SessionId::new("persisted-single-flight");
        SessionStore::new(sessions.path().to_path_buf()).save_acp(
            &session_id.to_string(),
            "test-model",
            "medium",
            &[],
            workspace.path(),
            &[],
            &[],
        );

        let builds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let build_counter = Arc::clone(&builds);
        let make_provider: ProviderFactory = Arc::new(move || {
            build_counter.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(MockProvider::new(vec![])))
        });
        let mut state_out = None;
        let agent = build_agent_with_state(
            make_provider,
            workspace.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            Some(sessions.path().to_path_buf()),
            SessionCompaction::new(None, false),
            None,
            false,
            crate::providers::ThinkingLevel::default(),
            &mut state_out,
        );
        let state = state_out.expect("ACP state");
        builds.store(0, Ordering::SeqCst);
        let workspace_path = workspace.path().to_path_buf();
        let id_for_client = session_id.clone();

        AcpClientRole
            .builder()
            .connect_with(
                agent,
                move |connection: ConnectionTo<AcpAgentRole>| async move {
                    connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    let first = connection.send_request(LoadSessionRequest::new(
                        id_for_client.clone(),
                        workspace_path.clone(),
                    ));
                    let second = connection
                        .send_request(LoadSessionRequest::new(id_for_client, workspace_path));
                    let (first, second) = tokio::join!(first.block_task(), second.block_task());
                    first?;
                    second?;
                    Ok(())
                },
            )
            .await
            .unwrap();

        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert_eq!(state.sessions.lock().await.len(), 1);
    }

    /// Build an ACP engine with one live session, driven the way a client
    /// would. The workspace is returned so it outlives the state.
    async fn state_with_live_session() -> (Arc<AcpState>, Arc<SessionHandle>, tempfile::TempDir) {
        let workspace = tempfile::tempdir().unwrap();
        let mut state_out = None;
        let agent = build_agent_with_state(
            mock_factory(vec![]),
            workspace.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            None,
            SessionCompaction::new(None, false),
            None,
            false,
            crate::providers::ThinkingLevel::default(),
            &mut state_out,
        );
        let state = state_out.expect("ACP state");
        let cwd = workspace.path().to_path_buf();

        AcpClientRole
            .builder()
            .connect_with(
                agent,
                move |connection: ConnectionTo<AcpAgentRole>| async move {
                    connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    connection
                        .send_request(NewSessionRequest::new(cwd))
                        .block_task()
                        .await?;
                    Ok(())
                },
            )
            .await
            .unwrap();

        let handle = state
            .sessions
            .lock()
            .await
            .values()
            .next()
            .cloned()
            .expect("one live session");
        (state, handle, workspace)
    }

    /// vikunja #1107 fix #2: a prompt parked on an approval when the transport
    /// dies must have that approval resolved. Left pending, the turn can never
    /// unwind and the cancel signal alone does not reach it.
    #[tokio::test]
    async fn transport_teardown_resolves_pending_approvals() {
        let (state, handle, _workspace) = state_with_live_session().await;
        let approval = handle
            .core
            .approvals
            .register(CoreApprovalRequest::unassigned(
                "tool-1",
                "exec",
                "run tests",
                false,
            ))
            .expect("register approval");
        assert_eq!(handle.core.approvals.pending().len(), 1);

        let summary = shutdown_all_bridges(&state).await;

        assert!(
            handle.core.approvals.pending().is_empty(),
            "transport teardown must resolve pending approvals",
        );
        assert_eq!(
            approval.receiver.await.expect("resolution").decision,
            CoreApprovalDecision::Deny,
        );
        assert_eq!(summary.sessions, 1);
        assert_eq!(summary.completed, 1);
        assert!(!summary.timed_out);
    }

    /// vikunja #1107 fix #2: when the ACP transport disappears, every prompt it
    /// owned must be signalled cancelled. Engine teardown previously drained
    /// only MCP bridges, so an in-flight turn kept running detached from its
    /// client — the incident's ~41-minute orphaned prompt.
    #[tokio::test]
    async fn transport_teardown_cancels_in_flight_turns() {
        let (state, handle, _workspace) = state_with_live_session().await;

        let statuses: Arc<StdMutex<Vec<crate::session_protocol::TurnStatus>>> =
            Arc::new(StdMutex::new(Vec::new()));
        let statuses_for_handler = Arc::clone(&statuses);
        let _subscription = handle
            .core
            .events
            .subscribe(
                4,
                Arc::new(move |_seq, event| {
                    if let CoreSessionEvent::TurnStatusChanged { status } = event {
                        statuses_for_handler
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .push(status);
                    }
                }),
            )
            .expect("subscribe");

        // A turn is in flight at the moment the transport dies.
        let _turn = handle.core.begin_turn().expect("begin turn");

        let summary = shutdown_all_bridges(&state).await;

        let seen = statuses.lock().unwrap_or_else(|p| p.into_inner()).clone();
        assert!(
            seen.contains(&crate::session_protocol::TurnStatus::Cancelling),
            "transport teardown must signal the in-flight turn cancelled; saw {seen:?}",
        );
        assert_eq!(summary.sessions, 1);
        assert_eq!(summary.completed, 1);
        assert!(!summary.timed_out);
    }

    /// vikunja #1293: all session bridges must begin shutdown concurrently,
    /// and a bridge that never returns must not hold ACP teardown forever.
    #[tokio::test]
    async fn transport_teardown_bounds_and_concurrently_starts_hung_bridges() {
        let (_state_a, handle_a, _workspace_a) = state_with_live_session().await;
        let (_state_b, handle_b, _workspace_b) = state_with_live_session().await;
        let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let started_for_shutdown = Arc::clone(&started);

        let summary = shutdown_session_handles_with(
            vec![handle_a, handle_b],
            Duration::from_millis(25),
            move |_handle| {
                let started = Arc::clone(&started_for_shutdown);
                async move {
                    let index = started.fetch_add(1, Ordering::SeqCst);
                    if index > 0 {
                        futures_util::future::pending::<()>().await;
                    }
                }
            },
        )
        .await;

        assert_eq!(
            started.load(Ordering::SeqCst),
            2,
            "every bridge shutdown must start before waiting for any one bridge",
        );
        assert_eq!(summary.sessions, 2);
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.remaining, 1);
        assert!(summary.timed_out);
    }

    #[tokio::test]
    async fn blank_session_is_persisted_at_new_for_cold_load() {
        // vikunja #1046: a thread the user opens but never prompts must still
        // be resumable after a process restart. session/new now persists an
        // empty record so a cold session/load finds it instead of erroring
        // with "no session found".
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let mut state_out = None;
        let agent = build_agent_with_state(
            mock_factory(vec![]),
            workspace.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            Some(sessions.path().to_path_buf()),
            SessionCompaction::new(None, false),
            None,
            false,
            crate::providers::ThinkingLevel::default(),
            &mut state_out,
        );
        let _state = state_out.expect("ACP state");
        let workspace_path = workspace.path().to_path_buf();

        let session_id = AcpClientRole
            .builder()
            .connect_with(
                agent,
                move |connection: ConnectionTo<AcpAgentRole>| async move {
                    connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    // Create a thread but never prompt it — a blank thread.
                    let session_id = connection
                        .send_request(NewSessionRequest::new(workspace_path))
                        .block_task()
                        .await?
                        .session_id;
                    Ok(session_id)
                },
            )
            .await
            .unwrap();

        // Simulate a process restart: a fresh store over the same dir must find
        // the blank session persisted with empty history.
        let persisted = SessionStore::new(sessions.path().to_path_buf())
            .load(&session_id.to_string())
            .expect("blank session must be persisted at session/new (vikunja #1046)");
        assert!(
            persisted.messages.is_empty(),
            "a never-prompted session persists with no history"
        );
    }

    #[tokio::test]
    async fn acp_reports_unsupported_images_without_policy_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let agent = build_agent(
            mock_factory(vec![end_turn_resp("must not run")]),
            dir.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            None,
            None,
            None,
        );
        let updates: Arc<StdMutex<Vec<SessionUpdate>>> = Arc::new(StdMutex::new(Vec::new()));
        let updates_for_handler = Arc::clone(&updates);

        let stop_reason = AcpClientRole
            .builder()
            .on_receive_notification(
                move |notification: SessionNotification, _cx| {
                    let updates = Arc::clone(&updates_for_handler);
                    async move {
                        updates.lock().unwrap().push(notification.update);
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(NewSessionRequest::new(dir.path()))
                    .block_task()
                    .await?;
                let response = connection
                    .send_request(PromptRequest::new(
                        session.session_id,
                        vec![AcpContentBlock::Image(ImageContent::new(
                            "aW1hZ2U=",
                            "image/png",
                        ))],
                    ))
                    .block_task()
                    .await?;
                Ok(response.stop_reason)
            })
            .await
            .unwrap();

        assert_eq!(stop_reason, AcpStopReason::EndTurn);
        let texts: Vec<String> = updates
            .lock()
            .unwrap()
            .iter()
            .filter_map(|update| match update {
                SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                    AcpContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert!(texts
            .iter()
            .any(|text| text.contains("does not support image prompts")));
    }

    #[tokio::test]
    async fn acp_distinguishes_provider_failure_from_genuine_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let agent = build_agent(
            mock_factory(vec![
                crate::providers::LlmResponse::error(
                    "network error: Authorization: Bearer secret-token",
                ),
                crate::providers::LlmResponse {
                    retryable: false,
                    content: Vec::new(),
                    stop_reason: crate::providers::StopReason::Refusal,
                    error_message: None,
                    context_overflow: false,
                    usage: Usage::default(),
                },
            ]),
            dir.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            None,
            None,
            None,
        );
        let updates: Arc<StdMutex<Vec<SessionUpdate>>> = Arc::new(StdMutex::new(Vec::new()));
        let updates_for_handler = Arc::clone(&updates);

        let stop_reason = AcpClientRole
            .builder()
            .on_receive_notification(
                move |notification: SessionNotification, _cx| {
                    let updates = Arc::clone(&updates_for_handler);
                    async move {
                        updates.lock().unwrap().push(notification.update);
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(NewSessionRequest::new(dir.path()))
                    .block_task()
                    .await?;
                let failure = connection
                    .send_request(PromptRequest::new(
                        session.session_id.clone(),
                        vec![AcpContentBlock::Text(TextContent::new("go"))],
                    ))
                    .block_task()
                    .await?;
                let refusal = connection
                    .send_request(PromptRequest::new(
                        session.session_id,
                        vec![AcpContentBlock::Text(TextContent::new("refuse"))],
                    ))
                    .block_task()
                    .await?;
                Ok((failure.stop_reason, refusal.stop_reason))
            })
            .await
            .unwrap();

        assert_eq!(stop_reason.0, AcpStopReason::EndTurn);
        assert_eq!(stop_reason.1, AcpStopReason::Refusal);
        let text = updates
            .lock()
            .unwrap()
            .iter()
            .filter_map(|update| match update {
                SessionUpdate::AgentThoughtChunk(chunk) => match &chunk.content {
                    AcpContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Provider network request failed"));
        assert!(text.contains("Provider refused the request based on content policy"));
        assert!(!text.contains("secret-token"));
        assert!(!text.contains("Authorization"));
    }

    #[tokio::test]
    async fn acp_commands_execute_without_llm_and_clear_persisted_history() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        // Only the ordinary prompt has a scripted response. If any command
        // reaches the provider, MockProvider exhaustion makes it a refusal.
        let agent = build_agent(
            mock_factory(vec![end_turn_resp("remembered")]),
            workspace.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            Some(sessions.path().to_path_buf()),
            None,
            None,
        );

        let updates: Arc<StdMutex<Vec<SessionUpdate>>> = Arc::new(StdMutex::new(Vec::new()));
        let updates_for_handler = Arc::clone(&updates);

        let (session_id, command_stops) = AcpClientRole
            .builder()
            .on_receive_notification(
                move |notif: SessionNotification, _cx| {
                    let updates = Arc::clone(&updates_for_handler);
                    async move {
                        updates.lock().unwrap().push(notif.update);
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session_id = connection
                    .send_request(NewSessionRequest::new(workspace.path()))
                    .block_task()
                    .await?
                    .session_id;

                connection
                    .send_request(PromptRequest::new(
                        session_id.clone(),
                        vec![AcpContentBlock::Text(TextContent::new("remember this"))],
                    ))
                    .block_task()
                    .await?;

                let mut command_stops = Vec::new();
                for command in ["/usage", "/help", "/clear"] {
                    let response = connection
                        .send_request(PromptRequest::new(
                            session_id.clone(),
                            vec![AcpContentBlock::Text(TextContent::new(command))],
                        ))
                        .block_task()
                        .await?;
                    command_stops.push(response.stop_reason);
                }
                Ok((session_id, command_stops))
            })
            .await
            .unwrap();

        assert_eq!(
            command_stops,
            vec![
                AcpStopReason::EndTurn,
                AcpStopReason::EndTurn,
                AcpStopReason::EndTurn
            ],
            "commands must not consume another provider response"
        );

        let agent_texts: Vec<String> = updates
            .lock()
            .unwrap()
            .iter()
            .filter_map(|update| match update {
                SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                    AcpContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert!(
            agent_texts
                .iter()
                .any(|text| text.contains("input=10 output=5")),
            "/usage must report cumulative usage: {agent_texts:?}"
        );
        assert!(
            agent_texts.iter().any(|text| text.contains("/clear")),
            "/help must list ACP commands: {agent_texts:?}"
        );
        assert!(
            agent_texts.iter().any(|text| text == "[history cleared]"),
            "/clear must confirm the reset: {agent_texts:?}"
        );

        let persisted = SessionStore::new(sessions.path().to_path_buf())
            .load(&session_id.to_string())
            .expect("/clear must persist the session");
        assert!(
            persisted.messages.is_empty(),
            "/clear must persist empty history"
        );
    }

    #[test]
    fn acp_command_parser_only_accepts_advertised_exact_commands() {
        assert_eq!(parse_acp_command(" /usage "), Some(AcpCommand::Usage));
        assert_eq!(parse_acp_command("/compact"), None);
        assert_eq!(parse_acp_command("/help extra"), None);
    }

    #[test]
    fn legacy_session_info_uses_process_workspace_fallback() {
        let workspace = tempfile::tempdir().unwrap();
        let info = session_info(
            SessionSummary {
                id: "legacy".to_string(),
                model: "model".to_string(),
                message_count: 1,
                cwd: None,
                updated_at: Some(std::time::UNIX_EPOCH),
                first_user_line: Some("Legacy thread".to_string()),
            },
            workspace.path(),
        );

        assert_eq!(info.cwd, workspace.path());
        assert_eq!(info.title.as_deref(), Some("Legacy thread"));
        assert!(info
            .updated_at
            .as_deref()
            .is_some_and(|date| date.contains("1970")));
    }

    fn pagination_summary(id: &str, updated_secs: u64, cwd: &Path) -> SessionSummary {
        SessionSummary {
            id: id.to_string(),
            model: "model".to_string(),
            message_count: 1,
            cwd: Some(cwd.to_path_buf()),
            updated_at: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(updated_secs)),
            first_user_line: Some(format!("Session {id}")),
        }
    }

    #[test]
    fn session_list_pagination_uses_stable_keyset_cursor() {
        let workspace = tempfile::tempdir().unwrap();
        let summaries = vec![
            pagination_summary("s3", 3, workspace.path()),
            pagination_summary("s2", 2, workspace.path()),
            pagination_summary("s1", 1, workspace.path()),
        ];

        let (first, cursor) =
            paginate_session_summaries(summaries.clone(), None, None, 2, workspace.path()).unwrap();
        assert_eq!(
            first
                .iter()
                .map(|info| info.session_id.to_string())
                .collect::<Vec<_>>(),
            vec!["s3", "s2"]
        );
        let cursor = cursor.expect("first page must have a continuation cursor");

        // A new session before the anchor must not shift or duplicate page 2.
        let mut with_newer = vec![pagination_summary("s4", 4, workspace.path())];
        with_newer.extend(summaries);
        let (second, next) =
            paginate_session_summaries(with_newer, None, Some(&cursor), 2, workspace.path())
                .unwrap();
        assert_eq!(
            second
                .iter()
                .map(|info| info.session_id.to_string())
                .collect::<Vec<_>>(),
            vec!["s1"]
        );
        assert_eq!(next, None);
    }

    #[test]
    fn session_list_pagination_filters_before_slicing() {
        let workspace_a = tempfile::tempdir().unwrap();
        let workspace_b = tempfile::tempdir().unwrap();
        let summaries = vec![
            pagination_summary("a2", 3, workspace_a.path()),
            pagination_summary("b1", 2, workspace_b.path()),
            pagination_summary("a1", 1, workspace_a.path()),
        ];

        let (first, cursor) = paginate_session_summaries(
            summaries.clone(),
            Some(workspace_a.path()),
            None,
            1,
            workspace_a.path(),
        )
        .unwrap();
        assert_eq!(first[0].session_id.to_string(), "a2");
        let (second, next) = paginate_session_summaries(
            summaries,
            Some(workspace_a.path()),
            cursor.as_deref(),
            1,
            workspace_a.path(),
        )
        .unwrap();
        assert_eq!(second[0].session_id.to_string(), "a1");
        assert_eq!(next, None);
    }

    #[test]
    fn session_list_pagination_rejects_invalid_and_stale_cursors() {
        let workspace = tempfile::tempdir().unwrap();
        let summaries = vec![
            pagination_summary("s2", 2, workspace.path()),
            pagination_summary("s1", 1, workspace.path()),
        ];
        assert!(paginate_session_summaries(
            summaries.clone(),
            None,
            Some("not-a-cursor"),
            1,
            workspace.path(),
        )
        .is_err());

        let (_, cursor) =
            paginate_session_summaries(summaries, None, None, 1, workspace.path()).unwrap();
        let changed = vec![pagination_summary("s1", 1, workspace.path())];
        assert!(
            paginate_session_summaries(changed, None, cursor.as_deref(), 1, workspace.path(),)
                .expect_err("missing anchor must make the cursor stale")
                .contains("stale")
        );
    }

    #[tokio::test]
    async fn acp_supports_multiple_sessions_per_process() {
        // Zed keeps one `daimonos acp` process across chat threads and sends
        // a fresh session/new per thread. Both sessions must work and prompt
        // independently on the same process.
        let dir = tempfile::tempdir().unwrap();
        // The factory yields a fresh provider per session, each scripted with
        // one end-turn response.
        let make_provider = mock_factory(vec![end_turn_resp("hi")]);
        let agent = build_agent(
            make_provider,
            dir.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            None,
            None,
            None,
        );

        let (stop_a, stop_b) = AcpClientRole
            .builder()
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let s1 = connection
                    .send_request(NewSessionRequest::new(dir.path()))
                    .block_task()
                    .await?;
                // A second session/new must succeed (no more single-session limit).
                let s2 = connection
                    .send_request(NewSessionRequest::new(dir.path()))
                    .block_task()
                    .await?;
                assert_ne!(
                    s1.session_id, s2.session_id,
                    "sessions must have distinct ids"
                );

                // Both sessions can prompt.
                let a = connection
                    .send_request(PromptRequest::new(
                        s1.session_id,
                        vec![AcpContentBlock::Text(TextContent::new("go"))],
                    ))
                    .block_task()
                    .await?;
                let b = connection
                    .send_request(PromptRequest::new(
                        s2.session_id,
                        vec![AcpContentBlock::Text(TextContent::new("go"))],
                    ))
                    .block_task()
                    .await?;
                Ok((a.stop_reason, b.stop_reason))
            })
            .await
            .unwrap();

        assert_eq!(stop_a, AcpStopReason::EndTurn);
        assert_eq!(stop_b, AcpStopReason::EndTurn);
    }

    #[tokio::test]
    async fn busy_prompt_does_not_break_active_turn_permission_routing() {
        let dir = tempfile::tempdir().unwrap();
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let make_provider: ProviderFactory = {
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            Arc::new(move || {
                Ok(Box::new(GatedToolProvider {
                    calls: StdMutex::new(0),
                    started: Arc::clone(&started),
                    release: Arc::clone(&release),
                }) as Box<dyn LlmProvider>)
            })
        };
        let agent = build_agent(
            make_provider,
            dir.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy {
                approval_mode: crate::safety::ApprovalMode::Interactive,
                ..crate::safety::SafetyPolicy::default()
            }),
            None,
            None,
            None,
            None,
        );
        let permission_seen = Arc::new(AtomicBool::new(false));
        let permission_seen_for_handler = Arc::clone(&permission_seen);

        let stop_reason = AcpClientRole
            .builder()
            .on_receive_request(
                async move |request: RequestPermissionRequest, responder, _cx| {
                    permission_seen_for_handler.store(true, Ordering::Release);
                    let option_id = request.options.first().unwrap().option_id.clone();
                    responder.respond(
                        agent_client_protocol::schema::v1::RequestPermissionResponse::new(
                            RequestPermissionOutcome::Selected(
                                agent_client_protocol::schema::v1::SelectedPermissionOutcome::new(
                                    option_id,
                                ),
                            ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session_id = connection
                    .send_request(NewSessionRequest::new(dir.path()))
                    .block_task()
                    .await?
                    .session_id;
                let first_prompt = connection
                    .send_request(PromptRequest::new(
                        session_id.clone(),
                        vec![AcpContentBlock::Text(TextContent::new("first"))],
                    ))
                    .block_task();

                tokio::time::timeout(Duration::from_secs(2), started.notified())
                    .await
                    .expect("first prompt must enter the provider");
                let second_prompt = tokio::time::timeout(
                    Duration::from_secs(2),
                    connection
                        .send_request(PromptRequest::new(
                            session_id,
                            vec![AcpContentBlock::Text(TextContent::new("second"))],
                        ))
                        .block_task(),
                )
                .await;
                release.notify_one();
                let response = tokio::time::timeout(Duration::from_secs(5), first_prompt)
                    .await
                    .expect("first prompt must still complete after the busy rejection")?;
                assert!(
                    matches!(second_prompt, Ok(Err(_))),
                    "overlapping prompt must be rejected promptly as busy: {second_prompt:?}"
                );
                Ok(response.stop_reason)
            })
            .await
            .unwrap();

        assert_eq!(stop_reason, AcpStopReason::EndTurn);
        assert!(
            permission_seen.load(Ordering::Acquire),
            "the active turn's permission request must still reach the client"
        );
    }

    #[tokio::test]
    async fn acp_session_cancel_aborts_inflight_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let make_provider: ProviderFactory = Arc::new(|| Ok(Box::new(SlowProvider)));
        let agent = build_agent(
            make_provider,
            dir.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            None,
            None,
            None,
        );

        let stop_reason = AcpClientRole
            .builder()
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let new_session = connection
                    .send_request(NewSessionRequest::new(dir.path()))
                    .block_task()
                    .await?;
                let session_id = new_session.session_id.clone();

                let prompt_fut = connection
                    .send_request(PromptRequest::new(
                        session_id.clone(),
                        vec![AcpContentBlock::Text(TextContent::new("go"))],
                    ))
                    .block_task();

                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                connection.send_notification(CancelNotification::new(session_id))?;

                // SlowProvider sleeps 30s; if cancellation didn't actually
                // short-circuit the in-flight prompt, this timeout fires.
                let prompt_response = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    prompt_fut,
                )
                .await
                .expect(
                    "prompt should resolve quickly once cancelled, not wait for the slow provider",
                )?;

                Ok(prompt_response.stop_reason)
            })
            .await
            .unwrap();

        assert_eq!(stop_reason, AcpStopReason::Cancelled);
    }

    #[tokio::test]
    async fn acp_session_cancel_closes_announced_tool_call() {
        use agent_client_protocol::schema::v1::ClientCapabilities;

        let dir = tempfile::tempdir().unwrap();
        let mut state_out = None;
        let agent = build_agent_with_state(
            mock_factory(vec![tool_call_resp(
                "t1",
                "exec",
                serde_json::json!({"command": "sleep 30"}),
            )]),
            dir.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy {
                approval_mode: crate::safety::ApprovalMode::Interactive,
                ..crate::safety::SafetyPolicy::default()
            }),
            None,
            None,
            SessionCompaction::new(None, false),
            None,
            false,
            crate::providers::ThinkingLevel::default(),
            &mut state_out,
        );
        let state = state_out.expect("ACP state");
        let updates: Arc<StdMutex<Vec<SessionUpdate>>> = Arc::new(StdMutex::new(Vec::new()));
        let updates_for_handler = Arc::clone(&updates);
        let permission_seen = Arc::new(tokio::sync::Notify::new());
        let permission_seen_for_handler = Arc::clone(&permission_seen);

        let stop_reason = AcpClientRole
            .builder()
            .on_receive_notification(
                move |notification: SessionNotification, _cx| {
                    let updates = Arc::clone(&updates_for_handler);
                    async move {
                        updates.lock().unwrap().push(notification.update);
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: RequestPermissionRequest, responder, _cx| {
                    permission_seen_for_handler.notify_one();
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    let option_id = request.options.first().unwrap().option_id.clone();
                    responder.respond(
                        agent_client_protocol::schema::v1::RequestPermissionResponse::new(
                            RequestPermissionOutcome::Selected(
                                agent_client_protocol::schema::v1::SelectedPermissionOutcome::new(
                                    option_id,
                                ),
                            ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(
                        InitializeRequest::new(ProtocolVersion::V1).client_capabilities(
                            ClientCapabilities::new().meta(Meta::from_iter([(
                                "terminal_output".to_string(),
                                serde_json::json!(true),
                            )])),
                        ),
                    )
                    .block_task()
                    .await?;
                let session_id = connection
                    .send_request(NewSessionRequest::new(dir.path()))
                    .block_task()
                    .await?
                    .session_id;
                let prompt = connection
                    .send_request(PromptRequest::new(
                        session_id.clone(),
                        vec![AcpContentBlock::Text(TextContent::new("go"))],
                    ))
                    .block_task();

                tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    permission_seen.notified(),
                )
                .await
                .expect("permission request should arrive");
                let handle = state
                    .sessions
                    .lock()
                    .await
                    .get(&session_id)
                    .cloned()
                    .expect("live session handle");
                assert_eq!(handle.core.approvals.pending().len(), 1);

                connection.send_notification(CancelNotification::new(session_id))?;
                let response = tokio::time::timeout(std::time::Duration::from_secs(2), prompt)
                    .await
                    .expect("cancelled prompt should resolve")?;
                assert!(handle.core.approvals.pending().is_empty());
                Ok(response.stop_reason)
            })
            .await
            .unwrap();

        assert_eq!(stop_reason, AcpStopReason::Cancelled);
        let updates = updates.lock().unwrap();
        assert_no_orphan_tool_updates(&updates);
        let cancelled = updates.iter().find_map(|update| match update {
            SessionUpdate::ToolCallUpdate(call)
                if call.fields.status == Some(ToolCallStatus::Failed) =>
            {
                Some(call)
            }
            _ => None,
        });
        let cancelled = cancelled.expect("cancel must close the announced call");
        assert_eq!(cancelled.tool_call_id.to_string(), "t1");
        assert_eq!(
            cancelled.fields.raw_output,
            Some(serde_json::json!({"cancelled": true}))
        );
        assert_eq!(
            cancelled
                .meta
                .as_ref()
                .and_then(|meta| meta.get("terminal_exit"))
                .and_then(|exit| exit.get("signal")),
            Some(&serde_json::json!("cancelled"))
        );
    }

    #[tokio::test]
    async fn acp_session_cancel_cleans_up_retry_approval_and_tool_call() {
        use agent_client_protocol::schema::v1::ClientCapabilities;

        let dir = tempfile::tempdir().unwrap();
        let mut state_out = None;
        let agent = build_agent_with_state(
            mock_factory(vec![
                end_turn_resp("first"),
                tool_call_resp(
                    "retry-tool",
                    "exec",
                    serde_json::json!({"command": "sleep 30"}),
                ),
            ]),
            dir.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy {
                approval_mode: crate::safety::ApprovalMode::Interactive,
                ..crate::safety::SafetyPolicy::default()
            }),
            None,
            None,
            SessionCompaction::new(None, false),
            None,
            false,
            crate::providers::ThinkingLevel::default(),
            &mut state_out,
        );
        let state = state_out.expect("ACP state");
        let updates: Arc<StdMutex<Vec<SessionUpdate>>> = Arc::new(StdMutex::new(Vec::new()));
        let updates_for_handler = Arc::clone(&updates);
        let permission_seen = Arc::new(tokio::sync::Notify::new());
        let permission_seen_for_handler = Arc::clone(&permission_seen);

        let stop_reason = AcpClientRole
            .builder()
            .on_receive_notification(
                move |notification: SessionNotification, _cx| {
                    let updates = Arc::clone(&updates_for_handler);
                    async move {
                        updates.lock().unwrap().push(notification.update);
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: RequestPermissionRequest, responder, _cx| {
                    permission_seen_for_handler.notify_one();
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    let option_id = request.options.first().unwrap().option_id.clone();
                    responder.respond(
                        agent_client_protocol::schema::v1::RequestPermissionResponse::new(
                            RequestPermissionOutcome::Selected(
                                agent_client_protocol::schema::v1::SelectedPermissionOutcome::new(
                                    option_id,
                                ),
                            ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(
                        InitializeRequest::new(ProtocolVersion::V1).client_capabilities(
                            ClientCapabilities::new().meta(Meta::from_iter([(
                                "terminal_output".to_string(),
                                serde_json::json!(true),
                            )])),
                        ),
                    )
                    .block_task()
                    .await?;
                let session_id = connection
                    .send_request(NewSessionRequest::new(dir.path()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(PromptRequest::new(
                        session_id.clone(),
                        vec![AcpContentBlock::Text(TextContent::new("first"))],
                    ))
                    .block_task()
                    .await?;
                let retry = connection
                    .send_request(RetrySessionRequest {
                        session_id: session_id.clone(),
                    })
                    .block_task();

                tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    permission_seen.notified(),
                )
                .await
                .expect("retry permission request should arrive");
                let handle = state
                    .sessions
                    .lock()
                    .await
                    .get(&session_id)
                    .cloned()
                    .expect("live session handle");
                assert_eq!(handle.core.approvals.pending().len(), 1);

                connection.send_notification(CancelNotification::new(session_id))?;
                let response = tokio::time::timeout(std::time::Duration::from_secs(2), retry)
                    .await
                    .expect("cancelled retry should resolve")?;
                assert!(
                    handle.core.approvals.pending().is_empty(),
                    "retry cancellation must resolve every approval"
                );
                assert!(
                    handle
                        .active_tool_calls
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .is_empty(),
                    "retry cancellation must drain every announced tool id"
                );
                Ok(response.stop_reason)
            })
            .await
            .unwrap();

        assert_eq!(stop_reason, AcpStopReason::Cancelled);
        let updates = updates.lock().unwrap();
        assert_no_orphan_tool_updates(&updates);
        let terminal_updates = updates
            .iter()
            .filter(|update| {
                matches!(
                    update,
                    SessionUpdate::ToolCallUpdate(call)
                        if call.tool_call_id.to_string() == "retry-tool"
                            && call.fields.status == Some(ToolCallStatus::Failed)
                )
            })
            .count();
        assert_eq!(
            terminal_updates, 1,
            "every announced retry tool call must receive exactly one terminal update"
        );
    }

    #[tokio::test]
    async fn acp_session_delete_cancels_inflight_prompt() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let make_provider: ProviderFactory = Arc::new(|| Ok(Box::new(SlowProvider)));
        let agent = build_agent(
            make_provider,
            workspace.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            Some(sessions.path().to_path_buf()),
            None,
            None,
        );

        let stop_reason = AcpClientRole
            .builder()
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session_id = connection
                    .send_request(NewSessionRequest::new(workspace.path()))
                    .block_task()
                    .await?
                    .session_id;
                let prompt = connection
                    .send_request(PromptRequest::new(
                        session_id.clone(),
                        vec![AcpContentBlock::Text(TextContent::new("go"))],
                    ))
                    .block_task();

                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                connection
                    .send_request(DeleteSessionRequest::new(session_id))
                    .block_task()
                    .await?;
                let response = tokio::time::timeout(std::time::Duration::from_secs(5), prompt)
                    .await
                    .expect("delete should cancel the prompt promptly")?;
                Ok(response.stop_reason)
            })
            .await
            .unwrap();

        assert_eq!(stop_reason, AcpStopReason::Cancelled);
    }

    #[tokio::test]
    async fn acp_destructive_tool_call_requests_permission_and_allows() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "hi").unwrap();
        let make_provider = mock_factory(vec![
            tool_call_resp("t1", "exec", serde_json::json!({"command": "echo hi"})),
            end_turn_resp("done"),
        ]);
        let agent = build_agent(
            make_provider,
            dir.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy {
                approval_mode: crate::safety::ApprovalMode::Interactive,
                ..crate::safety::SafetyPolicy::default()
            }),
            None,
            None,
            None,
            None,
        );

        let updates: Arc<StdMutex<Vec<SessionUpdate>>> = Arc::new(StdMutex::new(Vec::new()));
        let updates_for_handler = Arc::clone(&updates);

        let stop_reason = AcpClientRole
            .builder()
            .on_receive_notification(
                move |notif: SessionNotification, _cx| {
                    let updates = Arc::clone(&updates_for_handler);
                    async move {
                        updates.lock().unwrap().push(notif.update);
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: agent_client_protocol::schema::v1::RequestPermissionRequest,
                             responder,
                             _cx| {
                    assert_eq!(
                        request.tool_call.fields.raw_input,
                        Some(serde_json::json!({"command": "echo hi"})),
                        "permission request must show the exact tool input"
                    );
                    let option_id = request.options.first().map(|o| o.option_id.clone()).unwrap();
                    responder.respond(agent_client_protocol::schema::v1::RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(
                            agent_client_protocol::schema::v1::SelectedPermissionOutcome::new(option_id),
                        ),
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let new_session = connection
                    .send_request(NewSessionRequest::new(dir.path()))
                    .block_task()
                    .await?;
                let prompt_response = connection
                    .send_request(PromptRequest::new(
                        new_session.session_id,
                        vec![AcpContentBlock::Text(TextContent::new("go"))],
                    ))
                    .block_task()
                    .await?;
                Ok(prompt_response.stop_reason)
            })
            .await
            .unwrap();

        assert_eq!(stop_reason, AcpStopReason::EndTurn);

        let updates = updates.lock().unwrap();
        let statuses: Vec<ToolCallStatus> = updates
            .iter()
            .filter_map(|u| match u {
                SessionUpdate::ToolCallUpdate(tcu) => tcu.fields.status,
                _ => None,
            })
            .collect();
        // An allowed permission request moves the tool call InProgress, then
        // (since the mocked ToolCall is a real `exec` and actually runs)
        // Completed — proving permission was granted, not denied.
        assert_eq!(
            statuses,
            vec![ToolCallStatus::InProgress, ToolCallStatus::Completed],
            "got: {updates:?}"
        );
    }

    #[tokio::test]
    async fn earlier_broker_resolution_survives_later_acp_denial() {
        let dir = tempfile::tempdir().unwrap();
        let mut state_out = None;
        let agent = build_agent_with_state(
            mock_factory(vec![
                tool_call_resp("t1", "exec", serde_json::json!({"command": "echo hi"})),
                end_turn_resp("done"),
            ]),
            dir.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy {
                approval_mode: crate::safety::ApprovalMode::Interactive,
                ..crate::safety::SafetyPolicy::default()
            }),
            None,
            None,
            SessionCompaction::new(None, false),
            None,
            false,
            crate::providers::ThinkingLevel::default(),
            &mut state_out,
        );
        let state = state_out.expect("ACP state");
        let permission_seen = Arc::new(tokio::sync::Notify::new());
        let permission_seen_for_handler = Arc::clone(&permission_seen);

        let stop_reason = AcpClientRole
            .builder()
            .on_receive_request(
                async move |_request: RequestPermissionRequest, responder, _cx| {
                    permission_seen_for_handler.notify_one();
                    // Keep the client dispatch loop free while its local answer
                    // is delayed, so an independent broker response can win.
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        let _ = responder.respond(
                            agent_client_protocol::schema::v1::RequestPermissionResponse::new(
                                RequestPermissionOutcome::Cancelled,
                            ),
                        );
                    });
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session_id = connection
                    .send_request(NewSessionRequest::new(dir.path()))
                    .block_task()
                    .await?
                    .session_id;
                let prompt = connection
                    .send_request(PromptRequest::new(
                        session_id.clone(),
                        vec![AcpContentBlock::Text(TextContent::new("go"))],
                    ))
                    .block_task();
                tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    permission_seen.notified(),
                )
                .await
                .expect("ACP permission request should be pending");

                let handle = state
                    .sessions
                    .lock()
                    .await
                    .get(&session_id)
                    .cloned()
                    .expect("live session handle");
                let approval = handle
                    .core
                    .approvals
                    .pending()
                    .into_iter()
                    .next()
                    .expect("broker approval");
                handle
                    .core
                    .approvals
                    .resolve(
                        &approval.id,
                        "remote-test",
                        &[ClientCapability::ApproveOnce],
                        CoreApprovalDecision::AllowOnce,
                    )
                    .expect("remote approval should win");

                let response = tokio::time::timeout(std::time::Duration::from_secs(2), prompt)
                    .await
                    .expect("winning broker resolution should survive the later ACP response")?;
                Ok(response.stop_reason)
            })
            .await
            .unwrap();

        assert_eq!(stop_reason, AcpStopReason::EndTurn);
    }

    #[tokio::test]
    async fn acp_cancelled_permission_finishes_the_announced_call() {
        let dir = tempfile::tempdir().unwrap();
        let agent = build_agent(
            mock_factory(vec![
                tool_call_resp("t1", "exec", serde_json::json!({"command": "echo hi"})),
                end_turn_resp("done"),
            ]),
            dir.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy {
                approval_mode: crate::safety::ApprovalMode::Interactive,
                ..crate::safety::SafetyPolicy::default()
            }),
            None,
            None,
            None,
            None,
        );
        let updates: Arc<StdMutex<Vec<SessionUpdate>>> = Arc::new(StdMutex::new(Vec::new()));
        let updates_for_handler = Arc::clone(&updates);

        AcpClientRole
            .builder()
            .on_receive_notification(
                move |notification: SessionNotification, _cx| {
                    let updates = Arc::clone(&updates_for_handler);
                    async move {
                        updates.lock().unwrap().push(notification.update);
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |_request: RequestPermissionRequest, responder, _cx| {
                    responder.respond(
                        agent_client_protocol::schema::v1::RequestPermissionResponse::new(
                            RequestPermissionOutcome::Cancelled,
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(NewSessionRequest::new(dir.path()))
                    .block_task()
                    .await?;
                connection
                    .send_request(PromptRequest::new(
                        session.session_id,
                        vec![AcpContentBlock::Text(TextContent::new("go"))],
                    ))
                    .block_task()
                    .await?;
                Ok(())
            })
            .await
            .unwrap();

        let updates = updates.lock().unwrap();
        assert_no_orphan_tool_updates(&updates);
        assert!(updates.iter().any(|update| matches!(
            update,
            SessionUpdate::ToolCallUpdate(call)
                if call.tool_call_id.to_string() == "t1"
                    && call.fields.status == Some(ToolCallStatus::Failed)
        )));
        assert!(!updates.iter().any(|update| matches!(
            update,
            SessionUpdate::ToolCallUpdate(call)
                if call.fields.status == Some(ToolCallStatus::Completed)
        )));
    }

    #[tokio::test]
    async fn acp_denied_tool_blocks_without_asking_permission() {
        let dir = tempfile::tempdir().unwrap();
        let make_provider = mock_factory(vec![
            tool_call_resp("t1", "exec", serde_json::json!({"command": "echo hi"})),
            end_turn_resp("done"),
        ]);
        let agent = build_agent(
            make_provider,
            dir.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy {
                denied_commands: vec!["exec".into()],
                ..crate::safety::SafetyPolicy::default()
            }),
            None,
            None,
            None,
            None,
        );

        let permission_requests_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::clone(&permission_requests_seen);
        let updates: Arc<StdMutex<Vec<SessionUpdate>>> = Arc::new(StdMutex::new(Vec::new()));
        let updates_for_handler = Arc::clone(&updates);

        AcpClientRole
            .builder()
            .on_receive_notification(
                move |notif: SessionNotification, _cx| {
                    let updates = Arc::clone(&updates_for_handler);
                    async move {
                        updates.lock().unwrap().push(notif.update);
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: agent_client_protocol::schema::v1::RequestPermissionRequest,
                             responder,
                             _cx| {
                    seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let option_id = request.options.first().map(|o| o.option_id.clone()).unwrap();
                    responder.respond(agent_client_protocol::schema::v1::RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(
                            agent_client_protocol::schema::v1::SelectedPermissionOutcome::new(option_id),
                        ),
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(
                        InitializeRequest::new(ProtocolVersion::V1).client_capabilities(
                            agent_client_protocol::schema::v1::ClientCapabilities::new().meta(
                                Meta::from_iter([(
                                    "terminal_output".to_string(),
                                    serde_json::json!(true),
                                )]),
                            ),
                        ),
                    )
                    .block_task()
                    .await?;
                let new_session = connection
                    .send_request(NewSessionRequest::new(dir.path()))
                    .block_task()
                    .await?;
                connection
                    .send_request(PromptRequest::new(
                        new_session.session_id,
                        vec![AcpContentBlock::Text(TextContent::new("go"))],
                    ))
                    .block_task()
                    .await?;
                Ok(())
            })
            .await
            .unwrap();

        assert_eq!(
            permission_requests_seen.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a denylisted tool must not even ask for permission"
        );
        let updates = updates.lock().unwrap();
        assert_no_orphan_tool_updates(&updates);
        let failed_updates: Vec<&ToolCallUpdate> = updates
            .iter()
            .filter_map(|update| match update {
                SessionUpdate::ToolCallUpdate(tool_call)
                    if tool_call.fields.status == Some(ToolCallStatus::Failed) =>
                {
                    Some(tool_call)
                }
                _ => None,
            })
            .collect();
        assert_eq!(failed_updates.len(), 1, "got: {updates:?}");
        let fields = &failed_updates[0].fields;
        assert_eq!(
            fields.raw_output,
            Some(serde_json::json!({
                "blocked": true,
                "reason": "blocked by policy: 'exec' is in the denied-commands list",
            }))
        );
        let message = fields
            .content
            .as_ref()
            .and_then(|content| content.first())
            .and_then(|content| match content {
                AcpToolCallContent::Content(content) => match &content.content {
                    AcpContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                },
                _ => None,
            });
        assert_eq!(
            message,
            Some("blocked: blocked by policy: 'exec' is in the denied-commands list")
        );
        assert_eq!(
            failed_updates[0]
                .meta
                .as_ref()
                .and_then(|meta| meta.get("terminal_exit"))
                .and_then(|exit| exit.get("signal")),
            Some(&serde_json::json!("blocked"))
        );
    }

    #[tokio::test]
    async fn acp_session_new_uses_client_provided_cwd() {
        // `build_agent`'s own `workspace` argument points at a directory
        // with no marker file; the *session's* cwd (from NewSessionRequest,
        // as a real ACP client like Zed would send) points at one that
        // does. If the session correctly uses the client-provided cwd
        // instead of build_agent's own workspace, the read_file tool call
        // succeeds.
        let build_agent_workspace = tempfile::tempdir().unwrap();
        let session_workspace = tempfile::tempdir().unwrap();
        std::fs::write(session_workspace.path().join("marker.txt"), "found me").unwrap();

        let make_provider = mock_factory(vec![
            tool_call_resp("t1", "read_file", serde_json::json!({"path": "marker.txt"})),
            end_turn_resp("done"),
        ]);
        let agent = build_agent(
            make_provider,
            build_agent_workspace.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            None,
            None,
            None,
        );

        let updates: Arc<StdMutex<Vec<SessionUpdate>>> = Arc::new(StdMutex::new(Vec::new()));
        let updates_for_handler = Arc::clone(&updates);

        AcpClientRole
            .builder()
            .on_receive_notification(
                move |notif: SessionNotification, _cx| {
                    let updates = Arc::clone(&updates_for_handler);
                    async move {
                        updates.lock().unwrap().push(notif.update);
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let new_session = connection
                    .send_request(NewSessionRequest::new(session_workspace.path()))
                    .block_task()
                    .await?;
                connection
                    .send_request(PromptRequest::new(
                        new_session.session_id,
                        vec![AcpContentBlock::Text(TextContent::new("go"))],
                    ))
                    .block_task()
                    .await?;
                Ok(())
            })
            .await
            .unwrap();

        let updates = updates.lock().unwrap();
        let completed_with_marker = updates.iter().any(|u| match u {
            SessionUpdate::ToolCallUpdate(tcu) => tcu
                .fields
                .content
                .as_ref()
                .is_some_and(|c| format!("{c:?}").contains("found me")),
            _ => false,
        });
        assert!(
            completed_with_marker,
            "tool should have read marker.txt from the session's cwd: {updates:?}"
        );
    }

    // --- model picker (vikunja #960) ---

    /// Captures the model each turn was sent with (from `CompleteOpts.model`).
    struct ModelCaptureProvider {
        seen: Arc<StdMutex<Vec<String>>>,
    }

    #[async_trait]
    impl LlmProvider for ModelCaptureProvider {
        async fn complete(
            &self,
            _ctx: &crate::providers::Context,
            opts: &CompleteOpts,
        ) -> crate::providers::LlmResponse {
            self.seen.lock().unwrap().push(opts.model.clone());
            end_turn_resp("ok")
        }

        async fn context_window(&self, model: &str) -> Option<u64> {
            Some(if model == "model-b" {
                1_000_000
            } else {
                200_000
            })
        }
    }

    /// Captures the reasoning effort each turn was sent with (from
    /// `CompleteOpts.thinking`). Used to prove the agent-env thinking level is
    /// threaded into the ACP prompt path (vikunja #1122).
    struct ThinkingCaptureProvider {
        seen: Arc<StdMutex<Vec<crate::providers::ThinkingLevel>>>,
    }

    #[async_trait]
    impl LlmProvider for ThinkingCaptureProvider {
        async fn complete(
            &self,
            _ctx: &crate::providers::Context,
            opts: &CompleteOpts,
        ) -> crate::providers::LlmResponse {
            self.seen.lock().unwrap().push(opts.thinking.clone());
            end_turn_resp("ok")
        }
    }

    #[tokio::test]
    async fn acp_prompt_turn_uses_agent_env_thinking_level() {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let make_provider: ProviderFactory = {
            let seen = Arc::clone(&seen);
            Arc::new(move || {
                Ok(Box::new(ThinkingCaptureProvider {
                    seen: Arc::clone(&seen),
                }))
            })
        };
        let mut state_out = None;
        let agent = build_agent_with_state(
            make_provider,
            dir.path(),
            Arc::new(Config::default()),
            "model-a".to_string(),
            vec!["model-a".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            None,
            SessionCompaction::new(None, false),
            None,
            false,
            crate::providers::ThinkingLevel::High,
            &mut state_out,
        );
        let _state = state_out.expect("ACP state");

        AcpClientRole
            .builder()
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let new_session = connection
                    .send_request(NewSessionRequest::new(dir.path()))
                    .block_task()
                    .await?;
                connection
                    .send_request(PromptRequest::new(
                        new_session.session_id,
                        vec![AcpContentBlock::Text(TextContent::new("go"))],
                    ))
                    .block_task()
                    .await?;
                Ok(())
            })
            .await
            .unwrap();

        assert_eq!(
            *seen.lock().unwrap(),
            vec![crate::providers::ThinkingLevel::High],
            "the prompt turn must run at the agent-env reasoning effort"
        );
    }

    /// Pull the single model `SessionConfigOption` out of a config_options list.
    fn model_option(options: &[SessionConfigOption]) -> &SessionConfigOption {
        options
            .iter()
            .find(|o| o.id.to_string() == MODEL_CONFIG_ID)
            .expect("a 'model' config option should be advertised")
    }

    fn select_state(
        option: &SessionConfigOption,
    ) -> &agent_client_protocol::schema::v1::SessionConfigSelect {
        match &option.kind {
            agent_client_protocol::schema::v1::SessionConfigKind::Select(s) => s,
            _ => panic!("model option should be a Select"),
        }
    }

    #[test]
    fn canonical_model_options_match_the_acp_model_selection() {
        let models = vec!["model-a".to_string(), "model-b".to_string()];
        let options = canonical_model_options(&models, "model-b");
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].id, MODEL_CONFIG_ID);
        assert_eq!(
            options[0].value,
            CoreRuntimeValue::String("model-b".to_string())
        );
        assert!(options[0].accepts(&CoreRuntimeValue::String("model-a".to_string())));
    }

    #[tokio::test]
    async fn acp_session_new_advertises_model_config_options() {
        let dir = tempfile::tempdir().unwrap();
        let make_provider = mock_factory(vec![]);
        let models = vec![
            "model-a".to_string(),
            "model-b".to_string(),
            "model-c".to_string(),
        ];
        let agent = build_agent(
            make_provider,
            dir.path(),
            Arc::new(Config::default()),
            "model-a".to_string(),
            models,
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            None,
            None,
            None,
        );

        let config_options = AcpClientRole
            .builder()
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let new_session = connection
                    .send_request(NewSessionRequest::new(dir.path()))
                    .block_task()
                    .await?;
                Ok(new_session.config_options)
            })
            .await
            .unwrap()
            .expect("session/new should advertise config_options");

        let option = model_option(&config_options);
        assert_eq!(option.category, Some(SessionConfigOptionCategory::Model));
        let select = select_state(option);
        assert_eq!(select.current_value.to_string(), "model-a");
        let ids: Vec<String> = match &select.options {
            agent_client_protocol::schema::v1::SessionConfigSelectOptions::Ungrouped(opts) => {
                opts.iter().map(|o| o.value.to_string()).collect()
            }
            _ => panic!("expected ungrouped options"),
        };
        assert_eq!(ids, vec!["model-a", "model-b", "model-c"]);
    }

    #[tokio::test]
    async fn acp_set_config_option_switches_model_for_next_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let make_provider: ProviderFactory = {
            let seen = Arc::clone(&seen);
            Arc::new(move || {
                Ok(Box::new(ModelCaptureProvider {
                    seen: Arc::clone(&seen),
                }))
            })
        };
        let models = vec!["model-a".to_string(), "model-b".to_string()];
        let agent = build_agent(
            make_provider,
            dir.path(),
            Arc::new(Config::default()),
            "model-a".to_string(),
            models,
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            None,
            None,
            None,
        );
        let updates: Arc<StdMutex<Vec<SessionUpdate>>> = Arc::new(StdMutex::new(Vec::new()));
        let updates_for_handler = Arc::clone(&updates);

        let echoed_current = AcpClientRole
            .builder()
            .on_receive_notification(
                move |notification: SessionNotification, _cx| {
                    let updates = Arc::clone(&updates_for_handler);
                    async move {
                        updates.lock().unwrap().push(notification.update);
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let new_session = connection
                    .send_request(NewSessionRequest::new(dir.path()))
                    .block_task()
                    .await?;
                let session_id = new_session.session_id;

                // Pick model-b via the picker.
                let set_resp = connection
                    .send_request(SetSessionConfigOptionRequest::new(
                        session_id.clone(),
                        MODEL_CONFIG_ID,
                        agent_client_protocol::schema::v1::SessionConfigOptionValue::value_id(
                            "model-b",
                        ),
                    ))
                    .block_task()
                    .await?;

                // Then send a prompt — it should run on model-b.
                connection
                    .send_request(PromptRequest::new(
                        session_id,
                        vec![AcpContentBlock::Text(TextContent::new("go"))],
                    ))
                    .block_task()
                    .await?;

                Ok(select_state(model_option(&set_resp.config_options))
                    .current_value
                    .to_string())
            })
            .await
            .unwrap();

        assert_eq!(
            echoed_current, "model-b",
            "set_config_option response must echo the new selection"
        );
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["model-b".to_string()],
            "the prompt turn must use the picked model"
        );
        let usage_size = updates
            .lock()
            .unwrap()
            .iter()
            .find_map(|update| match update {
                SessionUpdate::UsageUpdate(usage) => Some(usage.size),
                _ => None,
            });
        assert_eq!(
            usage_size,
            Some(1_000_000),
            "usage must use the picked model's provider-reported window"
        );
    }

    #[tokio::test]
    async fn acp_set_config_option_ignores_unadvertised_model() {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let make_provider: ProviderFactory = {
            let seen = Arc::clone(&seen);
            Arc::new(move || {
                Ok(Box::new(ModelCaptureProvider {
                    seen: Arc::clone(&seen),
                }))
            })
        };
        let models = vec!["model-a".to_string(), "model-b".to_string()];
        let agent = build_agent(
            make_provider,
            dir.path(),
            Arc::new(Config::default()),
            "model-a".to_string(),
            models,
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            None,
            None,
            None,
        );

        let echoed_current = AcpClientRole
            .builder()
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let new_session = connection
                    .send_request(NewSessionRequest::new(dir.path()))
                    .block_task()
                    .await?;
                let set_resp = connection
                    .send_request(SetSessionConfigOptionRequest::new(
                        new_session.session_id,
                        MODEL_CONFIG_ID,
                        agent_client_protocol::schema::v1::SessionConfigOptionValue::value_id(
                            "model-evil",
                        ),
                    ))
                    .block_task()
                    .await?;
                Ok(select_state(model_option(&set_resp.config_options))
                    .current_value
                    .to_string())
            })
            .await
            .unwrap();

        assert_eq!(
            echoed_current, "model-a",
            "an unadvertised model must be ignored, current unchanged"
        );
    }

    // --- session/load (vikunja #961) ---

    #[tokio::test]
    async fn acp_advertises_load_session_capability() {
        // Zed refuses to reopen a thread ("Loading or resuming sessions is
        // not supported by this agent.") unless load_session is advertised.
        let dir = tempfile::tempdir().unwrap();
        let agent = build_agent(
            mock_factory(vec![]),
            dir.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            None,
            None,
            None,
        );

        let load_session = AcpClientRole
            .builder()
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                let init = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                Ok(init.agent_capabilities.load_session)
            })
            .await
            .unwrap();

        assert!(
            load_session,
            "agent must advertise load_session so Zed reopens threads"
        );
    }

    #[tokio::test]
    async fn acp_advertises_embedded_context_and_provider_gated_images() {
        let workspace = tempfile::tempdir().unwrap();
        let image_agent = build_agent(
            image_capable_factory(),
            workspace.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            None,
            None,
            None,
        );
        let image_capabilities = AcpClientRole
            .builder()
            .connect_with(
                image_agent,
                |connection: ConnectionTo<AcpAgentRole>| async move {
                    let init = connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    Ok(init.agent_capabilities.prompt_capabilities)
                },
            )
            .await
            .unwrap();
        assert!(image_capabilities.embedded_context);
        assert!(image_capabilities.image);
        assert!(!image_capabilities.audio);

        let text_only_agent = build_agent(
            mock_factory(vec![]),
            workspace.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            None,
            None,
            None,
        );
        let text_only_capabilities = AcpClientRole
            .builder()
            .connect_with(
                text_only_agent,
                |connection: ConnectionTo<AcpAgentRole>| async move {
                    let init = connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    Ok(init.agent_capabilities.prompt_capabilities)
                },
            )
            .await
            .unwrap();
        assert!(text_only_capabilities.embedded_context);
        assert!(!text_only_capabilities.image);
    }

    #[tokio::test]
    async fn acp_advertises_list_delete_only_when_persistence_enabled() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let persistent_agent = build_agent(
            mock_factory(vec![]),
            workspace.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            Some(sessions.path().to_path_buf()),
            None,
            None,
        );
        let persistent_capabilities = AcpClientRole
            .builder()
            .connect_with(
                persistent_agent,
                |connection: ConnectionTo<AcpAgentRole>| async move {
                    let init = connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    Ok(init.agent_capabilities.session_capabilities)
                },
            )
            .await
            .unwrap();
        assert!(persistent_capabilities.list.is_some());
        assert!(persistent_capabilities.delete.is_some());

        let transient_agent = build_agent(
            mock_factory(vec![]),
            workspace.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            None,
            None,
            None,
        );
        let transient_capabilities = AcpClientRole
            .builder()
            .connect_with(
                transient_agent,
                |connection: ConnectionTo<AcpAgentRole>| async move {
                    let init = connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    Ok(init.agent_capabilities.session_capabilities)
                },
            )
            .await
            .unwrap();
        assert!(transient_capabilities.list.is_none());
        assert!(transient_capabilities.delete.is_none());
    }

    #[tokio::test]
    async fn acp_session_list_filters_and_delete_removes_disk_and_live_state() {
        use agent_client_protocol::schema::v1::{DeleteSessionRequest, ListSessionsRequest};

        let workspace = tempfile::tempdir().unwrap();
        let other_workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let expected_workspace = workspace.path().to_path_buf();
        let request_workspace = expected_workspace.clone();
        let request_other_workspace = other_workspace.path().to_path_buf();
        let agent = build_agent(
            mock_factory(vec![end_turn_resp("remembered")]),
            workspace.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            Some(sessions.path().to_path_buf()),
            None,
            None,
        );

        let (listed, filtered_count, remaining_count, load_failed) = AcpClientRole
            .builder()
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session_id = connection
                    .send_request(NewSessionRequest::new(request_workspace.clone()))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(PromptRequest::new(
                        session_id.clone(),
                        vec![AcpContentBlock::Text(TextContent::new("remember this"))],
                    ))
                    .block_task()
                    .await?;

                let listed = connection
                    .send_request(ListSessionsRequest::new().cwd(request_workspace.clone()))
                    .block_task()
                    .await?;
                let filtered_count = connection
                    .send_request(ListSessionsRequest::new().cwd(request_other_workspace))
                    .block_task()
                    .await?
                    .sessions
                    .len();

                connection
                    .send_request(DeleteSessionRequest::new(session_id.clone()))
                    .block_task()
                    .await?;
                // Deletion is idempotent.
                connection
                    .send_request(DeleteSessionRequest::new(session_id.clone()))
                    .block_task()
                    .await?;

                let remaining_count = connection
                    .send_request(ListSessionsRequest::new())
                    .block_task()
                    .await?
                    .sessions
                    .len();
                let load_failed = connection
                    .send_request(LoadSessionRequest::new(session_id, request_workspace))
                    .block_task()
                    .await
                    .is_err();
                Ok((listed, filtered_count, remaining_count, load_failed))
            })
            .await
            .unwrap();

        assert_eq!(listed.sessions.len(), 1);
        let info = &listed.sessions[0];
        assert_eq!(info.cwd, expected_workspace);
        assert_eq!(info.title.as_deref(), Some("remember this"));
        assert!(info.updated_at.is_some());
        assert_eq!(listed.next_cursor, None);
        assert_eq!(filtered_count, 0);
        assert_eq!(remaining_count, 0);
        assert!(load_failed, "deleted live session must no longer load");
        assert!(
            SessionStore::new(sessions.path().to_path_buf())
                .list()
                .is_empty(),
            "deleted session must be removed from disk"
        );
    }

    #[tokio::test]
    async fn acp_session_list_returns_cursor_pages_and_rejects_bad_anchors() {
        use agent_client_protocol::schema::v1::ListSessionsRequest;

        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let store = SessionStore::new(sessions.path().to_path_buf());
        store.save_with_cwd("session-a", "test-model", &[], workspace.path());
        store.save_with_cwd("session-b", "test-model", &[], workspace.path());

        let mut cfg = Config::default();
        cfg.acp.session_list_page_size = 1;
        let agent = build_agent(
            mock_factory(vec![]),
            workspace.path(),
            Arc::new(cfg),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            Some(sessions.path().to_path_buf()),
            None,
            None,
        );
        let store_for_client = store.clone();

        let (first_id, second_id, final_cursor, invalid_failed, stale_failed) = AcpClientRole
            .builder()
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;

                let first = connection
                    .send_request(ListSessionsRequest::new())
                    .block_task()
                    .await?;
                let first_id = first.sessions[0].session_id.to_string();
                let cursor = first
                    .next_cursor
                    .expect("one-item page must provide a cursor");
                let second = connection
                    .send_request(ListSessionsRequest::new().cursor(cursor))
                    .block_task()
                    .await?;
                let second_id = second.sessions[0].session_id.to_string();

                let invalid_failed = connection
                    .send_request(ListSessionsRequest::new().cursor("invalid"))
                    .block_task()
                    .await
                    .is_err();

                let first_again = connection
                    .send_request(ListSessionsRequest::new())
                    .block_task()
                    .await?;
                let stale_anchor = first_again.sessions[0].session_id.to_string();
                let stale_cursor = first_again.next_cursor.expect("cursor");
                store_for_client.delete(&stale_anchor).unwrap();
                let stale_failed = connection
                    .send_request(ListSessionsRequest::new().cursor(stale_cursor))
                    .block_task()
                    .await
                    .is_err();

                Ok((
                    first_id,
                    second_id,
                    second.next_cursor,
                    invalid_failed,
                    stale_failed,
                ))
            })
            .await
            .unwrap();

        assert_ne!(first_id, second_id);
        assert_eq!(final_cursor, None);
        assert!(invalid_failed);
        assert!(stale_failed);
    }

    #[tokio::test]
    async fn acp_session_load_replays_history_for_known_session() {
        // A live session's in-memory history must be replayed back as
        // session/update notifications so Zed rebuilds the reopened thread.
        let dir = tempfile::tempdir().unwrap();
        let make_provider = mock_factory(vec![
            tool_call_resp(
                "plan-replay",
                UPDATE_PLAN_TOOL,
                serde_json::json!({
                    "entries": [
                        {"content": "inspect", "priority": "high", "status": "completed"},
                        {"content": "implement", "priority": "medium", "status": "in_progress"}
                    ]
                }),
            ),
            thinking_resp("reasoning...", "recalled-text"),
        ]);
        let agent = build_agent(
            make_provider,
            dir.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            None,
            None,
            None,
        );

        let updates: Arc<StdMutex<Vec<SessionUpdate>>> = Arc::new(StdMutex::new(Vec::new()));
        let updates_for_handler = Arc::clone(&updates);
        let updates_for_closure = Arc::clone(&updates);

        let (config_options, replay_start) = AcpClientRole
            .builder()
            .on_receive_notification(
                move |notif: SessionNotification, _cx| {
                    let updates = Arc::clone(&updates_for_handler);
                    async move {
                        updates.lock().unwrap().push(notif.update);
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let new_session = connection
                    .send_request(NewSessionRequest::new(dir.path()))
                    .block_task()
                    .await?;
                let session_id = new_session.session_id;
                connection
                    .send_request(PromptRequest::new(
                        session_id.clone(),
                        vec![AcpContentBlock::Text(TextContent::new("remember-this"))],
                    ))
                    .block_task()
                    .await?;
                // Everything the client sees from here on is replay.
                let replay_start = updates_for_closure.lock().unwrap().len();
                let load_resp = connection
                    .send_request(LoadSessionRequest::new(session_id, dir.path()))
                    .block_task()
                    .await?;
                Ok((load_resp.config_options, replay_start))
            })
            .await
            .unwrap();

        assert!(
            config_options.is_some(),
            "session/load must echo the model-picker config_options"
        );

        let updates = updates.lock().unwrap();
        let replayed = &updates[replay_start..];
        let thought_texts = |updates: &[SessionUpdate]| {
            updates
                .iter()
                .filter_map(|update| match update {
                    SessionUpdate::AgentThoughtChunk(chunk) => match &chunk.content {
                        AcpContentBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    },
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            thought_texts(&updates[..replay_start]),
            vec!["reasoning..."],
            "live thinking must stream as an AgentThoughtChunk"
        );
        assert_eq!(
            thought_texts(replayed),
            vec!["reasoning..."],
            "session/load must replay persisted thinking"
        );
        let plans = |updates: &[SessionUpdate]| {
            updates
                .iter()
                .filter_map(|update| match update {
                    SessionUpdate::Plan(plan) => Some(plan.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let live_plans = plans(&updates[..replay_start]);
        let replayed_plans = plans(replayed);
        assert_eq!(live_plans.len(), 1, "live plan update missing");
        assert_eq!(replayed_plans.len(), 1, "replayed plan update missing");
        assert_eq!(replayed_plans[0].entries.len(), 2);
        assert_eq!(replayed_plans[0].entries[1].content, "implement");
        assert_eq!(
            replayed_plans[0].entries[1].status,
            AcpPlanStatus::InProgress
        );
        assert!(
            updates.iter().all(|update| !matches!(
                update,
                SessionUpdate::ToolCall(call) if call.title == UPDATE_PLAN_TOOL
            )),
            "update_plan should use native Plan UI without generic tool chrome"
        );
        // UserMessageChunk is a kind the normal prompt path never emits, so
        // its presence during load proves replay ran.
        let user_texts: Vec<String> = replayed
            .iter()
            .filter_map(|u| match u {
                SessionUpdate::UserMessageChunk(c) => match &c.content {
                    AcpContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert!(
            user_texts.iter().any(|t| t == "remember-this"),
            "replay must include the historical user message: {replayed:?}"
        );
        let agent_texts: Vec<String> = replayed
            .iter()
            .filter_map(|u| match u {
                SessionUpdate::AgentMessageChunk(c) => match &c.content {
                    AcpContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert!(
            agent_texts.iter().any(|t| t == "recalled-text"),
            "replay must include the historical assistant message: {replayed:?}"
        );
        assert!(
            replayed.iter().any(|update| matches!(
                update,
                SessionUpdate::AvailableCommandsUpdate(commands)
                    if commands.available_commands.len() == 3
            )),
            "session/load must re-advertise slash commands: {replayed:?}"
        );
    }

    #[tokio::test]
    async fn session_replay_drops_tool_result_without_announcement() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let session_id = "orphan-result-session";
        SessionStore::new(sessions.path().to_path_buf()).save_acp(
            session_id,
            "test-model",
            "medium",
            &[crate::providers::Message {
                role: crate::providers::Role::User,
                content: vec![crate::providers::ContentBlock::ToolResult {
                    tool_use_id: "never-announced".to_string(),
                    content: "late result".to_string(),
                    is_error: false,
                }],
            }],
            workspace.path(),
            &[],
            &[],
        );
        let agent = build_agent(
            mock_factory(vec![]),
            workspace.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            Some(sessions.path().to_path_buf()),
            None,
            None,
        );
        let updates: Arc<StdMutex<Vec<SessionUpdate>>> = Arc::new(StdMutex::new(Vec::new()));
        let updates_for_handler = Arc::clone(&updates);

        AcpClientRole
            .builder()
            .on_receive_notification(
                move |notification: SessionNotification, _cx| {
                    let updates = Arc::clone(&updates_for_handler);
                    async move {
                        updates.lock().unwrap().push(notification.update);
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                connection
                    .send_request(LoadSessionRequest::new(
                        SessionId::new(session_id),
                        workspace.path(),
                    ))
                    .block_task()
                    .await?;
                Ok(())
            })
            .await
            .unwrap();

        let updates = updates.lock().unwrap();
        assert!(
            updates
                .iter()
                .all(|update| !matches!(update, SessionUpdate::ToolCallUpdate(_))),
            "orphan replay result must be dropped: {updates:?}"
        );
    }

    #[tokio::test]
    async fn acp_session_load_unknown_id_errors() {
        // No live session and no persistence: session/load must error, the
        // same way Zed's native providers reject an id not in their store,
        // rather than fabricate an empty "resumed" thread.
        let dir = tempfile::tempdir().unwrap();
        let make_provider = mock_factory(vec![]);
        let agent = build_agent(
            make_provider,
            dir.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            None,
            None,
            None,
        );

        let load_errored = AcpClientRole
            .builder()
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                // An id that was never created via session/new. Return the raw
                // result — we expect an error, so we must not `?` it away.
                let made_up = SessionId::new("never-created-session-id");
                let result = connection
                    .send_request(LoadSessionRequest::new(made_up, dir.path()))
                    .block_task()
                    .await;
                Ok(result.is_err())
            })
            .await
            .unwrap();

        assert!(
            load_errored,
            "session/load of an unknown id with no persistence must error"
        );
    }

    #[tokio::test]
    async fn acp_session_load_resumes_persisted_session_across_processes() {
        // Cross-process resume (the parity item with native providers): a
        // session prompted in one `build_agent` is persisted to disk; a SECOND
        // `build_agent` over the SAME sessions_dir (simulating a Zed restart /
        // respawned process) must load that history from disk and replay it.
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = tempfile::tempdir().unwrap();

        // --- process 1: create a session and prompt it (persists to disk) ---
        let agent1 = build_agent(
            mock_factory(vec![end_turn_resp("persisted-answer")]),
            dir.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            Some(sessions_dir.path().to_path_buf()),
            None,
            None,
        );
        let ws1 = dir.path().to_path_buf();
        let session_id = AcpClientRole
            .builder()
            .connect_with(
                agent1,
                |connection: ConnectionTo<AcpAgentRole>| async move {
                    connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    let new_session = connection
                        .send_request(NewSessionRequest::new(ws1))
                        .block_task()
                        .await?;
                    let session_id = new_session.session_id;
                    connection
                        .send_request(PromptRequest::new(
                            session_id.clone(),
                            vec![AcpContentBlock::Text(TextContent::new("persist-me"))],
                        ))
                        .block_task()
                        .await?;
                    Ok(session_id)
                },
            )
            .await
            .unwrap();

        // --- process 2: fresh agent, same sessions_dir, empty in-memory map ---
        let agent2 = build_agent(
            mock_factory(vec![]),
            dir.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            Some(sessions_dir.path().to_path_buf()),
            None,
            None,
        );

        let updates: Arc<StdMutex<Vec<SessionUpdate>>> = Arc::new(StdMutex::new(Vec::new()));
        let updates_for_handler = Arc::clone(&updates);
        let ws2 = dir.path().to_path_buf();
        let load_ok = AcpClientRole
            .builder()
            .on_receive_notification(
                move |notif: SessionNotification, _cx| {
                    let updates = Arc::clone(&updates_for_handler);
                    async move {
                        updates.lock().unwrap().push(notif.update);
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(
                agent2,
                |connection: ConnectionTo<AcpAgentRole>| async move {
                    connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    let result = connection
                        .send_request(LoadSessionRequest::new(session_id, ws2))
                        .block_task()
                        .await;
                    Ok(result.is_ok())
                },
            )
            .await
            .unwrap();

        assert!(load_ok, "a persisted session must load in a fresh process");
        let updates = updates.lock().unwrap();
        let user_texts: Vec<String> = updates
            .iter()
            .filter_map(|u| match u {
                SessionUpdate::UserMessageChunk(c) => match &c.content {
                    AcpContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        let agent_texts: Vec<String> = updates
            .iter()
            .filter_map(|u| match u {
                SessionUpdate::AgentMessageChunk(c) => match &c.content {
                    AcpContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert!(
            user_texts.iter().any(|t| t == "persist-me"),
            "cross-process load must replay the persisted user message: {updates:?}"
        );
        assert!(
            agent_texts.iter().any(|t| t == "persisted-answer"),
            "cross-process load must replay the persisted assistant message: {updates:?}"
        );
    }

    // --- pure mapping helpers ---

    #[tokio::test]
    async fn eof_aware_reader_notifies_on_input_error() {
        let closed = Arc::new(tokio::sync::Notify::new());
        let input_error = Arc::new(StdMutex::new(None));
        let mut reader =
            EofAwareReader::new(ErrorReader, Arc::clone(&closed), Arc::clone(&input_error));
        let mut buffer = [0_u8; 1];

        assert!(reader.read(&mut buffer).await.is_err());
        tokio::time::timeout(Duration::from_millis(100), closed.notified())
            .await
            .expect("input error must publish a stored close notification");
        assert_eq!(input_error.lock().unwrap().as_deref(), Some("input failed"));
    }

    #[test]
    fn tool_kind_maps_read_tools() {
        assert_eq!(tool_kind_for("read_file"), ToolKind::Read);
        assert_eq!(tool_kind_for("search"), ToolKind::Read);
    }

    #[test]
    fn tool_kind_maps_edit_tools() {
        assert_eq!(tool_kind_for("write_file"), ToolKind::Edit);
        assert_eq!(tool_kind_for("edit_file"), ToolKind::Edit);
    }

    #[test]
    fn tool_kind_maps_execute_tools() {
        assert_eq!(tool_kind_for("exec"), ToolKind::Execute);
        assert_eq!(tool_kind_for("git"), ToolKind::Execute);
    }

    #[test]
    fn tool_kind_unknown_tool_is_other() {
        assert_eq!(tool_kind_for("some_future_tool"), ToolKind::Other);
    }

    #[test]
    fn live_session_refreshes_changed_or_failed_mcp_bridge() {
        let spec = |name: &str| ServerSpec::Stdio {
            name: name.to_string(),
            command: "server".to_string(),
            args: vec![],
            env: HashMap::new(),
        };
        let current = vec![spec("one")];

        assert!(!should_refresh_mcp_bridge(&current, &current, false));
        assert!(should_refresh_mcp_bridge(&current, &[spec("two")], false));
        assert!(should_refresh_mcp_bridge(&current, &current, true));
    }

    #[test]
    fn usage_update_reports_latest_context_snapshot() {
        let usage = Usage {
            input: 100,
            output: 10,
            cache_read: 30,
            cache_write: 20,
            ..Usage::default()
        };
        let update = usage_update(Some(1_000_000), &usage, 1.25).expect("usage update");
        assert_eq!(update.used, 160);
        assert_eq!(update.size, 1_000_000);
        assert!(usage_update(None, &usage, 1.25).is_none());
    }

    #[test]
    fn model_following_compaction_uses_selected_model_window() {
        let base = CompactionPolicy {
            high_water: 0.75,
            low_water: 0.5,
            context_window: 200_000,
            output_reservation: 8192,
            summary_model: None,
            summary_prompt: None,
        };
        let dynamic = SessionCompaction::new(Some(base.clone()), true);
        assert_eq!(
            dynamic
                .policy_for("anthropic/claude-opus-4.8", Some(1_000_000))
                .unwrap()
                .unwrap()
                .context_window,
            1_000_000
        );

        let explicit = SessionCompaction::new(Some(base), false);
        assert_eq!(
            explicit
                .policy_for("anthropic/claude-opus-4.8", Some(1_000_000))
                .unwrap()
                .unwrap()
                .context_window,
            200_000
        );
    }

    #[test]
    fn map_stop_reason_end_turn_and_tool_use_both_end_turn() {
        use crate::providers::StopReason;
        assert_eq!(map_stop_reason(StopReason::EndTurn), AcpStopReason::EndTurn);
        assert_eq!(map_stop_reason(StopReason::ToolUse), AcpStopReason::EndTurn);
    }

    #[test]
    fn map_stop_reason_aborted_is_cancelled() {
        use crate::providers::StopReason;
        assert_eq!(
            map_stop_reason(StopReason::Aborted),
            AcpStopReason::Cancelled
        );
    }

    #[test]
    fn map_stop_reason_error_is_end_turn_not_false_refusal() {
        use crate::providers::StopReason;
        assert_eq!(map_stop_reason(StopReason::Error), AcpStopReason::EndTurn);
    }

    #[test]
    fn map_stop_reason_genuine_refusal_is_refusal() {
        use crate::providers::StopReason;
        assert_eq!(map_stop_reason(StopReason::Refusal), AcpStopReason::Refusal);
    }

    #[test]
    fn map_stop_reason_max_tokens() {
        use crate::providers::StopReason;
        assert_eq!(
            map_stop_reason(StopReason::MaxTokens),
            AcpStopReason::MaxTokens
        );
    }

    #[test]
    fn canonical_assistant_outcome_preserves_every_terminal_reason() {
        let turn = |stop_reason| TurnResult {
            text: String::new(),
            usage: Usage::default(),
            last_call_usage: Usage::default(),
            stop_reason,
            error_message: None,
            context_overflow: false,
        };
        assert_eq!(
            canonical_assistant_outcome(&turn(crate::providers::StopReason::EndTurn)),
            CoreAssistantOutcome::Completed
        );
        assert_eq!(
            canonical_assistant_outcome(&turn(crate::providers::StopReason::ToolUse)),
            CoreAssistantOutcome::Completed
        );
        assert_eq!(
            canonical_assistant_outcome(&turn(crate::providers::StopReason::Refusal)),
            CoreAssistantOutcome::Refused
        );
        assert_eq!(
            canonical_assistant_outcome(&turn(crate::providers::StopReason::Aborted)),
            CoreAssistantOutcome::Aborted
        );
        assert_eq!(
            canonical_assistant_outcome(&turn(crate::providers::StopReason::MaxTokens)),
            CoreAssistantOutcome::MaxTokens
        );
        let errored = TurnResult {
            error_message: Some("maximum context length exceeded".to_string()),
            context_overflow: true,
            ..turn(crate::providers::StopReason::Error)
        };
        assert_eq!(
            canonical_assistant_outcome(&errored),
            CoreAssistantOutcome::Errored {
                context_overflow: true,
                message: "Provider rejected the prompt because the context window was exceeded."
                    .to_string(),
            }
        );
    }

    #[test]
    fn provider_error_diagnostics_are_bounded_classes() {
        assert_eq!(
            safe_provider_error_message(
                false,
                Some(
                    "API 400 Bad Request: tool_use ids were found without tool_result blocks immediately after"
                )
            ),
            "Provider rejected invalid tool-call history. Restart/reload the session or use /clear."
        );
        assert_eq!(
            safe_provider_error_message(
                false,
                Some("tool use is fine without tool result overhead")
            ),
            "Provider request failed."
        );
        assert_eq!(
            safe_provider_error_message(
                false,
                Some("tool_use ids were found without tool_result blocks")
            ),
            "Provider request failed."
        );
        assert_eq!(
            safe_provider_error_message(
                false,
                Some(
                    "API 400: context overflow; tool_use ids were found without tool_result blocks"
                )
            ),
            "Provider rejected the prompt because the context window was exceeded."
        );
        assert_eq!(
            safe_provider_error_message(false, Some("API 401: echoed-secret")),
            "Provider authentication failed (HTTP 401)."
        );
        assert_eq!(
            safe_provider_error_message(false, Some("openrouter 429: slow down")),
            "Provider rate limit exceeded (HTTP 429)."
        );
        assert_eq!(
            safe_provider_error_message(
                false,
                Some(
                    "openrouter 402 Payment Required: {\"error\":{\"message\":\"Insufficient credits. Add more using https://openrouter.ai/settings/credits\",\"code\":402}}"
                )
            ),
            "Provider billing/credit issue (HTTP 402). Check the provider account balance."
        );
        assert_eq!(
            safe_provider_error_message(false, Some("Insufficient credits")),
            "Provider billing/credit issue (HTTP 402). Check the provider account balance."
        );
        assert_eq!(
            safe_provider_error_message(false, Some("payment_required")),
            "Provider billing/credit issue (HTTP 402). Check the provider account balance."
        );
        assert_eq!(
            safe_provider_error_message(false, Some("HTTP4021 request id")),
            "Provider request failed."
        );
        // Billing is surfaced ahead of an incidental auth-ish word.
        assert_eq!(
            safe_provider_error_message(false, Some("402 unauthorized: payment required")),
            "Provider billing/credit issue (HTTP 402). Check the provider account balance."
        );
        assert_eq!(
            safe_provider_error_message(false, Some("unexpected private payload")),
            "Provider request failed."
        );
        assert_eq!(
            safe_provider_error_message(false, Some("prompt is too long")),
            "Provider rejected the prompt because the context window was exceeded."
        );
        assert_eq!(
            safe_provider_error_message(false, Some("upstream gateway error")),
            "Provider network request failed."
        );
        assert_eq!(
            safe_provider_error_message(false, Some("request id 1401")),
            "Provider request failed."
        );
        assert_eq!(
            safe_provider_error_message(false, Some("Unauthorized")),
            "Provider authentication failed (HTTP 401)."
        );
        assert_eq!(
            safe_provider_error_message(false, Some("not authorized")),
            "Provider authentication failed (HTTP 401)."
        );
        assert_eq!(
            safe_provider_error_message(false, Some("Authorization failed")),
            "Provider authorization failed (HTTP 403)."
        );
        assert_eq!(
            safe_provider_error_message(false, Some("rate_limit_exceeded")),
            "Provider rate limit exceeded (HTTP 429)."
        );
        assert_eq!(
            safe_provider_error_message(false, Some("HTTP401 Unauthorized")),
            "Provider authentication failed (HTTP 401)."
        );
        assert_eq!(
            safe_provider_error_message(false, Some("HTTP4012 request id")),
            "Provider request failed."
        );
        assert_eq!(
            safe_provider_error_message(false, Some("permission-denied")),
            "Provider authorization failed (HTTP 403)."
        );
        assert_eq!(
            safe_provider_error_message(false, Some("context-overflow")),
            "Provider rejected the prompt because the context window was exceeded."
        );
        assert_eq!(
            safe_provider_error_message(false, Some("context window limit is 200k")),
            "Provider request failed."
        );
        assert_eq!(
            safe_provider_error_message(false, Some("context_length_exceeded")),
            "Provider rejected the prompt because the context window was exceeded."
        );
        assert_eq!(
            safe_provider_error_message(false, Some("could not inspect context window metadata")),
            "Provider request failed."
        );
        assert_eq!(
            safe_provider_error_message(false, Some("429 context-overflow")),
            "Provider rejected the prompt because the context window was exceeded."
        );
        assert_eq!(
            safe_provider_error_message(false, Some("timed_out")),
            "Provider request timed out."
        );
        assert_eq!(
            safe_provider_error_message(false, Some("stream_error")),
            "Provider response stream failed."
        );
    }

    // --- raw provider-error debug logging (#1062) ---

    // Minimal in-memory tracing layer that records events on the
    // shared session-core target whose `event` field == "provider_request_raw",
    // capturing their `raw_error` field. Used to assert the DEBUG gate without
    // adding a test-only dependency.
    #[derive(Clone, Default)]
    struct RawErrorCapture {
        events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl RawErrorCapture {
        fn captured(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for RawErrorCapture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().target() != "daimonos::session_core" {
                return;
            }
            struct Visitor {
                is_raw_event: bool,
                raw_error: Option<String>,
            }
            impl tracing::field::Visit for Visitor {
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    match field.name() {
                        "event" if value == "provider_request_raw" => self.is_raw_event = true,
                        "raw_error" => self.raw_error = Some(value.to_string()),
                        _ => {}
                    }
                }
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    let rendered = format!("{value:?}");
                    let trimmed = rendered.trim_matches('"').to_string();
                    match field.name() {
                        "event" if trimmed == "provider_request_raw" => self.is_raw_event = true,
                        "raw_error" => self.raw_error = Some(trimmed),
                        _ => {}
                    }
                }
            }
            let mut visitor = Visitor {
                is_raw_event: false,
                raw_error: None,
            };
            event.record(&mut visitor);
            if visitor.is_raw_event {
                if let Some(raw) = visitor.raw_error {
                    self.events
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push(raw);
                }
            }
        }
    }

    fn capture_raw_error_logging(level: tracing::level_filters::LevelFilter) -> Vec<String> {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::Layer as _;
        let capture = RawErrorCapture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone().with_filter(level));
        tracing::subscriber::with_default(subscriber, || {
            // Unclassified error => "Provider request failed." bucket.
            log_raw_provider_error(
                "session-xyz",
                "Provider request failed.",
                Some("openrouter: unexpected response shape (id 8842)"),
            );
            // No raw error => no emission regardless of level.
            log_raw_provider_error("session-xyz", "Provider request failed.", None);
        });
        capture.captured()
    }

    #[test]
    fn raw_provider_error_not_logged_at_default_level() {
        // WARN is the effective default; the DEBUG raw-error event must be filtered out.
        let captured = capture_raw_error_logging(tracing::level_filters::LevelFilter::WARN);
        assert!(
            captured.is_empty(),
            "raw provider error must not be emitted at the default (warn) level"
        );
    }

    #[test]
    fn raw_provider_error_logged_when_debug_enabled() {
        let captured = capture_raw_error_logging(tracing::level_filters::LevelFilter::DEBUG);
        assert_eq!(
            captured,
            vec!["openrouter: unexpected response shape (id 8842)".to_string()],
            "with debug enabled the raw error must be emitted once (and never when error is None)"
        );
    }

    #[test]
    fn sanitize_provider_error_masks_secret_shapes_and_caps_length() {
        // Bearer tokens / Authorization / sk-/or- keys / long alnum blobs are masked.
        let s = sanitize_provider_error("auth failed Bearer abc123XYZ token");
        assert!(s.contains("[REDACTED]"), "bearer token must be masked: {s}");
        assert!(!s.contains("abc123XYZ") || s.contains("[REDACTED]"));

        let key = sanitize_provider_error("bad key sk-abcdef0123456789ABCDEF here");
        assert!(key.contains("[REDACTED]"), "sk- key must be masked: {key}");
        assert!(!key.contains("sk-abcdef0123456789ABCDEF"));

        let blob = sanitize_provider_error("leaked ABCDEFGHIJKLMNOPQRSTUVWXYZ0123 blob");
        assert!(
            blob.contains("[REDACTED]"),
            "long key-ish blob must be masked: {blob}"
        );
        assert!(!blob.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ0123"));

        // Ordinary short words are preserved (message stays useful).
        let ordinary = sanitize_provider_error("upstream returned an empty body");
        assert_eq!(ordinary, "upstream returned an empty body");

        // Length is hard-capped.
        let long = "x ".repeat(1000);
        let capped = sanitize_provider_error(&long);
        assert!(capped.len() <= RAW_ERROR_LOG_CAP + "…[truncated]".len());
        assert!(capped.ends_with("…[truncated]"));
    }

    #[test]
    fn client_facing_message_stays_friendly_for_unclassified_error() {
        // The client-facing classification is unchanged: an unclassified raw
        // error yields the catch-all friendly message; raw text never leaks in.
        let raw = "openrouter: unexpected response shape (id 8842)";
        assert_eq!(
            safe_provider_error_message(false, Some(raw)),
            "Provider request failed."
        );
    }

    // --- prompt_message ---

    #[test]
    fn prompt_message_preserves_text_resources_links_and_images() {
        use agent_client_protocol::schema::v1::{
            EmbeddedResource, EmbeddedResourceResource, ImageContent, ResourceLink,
            TextResourceContents,
        };

        let blocks = vec![
            AcpContentBlock::Text(TextContent::new("hello")),
            AcpContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(
                    "fn main() {}",
                    "file:///workspace/src/main.rs",
                )),
            )),
            AcpContentBlock::ResourceLink(ResourceLink::new(
                "README",
                "file:///workspace/README.md",
            )),
            AcpContentBlock::Image(
                ImageContent::new("aW1hZ2U=", "image/png").uri("file:///workspace/image.png"),
            ),
        ];
        let message = prompt_message(blocks);
        assert_eq!(message.role, CoreRole::User);
        assert!(matches!(&message.content[0], CoreBlock::Text(text) if text == "hello"));
        assert!(matches!(&message.content[1], CoreBlock::Text(text)
                if text.contains("file:///workspace/src/main.rs")
                    && text.contains("fn main() {}")));
        assert!(matches!(&message.content[2], CoreBlock::Text(text)
                if text.contains("README") && text.contains("file:///workspace/README.md")));
        assert!(matches!(
            &message.content[3],
            CoreBlock::Image {
                data,
                media_type,
                uri: Some(uri),
            } if data == "aW1hZ2U="
                && media_type == "image/png"
                && uri == "file:///workspace/image.png"
        ));
    }

    #[test]
    fn prompt_message_does_not_silently_drop_unsupported_audio() {
        use agent_client_protocol::schema::v1::AudioContent;

        let message = prompt_message(vec![AcpContentBlock::Audio(AudioContent::new(
            "YXVkaW8=",
            "audio/wav",
        ))]);
        assert!(matches!(&message.content[0], CoreBlock::Text(text)
                if text.contains("Unsupported ACP audio block")
                    && text.contains("audio/wav")));
    }

    // --- Diff content for write_file/edit_file (vikunja #983) ---

    #[test]
    fn tool_target_path_joins_relative_to_workspace() {
        let ws = Path::new("/workspace");
        assert_eq!(
            tool_target_path(ws, &serde_json::json!({"path": "src/f.rs"})),
            Some(PathBuf::from("/workspace/src/f.rs"))
        );
        assert_eq!(
            tool_target_path(ws, &serde_json::json!({"path": "/abs/f.rs"})),
            Some(PathBuf::from("/abs/f.rs"))
        );
        assert_eq!(tool_target_path(ws, &serde_json::json!({})), None);
    }

    #[test]
    fn replay_edit_pairs_applies_sequentially() {
        let pairs = vec![
            serde_json::json!(["hello", "goodbye"]),
            serde_json::json!(["foo", "baz"]),
        ];
        assert_eq!(
            replay_edit_pairs("hello foo hello", &pairs),
            Some("goodbye baz hello".to_string())
        );
    }

    #[test]
    fn replay_edit_pairs_bails_on_unmatched_pair() {
        let pairs = vec![serde_json::json!(["not-present", "x"])];
        assert_eq!(replay_edit_pairs("hello", &pairs), None);
    }

    #[test]
    fn diff_for_write_file_uses_input_content() {
        let info = ToolCallInfo {
            id: "t1".to_string(),
            name: "write_file".to_string(),
            input: serde_json::json!({"path": "f.txt", "content": "new text\n"}),
        };
        let diff = diff_for_completed_edit(
            &info,
            r#"{"path":"f.txt"}"#,
            Path::new("/ws"),
            Some("old text\n".to_string()),
        )
        .unwrap();
        assert_eq!(diff.path, PathBuf::from("/ws/f.txt"));
        assert_eq!(diff.old_text.as_deref(), Some("old text\n"));
        assert_eq!(diff.new_text, "new text\n");
    }

    #[tokio::test]
    async fn canonical_tool_lifecycle_does_not_require_an_acp_connection() {
        let dir = tempfile::tempdir().unwrap();
        let connection: CurrentConnection = Arc::new(StdMutex::new(None));
        let active_tool_calls: ActiveToolCalls = Arc::new(StdMutex::new(HashMap::new()));
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let seen_for_handler = Arc::clone(&seen);
        let events = Arc::new(SessionEventRouter::new(Some(Arc::new(
            move |_sequence, event| {
                seen_for_handler
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(event);
            },
        ))));
        let approvals = Arc::new(ApprovalBroker::new(false));
        let tool_lifecycle = Arc::new(CanonicalToolLifecycle::new(
            Arc::clone(&events),
            approvals,
            Arc::new(crate::safety::SafetyPolicy::default()),
            4,
        ));
        let diff_stash: DiffStash = Arc::new(StdMutex::new(HashMap::new()));
        let info = ToolCallInfo {
            id: "t1".to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({"path": "f.txt"}),
        };
        let before = build_before_tool_call_hook(
            Arc::clone(&connection),
            SessionId::new("session-1"),
            Arc::clone(&active_tool_calls),
            Arc::clone(&tool_lifecycle),
            Arc::clone(&diff_stash),
            dir.path().to_path_buf(),
            false,
        );
        assert!(matches!(before(&info).await, BeforeHookResult::Allow));

        let after = build_after_tool_call_hook(
            connection,
            SessionId::new("session-1"),
            active_tool_calls,
            tool_lifecycle,
            diff_stash,
            dir.path().to_path_buf(),
        );
        assert!(matches!(
            after(&info, "ok", false),
            AfterHookResult::Continue
        ));

        let seen = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(matches!(
            seen.as_slice(),
            [
                CoreSessionEvent::ToolCallStarted { id, .. },
                CoreSessionEvent::ToolCallUpdated {
                    id: updated_id,
                    status: crate::session_protocol::ToolCallStateStatus::InProgress,
                },
                CoreSessionEvent::ToolCallFinished {
                    id: finished_id,
                    status: crate::session_protocol::ToolCallStateStatus::Completed,
                    ..
                }
            ] if id == "t1" && updated_id == "t1" && finished_id == "t1"
        ));
    }

    #[tokio::test]
    async fn detached_after_hook_still_releases_acp_liveness() {
        let dir = tempfile::tempdir().unwrap();
        let connection: CurrentConnection = Arc::new(StdMutex::new(None));
        let active_tool_calls: ActiveToolCalls = Arc::new(StdMutex::new(HashMap::new()));
        let events = Arc::new(SessionEventRouter::default());
        let tool_lifecycle = Arc::new(CanonicalToolLifecycle::new(
            events,
            Arc::new(ApprovalBroker::new(false)),
            Arc::new(crate::safety::SafetyPolicy::default()),
            4,
        ));
        let info = ToolCallInfo {
            id: "t1".to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({"path": "f.txt"}),
        };
        assert!(matches!(
            tool_lifecycle.before(&info).await,
            BeforeHookResult::Allow
        ));
        active_tool_calls.lock().unwrap().insert(
            info.id.clone(),
            ActiveToolCall {
                terminal_output: false,
                raw_input: Some(info.input.clone()),
            },
        );

        let after = build_after_tool_call_hook(
            connection,
            SessionId::new("session-1"),
            Arc::clone(&active_tool_calls),
            tool_lifecycle,
            Arc::new(StdMutex::new(HashMap::new())),
            dir.path().to_path_buf(),
        );
        assert!(matches!(
            after(&info, "ok", false),
            AfterHookResult::Continue
        ));
        assert!(
            active_tool_calls.lock().unwrap().is_empty(),
            "detach must not leave a completed ACP tool id live"
        );
    }

    #[test]
    fn diff_for_write_file_new_file_has_no_old_text() {
        let info = ToolCallInfo {
            id: "t1".to_string(),
            name: "write_file".to_string(),
            input: serde_json::json!({"path": "f.txt", "content": "created\n"}),
        };
        let diff = diff_for_completed_edit(&info, "{}", Path::new("/ws"), None).unwrap();
        assert_eq!(diff.old_text, None);
        assert_eq!(diff.new_text, "created\n");
    }

    #[test]
    fn diff_for_edit_file_replays_applied_pairs() {
        let info = ToolCallInfo {
            id: "t1".to_string(),
            name: "edit_file".to_string(),
            input: serde_json::json!({"path": "f.txt", "edits": ["hello", "goodbye"]}),
        };
        let diff = diff_for_completed_edit(
            &info,
            r#"{"applied":1,"diffs":[["hello","goodbye"]]}"#,
            Path::new("/ws"),
            Some("hello world\n".to_string()),
        )
        .unwrap();
        assert_eq!(diff.old_text.as_deref(), Some("hello world\n"));
        assert_eq!(diff.new_text, "goodbye world\n");
    }

    #[test]
    fn diff_for_edit_file_falls_back_without_applied_diffs() {
        let info = ToolCallInfo {
            id: "t1".to_string(),
            name: "edit_file".to_string(),
            input: serde_json::json!({"path": "f.txt", "edits": ["missing", "x"]}),
        };
        // applied == 0: no `diffs` key in the tool output.
        let result = diff_for_completed_edit(
            &info,
            r#"{"applied":0}"#,
            Path::new("/ws"),
            Some("hello\n".to_string()),
        );
        assert!(result.is_none());
    }

    #[test]
    fn diff_for_edit_file_falls_back_without_pre_edit_capture() {
        let info = ToolCallInfo {
            id: "t1".to_string(),
            name: "edit_file".to_string(),
            input: serde_json::json!({"path": "f.txt", "edits": ["a", "b"]}),
        };
        let result = diff_for_completed_edit(
            &info,
            r#"{"applied":1,"diffs":[["a","b"]]}"#,
            Path::new("/ws"),
            None,
        );
        assert!(result.is_none());
    }

    #[test]
    fn diff_not_built_for_non_edit_tools() {
        let info = ToolCallInfo {
            id: "t1".to_string(),
            name: "exec".to_string(),
            input: serde_json::json!({"path": "f.txt", "content": "x"}),
        };
        assert!(diff_for_completed_edit(&info, "{}", Path::new("/ws"), None).is_none());
    }

    // --- tool-call locations for follow-the-agent (vikunja #986) ---

    #[test]
    fn locations_for_read_file_include_offset_as_line() {
        let locs = tool_call_locations(
            Path::new("/ws"),
            "read_file",
            &serde_json::json!({"path": "src/f.rs", "offset": 42}),
        );
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].path, PathBuf::from("/ws/src/f.rs"));
        assert_eq!(locs[0].line, Some(42));
    }

    #[test]
    fn locations_for_read_file_without_offset_have_no_line() {
        let locs = tool_call_locations(
            Path::new("/ws"),
            "read_file",
            &serde_json::json!({"path": "f.rs"}),
        );
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].line, None);
    }

    #[test]
    fn locations_for_write_and_edit_carry_path_only() {
        for tool in ["write_file", "edit_file"] {
            let locs = tool_call_locations(
                Path::new("/ws"),
                tool,
                &serde_json::json!({"path": "f.rs", "offset": 3}),
            );
            assert_eq!(locs.len(), 1, "{tool}");
            assert_eq!(locs[0].path, PathBuf::from("/ws/f.rs"), "{tool}");
            assert_eq!(locs[0].line, None, "{tool}");
        }
    }

    #[test]
    fn locations_empty_for_non_file_tools_and_missing_path() {
        assert!(tool_call_locations(
            Path::new("/ws"),
            "exec",
            &serde_json::json!({"path": "f.rs"})
        )
        .is_empty());
        assert!(tool_call_locations(
            Path::new("/ws"),
            "search",
            &serde_json::json!({"pattern": "x"})
        )
        .is_empty());
    }

    #[tokio::test]
    async fn acp_tool_call_advertises_location() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "hi\n").unwrap();
        let updates = run_tool_call_flow(
            dir.path(),
            vec![
                tool_call_resp("t1", "read_file", serde_json::json!({"path": "f.txt"})),
                end_turn_resp("done"),
            ],
        )
        .await;

        let locations: Vec<&ToolCallLocation> = updates
            .iter()
            .filter_map(|u| match u {
                SessionUpdate::ToolCall(tc) => Some(&tc.locations),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(locations.len(), 1, "got: {updates:?}");
        assert_eq!(locations[0].path, dir.path().join("f.txt"));
    }

    // --- raw_output on completion (vikunja #991) ---

    fn raw_outputs(updates: &[SessionUpdate]) -> Vec<serde_json::Value> {
        updates
            .iter()
            .filter_map(|u| match u {
                SessionUpdate::ToolCallUpdate(tcu) => tcu.fields.raw_output.clone(),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn acp_completed_tool_call_carries_structured_raw_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "hi\n").unwrap();
        let updates = run_tool_call_flow(
            dir.path(),
            vec![
                tool_call_resp("t1", "read_file", serde_json::json!({"path": "f.txt"})),
                end_turn_resp("done"),
            ],
        )
        .await;

        let outputs = raw_outputs(&updates);
        assert_eq!(outputs.len(), 1, "got: {updates:?}");
        assert!(
            outputs[0].is_object(),
            "read_file output should parse as a JSON object: {:?}",
            outputs[0]
        );
        assert_eq!(outputs[0]["content"], "hi\n");
    }

    #[tokio::test]
    async fn acp_usage_uses_final_request_not_accumulated_tool_loop() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "hi\n").unwrap();
        let mut tool_call = tool_call_resp("t1", "read_file", serde_json::json!({"path": "f.txt"}));
        tool_call.usage = Usage {
            input: 100_000,
            output: 1_000,
            ..Usage::default()
        };
        let mut final_response = end_turn_resp("done");
        final_response.usage = Usage {
            input: 20_000,
            output: 500,
            cache_read: 3_000,
            cache_write: 2_000,
            ..Usage::default()
        };

        let updates = run_tool_call_flow(dir.path(), vec![tool_call, final_response]).await;
        let usage = updates
            .iter()
            .find_map(|update| match update {
                SessionUpdate::UsageUpdate(usage) => Some(usage),
                _ => None,
            })
            .expect("usage update");

        assert_eq!(usage.used, 25_500);
        assert_eq!(usage.size, 200_000);
    }

    #[tokio::test]
    async fn acp_plain_text_tool_result_becomes_json_string_raw_output() {
        let dir = tempfile::tempdir().unwrap();
        // A tool that doesn't exist in agent mode produces a plain-text
        // error message, not JSON — raw_output must wrap it as a string.
        let updates = run_tool_call_flow(
            dir.path(),
            vec![
                tool_call_resp("t1", "nonexistent_tool", serde_json::json!({})),
                end_turn_resp("done"),
            ],
        )
        .await;

        let outputs = raw_outputs(&updates);
        assert_eq!(outputs.len(), 1, "got: {updates:?}");
        assert!(
            outputs[0].as_str().unwrap_or("").contains("not available"),
            "got: {:?}",
            outputs[0]
        );
    }

    fn assert_no_orphan_tool_updates(updates: &[SessionUpdate]) {
        let mut active = HashSet::new();
        for (index, update) in updates.iter().enumerate() {
            match update {
                SessionUpdate::ToolCall(call) => {
                    active.insert(call.tool_call_id.to_string());
                }
                SessionUpdate::ToolCallUpdate(call) => {
                    let id = call.tool_call_id.to_string();
                    assert!(
                        active.contains(&id),
                        "orphan or post-completion update for {id} at index {index}: {updates:?}"
                    );
                    if matches!(
                        call.fields.status,
                        Some(ToolCallStatus::Completed | ToolCallStatus::Failed)
                    ) {
                        active.remove(&id);
                    }
                }
                _ => {}
            }
        }
        assert!(
            active.is_empty(),
            "tool calls did not reach a terminal state: {active:?}; updates: {updates:?}"
        );
    }

    #[test]
    fn tool_call_liveness_rejects_orphans_and_post_completion_updates() {
        let active: ActiveToolCalls = Arc::new(StdMutex::new(HashMap::new()));
        assert!(!tool_call_update_is_live(&active, "t1", false));
        assert!(!tool_call_update_is_live(&active, "t1", true));

        active.lock().unwrap().insert(
            "t1".to_string(),
            ActiveToolCall {
                terminal_output: false,
                raw_input: Some(serde_json::json!({})),
            },
        );
        assert!(tool_call_update_is_live(&active, "t1", false));
        assert!(tool_call_update_is_live(&active, "t1", true));
        assert!(!tool_call_update_is_live(&active, "t1", false));
        assert!(!tool_call_update_is_live(&active, "t1", true));
    }

    #[test]
    fn active_tool_call_retains_input_only_for_permission_request() {
        let info = ToolCallInfo {
            id: "t1".to_string(),
            name: "write_file".to_string(),
            input: serde_json::json!({"path": "large.txt", "content": "x".repeat(10_000)}),
        };
        assert!(ActiveToolCall::new(&info, false, false).raw_input.is_none());
        assert_eq!(
            ActiveToolCall::new(&info, false, true).raw_input,
            Some(info.input)
        );
    }

    /// Run one scripted tool call through the full ACP flow and return the
    /// collected session updates.
    async fn run_tool_call_flow(
        workspace: &Path,
        responses: Vec<crate::providers::LlmResponse>,
    ) -> Vec<SessionUpdate> {
        let agent = build_agent(
            mock_factory(responses),
            workspace,
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            None,
            None,
            None,
        );
        let updates: Arc<StdMutex<Vec<SessionUpdate>>> = Arc::new(StdMutex::new(Vec::new()));
        let updates_for_handler = Arc::clone(&updates);
        let ws = workspace.to_path_buf();
        AcpClientRole
            .builder()
            .on_receive_notification(
                move |notif: SessionNotification, _cx| {
                    let updates = Arc::clone(&updates_for_handler);
                    async move {
                        updates.lock().unwrap().push(notif.update);
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let new_session = connection
                    .send_request(NewSessionRequest::new(ws))
                    .block_task()
                    .await?;
                connection
                    .send_request(PromptRequest::new(
                        new_session.session_id,
                        vec![AcpContentBlock::Text(TextContent::new("go"))],
                    ))
                    .block_task()
                    .await?;
                Ok(())
            })
            .await
            .unwrap();
        let updates = Arc::try_unwrap(updates).unwrap().into_inner().unwrap();
        assert_no_orphan_tool_updates(&updates);
        updates
    }

    #[tokio::test]
    async fn invalid_plan_input_surfaces_failed_tool_chrome() {
        let workspace = tempfile::tempdir().unwrap();
        let updates = run_tool_call_flow(
            workspace.path(),
            vec![
                tool_call_resp(
                    "bad-plan",
                    UPDATE_PLAN_TOOL,
                    serde_json::json!({
                        "entries": [{
                            "content": " ",
                            "priority": "high",
                            "status": "pending"
                        }]
                    }),
                ),
                end_turn_resp("done"),
            ],
        )
        .await;

        assert!(updates.iter().any(|update| matches!(
            update,
            SessionUpdate::ToolCall(call) if call.title == UPDATE_PLAN_TOOL
        )));
        assert!(updates.iter().any(|update| matches!(
            update,
            SessionUpdate::ToolCallUpdate(call)
                if call.fields.status == Some(ToolCallStatus::Failed)
        )));
        assert!(!updates
            .iter()
            .any(|update| matches!(update, SessionUpdate::Plan(_))));
    }

    #[tokio::test]
    async fn acp_streams_exec_terminal_metadata_when_client_advertises_support() {
        use agent_client_protocol::schema::v1::ClientCapabilities;

        let workspace = tempfile::tempdir().unwrap();
        let agent = build_agent(
            mock_factory(vec![
                tool_call_resp(
                    "t1",
                    "exec",
                    serde_json::json!({"command": "printf streamed-terminal"}),
                ),
                end_turn_resp("done"),
            ]),
            workspace.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
            None,
            None,
            None,
            None,
        );
        let updates: Arc<StdMutex<Vec<SessionUpdate>>> = Arc::new(StdMutex::new(Vec::new()));
        let updates_for_handler = Arc::clone(&updates);
        let cwd = workspace.path().to_path_buf();

        AcpClientRole
            .builder()
            .on_receive_notification(
                move |notification: SessionNotification, _cx| {
                    let updates = Arc::clone(&updates_for_handler);
                    async move {
                        updates.lock().unwrap().push(notification.update);
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                let capabilities = ClientCapabilities::new().meta(
                    agent_client_protocol::schema::v1::Meta::from_iter([(
                        "terminal_output".to_string(),
                        serde_json::json!(true),
                    )]),
                );
                connection
                    .send_request(
                        InitializeRequest::new(ProtocolVersion::V1)
                            .client_capabilities(capabilities),
                    )
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                connection
                    .send_request(PromptRequest::new(
                        session.session_id,
                        vec![AcpContentBlock::Text(TextContent::new("run it"))],
                    ))
                    .block_task()
                    .await?;
                Ok(())
            })
            .await
            .unwrap();

        let updates = updates.lock().unwrap();
        assert_no_orphan_tool_updates(&updates);
        let announcement_index = updates
            .iter()
            .position(|update| matches!(update, SessionUpdate::ToolCall(call) if call.tool_call_id.to_string() == "t1"))
            .expect("exec tool-call announcement");
        let completion_index = updates
            .iter()
            .position(|update| {
                matches!(
                    update,
                    SessionUpdate::ToolCallUpdate(call)
                        if call.tool_call_id.to_string() == "t1"
                            && call.fields.status == Some(ToolCallStatus::Completed)
                )
            })
            .expect("exec tool-call completion");
        for (index, update) in updates.iter().enumerate() {
            if let SessionUpdate::ToolCallUpdate(call) = update {
                assert_eq!(call.tool_call_id.to_string(), "t1");
                let is_terminal_frame = call.meta.as_ref().is_some_and(|meta| {
                    meta.contains_key("terminal_output") || meta.contains_key("terminal_exit")
                });
                if is_terminal_frame {
                    assert!(
                        announcement_index < index && index < completion_index,
                        "terminal frame must be after announcement and before completion: {updates:?}"
                    );
                }
            }
        }
        let terminal_info = updates.iter().find_map(|update| match update {
            SessionUpdate::ToolCall(call) => call
                .meta
                .as_ref()
                .and_then(|meta| meta.get("terminal_info")),
            _ => None,
        });
        assert_eq!(terminal_info.unwrap()["terminal_id"], "t1");
        assert_eq!(
            terminal_info.unwrap()["cwd"],
            workspace.path().to_string_lossy().as_ref()
        );
        assert!(updates.iter().any(|update| matches!(
            update,
            SessionUpdate::ToolCall(call) if call.title == "printf streamed-terminal"
        )));

        let terminal_output = updates.iter().find_map(|update| match update {
            SessionUpdate::ToolCallUpdate(call) => call
                .meta
                .as_ref()
                .and_then(|meta| meta.get("terminal_output")),
            _ => None,
        });
        assert_eq!(terminal_output.unwrap()["terminal_id"], "t1");
        assert!(terminal_output.unwrap()["data"]
            .as_str()
            .unwrap()
            .contains("streamed-terminal"));

        let terminal_exit = updates.iter().find_map(|update| match update {
            SessionUpdate::ToolCallUpdate(call) => call
                .meta
                .as_ref()
                .and_then(|meta| meta.get("terminal_exit")),
            _ => None,
        });
        assert_eq!(terminal_exit.unwrap()["terminal_id"], "t1");
        assert_eq!(terminal_exit.unwrap()["exit_code"], 0);
    }

    #[tokio::test]
    async fn acp_omits_terminal_metadata_without_client_capability() {
        let workspace = tempfile::tempdir().unwrap();
        let updates = run_tool_call_flow(
            workspace.path(),
            vec![
                tool_call_resp(
                    "t1",
                    "exec",
                    serde_json::json!({"command": "printf no-terminal"}),
                ),
                end_turn_resp("done"),
            ],
        )
        .await;

        assert!(updates.iter().all(|update| match update {
            SessionUpdate::ToolCall(call) => call
                .meta
                .as_ref()
                .is_none_or(|meta| !meta.contains_key("terminal_info")),
            SessionUpdate::ToolCallUpdate(call) => call.meta.as_ref().is_none_or(|meta| {
                !meta.contains_key("terminal_output") && !meta.contains_key("terminal_exit")
            }),
            _ => true,
        }));
        assert!(updates.iter().any(|update| matches!(
            update,
            SessionUpdate::ToolCall(call) if call.title == "exec"
        )));
    }

    fn diff_contents(updates: &[SessionUpdate]) -> Vec<AcpDiff> {
        updates
            .iter()
            .filter_map(|u| match u {
                SessionUpdate::ToolCallUpdate(tcu) => tcu.fields.content.as_ref(),
                _ => None,
            })
            .flatten()
            .filter_map(|c| match c {
                AcpToolCallContent::Diff(d) => Some(d.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn acp_write_file_completion_carries_diff_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "before\n").unwrap();
        let updates = run_tool_call_flow(
            dir.path(),
            vec![
                tool_call_resp(
                    "t1",
                    "write_file",
                    serde_json::json!({"path": "f.txt", "content": "after\n"}),
                ),
                end_turn_resp("done"),
            ],
        )
        .await;

        let diffs = diff_contents(&updates);
        assert_eq!(diffs.len(), 1, "expected one Diff content: {updates:?}");
        assert_eq!(diffs[0].path, dir.path().join("f.txt"));
        assert_eq!(diffs[0].old_text.as_deref(), Some("before\n"));
        assert_eq!(diffs[0].new_text, "after\n");
    }

    #[tokio::test]
    async fn acp_edit_file_completion_carries_full_file_diff() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "hello world\n").unwrap();
        let updates = run_tool_call_flow(
            dir.path(),
            vec![
                tool_call_resp(
                    "t1",
                    "edit_file",
                    serde_json::json!({"path": "f.txt", "edits": ["hello", "goodbye"]}),
                ),
                end_turn_resp("done"),
            ],
        )
        .await;

        let diffs = diff_contents(&updates);
        assert_eq!(diffs.len(), 1, "expected one Diff content: {updates:?}");
        assert_eq!(diffs[0].old_text.as_deref(), Some("hello world\n"));
        assert_eq!(diffs[0].new_text, "goodbye world\n");
    }

    #[tokio::test]
    async fn acp_failed_edit_keeps_text_content() {
        let dir = tempfile::tempdir().unwrap();
        // No file and edits that can't apply -> edit_file errors (path missing).
        let updates = run_tool_call_flow(
            dir.path(),
            vec![
                tool_call_resp(
                    "t1",
                    "edit_file",
                    serde_json::json!({"path": "missing.txt", "edits": ["a", "b"]}),
                ),
                end_turn_resp("done"),
            ],
        )
        .await;

        assert!(
            diff_contents(&updates).is_empty(),
            "failed edit must not render a diff: {updates:?}"
        );
        let failed = updates.iter().any(|u| {
            matches!(u, SessionUpdate::ToolCallUpdate(tcu)
                if tcu.fields.status == Some(ToolCallStatus::Failed))
        });
        assert!(failed, "expected a Failed tool-call update: {updates:?}");
    }
}
