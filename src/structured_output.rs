//! Structured final output: validated terminal calls as model-side run endings
//! (ADR-024, WP05).
//!
//! Context: docs/contexts/reviewer-runtime/CONTEXT.md and
//! docs/adr/024-validated-terminal-output.md.
//!
//! The caller designates one final-answer tool name and supplies its schema.
//! A turn ends the run when it is not an ordinary evidence step: either there
//! are no tool calls, the designated final-answer call appears, or the endpoint
//! failed. Success is exactly one call to the designated name whose arguments
//! validate against the caller schema (text/reasoning alongside it is allowed).
//! A mixed ending executes none of its calls and pairs every call with an
//! ordered error result. Corrective retries are bounded independently from the
//! inclusive model-request count. Model-facing corrective wording lives in
//! `prompts/final_output.md`, never in Rust literals here.
//!
//! Exit-code contract (reviewer-runtime INTERFACES.md): success exit 0
//! (argument JSON + newline on stdout), a typed final-output failure exit 20,
//! and the inclusive model-request ceiling exit 24 with reason `turn_limit`
//! (PM FR-01 / ADR-024). The CLI maps each [`FinalOutputOutcome`] variant.

use serde_json::{json, Value};
use std::path::Path;

use crate::providers::{ContentBlock, LlmResponse, Message, Role, StopReason, ToolSchema};
use crate::schema_validation::{CompiledSchema, SchemaError, DEFAULT_MAX_VALIDATION_ERRORS};

/// Embedded model-facing corrective text. The `.md` file owns the wording; the
/// runtime only appends concrete reasons and validation errors as data.
pub const FINAL_OUTPUT_PROMPT: &str = include_str!("../prompts/final_output.md");

/// Required FR-01 structured-failure reasons. Never collapsed into free text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalFailureReason {
    NoFinalCall,
    MixedCalls,
    InvalidArguments,
    RetriesExhausted,
    EndpointError,
    SchemaUnusable,
}

impl FinalFailureReason {
    pub fn as_str(self) -> &'static str {
        match self {
            FinalFailureReason::NoFinalCall => "no_final_call",
            FinalFailureReason::MixedCalls => "mixed_calls",
            FinalFailureReason::InvalidArguments => "invalid_arguments",
            FinalFailureReason::RetriesExhausted => "retries_exhausted",
            FinalFailureReason::EndpointError => "endpoint_error",
            FinalFailureReason::SchemaUnusable => "schema_unusable",
        }
    }
}

/// One failed terminal ending: its reason plus the last validation errors.
#[derive(Debug, Clone)]
pub struct FinalAttemptFailure {
    pub reason: FinalFailureReason,
    pub validation_errors: Vec<String>,
}

impl FinalAttemptFailure {
    fn new(reason: FinalFailureReason, validation_errors: Vec<String>) -> Self {
        FinalAttemptFailure {
            reason,
            validation_errors,
        }
    }
}

/// How one classified turn relates to the run's ending.
#[derive(Debug)]
pub enum TurnClassification {
    /// Ordinary tool calls only (no final-answer call, no endpoint error).
    /// Consumes a model request but never a corrective retry.
    Evidence,
    /// Exactly one valid final-answer call. The run ends successfully.
    Success { arguments: Value },
    /// A terminal ending that failed. For a mixed ending, `error_results`
    /// carries one ordered error result per requested call (execute none).
    Failure {
        failure: FinalAttemptFailure,
        error_results: Vec<ContentBlock>,
    },
}

/// Terminal decision after accounting a failed ending against the retry bound.
#[derive(Debug)]
pub enum FailureDisposition {
    /// A corrective retry is permitted; send the corrective message and
    /// continue (subject to the independent model-request ceiling).
    Retry(FinalAttemptFailure),
    /// No further ending is permitted. Report the terminal failure JSON.
    Fail(FinalAttemptFailure),
}

/// Immutable per-run final-output configuration with the schema compiled once.
pub struct FinalOutputSpec {
    tool_name: String,
    tool_description: String,
    schema: Value,
    validator: CompiledSchema,
    corrective_retries: u32,
    max_model_requests: Option<u32>,
    max_validation_errors: usize,
}

