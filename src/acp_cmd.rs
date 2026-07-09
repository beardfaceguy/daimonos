//! `daimonos acp` — a native Agent Client Protocol engine (vikunja #954),
//! so Zed (and other ACP editors) can drive daimonos directly over stdio
//! instead of through the MCP adapter.
//!
//! Scope (v1): one active session at a time, text-only prompts, tool-call
//! lifecycle + permission requests + live usage reporting via
//! `session/update`. Cancellable via `session/cancel`. Out of scope for v1:
//! multiple concurrent sessions, `session/load` (resume), and the
//! `fs/*`/`terminal/*` client-proxy methods — daimonos has its own
//! file/exec tools and doesn't need to shell out through the client for them.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock as AcpContentBlock, ContentChunk,
    Cost as AcpCost, InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse,
    PermissionOption, PermissionOptionKind, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, SessionId, SessionNotification,
    SessionUpdate, StopReason as AcpStopReason, TextContent, ToolCall, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind, UsageUpdate,
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

/// v1 shared state: one active session at a time.
struct AcpState {
    session: tokio::sync::Mutex<Option<AgentSession>>,
    cancel: CancelSlot,
    connection: CurrentConnection,
}

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

/// Send `session/request_permission` and await the client's answer.
/// Must be sent via the *current* dispatch's connection handle (see
/// [`CurrentConnection`]) — a handle captured at `session/new` time and
/// reused later for a request does not get its response routed back
/// correctly by this SDK.
async fn request_permission(
    cx: &ConnectionTo<AcpClientRole>,
    session_id: &SessionId,
    info: &ToolCallInfo,
) -> BeforeHookResult {
    let update = ToolCallUpdate::new(
        info.id.clone(),
        ToolCallUpdateFields::new().raw_input(Some(info.input.clone())),
    );
    let options = vec![
        PermissionOption::new("allow", "Allow", PermissionOptionKind::AllowOnce),
        PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
    ];
    let request = RequestPermissionRequest::new(session_id.clone(), update, options);
    match cx.send_request(request).block_task().await {
        Ok(response) => match response.outcome {
            RequestPermissionOutcome::Selected(sel) if sel.option_id.to_string() == "allow" => {
                BeforeHookResult::Allow
            }
            _ => BeforeHookResult::Block(format!("permission denied for '{}'", info.name)),
        },
        Err(_) => BeforeHookResult::Block(format!("permission request failed for '{}'", info.name)),
    }
}

