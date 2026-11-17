//! Same-tool capability probe (FR-04 requirements 14-17, ADR-027, WP09).
//!
//! Context: docs/contexts/reviewer-runtime/CONTEXT.md and
//! docs/adr/027-frozen-catalog-capability-probe.md.
//!
//! Before any evidence tool runs, Daimonos sends exactly ONE unretried probe
//! request that offers the byte-equivalent frozen run tool catalog (the FR-01
//! final-answer tool plus every tool the run will offer) and instructs the
//! model to call the final-answer tool once with a caller-supplied minimal
//! payload. Only one schema-valid call to that tool passes; every other
//! outcome (text only, the wrong tool, two or more calls, invalid arguments,
//! or an endpoint error — a 503, a 3xx redirect, or a timeout) yields the
//! distinct `model_unsupported` outcome and no further work. The probe never
//! executes a returned tool call.
//!
//! The caller's minimal payload is validated LOCALLY against the final schema
//! before anything is sent ([`load_payload`]); an invalid payload is a caller
//! configuration error, not a model failure. The probe reuses WP05's
//! [`FinalOutputSpec::classify`] as the single source of truth for what a valid
//! final ending is, so the probe and the run judge the final tool identically.
//! Model-facing wording lives entirely in `prompts/`: `capability_probe.md`
//! holds the system-turn framing and `capability_probe_directive.md` holds the
//! user-turn directive template. The runtime interpolates only the concrete
//! final-tool name and the payload into the directive as data; no model-facing
//! sentence is constructed from a Rust literal here.

use std::path::Path;

use serde_json::Value;

use crate::providers::{
    self, ContentBlock, Context, ControlledModelSpec, ControlledRequest, LlmResponse, Message,
    StopReason, ToolSchema,
};
use crate::structured_output::{FinalFailureReason, FinalOutputSpec, TurnClassification};

/// Embedded model-facing probe system prompt (the capability-check framing).
/// The `.md` file owns all of this wording; no part of it is built in Rust.
pub const PROBE_PROMPT: &str = include_str!("../prompts/capability_probe.md");

/// Embedded model-facing probe directive template (the probe's user turn). The
/// `.md` file owns the wording; the runtime interpolates only the concrete
/// final-tool name (`{tool_name}`) and the compact-JSON payload (`{payload}`)
/// as data. Trailing whitespace is trimmed so the recorded request is exact
/// regardless of a trailing newline in the file.
pub const PROBE_DIRECTIVE_TEMPLATE: &str = include_str!("../prompts/capability_probe_directive.md");

/// Everything needed to send the single controlled probe request: the five
/// identity values and request settings (cloned from the run's controlled
/// profile, with `request_retries` forced to 0 so the probe is exactly one
/// attempt regardless of the run's retry setting — ADR-027), the resolved
/// credential (never logged), and the caller's locally validated payload.
pub struct ProbeSpec {
    pub spec: ControlledModelSpec,
    pub request: ControlledRequest,
    pub credential: String,
    pub payload: Value,
}

/// The probe verdict. A pass is exactly one schema-valid call to the final
/// tool; every other outcome is `model_unsupported` with a stable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    Passed,
    ModelUnsupported { reason: &'static str },
}

impl ProbeOutcome {
    pub fn passed(&self) -> bool {
        matches!(self, ProbeOutcome::Passed)
    }

    /// The audit `probe_result.status` string (WP08 `record_probe_result`).
    pub fn audit_status(&self) -> &'static str {
        match self {
            ProbeOutcome::Passed => "model_supported",
            ProbeOutcome::ModelUnsupported { .. } => "model_unsupported",
        }
    }

    /// The audit `probe_result.reason`, present only on a failure.
    pub fn audit_reason(&self) -> Option<&'static str> {
        match self {
            ProbeOutcome::Passed => None,
            ProbeOutcome::ModelUnsupported { reason } => Some(reason),
        }
    }
}