impl FinalOutputSpec {
    /// Compile a spec, validating the caller schema once. A schema that is
    /// invalid or needs external retrieval becomes a [`FinalAttemptFailure`]
    /// with [`FinalFailureReason::SchemaUnusable`] and no I/O performed.
    pub fn compile(
        tool_name: impl Into<String>,
        tool_description: impl Into<String>,
        schema: Value,
        corrective_retries: u32,
        max_model_requests: Option<u32>,
    ) -> Result<FinalOutputSpec, FinalAttemptFailure> {
        let validator = CompiledSchema::compile(&schema).map_err(|e| {
            let detail = match &e {
                SchemaError::ExternalReference(m) | SchemaError::Invalid(m) => m.clone(),
            };
            FinalAttemptFailure::new(FinalFailureReason::SchemaUnusable, vec![detail])
        })?;
        Ok(FinalOutputSpec {
            tool_name: tool_name.into(),
            tool_description: tool_description.into(),
            schema,
            validator,
            corrective_retries,
            max_model_requests,
            max_validation_errors: DEFAULT_MAX_VALIDATION_ERRORS,
        })
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    /// Accessors for the independent bounds. Not needed by the current loop
    /// wiring (the counters live in `FinalOutputState`); kept for tests and
    /// future auditing callers.
    #[allow(dead_code)]
    pub fn corrective_retries(&self) -> u32 {
        self.corrective_retries
    }

    #[allow(dead_code)]
    pub fn max_model_requests(&self) -> Option<u32> {
        self.max_model_requests
    }

    /// The final-answer tool offered to the model with the caller schema
    /// **unchanged** (no subset rewriting, no provider strict flag).
    pub fn tool_schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.tool_name.clone(),
            description: self.tool_description.clone(),
            input_schema: self.schema.clone(),
        }
    }

    /// A fresh counter state for one run.
    pub fn new_state(&self) -> FinalOutputState {
        FinalOutputState {
            corrective_retries: self.corrective_retries,
            max_model_requests: self.max_model_requests,
            final_attempts: 0,
            model_requests: 0,
        }
    }

    fn is_final_name(&self, name: &str) -> bool {
        name == self.tool_name
    }

    /// Classify one model response relative to the final-output contract.
    /// Pure: it touches no counters.
    pub fn classify(&self, response: &LlmResponse) -> TurnClassification {
        if response.stop_reason == StopReason::Error {
            let detail = response
                .error_message
                .clone()
                .unwrap_or_else(|| "endpoint error".to_string());
            return TurnClassification::Failure {
                failure: FinalAttemptFailure::new(FinalFailureReason::EndpointError, vec![detail]),
                error_results: Vec::new(),
            };
        }

        let calls: Vec<(&str, &str, &Value)> = response
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall { id, name, input } => {
                    Some((id.as_str(), name.as_str(), input))
                }
                _ => None,
            })
            .collect();

        let final_count = calls
            .iter()
            .filter(|(_, name, _)| self.is_final_name(name))
            .count();

        if final_count == 0 {
            if calls.is_empty() {
                // No calls at all: a text-only ending.
                return TurnClassification::Failure {
                    failure: FinalAttemptFailure::new(FinalFailureReason::NoFinalCall, Vec::new()),
                    error_results: Vec::new(),
                };
            }
            // Ordinary tool calls only: a normal evidence step.
            return TurnClassification::Evidence;
        }

        // A clean success is exactly one final call and no other calls.
        if final_count == 1 && calls.len() == 1 {
            let (id, _, input) = calls[0];
            let errors = self.validator.validate(input, self.max_validation_errors);
            if errors.is_empty() {
                return TurnClassification::Success {
                    arguments: input.clone(),
                };
            }
            // FR-01 req 6: a failed ending that contains tool calls answers
            // every call. The single invalid final-answer call is paired with
            // an ordered error result so a corrective turn is wire-valid on
            // both endpoints (ADR-024 strict unanswered-call rule) instead of
            // leaving an orphan `tool_use`/`tool_call`.
            let invalid_text = prompt_section(FinalFailureReason::InvalidArguments.as_str());
            let content = if invalid_text.is_empty() {
                FinalFailureReason::InvalidArguments.as_str().to_string()
            } else {
                invalid_text
            };
            return TurnClassification::Failure {
                failure: FinalAttemptFailure::new(FinalFailureReason::InvalidArguments, errors),
                error_results: vec![ContentBlock::ToolResult {
                    tool_use_id: id.to_string(),
                    content,
                    is_error: true,
                }],
            };
        }

        // Mixed ending: the final call appears alongside other calls (or more
        // than once). Execute none; pair every call with an ordered error so
        // the wire's unanswered-call rule is satisfied on any corrective turn.
        let mixed_text = prompt_section("mixed_calls");
        let error_results = calls
            .iter()
            .map(|(id, _, _)| ContentBlock::ToolResult {
                tool_use_id: (*id).to_string(),
                content: mixed_text.clone(),
                is_error: true,
            })
            .collect();
        TurnClassification::Failure {
            failure: FinalAttemptFailure::new(FinalFailureReason::MixedCalls, Vec::new()),
            error_results,
        }
    }

    /// The corrective user message for a failed ending: the prompt guidance
    /// plus the concrete reason and validation errors appended as data. The
    /// caller prepends any `error_results` blocks so one wire-valid user turn
    /// answers every unexecuted call and restates the final-answer rule.
    pub fn corrective_message(&self, failure: &FinalAttemptFailure) -> Message {
        let mut body = prompt_section("retry");
        body.push_str("\n- reason: ");
        body.push_str(failure.reason.as_str());
        let reason_hint = prompt_section(failure.reason.as_str());
        if !reason_hint.is_empty() {
            body.push('\n');
            body.push_str(&reason_hint);
        }
        for err in &failure.validation_errors {
            body.push_str("\n- ");
            body.push_str(err);
        }
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text(body)],
        }
    }
}