fn build_before_tool_call_hook(connection: CurrentConnection, session_id: SessionId) -> BeforeHook {
    Box::new(move |info: &ToolCallInfo| {
        let connection = Arc::clone(&connection);
        let session_id = session_id.clone();
        Box::pin(async move {
            let Some(cx) = current_cx(&connection) else {
                return BeforeHookResult::Block("no active ACP connection".to_string());
            };

            let tool_call = ToolCall::new(info.id.clone(), info.name.clone())
                .kind(tool_kind_for(&info.name))
                .status(ToolCallStatus::Pending)
                .raw_input(Some(info.input.clone()));
            send_notification(&cx, &session_id, SessionUpdate::ToolCall(tool_call));

            let decision = if crate::safety::is_destructive_tool(&info.name) {
                request_permission(&cx, &session_id, info).await
            } else {
                BeforeHookResult::Allow
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

/// Build the [`AgentConfig`] for one ACP session — mirrors
/// `chat_cmd::build_agent_config`, but every hook reports through
/// `session/update`/`session/request_permission` instead of the terminal.
fn build_agent_config(
    workspace: &Path,
    model: String,
    connection: CurrentConnection,
    session_id: SessionId,
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
        before_tool_call: Some(build_before_tool_call_hook(Arc::clone(&connection), session_id.clone())),
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

/// Run one prompt turn against the active session, racing it against
/// `session/cancel`. Returns the ACP stop reason.
async fn run_prompt_turn(
    state: &Arc<AcpState>,
    cx: &ConnectionTo<AcpClientRole>,
    session_id: &SessionId,
    model: &str,
    text: String,
) -> AcpStopReason {
    // Refresh the shared connection handle with *this* dispatch's cx before
    // running the turn — see `CurrentConnection`'s doc comment for why.
    *state.connection.lock().unwrap_or_else(|p| p.into_inner()) = Some(cx.clone());

    let notify = Arc::new(tokio::sync::Notify::new());
    *state.cancel.lock().unwrap_or_else(|p| p.into_inner()) = Some(notify.clone());

    let mut guard = state.session.lock().await;
    let Some(agent_session) = guard.as_mut() else {
        return AcpStopReason::Refusal;
    };

    let outcome = tokio::select! {
        turn = agent_session.prompt(text) => Some(turn),
        _ = notify.notified() => None,
    };
    drop(guard);
    *state.cancel.lock().unwrap_or_else(|p| p.into_inner()) = None;

    match outcome {
        Some(turn) => {
            emit_usage_update(cx, session_id, model, &turn.usage);
            map_stop_reason(turn.stop_reason)
        }
        None => AcpStopReason::Cancelled,
    }
}

/// Build the fully-configured ACP agent, ready to `.connect_to(transport)`.
/// Split out from [`run_acp`] so tests can connect it to an in-process
/// [`AcpClientRole`] builder instead of real stdio.
fn build_agent(
    provider: Box<dyn LlmProvider>,
    workspace: &Path,
    cfg: Arc<Config>,
    model: String,
    token_log: Option<PathBuf>,
) -> impl ConnectTo<AcpClientRole> {
    let workspace = workspace.to_path_buf();
    let state = Arc::new(AcpState {
        session: tokio::sync::Mutex::new(None),
        cancel: Arc::new(StdMutex::new(None)),
        connection: Arc::new(StdMutex::new(None)),
    });
    let provider = Arc::new(std::sync::Mutex::new(Some(provider)));

    AcpAgentRole
        .builder()
        .name("daimonos")
        .on_receive_request(
            async move |req: InitializeRequest, responder, _cx| {
                responder.respond(
                    InitializeResponse::new(req.protocol_version).agent_capabilities(AgentCapabilities::new()),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request({
            let state = Arc::clone(&state);
            let workspace = workspace.clone();
            let cfg = Arc::clone(&cfg);
            let model = model.clone();
            let token_log = token_log.clone();
            let provider = Arc::clone(&provider);
            move |_req: NewSessionRequest,
                  responder: agent_client_protocol::Responder<NewSessionResponse>,
                  cx: ConnectionTo<AcpClientRole>| {
                let state = Arc::clone(&state);
                let workspace = workspace.clone();
                let cfg = Arc::clone(&cfg);
                let model = model.clone();
                let token_log = token_log.clone();
                let provider = Arc::clone(&provider);
                async move {
                    let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
                    let Some(provider) = provider.lock().unwrap_or_else(|p| p.into_inner()).take() else {
                        return responder.respond_with_error(agent_client_protocol::util::internal_error(
                            "daimonos acp v1 supports only one session per process",
                        ));
                    };
                    *state.connection.lock().unwrap_or_else(|p| p.into_inner()) = Some(cx);
                    let config = build_agent_config(
                        &workspace,
                        model,
                        Arc::clone(&state.connection),
                        session_id.clone(),
                        token_log,
                    );
                    let tool_session = Session::new(workspace.clone(), cfg);
                    let agent_session = AgentSession::new(provider, tool_session, config);
                    *state.session.lock().await = Some(agent_session);
                    responder.respond(NewSessionResponse::new(session_id))
                }
            }
        }, agent_client_protocol::on_receive_request!())
        .on_receive_request({
            let state = Arc::clone(&state);
            let model = model.clone();
            move |req: PromptRequest,
                  responder: agent_client_protocol::Responder<PromptResponse>,
                  cx: ConnectionTo<AcpClientRole>| {
                let state = Arc::clone(&state);
                let model = model.clone();
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
                        let text = prompt_text(req.prompt);
                        let stop_reason = run_prompt_turn(&state, &spawn_cx, &session_id, &model, text).await;
                        let _ = responder.respond(PromptResponse::new(stop_reason));
                        Ok(())
                    });
                    Ok(())
                }
            }
        }, agent_client_protocol::on_receive_request!())
        .on_receive_notification({
            let state = Arc::clone(&state);
            move |_notif: CancelNotification, _cx| {
                let state = Arc::clone(&state);
                async move {
                    if let Some(notify) = state.cancel.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
                        notify.notify_one();
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
pub async fn run_acp(
    provider: Box<dyn LlmProvider>,
    workspace: &Path,
    cfg: Arc<Config>,
    model: String,
    token_log: Option<PathBuf>,
) -> anyhow::Result<()> {
    build_agent(provider, workspace, cfg, model, token_log)
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
        let provider = Box::new(MockProvider::new(vec![end_turn_resp("hello from daimonos")]));
        let agent = build_agent(provider, dir.path(), Arc::new(Config::default()), "test-model".to_string(), None);

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
    async fn acp_second_session_new_errors_v1_single_session() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(MockProvider::new(vec![end_turn_resp("ok")]));
        let agent = build_agent(provider, dir.path(), Arc::new(Config::default()), "test-model".to_string(), None);

        let result = AcpClientRole
            .builder()
            .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                connection
                    .send_request(NewSessionRequest::new(dir.path()))
                    .block_task()
                    .await?;
                // Second session/new: v1 only supports one session per process.
                let second = connection.send_request(NewSessionRequest::new(dir.path())).block_task().await;
                Ok(second.is_err())
            })
            .await
            .unwrap();

        assert!(result, "a second session/new must fail in the v1 single-session engine");
    }

    #[tokio::test]
    async fn acp_session_cancel_aborts_inflight_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let provider: Box<dyn LlmProvider> = Box::new(SlowProvider);
        let agent = build_agent(provider, dir.path(), Arc::new(Config::default()), "test-model".to_string(), None);

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
        let provider = Box::new(MockProvider::new(vec![
            tool_call_resp("t1", "exec", serde_json::json!({"command": "echo hi"})),
            end_turn_resp("done"),
        ]));
        let agent = build_agent(provider, dir.path(), Arc::new(Config::default()), "test-model".to_string(), None);

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
