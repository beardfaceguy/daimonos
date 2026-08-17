#![allow(dead_code)] // Wired into SessionCore/transport in the next incremental slice.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientCapability {
    Observe,
    Prompt,
    Configure,
    Interrupt,
    Stop,
    ApproveOnce,
    ApproveAlways,
}

pub fn has_capability(granted: &[ClientCapability], required: ClientCapability) -> bool {
    granted.contains(&required)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    Terminal,
    Android,
    Headless,
    Browser,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub id: String,
    pub kind: ClientKind,
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    AllowAlways,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Attach {
        protocol_version: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        ticket: Option<String>,
        client: ClientInfo,
        requested_capabilities: Vec<ClientCapability>,
    },
    Resume {
        protocol_version: u16,
        session_id: String,
        last_seen_seq: u64,
        ticket: Option<String>,
        client: ClientInfo,
        requested_capabilities: Vec<ClientCapability>,
    },
    Prompt {
        request_id: String,
        text: String,
    },
    ApprovalResponse {
        approval_id: String,
        decision: ApprovalDecision,
    },
    Interrupt {
        request_id: Option<String>,
    },
    StopSession {
        request_id: String,
    },
    ListSessions {
        request_id: String,
        cursor: Option<String>,
    },
    SyncRequest {
        last_seen_seq: u64,
    },
    SetConfig {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        config_id: String,
        value: RuntimeValue,
    },
    Ping,
    Detach,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    AttachOk {
        protocol_version: u16,
        session_id: String,
        granted_capabilities: Vec<ClientCapability>,
        seq: u64,
    },
    AttachDenied {
        reason: String,
    },
    Event {
        seq: u64,
        event: SessionEvent,
    },
    Snapshot {
        seq: u64,
        state: SessionSnapshot,
    },
    Error {
        request_id: Option<String>,
        code: String,
        message: String,
    },
    CommandResult {
        request_id: String,
        operation: String,
        changed: bool,
    },
    SessionList {
        request_id: String,
        sessions: Vec<SessionListEntry>,
        next_cursor: Option<String>,
    },
    Pong,
    Revoked {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Idle,
    Running,
    WaitingForApproval,
    Cancelling,
    /// Terminal state for a turn that was cancelled, as distinct from `Idle`
    /// for one that ran to completion. Without it both outcomes end the stream
    /// on `Idle` and a replaying consumer cannot tell them apart.
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptRole {
    User,
    Assistant,
    System,
    Thought,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptEntry {
    pub id: u64,
    pub role: TranscriptRole,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<AssistantOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListEntry {
    pub session_id: String,
    pub active: bool,
    pub attached_clients: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStateStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallState {
    pub id: String,
    pub name: String,
    pub title: String,
    pub status: ToolCallStateStatus,
    pub output: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub tool_call_id: String,
    pub tool: String,
    pub detail: String,
    pub allow_always_available: bool,
}

impl ApprovalRequest {
    /// Build a request whose wire id will be assigned by `ApprovalBroker`.
    pub fn unassigned(
        tool_call_id: impl Into<String>,
        tool: impl Into<String>,
        detail: impl Into<String>,
        allow_always_available: bool,
    ) -> Self {
        Self {
            id: String::new(),
            tool_call_id: tool_call_id.into(),
            tool: tool.into(),
            detail: detail.into(),
            allow_always_available,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantOutcome {
    Completed,
    Errored {
        context_overflow: bool,
        /// Privacy-safe diagnostic text suitable for every client projection.
        message: String,
    },
    Refused,
    Aborted,
    MaxTokens,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    UserMessage {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    AssistantDelta {
        text: String,
    },
    AssistantDone {
        outcome: AssistantOutcome,
    },
    ThoughtDelta {
        text: String,
    },
    ToolCallStarted {
        id: String,
        name: String,
        title: String,
        input_summary: Option<String>,
    },
    ToolCallUpdated {
        id: String,
        status: ToolCallStateStatus,
    },
    ToolCallFinished {
        id: String,
        status: ToolCallStateStatus,
        output: String,
    },
    ApprovalRequested {
        request: ApprovalRequest,
    },
    ApprovalResolved {
        approval_id: String,
        decision: ApprovalDecision,
        resolved_by: String,
    },
    RuntimeOptionsChanged {
        options: Vec<RuntimeOption>,
    },
    ContextUsageChanged {
        usage: ContextUsage,
    },
    TurnStatusChanged {
        status: TurnStatus,
    },
    SessionEnding {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub seq: u64,
    pub turn_status: TurnStatus,
    pub transcript: Vec<TranscriptEntry>,
    pub tool_calls: Vec<ToolCallState>,
    pub pending_approvals: Vec<ApprovalRequest>,
    pub runtime_options: Vec<RuntimeOption>,
    pub context_usage: Option<ContextUsage>,
    #[serde(default)]
    pub history_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RuntimeValue {
    String(String),
    Bool(bool),
    Integer(i64),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeChoice {
    pub id: String,
    pub label: String,
}

impl RuntimeChoice {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeOptionSpec {
    Select { choices: Vec<RuntimeChoice> },
    Boolean,
    Integer { min: i64, max: i64, step: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeOption {
    pub id: String,
    pub label: String,
    pub value: RuntimeValue,
    pub default_value: RuntimeValue,
    pub spec: RuntimeOptionSpec,
    pub mutable_while_running: bool,
    pub help: Option<String>,
}

impl RuntimeOption {
    pub fn select(
        id: impl Into<String>,
        label: impl Into<String>,
        value: RuntimeValue,
        choices: Vec<RuntimeChoice>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            default_value: value.clone(),
            value,
            spec: RuntimeOptionSpec::Select { choices },
            mutable_while_running: false,
            help: None,
        }
    }

    pub fn boolean(id: impl Into<String>, label: impl Into<String>, value: RuntimeValue) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            default_value: value.clone(),
            value,
            spec: RuntimeOptionSpec::Boolean,
            mutable_while_running: false,
            help: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn integer(
        id: impl Into<String>,
        label: impl Into<String>,
        value: RuntimeValue,
        min: i64,
        max: i64,
        step: i64,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            default_value: value.clone(),
            value,
            spec: RuntimeOptionSpec::Integer {
                min,
                max,
                step: step.max(1),
            },
            mutable_while_running: false,
            help: None,
        }
    }

    pub fn accepts(&self, candidate: &RuntimeValue) -> bool {
        match (&self.spec, candidate) {
            (RuntimeOptionSpec::Select { choices }, RuntimeValue::String(value)) => {
                choices.iter().any(|choice| choice.id == *value)
            }
            (RuntimeOptionSpec::Boolean, RuntimeValue::Bool(_)) => true,
            (RuntimeOptionSpec::Integer { min, max, step }, RuntimeValue::Integer(value)) => {
                *step > 0
                    && min <= max
                    && value >= min
                    && value <= max
                    && value
                        .checked_sub(*min)
                        .is_some_and(|offset| offset % step == 0)
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextBudgetError {
    OutputReservationNotBelowWindow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUsage {
    pub prompt_tokens: u64,
    /// True when prompt_tokens came from the previous model's last observed
    /// occupancy and is being projected onto a newly selected model window.
    #[serde(default, skip_serializing_if = "is_false")]
    pub estimated: bool,
    pub model_context_window: Option<u64>,
    pub output_reservation: u64,
    pub effective_input_budget: Option<u64>,
    pub utilization_basis_points: Option<u16>,
    pub compaction_high_water_tokens: Option<u64>,
    pub budget_error: Option<ContextBudgetError>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl ContextUsage {
    pub fn new(
        prompt_tokens: u64,
        model_context_window: Option<u64>,
        output_reservation: u64,
        compaction_high_water_tokens: Option<u64>,
    ) -> Self {
        let budget_error = model_context_window
            .filter(|window| output_reservation >= *window)
            .map(|_| ContextBudgetError::OutputReservationNotBelowWindow);
        let effective_input_budget = model_context_window
            .and_then(|window| window.checked_sub(output_reservation))
            .filter(|budget| *budget > 0);
        let utilization_basis_points = effective_input_budget.map(|budget| {
            let basis_points = (u128::from(prompt_tokens) * 10_000) / u128::from(budget);
            basis_points.min(10_000) as u16
        });
        Self {
            prompt_tokens,
            estimated: false,
            model_context_window,
            output_reservation,
            effective_input_budget,
            utilization_basis_points,
            compaction_high_water_tokens,
            budget_error,
        }
    }

    pub fn mark_estimated(mut self) -> Self {
        self.estimated = true;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SequenceError {
    NotIncreasing { previous: u64, next: u64 },
}

pub fn validate_next_sequence(previous: Option<u64>, next: u64) -> Result<(), SequenceError> {
    if let Some(previous) = previous {
        if next <= previous {
            return Err(SequenceError::NotIncreasing { previous, next });
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolLimits {
    pub max_frame_bytes: usize,
    /// UTF-8 byte cap. Validation rejects; it never truncates at this boundary.
    pub max_prompt_bytes: usize,
    /// UTF-8 byte cap. Validation rejects; it never truncates at this boundary.
    pub max_label_bytes: usize,
    pub max_identifier_bytes: usize,
    pub max_ticket_bytes: usize,
    pub max_runtime_value_bytes: usize,
    pub max_capabilities: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolValidationError {
    FieldTooLarge {
        field: &'static str,
        max_bytes: usize,
    },
    TooManyCapabilities {
        max: usize,
    },
}

impl std::fmt::Display for ProtocolValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FieldTooLarge { field, max_bytes } => {
                write!(formatter, "{field} exceeds {max_bytes} bytes")
            }
            Self::TooManyCapabilities { max } => {
                write!(formatter, "requested capabilities exceed limit {max}")
            }
        }
    }
}

impl ProtocolLimits {
    pub fn validate_client_message(
        &self,
        message: &ClientMessage,
    ) -> Result<(), ProtocolValidationError> {
        let check = |field: &'static str, value: &str, max_bytes: usize| {
            (value.len() > max_bytes)
                .then_some(ProtocolValidationError::FieldTooLarge { field, max_bytes })
        };
        let error = match message {
            ClientMessage::Attach {
                session_id,
                ticket,
                client,
                requested_capabilities,
                ..
            } => check("attach.client.id", &client.id, self.max_identifier_bytes)
                .or_else(|| check("attach.client.label", &client.label, self.max_label_bytes))
                .or_else(|| {
                    session_id.as_deref().and_then(|session_id| {
                        check("attach.session_id", session_id, self.max_identifier_bytes)
                    })
                })
                .or_else(|| {
                    ticket
                        .as_deref()
                        .and_then(|ticket| check("attach.ticket", ticket, self.max_ticket_bytes))
                })
                .or_else(|| {
                    (requested_capabilities.len() > self.max_capabilities).then_some(
                        ProtocolValidationError::TooManyCapabilities {
                            max: self.max_capabilities,
                        },
                    )
                }),
            ClientMessage::Resume {
                session_id,
                ticket,
                client,
                requested_capabilities,
                ..
            } => check("resume.client.id", &client.id, self.max_identifier_bytes)
                .or_else(|| check("resume.client.label", &client.label, self.max_label_bytes))
                .or_else(|| check("resume.session_id", session_id, self.max_identifier_bytes))
                .or_else(|| {
                    ticket
                        .as_deref()
                        .and_then(|ticket| check("resume.ticket", ticket, self.max_ticket_bytes))
                })
                .or_else(|| {
                    (requested_capabilities.len() > self.max_capabilities).then_some(
                        ProtocolValidationError::TooManyCapabilities {
                            max: self.max_capabilities,
                        },
                    )
                }),
            ClientMessage::Prompt { request_id, text } => {
                check("prompt.request_id", request_id, self.max_identifier_bytes)
                    .or_else(|| check("prompt.text", text, self.max_prompt_bytes))
            }
            ClientMessage::ApprovalResponse { approval_id, .. } => check(
                "approval_response.approval_id",
                approval_id,
                self.max_identifier_bytes,
            ),
            ClientMessage::Interrupt { request_id } => request_id
                .as_deref()
                .and_then(|id| check("interrupt.request_id", id, self.max_identifier_bytes)),
            ClientMessage::StopSession { request_id } => check(
                "stop_session.request_id",
                request_id,
                self.max_identifier_bytes,
            ),
            ClientMessage::ListSessions { request_id, cursor } => check(
                "list_sessions.request_id",
                request_id,
                self.max_identifier_bytes,
            )
            .or_else(|| {
                cursor.as_deref().and_then(|cursor| {
                    check("list_sessions.cursor", cursor, self.max_identifier_bytes)
                })
            }),
            ClientMessage::SetConfig {
                request_id,
                config_id,
                value,
            } => request_id
                .as_deref()
                .and_then(|id| check("set_config.request_id", id, self.max_identifier_bytes))
                .or_else(|| check("set_config.config_id", config_id, self.max_identifier_bytes))
                .or_else(|| {
                    if let RuntimeValue::String(value) = value {
                        check("set_config.value", value, self.max_runtime_value_bytes)
                    } else {
                        None
                    }
                }),
            ClientMessage::SyncRequest { .. } | ClientMessage::Ping | ClientMessage::Detach => None,
        };
        error.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn client() -> ClientInfo {
        ClientInfo {
            id: "phone-1".to_string(),
            kind: ClientKind::Android,
            label: "Pixel".to_string(),
        }
    }

    #[test]
    fn client_prompt_wire_shape_is_stable_and_accepts_unknown_fields() {
        let message = ClientMessage::Prompt {
            request_id: "p1".to_string(),
            text: "run the tests".to_string(),
        };
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(
            value,
            json!({
                "type": "prompt",
                "request_id": "p1",
                "text": "run the tests"
            })
        );

        let with_future_field = json!({
            "type": "prompt",
            "request_id": "p1",
            "text": "run the tests",
            "future_client_hint": true
        });
        assert_eq!(
            serde_json::from_value::<ClientMessage>(with_future_field).unwrap(),
            message
        );
    }

    #[test]
    fn attach_and_server_event_round_trip() {
        let attach = ClientMessage::Attach {
            protocol_version: PROTOCOL_VERSION,
            session_id: None,
            ticket: Some("opaque-ticket".to_string()),
            client: client(),
            requested_capabilities: vec![
                ClientCapability::Observe,
                ClientCapability::Prompt,
                ClientCapability::ApproveOnce,
            ],
        };
        let encoded = serde_json::to_string(&attach).unwrap();
        assert_eq!(
            serde_json::from_str::<ClientMessage>(&encoded).unwrap(),
            attach
        );

        let event = ServerMessage::Event {
            seq: 7,
            event: SessionEvent::AssistantDelta {
                text: "working".to_string(),
            },
        };
        let encoded = serde_json::to_string(&event).unwrap();
        assert_eq!(
            serde_json::from_str::<ServerMessage>(&encoded).unwrap(),
            event
        );
    }

    #[test]
    fn attach_optionally_selects_a_persisted_session() {
        let selected = ClientMessage::Attach {
            protocol_version: PROTOCOL_VERSION,
            session_id: Some("session-42".to_string()),
            ticket: None,
            client: client(),
            requested_capabilities: vec![ClientCapability::Observe],
        };
        assert_eq!(
            serde_json::to_value(&selected).unwrap(),
            json!({
                "type": "attach",
                "protocol_version": PROTOCOL_VERSION,
                "session_id": "session-42",
                "ticket": null,
                "client": client(),
                "requested_capabilities": ["observe"]
            })
        );

        let create_new = json!({
            "type": "attach",
            "protocol_version": PROTOCOL_VERSION,
            "ticket": null,
            "client": client(),
            "requested_capabilities": ["observe"]
        });
        assert!(matches!(
            serde_json::from_value::<ClientMessage>(create_new).unwrap(),
            ClientMessage::Attach {
                session_id: None,
                ..
            }
        ));
    }

    #[test]
    fn stop_session_has_stable_wire_shape_and_identifier_limit() {
        let message = ClientMessage::StopSession {
            request_id: "stop-1".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&message).unwrap(),
            json!({"type": "stop_session", "request_id": "stop-1"})
        );
        let limits = ProtocolLimits {
            max_frame_bytes: 1_024,
            max_prompt_bytes: 64,
            max_label_bytes: 64,
            max_identifier_bytes: 4,
            max_ticket_bytes: 64,
            max_runtime_value_bytes: 64,
            max_capabilities: 8,
        };
        assert_eq!(
            limits.validate_client_message(&message),
            Err(ProtocolValidationError::FieldTooLarge {
                field: "stop_session.request_id",
                max_bytes: 4,
            })
        );
    }

    #[test]
    fn assistant_done_wire_shape_carries_faithful_outcome() {
        let cases = [
            (
                AssistantOutcome::Completed,
                json!({"type": "assistant_done", "outcome": {"type": "completed"}}),
            ),
            (
                AssistantOutcome::Errored {
                    context_overflow: true,
                    message: "context exceeded".to_string(),
                },
                json!({
                    "type": "assistant_done",
                    "outcome": {
                        "type": "errored",
                        "context_overflow": true,
                        "message": "context exceeded"
                    }
                }),
            ),
            (
                AssistantOutcome::Refused,
                json!({"type": "assistant_done", "outcome": {"type": "refused"}}),
            ),
            (
                AssistantOutcome::Aborted,
                json!({"type": "assistant_done", "outcome": {"type": "aborted"}}),
            ),
            (
                AssistantOutcome::MaxTokens,
                json!({"type": "assistant_done", "outcome": {"type": "max_tokens"}}),
            ),
        ];
        for (outcome, expected) in cases {
            let event = SessionEvent::AssistantDone { outcome };
            assert_eq!(serde_json::to_value(&event).unwrap(), expected);
            assert_eq!(
                serde_json::from_value::<SessionEvent>(expected).unwrap(),
                event
            );
        }
    }

    #[test]
    fn capability_set_is_explicit_not_role_implied() {
        let granted = vec![ClientCapability::Observe, ClientCapability::Prompt];
        assert!(has_capability(&granted, ClientCapability::Observe));
        assert!(has_capability(&granted, ClientCapability::Prompt));
        assert!(!has_capability(&granted, ClientCapability::Configure));
        assert!(!has_capability(&granted, ClientCapability::Interrupt));
        assert!(!has_capability(&granted, ClientCapability::Stop));
        assert!(!has_capability(&granted, ClientCapability::ApproveOnce));
        assert!(!has_capability(&granted, ClientCapability::ApproveAlways));
    }

    #[test]
    fn context_usage_uses_effective_input_budget_and_clamps() {
        let usage = ContextUsage::new(100, Some(200), 20, Some(160));
        assert_eq!(usage.effective_input_budget, Some(180));
        assert_eq!(usage.utilization_basis_points, Some(5_555));
        assert_eq!(usage.compaction_high_water_tokens, Some(160));
        assert!(!usage.estimated);
        let estimated = usage.clone().mark_estimated();
        assert!(estimated.estimated);
        assert_eq!(
            serde_json::from_value::<ContextUsage>(serde_json::to_value(&estimated).unwrap())
                .unwrap(),
            estimated
        );

        let over = ContextUsage::new(300, Some(200), 20, None);
        assert_eq!(over.utilization_basis_points, Some(10_000));

        let unknown = ContextUsage::new(100, None, 20, None);
        assert_eq!(unknown.effective_input_budget, None);
        assert_eq!(unknown.utilization_basis_points, None);
        assert_eq!(unknown.budget_error, None);

        let invalid = ContextUsage::new(100, Some(20), 20, None);
        assert_eq!(invalid.effective_input_budget, None);
        assert_eq!(
            invalid.budget_error,
            Some(ContextBudgetError::OutputReservationNotBelowWindow)
        );
    }

    #[test]
    fn runtime_option_validates_select_boolean_and_integer_values() {
        let mode = RuntimeOption::select(
            "mode",
            "Mode",
            RuntimeValue::String("agent".to_string()),
            vec![
                RuntimeChoice::new("agent", "Agent"),
                RuntimeChoice::new("plan", "Plan"),
                RuntimeChoice::new("ask", "Ask"),
            ],
        );
        assert!(mode.accepts(&RuntimeValue::String("plan".to_string())));
        assert!(!mode.accepts(&RuntimeValue::String("unknown".to_string())));
        assert!(!mode.accepts(&RuntimeValue::Bool(true)));

        let toggle = RuntimeOption::boolean("fast", "Fast", RuntimeValue::Bool(false));
        assert!(toggle.accepts(&RuntimeValue::Bool(true)));
        assert!(!toggle.accepts(&RuntimeValue::Integer(1)));

        let budget = RuntimeOption::integer(
            "context_budget",
            "Context budget",
            RuntimeValue::Integer(100_000),
            16_000,
            200_000,
            1_000,
        );
        assert!(budget.accepts(&RuntimeValue::Integer(200_000)));
        assert!(!budget.accepts(&RuntimeValue::Integer(200_001)));
        assert!(!budget.accepts(&RuntimeValue::Integer(16_500)));
    }

    #[test]
    fn malformed_integer_option_never_panics_or_overflows() {
        let zero_step: RuntimeOption = serde_json::from_value(json!({
            "id": "budget",
            "label": "Budget",
            "value": 0,
            "default_value": 0,
            "spec": { "type": "integer", "min": -9223372036854775808_i64, "max": 9223372036854775807_i64, "step": 0 },
            "mutable_while_running": false,
            "help": null
        }))
        .unwrap();
        assert!(!zero_step.accepts(&RuntimeValue::Integer(0)));

        let huge_step: RuntimeOption = serde_json::from_value(json!({
            "id": "budget",
            "label": "Budget",
            "value": -9223372036854775808_i64,
            "default_value": -9223372036854775808_i64,
            "spec": { "type": "integer", "min": -9223372036854775808_i64, "max": 9223372036854775807_i64, "step": 2 },
            "mutable_while_running": false,
            "help": null
        }))
        .unwrap();
        assert!(!huge_step.accepts(&RuntimeValue::Integer(9223372036854775807_i64)));
    }

    #[test]
    fn snapshot_round_trip_carries_canonical_session_state() {
        let snapshot = SessionSnapshot {
            session_id: "s1".to_string(),
            seq: 9,
            turn_status: TurnStatus::Idle,
            transcript: vec![TranscriptEntry {
                id: 1,
                role: TranscriptRole::Assistant,
                text: "done".to_string(),
                outcome: Some(AssistantOutcome::Completed),
            }],
            tool_calls: vec![ToolCallState {
                id: "t1".to_string(),
                name: "exec".to_string(),
                title: "cargo test".to_string(),
                status: ToolCallStateStatus::Completed,
                output: Some("ok".to_string()),
            }],
            pending_approvals: vec![],
            runtime_options: vec![],
            context_usage: Some(ContextUsage::new(50, Some(200), 0, None)),
            history_truncated: false,
        };
        let message = ServerMessage::Snapshot {
            seq: snapshot.seq,
            state: snapshot,
        };
        let encoded = serde_json::to_string(&message).unwrap();
        assert_eq!(
            serde_json::from_str::<ServerMessage>(&encoded).unwrap(),
            message
        );
    }

    #[test]
    fn unassigned_approval_request_makes_broker_owned_id_explicit() {
        let request = ApprovalRequest::unassigned("tool-1", "exec", "run tests", true);
        assert!(request.id.is_empty());
        assert_eq!(request.tool_call_id, "tool-1");
        assert_eq!(request.tool, "exec");
        assert!(request.allow_always_available);
    }

    #[test]
    fn tool_start_event_projects_summary_not_raw_arguments() {
        let event = SessionEvent::ToolCallStarted {
            id: "t1".to_string(),
            name: "exec".to_string(),
            title: "run command".to_string(),
            input_summary: Some("shell command".to_string()),
        };
        let encoded = serde_json::to_string(&event).unwrap();
        assert!(encoded.contains("shell command"));
        assert!(!encoded.contains("super-secret-token"));
        assert!(!encoded.contains("raw_input"));

        let explicit_output = SessionEvent::ToolCallFinished {
            id: "t1".to_string(),
            status: ToolCallStateStatus::Completed,
            output: "explicit tool output".to_string(),
        };
        assert!(serde_json::to_string(&explicit_output)
            .unwrap()
            .contains("explicit tool output"));
    }

    #[test]
    fn event_sequences_must_strictly_advance() {
        assert!(validate_next_sequence(None, 1).is_ok());
        assert!(validate_next_sequence(Some(7), 8).is_ok());
        assert_eq!(
            validate_next_sequence(Some(7), 7),
            Err(SequenceError::NotIncreasing {
                previous: 7,
                next: 7,
            })
        );
        assert_eq!(
            validate_next_sequence(Some(7), 6),
            Err(SequenceError::NotIncreasing {
                previous: 7,
                next: 6,
            })
        );
    }

    #[test]
    fn protocol_limits_reject_oversized_untrusted_fields() {
        let limits = ProtocolLimits {
            max_frame_bytes: 1_024,
            max_prompt_bytes: 8,
            max_label_bytes: 6,
            max_identifier_bytes: 4,
            max_ticket_bytes: 6,
            max_runtime_value_bytes: 5,
            max_capabilities: 3,
        };
        assert!(limits
            .validate_client_message(&ClientMessage::Prompt {
                request_id: "p1".to_string(),
                text: "12345678".to_string(),
            })
            .is_ok());
        assert_eq!(
            limits.validate_client_message(&ClientMessage::Prompt {
                request_id: "p1".to_string(),
                text: "123456789".to_string(),
            }),
            Err(ProtocolValidationError::FieldTooLarge {
                field: "prompt.text",
                max_bytes: 8,
            })
        );
        assert_eq!(
            limits.validate_client_message(&ClientMessage::Attach {
                protocol_version: PROTOCOL_VERSION,
                session_id: None,
                ticket: None,
                client: ClientInfo {
                    id: "id".to_string(),
                    kind: ClientKind::Headless,
                    label: "1234567".to_string(),
                },
                requested_capabilities: vec![ClientCapability::Observe],
            }),
            Err(ProtocolValidationError::FieldTooLarge {
                field: "attach.client.label",
                max_bytes: 6,
            })
        );
    }

    #[test]
    fn protocol_limits_cover_all_untrusted_identifiers_tickets_and_values() {
        let limits = ProtocolLimits {
            max_frame_bytes: 1_024,
            max_prompt_bytes: 64,
            max_label_bytes: 64,
            max_identifier_bytes: 4,
            max_ticket_bytes: 6,
            max_runtime_value_bytes: 5,
            max_capabilities: 2,
        };
        let too_large = |field| ProtocolValidationError::FieldTooLarge {
            field,
            max_bytes: 4,
        };

        assert_eq!(
            limits.validate_client_message(&ClientMessage::Prompt {
                request_id: "12345".to_string(),
                text: "ok".to_string(),
            }),
            Err(too_large("prompt.request_id"))
        );
        assert_eq!(
            limits.validate_client_message(&ClientMessage::ApprovalResponse {
                approval_id: "12345".to_string(),
                decision: ApprovalDecision::Deny,
            }),
            Err(too_large("approval_response.approval_id"))
        );
        assert_eq!(
            limits.validate_client_message(&ClientMessage::Interrupt {
                request_id: Some("12345".to_string()),
            }),
            Err(too_large("interrupt.request_id"))
        );
        assert_eq!(
            limits.validate_client_message(&ClientMessage::SetConfig {
                request_id: None,
                config_id: "12345".to_string(),
                value: RuntimeValue::Bool(true),
            }),
            Err(too_large("set_config.config_id"))
        );
        assert_eq!(
            limits.validate_client_message(&ClientMessage::SetConfig {
                request_id: None,
                config_id: "mode".to_string(),
                value: RuntimeValue::String("123456".to_string()),
            }),
            Err(ProtocolValidationError::FieldTooLarge {
                field: "set_config.value",
                max_bytes: 5,
            })
        );
        assert_eq!(
            limits.validate_client_message(&ClientMessage::Attach {
                protocol_version: PROTOCOL_VERSION,
                session_id: None,
                ticket: Some("1234567".to_string()),
                client: ClientInfo {
                    id: "id".to_string(),
                    kind: ClientKind::Android,
                    label: "phone".to_string(),
                },
                requested_capabilities: vec![ClientCapability::Observe],
            }),
            Err(ProtocolValidationError::FieldTooLarge {
                field: "attach.ticket",
                max_bytes: 6,
            })
        );
        assert_eq!(
            limits.validate_client_message(&ClientMessage::Attach {
                protocol_version: PROTOCOL_VERSION,
                session_id: Some("12345".to_string()),
                ticket: None,
                client: ClientInfo {
                    id: "id".to_string(),
                    kind: ClientKind::Android,
                    label: "phone".to_string(),
                },
                requested_capabilities: vec![ClientCapability::Observe],
            }),
            Err(too_large("attach.session_id"))
        );
        assert_eq!(
            limits.validate_client_message(&ClientMessage::Attach {
                protocol_version: PROTOCOL_VERSION,
                session_id: None,
                ticket: None,
                client: ClientInfo {
                    id: "12345".to_string(),
                    kind: ClientKind::Android,
                    label: "phone".to_string(),
                },
                requested_capabilities: vec![ClientCapability::Observe],
            }),
            Err(too_large("attach.client.id"))
        );
        assert_eq!(
            limits.validate_client_message(&ClientMessage::Attach {
                protocol_version: PROTOCOL_VERSION,
                session_id: None,
                ticket: None,
                client: ClientInfo {
                    id: "id".to_string(),
                    kind: ClientKind::Android,
                    label: "phone".to_string(),
                },
                requested_capabilities: vec![
                    ClientCapability::Observe,
                    ClientCapability::Prompt,
                    ClientCapability::Interrupt,
                ],
            }),
            Err(ProtocolValidationError::TooManyCapabilities { max: 2 })
        );
    }

    #[test]
    fn android_v2_contract_fixtures_match_canonical_wire_types() {
        let attach: ClientMessage =
            serde_json::from_str(include_str!("../contracts/android/v2/attach_request.json"))
                .unwrap();
        assert!(matches!(
            attach,
            ClientMessage::Attach {
                protocol_version: PROTOCOL_VERSION,
                client: ClientInfo {
                    kind: ClientKind::Android,
                    ..
                },
                ..
            }
        ));

        let snapshot: ServerMessage =
            serde_json::from_str(include_str!("../contracts/android/v2/snapshot.json")).unwrap();
        assert!(matches!(
            &snapshot,
            ServerMessage::Snapshot {
                seq: 4,
                state: SessionSnapshot {
                    turn_status: TurnStatus::WaitingForApproval,
                    ..
                }
            }
        ));

        let events: Vec<ServerMessage> =
            serde_json::from_str(include_str!("../contracts/android/v2/event_stream.json"))
                .unwrap();
        assert_eq!(events.len(), 5);
        assert!(events
            .iter()
            .all(|message| matches!(message, ServerMessage::Event { .. })));
        let ServerMessage::Snapshot { state, .. } = snapshot else {
            unreachable!("snapshot fixture");
        };
        let mut view = crate::frontend_state::ViewState::new("fixture");
        view.apply_snapshot(state);
        for message in events {
            let ServerMessage::Event { seq, event } = message else {
                unreachable!("event fixture");
            };
            assert_eq!(
                view.apply_event(seq, event),
                crate::frontend_state::ApplyOutcome::Applied
            );
        }
        assert!(view.pending_approvals().is_empty());
        assert_eq!(view.tool_calls()[0].status, ToolCallStateStatus::Completed);

        let commands: Vec<ClientMessage> =
            serde_json::from_str(include_str!("../contracts/android/v2/client_commands.json"))
                .unwrap();
        assert_eq!(commands.len(), 6);
        assert!(matches!(commands.last(), Some(ClientMessage::Detach)));
    }
}