/// Independent run counters: candidate endings, corrective retries, and the
/// inclusive model-request ceiling.
#[derive(Debug, Clone)]
pub struct FinalOutputState {
    corrective_retries: u32,
    max_model_requests: Option<u32>,
    final_attempts: u32,
    model_requests: u32,
}

impl FinalOutputState {
    /// Counter accessors. The failure/limit JSON carries these for the caller;
    /// the accessors back assertions in tests and future auditing callers.
    #[allow(dead_code)]
    pub fn final_attempts(&self) -> u32 {
        self.final_attempts
    }

    #[allow(dead_code)]
    pub fn model_requests(&self) -> u32 {
        self.model_requests
    }

    /// True when another model request (ordinary, probe, or transport retry)
    /// fits under the inclusive ceiling. A request-limit refusal sends nothing.
    pub fn can_request(&self) -> bool {
        match self.max_model_requests {
            Some(max) => self.model_requests < max,
            None => true,
        }
    }

    /// Record one HTTP attempt. Increment before every request, including the
    /// probe and explicit transport retries.
    pub fn record_request(&mut self) {
        self.model_requests = self.model_requests.saturating_add(1);
    }

    /// Account a failed ending against the corrective-retry bound.
    ///
    /// `corrective_retries` counts *extra* endings after the first, so the run
    /// permits `corrective_retries + 1` endings. When retries remain the caller
    /// should still confirm [`FinalOutputState::can_request`] before sending the
    /// corrective turn.
    pub fn register_failure(&mut self, failure: FinalAttemptFailure) -> FailureDisposition {
        self.final_attempts = self.final_attempts.saturating_add(1);
        if self.final_attempts <= self.corrective_retries {
            FailureDisposition::Retry(failure)
        } else if self.corrective_retries == 0 {
            // A single permitted ending: report its actual reason.
            FailureDisposition::Fail(failure)
        } else {
            // Corrective retries were available and are now spent.
            FailureDisposition::Fail(FinalAttemptFailure::new(
                FinalFailureReason::RetriesExhausted,
                failure.validation_errors,
            ))
        }
    }

    /// The bounded terminal failure JSON: `reason`, `validation_errors`,
    /// `attempts`, and `model_requests`.
    pub fn failure_json(&self, failure: &FinalAttemptFailure) -> Value {
        json!({
            "reason": failure.reason.as_str(),
            "validation_errors": failure.validation_errors,
            "attempts": self.final_attempts,
            "model_requests": self.model_requests,
        })
    }