/// Send the single unretried probe and classify the response. `tools` is the
/// run's already-frozen, byte-equivalent tool catalog (the final-answer tool
/// plus every tool the run will offer); no dummy substitute is allowed
/// (ADR-027). Never executes a returned call. Reuses the run's controlled
/// transport ([`providers::controlled_send`]); a 503, a 3xx redirect, or a
/// timeout surfaces as `StopReason::Error` and classifies as `endpoint_error`.
pub async fn run_probe(
    probe: &ProbeSpec,
    tools: &[ToolSchema],
    final_spec: &FinalOutputSpec,
    audit: Option<&crate::run_audit::RunAudit>,
) -> ProbeOutcome {
    let payload = serde_json::to_string(&probe.payload).unwrap_or_else(|_| "{}".to_string());
    // The wording is owned by `prompts/capability_probe_directive.md`; only the
    // tool name and the payload are interpolated as data (R1). The template is
    // rendered in a SINGLE pass ([`render_directive`]) so neither value is ever
    // rescanned: a tool name that is literally `{payload}`, or a payload that
    // contains `{tool_name}`, is copied verbatim (r5).
    let directive = render_directive(
        PROBE_DIRECTIVE_TEMPLATE.trim_end(),
        final_spec.tool_name(),
        &payload,
    );
    let ctx = Context {
        messages: vec![Message::user(directive)],
        system: Some(PROBE_PROMPT.to_string()),
        tools: tools.to_vec(),
        stable_prefix_len: 0,
    };
    let mut obs = providers::ControlledObservation::default();
    let response = providers::controlled_send(
        &probe.spec,
        &probe.request,
        &probe.credential,
        &ctx,
        &mut obs,
    )
    .await;
    let outcome = evaluate(final_spec, &response);
    // WP08 audit (reqs 18-19): record the single probe outcome. A write failure
    // poisons the sink; the frontend's audit finalize surfaces it as exit 23.
    if let Some(audit) = audit {
        // WP09 (G4): record the probe's own request metadata on the probe_result
        // event (the probe is one request to the model endpoint).
        let _ = audit.record_probe_result(
            outcome.audit_status(),
            outcome.audit_reason(),
            obs.http_status,
            obs.retry_index,
            obs.wire,
        );
    }
    outcome
}

/// Render the probe directive template in ONE pass. Each `{tool_name}` and
/// `{payload}` placeholder in `template` is replaced exactly once with the
/// given value, and text that a replacement inserts is never scanned for a
/// further placeholder. This keeps the final-tool name and the payload as
/// opaque data: a tool name that is the literal text `{payload}`, or a payload
/// value that contains `{tool_name}`, is copied verbatim rather than
/// re-substituted (r5). Only these two placeholders are recognized; any other
/// `{...}` text in the template is literal.
fn render_directive(template: &str, tool_name: &str, payload: &str) -> String {
    let mut out = String::with_capacity(template.len() + tool_name.len() + payload.len());
    let mut rest = template;
    loop {
        let next_tool = rest.find("{tool_name}");
        let next_payload = rest.find("{payload}");
        let (pos, placeholder_len, value) = match (next_tool, next_payload) {
            (None, None) => {
                out.push_str(rest);
                return out;
            }
            (Some(t), None) => (t, "{tool_name}".len(), tool_name),
            (None, Some(p)) => (p, "{payload}".len(), payload),
            (Some(t), Some(p)) if t <= p => (t, "{tool_name}".len(), tool_name),
            (Some(_), Some(p)) => (p, "{payload}".len(), payload),
        };
        out.push_str(&rest[..pos]);
        out.push_str(value);
        rest = &rest[pos + placeholder_len..];
    }
}

