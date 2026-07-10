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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use serde::{Deserialize, Serialize};

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock as AcpContentBlock,
    ContentChunk, Cost as AcpCost, InitializeRequest, InitializeResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest,
    NewSessionResponse, PermissionOption, PermissionOptionKind, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelectOption, SessionId, SessionNotification,
    SessionUpdate, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
    StopReason as AcpStopReason, TextContent, ToolCall, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind, UsageUpdate,
};
use agent_client_protocol::{
    Agent as AcpAgentRole, Client as AcpClientRole, ConnectTo, ConnectionTo, Dispatch, Stdio,
};

use crate::agent::{
    AfterHook, AfterHookResult, AgentConfig, AgentSession, BeforeHook, BeforeHookResult,
    TokenLogConfig, ToolCallInfo,
};
use crate::agent_cmd::default_system_prompt;
use crate::config::Config;
use crate::providers::{CompleteOpts, LlmProvider, StreamEvent, ToolSchema, Usage};
use crate::session::Session;
use crate::tool_facade;

/// One in-flight prompt's cancellation switch. Stored outside the
/// session-holding lock (in its own quickly-acquired-and-released
/// `std::sync::Mutex`) so a `session/cancel` notification — dispatched
/// concurrently while a prompt is in flight — never has to wait on the same
/// lock the long-running prompt holds.
type CancelSlot = Arc<StdMutex<Option<Arc<tokio::sync::Notify>>>>;

/// The tool-call hooks (baked into `AgentConfig` once, at `session/new`
/// time) need a `ConnectionTo<Client>` to send notifications/requests on.
/// Requests specifically (`session/request_permission`) only get their
/// response routed correctly when sent via the `ConnectionTo` handle from
/// the *current* dispatch — reusing the handle captured at `session/new`
/// for a request made much later (during a subsequent `session/prompt`)
/// causes the response to be misrouted. Notifications don't have this
/// problem (they're fire-and-forget), so this only matters for
/// `request_permission`. Updated at the top of every `session/prompt` call
/// with that call's fresh handle; read fresh by the hooks on each use.
type CurrentConnection = Arc<StdMutex<Option<ConnectionTo<AcpClientRole>>>>;

/// The model the user has selected via the ACP model picker (vikunja #960).
/// A cheap `std::sync::Mutex` separate from the session lock so
/// `session/set_config_option` can update it instantly without stalling the
/// dispatch loop or waiting on an in-flight prompt's session lock. Applied
/// to the session at the top of each `run_prompt_turn`, so a switch takes
/// effect on the next prompt (you can't change model mid-turn anyway).
type CurrentModel = Arc<StdMutex<String>>;

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
    session: tokio::sync::Mutex<AgentSession>,
    cancel: CancelSlot,
    connection: CurrentConnection,
    current_model: CurrentModel,
}

/// Version tag on the persisted-session JSON, so a future on-disk format
/// change can be detected and old files ignored rather than mis-parsed.
const SESSION_PERSIST_VERSION: u32 = 1;

/// One session's on-disk record: enough to rebuild the thread on a
/// cross-process `session/load` (history + the model it was on).
#[derive(Serialize, Deserialize)]
struct PersistedSession {
    version: u32,
    session_id: String,
    model: String,
    messages: Vec<crate::providers::Message>,
}

/// On-disk store for ACP session history. Zed persists ACP session ids in its
/// own metadata DB and, after a restart, calls `session/load` with an id
/// minted in a previous daimonos process. In-memory replay can't serve that
/// (the process is gone), so — mirroring how Zed's native providers restore
/// full history from their local store — we persist each session to a JSON
/// file (one per id, rewritten after every completed prompt turn) and reload
/// it on `session/load`.
#[derive(Clone)]
struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    /// Filename for a session id, or `None` if the id has characters unsafe as
    /// a path component. Our ids are UUIDs; this only guards a hostile or
    /// malformed id against path traversal / collisions.
    fn file_name(session_id: &SessionId) -> Option<String> {
        let id = session_id.to_string();
        let safe =
            !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        safe.then(|| format!("{id}.json"))
    }

    /// Persist a session's history. Best-effort: a write failure is logged to
    /// stderr and never fails the prompt turn.
    fn save(&self, session_id: &SessionId, model: &str, messages: &[crate::providers::Message]) {
        let Some(name) = Self::file_name(session_id) else { return };
        let record = PersistedSession {
            version: SESSION_PERSIST_VERSION,
            session_id: session_id.to_string(),
            model: model.to_string(),
            messages: messages.to_vec(),
        };
        if let Err(e) = self.write_atomic(&name, &record) {
            eprintln!("acp: failed to persist session {session_id}: {e}");
        }
    }

    fn write_atomic(&self, name: &str, record: &PersistedSession) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let json = serde_json::to_vec(record).map_err(std::io::Error::other)?;
        // Write to a temp file then rename, so a crash mid-write can't leave a
        // truncated JSON file that would fail to load.
        let tmp = self.dir.join(format!("{name}.tmp"));
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, self.dir.join(name))
    }

    /// Load a persisted session, or `None` if absent / unreadable / a version
    /// we don't recognise (all treated as "not resumable", never an error).
    fn load(&self, session_id: &SessionId) -> Option<PersistedSession> {
        let name = Self::file_name(session_id)?;
        let bytes = std::fs::read(self.dir.join(name)).ok()?;
        let record: PersistedSession = serde_json::from_slice(&bytes).ok()?;
        (record.version == SESSION_PERSIST_VERSION).then_some(record)
    }
}