    /// The fallback terminal JSON for a run that exits the loop without an
    /// explicit final-output decision (e.g. a `MaxTokens` evidence turn with
    /// auto-continue disabled). Reports the run's REAL counters with reason
    /// `no_final_call` so the caller never sees `attempts: 0, model_requests: 0`
    /// after requests were actually sent (FR-01 req 8).
    pub fn no_final_call_json(&self) -> Value {
        json!({
            "reason": "no_final_call",
            "validation_errors": [],
            "attempts": self.final_attempts,
            "model_requests": self.model_requests,
        })
    }

    /// The bounded request-limit JSON when the inclusive model-request ceiling
    /// is reached before a valid ending. Distinct terminal reason (exit 24).
    /// The reason string is `turn_limit` per PM FR-01 and ADR-024; the exit
    /// code 24 is the ledger's `model_request_limit`.
    pub fn request_limit_json(&self) -> Value {
        json!({
            "reason": "turn_limit",
            "attempts": self.final_attempts,
            "model_requests": self.model_requests,
        })
    }
}

/// The terminal decision a final-output run carries out of the agent loop. The
/// caller maps each variant to the reviewed stdout/stderr + exit-code contract.
#[derive(Debug, Clone)]
pub enum FinalOutputOutcome {
    /// Exactly one valid final answer. `String` is the exact success stdout.
    /// Exit 0.
    Success(String),
    /// A terminal final-output failure. `Value` is the bounded failure JSON.
    /// Exit 20.
    Failure(Value),
    /// The inclusive model-request ceiling was reached. Exit 24.
    RequestLimit(Value),
}

/// A control-file / startup validation failure, carrying the reviewed exit code
/// and the exact bytes to emit on stderr before exiting.
#[derive(Debug, Clone)]
pub struct FinalOutputStartupError {
    pub exit_code: i32,
    /// Plain stderr text for a configuration error (exit 2).
    pub message: Option<String>,
    /// A JSON line for the final-output error shape (exit 20, schema_unusable).
    pub json: Option<Value>,
}

impl FinalOutputStartupError {
    fn config_invalid(message: impl Into<String>) -> Self {
        FinalOutputStartupError {
            exit_code: 2,
            message: Some(message.into()),
            json: None,
        }
    }
}

/// The reviewed final-output control file: `schema` XOR `schema_path`, the
/// designated tool name/description, and the independent retry/request bounds.
#[derive(serde::Deserialize)]
struct FinalOutputControl {
    #[serde(default)]
    schema: Option<Value>,
    #[serde(default)]
    schema_path: Option<String>,
    tool_name: String,
    #[serde(default)]
    tool_description: String,
    #[serde(default)]
    corrective_retries: u32,
    #[serde(default)]
    max_model_requests: Option<u32>,
}