/// Pure classification of one probe response. A pass is exactly one schema-valid
/// final-answer call (text/reasoning alongside it is allowed); every other
/// outcome maps to a stable `model_unsupported` reason.
fn evaluate(final_spec: &FinalOutputSpec, response: &LlmResponse) -> ProbeOutcome {
    match final_spec.classify(response) {
        TurnClassification::Success { .. } => ProbeOutcome::Passed,
        // Ordinary tool calls only: the model called a tool other than the
        // designated final-answer tool.
        TurnClassification::Evidence => ProbeOutcome::ModelUnsupported {
            reason: "wrong_tool",
        },
        TurnClassification::Failure { failure, .. } => {
            let reason = match failure.reason {
                FinalFailureReason::NoFinalCall => "text_only",
                FinalFailureReason::MixedCalls => "multiple_calls",
                FinalFailureReason::InvalidArguments => "invalid_arguments",
                FinalFailureReason::EndpointError => "endpoint_error",
                FinalFailureReason::RetriesExhausted | FinalFailureReason::SchemaUnusable => {
                    "endpoint_error"
                }
            };
            ProbeOutcome::ModelUnsupported { reason }
        }
    }
}

/// Load the caller's minimal probe payload and validate it LOCALLY against the
/// final-answer schema before any request is sent (ADR-027). Reuses the final
/// classifier on a synthetic single final-answer call so the payload is judged
/// by exactly the rule the run uses, with no network I/O and no change to the
/// WP05 public API. Returns the parsed payload, or a caller-facing message on
/// an unreadable file, invalid JSON, or a payload that does not validate.
pub fn load_payload(path: &Path, final_spec: &FinalOutputSpec) -> Result<Value, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("--capability-probe-payload {}: {e}", path.display()))?;
    let payload: Value = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "--capability-probe-payload {}: invalid JSON: {e}",
            path.display()
        )
    })?;
    let synthetic = LlmResponse {
        content: vec![ContentBlock::ToolCall {
            id: "probe-local".to_string(),
            name: final_spec.tool_name().to_string(),
            input: payload.clone(),
        }],
        stop_reason: StopReason::ToolUse,
        error_message: None,
        context_overflow: false,
        retryable: false,
        usage: Default::default(),
    };
    match final_spec.classify(&synthetic) {
        TurnClassification::Success { .. } => Ok(payload),
        _ => Err(format!(
            "--capability-probe-payload {}: does not validate against the final-answer schema",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec() -> FinalOutputSpec {
        FinalOutputSpec::compile(
            "final_answer",
            "Emit the final verdict.",
            json!({
                "type": "object",
                "properties": { "verdict": { "type": "string" } },
                "required": ["verdict"],
                "additionalProperties": false
            }),
            0,
            None,
        )
        .expect("schema compiles")
    }

    #[test]
    fn valid_payload_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.json");
        std::fs::write(
            &path,
            serde_json::to_string(&json!({"verdict": "ok"})).unwrap(),
        )
        .unwrap();
        let payload = load_payload(&path, &spec()).expect("valid payload");
        assert_eq!(payload, json!({"verdict": "ok"}));
    }

    #[test]
    fn payload_failing_schema_is_rejected_without_io() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.json");
        std::fs::write(
            &path,
            serde_json::to_string(&json!({"verdict": 42})).unwrap(),
        )
        .unwrap();
        let err = load_payload(&path, &spec()).unwrap_err();
        assert!(err.contains("does not validate"), "{err}");
    }

    #[test]
    fn render_directive_is_single_pass() {
        let template = "Call the tool `{tool_name}` exactly once with these arguments:\n{payload}";
        // An ordinary final-tool name gives exactly the current text.
        let ordinary = render_directive(template, "final_answer", "{\"verdict\":\"supported\"}");
        assert_eq!(
            ordinary,
            "Call the tool `final_answer` exactly once with these arguments:\n{\"verdict\":\"supported\"}"
        );
        // A single pass: a tool name that is the literal text `{payload}` stays
        // literal, and a payload that contains `{tool_name}` stays literal.
        // Neither replacement rescans the text the other inserts.
        let tricky = render_directive(template, "{payload}", "{\"a\":\"{tool_name}\"}");
        assert_eq!(
            tricky,
            "Call the tool `{payload}` exactly once with these arguments:\n{\"a\":\"{tool_name}\"}"
        );
    }

    #[test]
    fn non_json_payload_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.json");
        std::fs::write(&path, "not json {").unwrap();
        let err = load_payload(&path, &spec()).unwrap_err();
        assert!(err.contains("invalid JSON"), "{err}");
    }
}