/// Shared engine state across all sessions on one process.
struct AcpState {
    /// Active sessions keyed by id. The map lock is held only briefly to
    /// look up / insert a handle — never across a prompt turn.
    sessions: tokio::sync::Mutex<HashMap<SessionId, Arc<SessionHandle>>>,
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
}

/// The `SessionConfigId` for the model picker option.
const MODEL_CONFIG_ID: &str = "model";

/// Map a daimonos tool name to the closest ACP [`ToolKind`] for client UI
/// (icon/treatment) purposes. Best-effort — unmapped tools fall back to
/// `ToolKind::Other`, which is a harmless default, not an error.
fn tool_kind_for(name: &str) -> ToolKind {
    match name {
        "read_file" | "search" | "kgl_query" | "ls" | "get_tool_schema" | "list_all_tools"
        | "workspace_info" | "session_stats" => ToolKind::Read,
        "write_file" | "edit_file" => ToolKind::Edit,
        "exec" | "execute_script" | "cargo" | "npm" | "pytest" | "docker" | "git" | "gh"
        | "batch" | "kgl_assert" => ToolKind::Execute,
        "curl" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

/// Rough context-window budget per model family, for `UsageUpdate.size`.
/// Best-effort — Zed only uses this to render a proportion, not an exact
/// limit daimonos itself enforces.
fn context_window_size_for(model: &str) -> u64 {
    if model.contains("claude") || model.contains("opus") || model.contains("sonnet") || model.contains("haiku") {
        200_000
    } else {
        128_000
    }
}

fn current_cx(connection: &CurrentConnection) -> Option<ConnectionTo<AcpClientRole>> {
    connection.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

fn send_notification(cx: &ConnectionTo<AcpClientRole>, session_id: &SessionId, update: SessionUpdate) {
    let _ = cx.send_notification(SessionNotification::new(session_id.clone(), update));
}

/// Send `session/request_permission` and await the client's answer,
/// applying the operator's decision to `safety` (persisting an "always
/// allow" choice the same way the stdin prompt does). Must be sent via the
/// *current* dispatch's connection handle (see [`CurrentConnection`]) — a
/// handle captured at `session/new` time and reused later for a request
/// does not get its response routed back correctly by this SDK.
async fn request_permission(
    cx: &ConnectionTo<AcpClientRole>,
    session_id: &SessionId,
    info: &ToolCallInfo,
    safety: &crate::safety::SafetyPolicy,
) -> BeforeHookResult {
    let update = ToolCallUpdate::new(
        info.id.clone(),
        ToolCallUpdateFields::new().raw_input(Some(info.input.clone())),
    );
    let options = vec![
        PermissionOption::new("allow_once", "Allow", PermissionOptionKind::AllowOnce),
        PermissionOption::new("allow_always", "Always Allow", PermissionOptionKind::AllowAlways),
        PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
    ];
    let request = RequestPermissionRequest::new(session_id.clone(), update, options);
    match cx.send_request(request).block_task().await {
        Ok(response) => match response.outcome {
            RequestPermissionOutcome::Selected(sel) => match sel.option_id.to_string().as_str() {
                "allow_once" => BeforeHookResult::Allow,
                "allow_always" => {
                    safety.remember_always(&info.name);
                    BeforeHookResult::Allow
                }
                _ => BeforeHookResult::Block(format!("permission denied for '{}'", info.name)),
            },
            RequestPermissionOutcome::Cancelled => {
                BeforeHookResult::Block(format!("permission request cancelled for '{}'", info.name))
            }
            _ => BeforeHookResult::Block(format!("unrecognized permission outcome for '{}'", info.name)),
        },
        Err(_) => BeforeHookResult::Block(format!("permission request failed for '{}'", info.name)),
    }
}

fn build_before_tool_call_hook(
    connection: CurrentConnection,
    session_id: SessionId,
    safety: Arc<crate::safety::SafetyPolicy>,
) -> BeforeHook {
    Box::new(move |info: &ToolCallInfo| {
        let connection = Arc::clone(&connection);
        let session_id = session_id.clone();
        let safety = Arc::clone(&safety);
        Box::pin(async move {
            let Some(cx) = current_cx(&connection) else {
                return BeforeHookResult::Block("no active ACP connection".to_string());
            };

            let tool_call = ToolCall::new(info.id.clone(), info.name.clone())
                .kind(tool_kind_for(&info.name))
                .status(ToolCallStatus::Pending)
                .raw_input(Some(info.input.clone()));
            send_notification(&cx, &session_id, SessionUpdate::ToolCall(tool_call));

            // Denylist/allowlist/approval-mode gating first (same policy
            // `daimonos agent`/`daimonos chat` enforce) — only tools that
            // actually need a prompt go through session/request_permission.
            let decision = match safety.gate(&info.name) {
                crate::safety::Gate::Block(reason) => BeforeHookResult::Block(reason),
                crate::safety::Gate::Allow => BeforeHookResult::Allow,
                crate::safety::Gate::NeedsApproval => {
                    request_permission(&cx, &session_id, info, &safety).await
                }
            };

            let status = match &decision {
                BeforeHookResult::Allow => ToolCallStatus::InProgress,
                BeforeHookResult::Block(_) => ToolCallStatus::Failed,
            };
            let update = ToolCallUpdate::new(info.id.clone(), ToolCallUpdateFields::new().status(Some(status)));
            send_notification(&cx, &session_id, SessionUpdate::ToolCallUpdate(update));

            decision
        })
    })
}

fn build_after_tool_call_hook(connection: CurrentConnection, session_id: SessionId) -> AfterHook {
    Box::new(move |info: &ToolCallInfo, content: &str, is_error: bool| {
        let Some(cx) = current_cx(&connection) else {
            return AfterHookResult::Continue;
        };
        let status = if is_error { ToolCallStatus::Failed } else { ToolCallStatus::Completed };
        let update = ToolCallUpdate::new(
            info.id.clone(),
            ToolCallUpdateFields::new()
                .status(Some(status))
                .content(Some(vec![AcpContentBlock::Text(TextContent::new(content.to_string())).into()])),
        );
        send_notification(&cx, &session_id, SessionUpdate::ToolCallUpdate(update));
        AfterHookResult::Continue
    })
}

fn build_stream_hook(connection: CurrentConnection, session_id: SessionId) -> crate::agent::StreamHook {
    Box::new(move |ev: StreamEvent| {
        let Some(cx) = current_cx(&connection) else { return };
        if let StreamEvent::TextDelta(text) = ev {
            let chunk = ContentChunk::new(AcpContentBlock::Text(TextContent::new(text)));
            send_notification(&cx, &session_id, SessionUpdate::AgentMessageChunk(chunk));
        }
    })
}

fn emit_usage_update(cx: &ConnectionTo<AcpClientRole>, session_id: &SessionId, model: &str, usage: &Usage) {
    let update = UsageUpdate::new(usage.input + usage.output, context_window_size_for(model))
        .cost(AcpCost::new(usage.cost.total_usd, "USD"));
    send_notification(cx, session_id, SessionUpdate::UsageUpdate(update));
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
    let option = SessionConfigOption::select(MODEL_CONFIG_ID, "Model", current.to_string(), options)
        .category(Some(SessionConfigOptionCategory::Model));
    vec![option]
}

/// Build the [`AgentConfig`] for one ACP session — mirrors
/// `chat_cmd::build_agent_config`, but every hook reports through
/// `session/update`/`session/request_permission` instead of the terminal.
fn build_agent_config(
    workspace: &Path,
    model: String,
    connection: CurrentConnection,
    session_id: SessionId,
    safety: Arc<crate::safety::SafetyPolicy>,
    token_log: Option<PathBuf>,
) -> AgentConfig {
    let tools: Vec<ToolSchema> = tool_facade::active_schemas(workspace)
        .into_iter()
        .map(|s| ToolSchema { name: s.name, description: s.description, input_schema: s.input_schema })
        .collect();
    AgentConfig {
        system: Some(default_system_prompt()),
        tools,
        opts: CompleteOpts { model, ..CompleteOpts::default() },
        before_tool_call: Some(build_before_tool_call_hook(Arc::clone(&connection), session_id.clone(), safety)),
        after_tool_call: Some(build_after_tool_call_hook(Arc::clone(&connection), session_id.clone())),
        on_stream_event: Some(build_stream_hook(connection, session_id)),
        token_log: token_log.map(|path| TokenLogConfig { path, label: "acp".to_string() }),
    }
}

fn map_stop_reason(stop_reason: crate::providers::StopReason) -> AcpStopReason {
    use crate::providers::StopReason;
    match stop_reason {
        StopReason::EndTurn | StopReason::ToolUse => AcpStopReason::EndTurn,
        StopReason::MaxTokens => AcpStopReason::MaxTokens,
        StopReason::Aborted => AcpStopReason::Cancelled,
        StopReason::Error => AcpStopReason::Refusal,
    }
}

/// Extract the plain text from a prompt's content blocks. Non-text blocks
/// (images, resources, ...) are dropped — daimonos's tool loop is
/// text-in/text-out; multimodal prompt support is a fast-follow, not a v1
/// requirement.
fn prompt_text(blocks: Vec<AcpContentBlock>) -> String {
    blocks
        .into_iter()
        .filter_map(|b| match b {
            AcpContentBlock::Text(t) => Some(t.text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run one prompt turn against `handle`, racing it against `session/cancel`.
/// Returns the ACP stop reason. Holds only this session's own lock, so other
/// sessions run concurrently unaffected.
async fn run_prompt_turn(
    handle: &Arc<SessionHandle>,
    cx: &ConnectionTo<AcpClientRole>,
    session_id: &SessionId,
    text: String,
    store: Option<&SessionStore>,
) -> AcpStopReason {
    // Acquire exclusive access to this session *before* publishing the
    // turn's connection/cancel handles — otherwise a second overlapping
    // session/prompt for the *same* session could overwrite the in-flight
    // turn's routing/cancellation handle while both wait on this lock.
    let mut agent_session = handle.session.lock().await;

    // Now that we hold the lock, refresh the connection handle with *this*
    // dispatch's cx — see `CurrentConnection`'s doc comment for why.
    *handle.connection.lock().unwrap_or_else(|p| p.into_inner()) = Some(cx.clone());

    let notify = Arc::new(tokio::sync::Notify::new());
    *handle.cancel.lock().unwrap_or_else(|p| p.into_inner()) = Some(notify.clone());

    // Apply the picker's current model selection before the turn — a switch
    // made via session/set_config_option takes effect on the next prompt.
    let model = handle.current_model.lock().unwrap_or_else(|p| p.into_inner()).clone();
    agent_session.set_model(model.clone());

    let outcome = tokio::select! {
        turn = agent_session.prompt(text) => Some(turn),
        _ = notify.notified() => None,
    };

    // Snapshot the updated history while we still hold the session lock, so we
    // can persist it after releasing the lock (cross-process session/load
    // resume). A cancelled turn leaves history unchanged (prompt is
    // cancel-safe), so we only persist on completion.
    let history_snapshot = outcome.as_ref().map(|_| agent_session.history().to_vec());
    drop(agent_session);
    *handle.cancel.lock().unwrap_or_else(|p| p.into_inner()) = None;

    if let (Some(store), Some(messages)) = (store, history_snapshot) {
        store.save(session_id, &model, &messages);
    }

    match outcome {
        Some(turn) => {
            emit_usage_update(cx, session_id, &model, &turn.usage);
            map_stop_reason(turn.stop_reason)
        }
        None => AcpStopReason::Cancelled,
    }
}

/// Build a fresh session handle (provider + agent session + per-session
/// cells) without inserting it into the map. Shared by `session/new` and the
/// unknown-id branch of `session/load` (a respawned process has no in-memory
/// state for the requested id).
#[allow(clippy::too_many_arguments)]
fn build_session_handle(
    state: &AcpState,
    cfg: &Arc<Config>,
    safety: Arc<crate::safety::SafetyPolicy>,
    token_log: Option<PathBuf>,
    session_id: SessionId,
    session_workspace: PathBuf,
    cx: ConnectionTo<AcpClientRole>,
) -> Result<Arc<SessionHandle>, String> {
    // Build a fresh provider for this session — Zed keeps one process across
    // multiple chat threads, so we can't reuse a single moved-once provider.
    let provider = (state.make_provider)()?;
    let connection: CurrentConnection = Arc::new(StdMutex::new(Some(cx)));
    let config = build_agent_config(
        &session_workspace,
        state.default_model.clone(),
        Arc::clone(&connection),
        session_id.clone(),
        safety,
        token_log,
    );
    let tool_session = Session::new(session_workspace, Arc::clone(cfg));
    Ok(Arc::new(SessionHandle {
        session: tokio::sync::Mutex::new(AgentSession::new(provider, tool_session, config)),
        cancel: Arc::new(StdMutex::new(None)),
        connection,
        current_model: Arc::new(StdMutex::new(state.default_model.clone())),
    }))
}

/// Replay a loaded session's in-memory history back to the client as
/// `session/update` notifications, so Zed rebuilds the reopened thread's
/// view on `session/load`. User text → `UserMessageChunk`, assistant text →
/// `AgentMessageChunk`, tool calls/results → the `ToolCall`/`ToolCallUpdate`
/// lifecycle. `Thinking` blocks are dropped (not shown in the thread view).
fn replay_history(cx: &ConnectionTo<AcpClientRole>, session_id: &SessionId, history: &[crate::providers::Message]) {
    use crate::providers::{ContentBlock as CoreBlock, Role};
    for message in history {
        for block in &message.content {
            match block {
                CoreBlock::Text(text) => {
                    let chunk = ContentChunk::new(AcpContentBlock::Text(TextContent::new(text.clone())));
                    let update = match message.role {
                        Role::User => SessionUpdate::UserMessageChunk(chunk),
                        Role::Assistant => SessionUpdate::AgentMessageChunk(chunk),
                    };
                    send_notification(cx, session_id, update);
                }
                CoreBlock::ToolCall { id, name, input } => {
                    let tool_call = ToolCall::new(id.clone(), name.clone())
                        .kind(tool_kind_for(name))
                        .status(ToolCallStatus::InProgress)
                        .raw_input(Some(input.clone()));
                    send_notification(cx, session_id, SessionUpdate::ToolCall(tool_call));
                }
                CoreBlock::ToolResult { tool_use_id, content, is_error } => {
                    let status = if *is_error { ToolCallStatus::Failed } else { ToolCallStatus::Completed };
                    let update = ToolCallUpdate::new(
                        tool_use_id.clone(),
                        ToolCallUpdateFields::new()
                            .status(Some(status))
                            .content(Some(vec![AcpContentBlock::Text(TextContent::new(content.clone())).into()])),
                    );
                    send_notification(cx, session_id, SessionUpdate::ToolCallUpdate(update));
                }
                CoreBlock::Thinking(_) => {}
            }
        }
    }
}

/// Build the fully-configured ACP agent, ready to `.connect_to(transport)`.
/// Split out from [`run_acp`] so tests can connect it to an in-process
/// [`AcpClientRole`] builder instead of real stdio.
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
) -> impl ConnectTo<AcpClientRole> {
    let workspace = workspace.to_path_buf();
    let state = Arc::new(AcpState {
        sessions: tokio::sync::Mutex::new(HashMap::new()),
        make_provider,
        models,
        default_model: model,
        store: sessions_dir.map(|dir| SessionStore { dir }),
    });

    AcpAgentRole
        .builder()
        .name("daimonos")
        .on_receive_request(
            async move |req: InitializeRequest, responder, _cx| {
                responder.respond(
                    InitializeResponse::new(req.protocol_version)
                        // load_session(true): Zed calls session/load to
                        // reopen a thread on window refocus; without this
                        // capability it refuses with "Loading or resuming
                        // sessions is not supported by this agent."
                        .agent_capabilities(AgentCapabilities::new().load_session(true)),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request({
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
                    // Use the client-provided project root, not the CLI's
                    // own cwd — Zed passes the actual project it wants this
                    // session to operate on.
                    let session_workspace =
                        if req.cwd.as_os_str().is_empty() { workspace_fallback } else { req.cwd };
                    let handle = match build_session_handle(
                        &state,
                        &cfg,
                        safety,
                        token_log,
                        session_id.clone(),
                        session_workspace,
                        cx,
                    ) {
                        Ok(handle) => handle,
                        Err(e) => {
                            return responder.respond_with_error(
                                agent_client_protocol::util::internal_error(format!("provider init: {e}")),
                            );
                        }
                    };
                    state.sessions.lock().await.insert(session_id.clone(), handle);

                    // Advertise the model picker (vikunja #960); new sessions
                    // start on the default model.
                    let config_options = model_config_options(&state.models, &state.default_model);
                    responder.respond(NewSessionResponse::new(session_id).config_options(Some(config_options)))
                }
            }
        }, agent_client_protocol::on_receive_request!())
        .on_receive_request({
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
                    let existing = state.sessions.lock().await.get(&session_id).cloned();
                    let current_model = if let Some(handle) = existing {
                        // 1. Live in memory: replay the in-memory history.
                        let agent_session = handle.session.lock().await;
                        replay_history(&cx, &session_id, agent_session.history());
                        drop(agent_session);
                        handle.current_model.lock().unwrap_or_else(|p| p.into_inner()).clone()
                    } else if let Some(record) =
                        state.store.as_ref().and_then(|s| s.load(&session_id))
                    {
                        // 2. Persisted on disk (process was restarted):
                        // rebuild the session, seed its history + model, then
                        // replay so Zed reconstructs the thread.
                        let session_workspace =
                            if req.cwd.as_os_str().is_empty() { workspace_fallback } else { req.cwd };
                        let handle = match build_session_handle(
                            &state,
                            &cfg,
                            safety,
                            token_log,
                            session_id.clone(),
                            session_workspace,
                            cx.clone(),
                        ) {
                            Ok(handle) => handle,
                            Err(e) => {
                                return responder.respond_with_error(
                                    agent_client_protocol::util::internal_error(format!("provider init: {e}")),
                                );
                            }
                        };
                        let model = record.model.clone();
                        {
                            let mut agent_session = handle.session.lock().await;
                            agent_session.set_history(record.messages);
                            agent_session.set_model(model.clone());
                            replay_history(&cx, &session_id, agent_session.history());
                        }
                        *handle.current_model.lock().unwrap_or_else(|p| p.into_inner()) = model.clone();
                        state.sessions.lock().await.insert(session_id.clone(), handle);
                        model
                    } else {
                        // 3. Unknown: not live, nothing persisted. Match
                        // native providers — error rather than fake a resume.
                        return responder.respond_with_error(agent_client_protocol::util::internal_error(
                            format!("no session found with id '{session_id}'"),
                        ));
                    };
                    // Echo the model picker (vikunja #960) with the session's
                    // current model, as session/new does.
                    let config_options = model_config_options(&state.models, &current_model);
                    responder.respond(LoadSessionResponse::new().config_options(Some(config_options)))
                }
            }
        }, agent_client_protocol::on_receive_request!())
        .on_receive_request({
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
                        let session_id = req.session_id;
                        let handle = state.sessions.lock().await.get(&session_id).cloned();
                        let stop_reason = match handle {
                            Some(handle) => {
                                let text = prompt_text(req.prompt);
                                run_prompt_turn(&handle, &spawn_cx, &session_id, text, state.store.as_ref()).await
                            }
                            None => AcpStopReason::Refusal,
                        };
                        let _ = responder.respond(PromptResponse::new(stop_reason));
                        Ok(())
                    });
                    Ok(())
                }
            }
        }, agent_client_protocol::on_receive_request!())
        .on_receive_request({
            // Model picker (vikunja #960): the user picked a model in Zed's
            // dropdown. Update this session's current-model cell (cheap, no
            // dispatch-loop stall, no wait on an in-flight prompt's session
            // lock); it's applied to the session on the next prompt turn.
            let state = Arc::clone(&state);
            move |req: SetSessionConfigOptionRequest,
                  responder: agent_client_protocol::Responder<SetSessionConfigOptionResponse>,
                  _cx: ConnectionTo<AcpClientRole>| {
                let state = Arc::clone(&state);
                async move {
                    let handle = state.sessions.lock().await.get(&req.session_id).cloned();
                    // Current value defaults to the session's start model if
                    // the session is unknown (shouldn't happen from a real
                    // client, but keep the echo sensible).
                    let mut current = state.default_model.clone();
                    if let Some(handle) = handle {
                        if req.config_id.to_string() == MODEL_CONFIG_ID {
                            if let Some(value) = req.value.as_value_id() {
                                let picked = value.to_string();
                                // Only honor a value we actually advertised.
                                if state.models.iter().any(|m| m == &picked) {
                                    *handle.current_model.lock().unwrap_or_else(|p| p.into_inner()) = picked;
                                }
                            }
                        }
                        current = handle.current_model.lock().unwrap_or_else(|p| p.into_inner()).clone();
                    }
                    let options = model_config_options(&state.models, &current);
                    responder.respond(SetSessionConfigOptionResponse::new(options))
                }
            }
        }, agent_client_protocol::on_receive_request!())
        .on_receive_notification({
            let state = Arc::clone(&state);
            move |notif: CancelNotification, _cx| {
                let state = Arc::clone(&state);
                async move {
                    let handle = state.sessions.lock().await.get(&notif.session_id).cloned();
                    if let Some(handle) = handle {
                        if let Some(notify) = handle.cancel.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
                            notify.notify_one();
                        }
                    }
                    Ok(())
                }
            }
        }, agent_client_protocol::on_receive_notification!())
        .on_receive_dispatch(
            async move |message: Dispatch, cx: ConnectionTo<AcpClientRole>| {
                // `Dispatch::Response` here is a legitimate correlated
                // response to a request *we* sent (e.g. session/request_
                // permission) — it must pass through unclaimed so the
                // framework's own internal routing can deliver it to the
                // waiting `SentRequest`. Only genuinely unhandled incoming
                // requests/notifications should be rejected.
                if matches!(message, Dispatch::Response(..)) {
                    return Ok(agent_client_protocol::Handled::No { message, retry: false });
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
) -> anyhow::Result<()> {
    if let Err(e) = (make_provider)() {
        anyhow::bail!("provider init: {e}");
    }
    build_agent(make_provider, workspace, cfg, model, models, Arc::new(safety), token_log, sessions_dir)
        .connect_to(Stdio::new())
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::ProtocolVersion;
    use async_trait::async_trait;
    use std::collections::VecDeque;

    // --- MockProvider (mirrors agent.rs/agent_cmd.rs test doubles) ---

    struct MockProvider {
        responses: StdMutex<VecDeque<crate::providers::LlmResponse>>,
    }

    impl MockProvider {
        fn new(responses: Vec<crate::providers::LlmResponse>) -> Self {
            MockProvider { responses: StdMutex::new(VecDeque::from(responses)) }
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
                if let crate::providers::ContentBlock::Text(t) = block {
                    on_event(StreamEvent::TextDelta(t.clone()));
                }
            }
            response
        }
    }

    fn end_turn_resp(text: &str) -> crate::providers::LlmResponse {
        crate::providers::LlmResponse {
            content: vec![crate::providers::ContentBlock::Text(text.to_string())],
            stop_reason: crate::providers::StopReason::EndTurn,
            error_message: None,
            usage: Usage { input: 10, output: 5, ..Usage::default() },
        }
    }

    fn tool_call_resp(id: &str, name: &str, input: serde_json::Value) -> crate::providers::LlmResponse {
        crate::providers::LlmResponse {
            content: vec![crate::providers::ContentBlock::ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                input,
            }],
            stop_reason: crate::providers::StopReason::ToolUse,
            error_message: None,
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
            updates.iter().any(|u| matches!(u, SessionUpdate::AgentMessageChunk(_))),
            "expected an AgentMessageChunk update, got: {updates:?}"
        );
        assert!(
            updates.iter().any(|u| matches!(u, SessionUpdate::UsageUpdate(_))),
            "expected a UsageUpdate, got: {updates:?}"
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
                assert_ne!(s1.session_id, s2.session_id, "sessions must have distinct ids");

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
                let prompt_response = tokio::time::timeout(std::time::Duration::from_secs(5), prompt_fut)
                    .await
                    .expect("prompt should resolve quickly once cancelled, not wait for the slow provider")?;

                Ok(prompt_response.stop_reason)
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
            permission_requests_seen.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a denylisted tool must not even ask for permission"
        );
        let updates = updates.lock().unwrap();
        let statuses: Vec<ToolCallStatus> = updates
            .iter()
            .filter_map(|u| match u {
                SessionUpdate::ToolCallUpdate(tcu) => tcu.fields.status,
                _ => None,
            })
            .collect();
        assert_eq!(statuses, vec![ToolCallStatus::Failed], "got: {updates:?}");
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
        assert!(completed_with_marker, "tool should have read marker.txt from the session's cwd: {updates:?}");
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
    }

    /// Pull the single model `SessionConfigOption` out of a config_options list.
    fn model_option(
        options: &[SessionConfigOption],
    ) -> &SessionConfigOption {
        options
            .iter()
            .find(|o| o.id.to_string() == MODEL_CONFIG_ID)
            .expect("a 'model' config option should be advertised")
    }

    fn select_state(option: &SessionConfigOption) -> &agent_client_protocol::schema::v1::SessionConfigSelect {
        match &option.kind {
            agent_client_protocol::schema::v1::SessionConfigKind::Select(s) => s,
            _ => panic!("model option should be a Select"),
        }
    }

    #[tokio::test]
    async fn acp_session_new_advertises_model_config_options() {
        let dir = tempfile::tempdir().unwrap();
        let make_provider = mock_factory(vec![]);
        let models = vec!["model-a".to_string(), "model-b".to_string(), "model-c".to_string()];
        let agent = build_agent(
            make_provider,
            dir.path(),
            Arc::new(Config::default()),
            "model-a".to_string(),
            models,
            Arc::new(crate::safety::SafetyPolicy::default()),
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
            Arc::new(move || Ok(Box::new(ModelCaptureProvider { seen: Arc::clone(&seen) })))
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
                let session_id = new_session.session_id;

                // Pick model-b via the picker.
                let set_resp = connection
                    .send_request(SetSessionConfigOptionRequest::new(
                        session_id.clone(),
                        MODEL_CONFIG_ID,
                        agent_client_protocol::schema::v1::SessionConfigOptionValue::value_id("model-b"),
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

                Ok(select_state(model_option(&set_resp.config_options)).current_value.to_string())
            })
            .await
            .unwrap();

        assert_eq!(echoed_current, "model-b", "set_config_option response must echo the new selection");
        assert_eq!(*seen.lock().unwrap(), vec!["model-b".to_string()], "the prompt turn must use the picked model");
    }

    #[tokio::test]
    async fn acp_set_config_option_ignores_unadvertised_model() {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let make_provider: ProviderFactory = {
            let seen = Arc::clone(&seen);
            Arc::new(move || Ok(Box::new(ModelCaptureProvider { seen: Arc::clone(&seen) })))
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
        );

        let echoed_current = AcpClientRole
            .builder()
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
                let new_session =
                    connection.send_request(NewSessionRequest::new(dir.path())).block_task().await?;
                let set_resp = connection
                    .send_request(SetSessionConfigOptionRequest::new(
                        new_session.session_id,
                        MODEL_CONFIG_ID,
                        agent_client_protocol::schema::v1::SessionConfigOptionValue::value_id("model-evil"),
                    ))
                    .block_task()
                    .await?;
                Ok(select_state(model_option(&set_resp.config_options)).current_value.to_string())
            })
            .await
            .unwrap();

        assert_eq!(echoed_current, "model-a", "an unadvertised model must be ignored, current unchanged");
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

        assert!(load_session, "agent must advertise load_session so Zed reopens threads");
    }

    #[tokio::test]
    async fn acp_session_load_replays_history_for_known_session() {
        // A live session's in-memory history must be replayed back as
        // session/update notifications so Zed rebuilds the reopened thread.
        let dir = tempfile::tempdir().unwrap();
        let make_provider = mock_factory(vec![end_turn_resp("recalled-text")]);
        let agent = build_agent(
            make_provider,
            dir.path(),
            Arc::new(Config::default()),
            "test-model".to_string(),
            vec!["test-model".to_string()],
            Arc::new(crate::safety::SafetyPolicy::default()),
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
                connection.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
                let new_session =
                    connection.send_request(NewSessionRequest::new(dir.path())).block_task().await?;
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

        assert!(config_options.is_some(), "session/load must echo the model-picker config_options");

        let updates = updates.lock().unwrap();
        let replayed = &updates[replay_start..];
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
        );

        let load_errored = AcpClientRole
            .builder()
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
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

        assert!(load_errored, "session/load of an unknown id with no persistence must error");
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
        );
        let ws1 = dir.path().to_path_buf();
        let session_id = AcpClientRole
            .builder()
            .connect_with(agent1, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
                let new_session =
                    connection.send_request(NewSessionRequest::new(ws1)).block_task().await?;
                let session_id = new_session.session_id;
                connection
                    .send_request(PromptRequest::new(
                        session_id.clone(),
                        vec![AcpContentBlock::Text(TextContent::new("persist-me"))],
                    ))
                    .block_task()
                    .await?;
                Ok(session_id)
            })
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
            .connect_with(agent2, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
                let result = connection
                    .send_request(LoadSessionRequest::new(session_id, ws2))
                    .block_task()
                    .await;
                Ok(result.is_ok())
            })
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
    fn context_window_size_claude_models() {
        assert_eq!(context_window_size_for("claude-haiku-4-5"), 200_000);
        assert_eq!(context_window_size_for("anthropic/claude-opus-4-8"), 200_000);
    }

    #[test]
    fn context_window_size_unknown_model_has_fallback() {
        assert_eq!(context_window_size_for("some-other-model"), 128_000);
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
        assert_eq!(map_stop_reason(StopReason::Aborted), AcpStopReason::Cancelled);
    }

    #[test]
    fn map_stop_reason_error_is_refusal() {
        use crate::providers::StopReason;
        assert_eq!(map_stop_reason(StopReason::Error), AcpStopReason::Refusal);
    }

    #[test]
    fn map_stop_reason_max_tokens() {
        use crate::providers::StopReason;
        assert_eq!(map_stop_reason(StopReason::MaxTokens), AcpStopReason::MaxTokens);
    }

    // --- prompt_text ---

    #[test]
    fn prompt_text_joins_text_blocks() {
        let blocks = vec![
            AcpContentBlock::Text(TextContent::new("hello")),
            AcpContentBlock::Text(TextContent::new("world")),
        ];
        assert_eq!(prompt_text(blocks), "hello\nworld");
    }

    #[test]
    fn prompt_text_empty_for_no_blocks() {
        assert_eq!(prompt_text(vec![]), "");
    }
}