/// Parse and compile a final-output control file. Enforces `schema` XOR
/// `schema_path` mutual exclusion at startup (exit 2) and compiles the caller
/// schema once before any provider work. A schema needing external retrieval or
/// one that is invalid becomes `schema_unusable` (exit 20) with no I/O.
pub fn load_control(
    path: &Path,
    full_schema_mode: bool,
) -> Result<FinalOutputSpec, FinalOutputStartupError> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        FinalOutputStartupError::config_invalid(format!(
            "final-output control file {}: {e}",
            path.display()
        ))
    })?;
    let control: FinalOutputControl = serde_json::from_str(&raw).map_err(|e| {
        FinalOutputStartupError::config_invalid(format!(
            "final-output control file {}: invalid JSON: {e}",
            path.display()
        ))
    })?;
    if control.tool_name.trim().is_empty() {
        return Err(FinalOutputStartupError::config_invalid(
            "final-output control file: tool_name must not be empty",
        ));
    }
    let schema = match (control.schema, control.schema_path) {
        (Some(_), Some(_)) => {
            return Err(FinalOutputStartupError::config_invalid(
                "final-output control file: schema and schema_path are mutually exclusive",
            ));
        }
        (None, None) => {
            return Err(FinalOutputStartupError::config_invalid(
                "final-output control file: exactly one of schema or schema_path is required",
            ));
        }
        (Some(schema), None) => schema,
        (None, Some(rel)) => {
            let schema_path = match path.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent.join(&rel),
                _ => Path::new(&rel).to_path_buf(),
            };
            let text = std::fs::read_to_string(&schema_path).map_err(|e| {
                FinalOutputStartupError::config_invalid(format!(
                    "final-output schema_path {}: {e}",
                    schema_path.display()
                ))
            })?;
            serde_json::from_str(&text).map_err(|e| {
                FinalOutputStartupError::config_invalid(format!(
                    "final-output schema_path {}: invalid JSON: {e}",
                    schema_path.display()
                ))
            })?
        }
    };
    // WP17 r2 (Gap 1, ADR-030): under full-schema mode a supplied final-answer
    // schema is still offered unchanged, but a local recursive `$ref` cycle is
    // refused here as `schema_unusable` before any model request, reusing the
    // schema_contract recursion detector. External references and non-2020-12
    // validity are already refused by `FinalOutputSpec::compile` below; the
    // definition-name collision rule is deliberately NOT applied to a supplied
    // schema (D39). Default off leaves WP05 behavior unchanged.
    if full_schema_mode {
        if let Err(err) =
            crate::schema_contract::reject_recursive_supplied_schema(&control.tool_name, &schema)
        {
            return Err(FinalOutputStartupError {
                exit_code: 20,
                message: None,
                json: Some(json!({
                    "reason": err.reason(),
                    "validation_errors": [err.to_string()],
                    "attempts": 0,
                    "model_requests": 0,
                })),
            });
        }
    }
    FinalOutputSpec::compile(
        control.tool_name,
        control.tool_description,
        schema,
        control.corrective_retries,
        control.max_model_requests,
    )
    .map_err(|failure| FinalOutputStartupError {
        exit_code: 20,
        message: None,
        json: Some(json!({
            "reason": failure.reason.as_str(),
            "validation_errors": failure.validation_errors,
            "attempts": 0,
            "model_requests": 0,
        })),
    })
}

/// The exact stdout bytes for a successful final answer: the validated argument
/// JSON and exactly one trailing newline. Nothing else is emitted on stdout.
pub fn success_stdout(arguments: &Value) -> String {
    let mut out = serde_json::to_string(arguments).unwrap_or_else(|_| "null".to_string());
    out.push('\n');
    out
}

/// Extract a `## <key>` section's body from [`FINAL_OUTPUT_PROMPT`], trimmed.
/// Returns an empty string when the section is absent.
fn prompt_section(key: &str) -> String {
    let mut collecting = false;
    let mut lines: Vec<&str> = Vec::new();
    for line in FINAL_OUTPUT_PROMPT.lines() {
        if let Some(name) = line.strip_prefix("## ") {
            if collecting {
                break;
            }
            collecting = name.trim() == key;
            continue;
        }
        if collecting {
            lines.push(line);
        }
    }
    lines.join("\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(corrective_retries: u32, max_model_requests: Option<u32>) -> FinalOutputSpec {
        FinalOutputSpec::compile(
            "final_answer",
            "Emit the final result.",
            json!({
                "type": "object",
                "properties": { "answer": { "type": "string" } },
                "required": ["answer"],
                "additionalProperties": false
            }),
            corrective_retries,
            max_model_requests,
        )
        .expect("schema compiles")
    }

    fn call(id: &str, name: &str, input: Value) -> ContentBlock {
        ContentBlock::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            input,
        }
    }

    fn response(content: Vec<ContentBlock>, stop_reason: StopReason) -> LlmResponse {
        LlmResponse {
            content,
            stop_reason,
            error_message: None,
            context_overflow: false,
            retryable: false,
            usage: Default::default(),
        }
    }

    #[test]
    fn ordinary_tool_calls_are_evidence() {
        let s = spec(1, None);
        let r = response(
            vec![call("1", "read_file", json!({ "path": "a.rs" }))],
            StopReason::ToolUse,
        );
        assert!(matches!(s.classify(&r), TurnClassification::Evidence));
    }

    #[test]
    fn single_valid_final_call_succeeds_with_text_alongside() {
        let s = spec(1, None);
        let r = response(
            vec![
                ContentBlock::Text("here it is".into()),
                call("1", "final_answer", json!({ "answer": "done" })),
            ],
            StopReason::ToolUse,
        );
        match s.classify(&r) {
            TurnClassification::Success { arguments } => {
                assert_eq!(arguments, json!({ "answer": "done" }));
                assert_eq!(success_stdout(&arguments), "{\"answer\":\"done\"}\n");
            }
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[test]
    fn invalid_arguments_are_a_failure() {
        let s = spec(1, None);
        let r = response(
            vec![call("1", "final_answer", json!({ "answer": 42 }))],
            StopReason::ToolUse,
        );
        match s.classify(&r) {
            TurnClassification::Failure {
                failure,
                error_results,
            } => {
                assert_eq!(failure.reason, FinalFailureReason::InvalidArguments);
                assert!(!failure.validation_errors.is_empty());
                // FR-01 req 6 / R1: the single invalid call is answered with an
                // ordered error result so a corrective turn is wire-valid.
                assert_eq!(error_results.len(), 1);
                match &error_results[0] {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        is_error,
                        content,
                    } => {
                        assert_eq!(tool_use_id, "1");
                        assert!(is_error);
                        assert!(!content.is_empty());
                    }
                    _ => panic!("expected a paired tool result"),
                }
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn no_final_call_json_reports_real_counters() {
        let mut state = spec(1, None).new_state();
        state.record_request();
        state.record_request();
        let v = state.no_final_call_json();
        assert_eq!(v["reason"], "no_final_call");
        assert_eq!(v["attempts"], 0);
        assert_eq!(v["model_requests"], 2);
        assert!(v["validation_errors"].is_array());
    }

    #[test]
    fn mixed_ending_pairs_every_call_with_error_and_executes_none() {
        let s = spec(1, None);
        let r = response(
            vec![
                call("a", "read_file", json!({ "path": "x" })),
                call("b", "final_answer", json!({ "answer": "done" })),
            ],
            StopReason::ToolUse,
        );
        match s.classify(&r) {
            TurnClassification::Failure {
                failure,
                error_results,
            } => {
                assert_eq!(failure.reason, FinalFailureReason::MixedCalls);
                assert_eq!(error_results.len(), 2);
                let ids: Vec<&str> = error_results
                    .iter()
                    .map(|b| match b {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            is_error,
                            ..
                        } => {
                            assert!(is_error);
                            tool_use_id.as_str()
                        }
                        _ => panic!("expected tool result"),
                    })
                    .collect();
                assert_eq!(ids, vec!["a", "b"]);
            }
            other => panic!("expected mixed failure, got {other:?}"),
        }
    }

    #[test]
    fn text_only_ending_is_no_final_call() {
        let s = spec(1, None);
        let r = response(
            vec![ContentBlock::Text("I think...".into())],
            StopReason::EndTurn,
        );
        match s.classify(&r) {
            TurnClassification::Failure { failure, .. } => {
                assert_eq!(failure.reason, FinalFailureReason::NoFinalCall);
            }
            other => panic!("expected no_final_call, got {other:?}"),
        }
    }

    #[test]
    fn endpoint_error_is_a_failure() {
        let s = spec(1, None);
        let mut r = response(Vec::new(), StopReason::Error);
        r.error_message = Some("boom".into());
        match s.classify(&r) {
            TurnClassification::Failure { failure, .. } => {
                assert_eq!(failure.reason, FinalFailureReason::EndpointError);
            }
            other => panic!("expected endpoint_error, got {other:?}"),
        }
    }

    #[test]
    fn zero_retries_reports_actual_reason() {
        let mut state = spec(0, None).new_state();
        let d = state.register_failure(FinalAttemptFailure::new(
            FinalFailureReason::InvalidArguments,
            vec!["bad".into()],
        ));
        match d {
            FailureDisposition::Fail(f) => {
                assert_eq!(f.reason, FinalFailureReason::InvalidArguments);
            }
            other => panic!("expected terminal fail, got {other:?}"),
        }
        assert_eq!(state.final_attempts(), 1);
    }

    #[test]
    fn exhausted_retries_upgrade_reason_and_preserve_errors() {
        let mut state = spec(1, None).new_state();
        // First ending: retry permitted.
        let first = state.register_failure(FinalAttemptFailure::new(
            FinalFailureReason::InvalidArguments,
            vec!["e1".into()],
        ));
        assert!(matches!(first, FailureDisposition::Retry(_)));
        // Second ending: retries now spent.
        let second = state.register_failure(FinalAttemptFailure::new(
            FinalFailureReason::InvalidArguments,
            vec!["e2".into()],
        ));
        match second {
            FailureDisposition::Fail(f) => {
                assert_eq!(f.reason, FinalFailureReason::RetriesExhausted);
                assert_eq!(f.validation_errors, vec!["e2".to_string()]);
            }
            other => panic!("expected terminal fail, got {other:?}"),
        }
        assert_eq!(state.final_attempts(), 2);
    }

    #[test]
    fn model_request_ceiling_is_inclusive() {
        let mut state = spec(1, Some(2)).new_state();
        assert!(state.can_request());
        state.record_request();
        assert!(state.can_request());
        state.record_request();
        assert!(!state.can_request());
    }

    #[test]
    fn request_limit_json_uses_turn_limit_reason() {
        let mut state = spec(1, Some(1)).new_state();
        state.record_request();
        let v = state.request_limit_json();
        assert_eq!(v["reason"], "turn_limit");
        assert_eq!(v["model_requests"], 1);
    }

    #[test]
    fn unusable_schema_fails_before_any_request() {
        let result = FinalOutputSpec::compile(
            "final_answer",
            "x",
            json!({ "$ref": "https://example.com/remote.json" }),
            1,
            None,
        );
        match result {
            Err(f) => assert_eq!(f.reason, FinalFailureReason::SchemaUnusable),
            Ok(_) => panic!("expected schema_unusable"),
        }
    }

    #[test]
    fn failure_json_carries_counters() {
        let mut state = spec(0, None).new_state();
        state.record_request();
        let d = state.register_failure(FinalAttemptFailure::new(
            FinalFailureReason::NoFinalCall,
            vec![],
        ));
        if let FailureDisposition::Fail(f) = d {
            let v = state.failure_json(&f);
            assert_eq!(v["reason"], "no_final_call");
            assert_eq!(v["attempts"], 1);
            assert_eq!(v["model_requests"], 1);
            assert!(v["validation_errors"].is_array());
        } else {
            panic!("expected fail");
        }
    }

    #[test]
    fn corrective_message_includes_reason_and_errors() {
        let s = spec(1, None);
        let msg = s.corrective_message(&FinalAttemptFailure::new(
            FinalFailureReason::InvalidArguments,
            vec!["/answer: wrong type".into()],
        ));
        assert_eq!(msg.role, Role::User);
        let text = match &msg.content[0] {
            ContentBlock::Text(t) => t,
            _ => panic!("expected text"),
        };
        assert!(text.contains("invalid_arguments"));
        assert!(text.contains("/answer: wrong type"));
        assert!(!text.is_empty());
    }

    #[test]
    fn load_control_rejects_mutually_exclusive_schema_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.json");
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "schema": { "type": "object" },
                "schema_path": "other.json",
                "tool_name": "final_answer"
            }))
            .unwrap(),
        )
        .unwrap();
        let err = match load_control(&path, false) {
            Ok(_) => panic!("expected mutual-exclusion error"),
            Err(e) => e,
        };
        assert_eq!(err.exit_code, 2);
    }

    #[test]
    fn load_control_external_reference_is_exit_20_schema_unusable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.json");
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "schema": { "$ref": "https://example.com/remote.json" },
                "tool_name": "final_answer"
            }))
            .unwrap(),
        )
        .unwrap();
        let err = match load_control(&path, false) {
            Ok(_) => panic!("expected external-reference error"),
            Err(e) => e,
        };
        assert_eq!(err.exit_code, 20);
        assert_eq!(err.json.unwrap()["reason"], "schema_unusable");
    }
}
