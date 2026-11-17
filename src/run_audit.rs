//! WP08 — caller-requested, privacy-preserving local run audit (FR-04, reqs
//! 18-20).
//!
//! An opt-in run (`--audit-path PATH`) writes one private JSON Lines file for
//! the run: a versioned object per lifecycle, request, tool/refusal, and
//! terminal-status event. The record is the caller's own run evidence, owned by
//! the run lifecycle — it is separate from optional SQLite analytics and from
//! optional OTLP export (ADR-006), and works without FR-01 (structured output)
//! or WP07 (controlled transport).
//!
//! # Privacy boundary (ADR-026, ADR-006 D6)
//!
//! Redaction happens here, at event construction — never "log then scrub". No
//! credential value, request/response body, full evidence content, forwarded
//! header, or raw endpoint error body may enter a record. A base URL is reduced
//! to its host (userinfo/port/path/query dropped). Tool arguments and results
//! are recorded only as SHA-256 digests of their compact, stable-key-ordered
//! JSON — never their contents. Only the fields listed in the reviewer-runtime
//! INTERFACES.md "Audit, output, and resource contracts" section are recorded.
//!
//! # Failure contract (ADR-026, fail-CLOSED)
//!
//! When audit is explicitly enabled it is required. An open/write/flush failure
//! aborts the run before further model/tool work and yields exit
//! [`EXIT_AUDIT_WRITE_FAILED`] (`audit_write_failed`). This is deliberately
//! distinct from ADR-006's optional fail-open OTLP policy. A diagnostic for an
//! audit failure never echoes a secret and never depends on the failed sink.
//!
//! Context: ADR-026 (caller-requested local audit), ADR-006 (privacy-first
//! observability), reviewer-runtime INTERFACES.md and INVENTORIES.md.

use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

/// Audit record schema version. Independent of the Cargo package version and of
/// the tool-contract version.
pub const AUDIT_VERSION: &str = "1";

/// Stable run code for a required-audit failure (reviewer-runtime INTERFACES.md
/// exit table).
pub const CODE_AUDIT_WRITE_FAILED: &str = "audit_write_failed";

/// Process exit code for a required-audit failure (reviewer-runtime
/// INTERFACES.md: CLI exit 23).
pub const EXIT_AUDIT_WRITE_FAILED: i32 = 23;

/// A required-audit failure. Carries a bounded, secret-free diagnostic and,
/// when available, the original run failure code as secondary metadata so an
/// audit-write failure that preempts another failure still preserves it
/// (INTERFACES.md: "Preserve the original failure code as bounded secondary
/// metadata when available").
#[derive(Debug, Clone)]
pub struct AuditFailure {
    pub code: &'static str,
    pub message: String,
    pub secondary_code: Option<String>,
}

impl AuditFailure {
    fn new(message: String) -> Self {
        Self {
            code: CODE_AUDIT_WRITE_FAILED,
            message,
            secondary_code: None,
        }
    }

    /// Attach the original run failure code (e.g. a provider error) as bounded
    /// secondary metadata.
    pub fn with_secondary(mut self, code: impl Into<String>) -> Self {
        self.secondary_code = Some(code.into());
        self
    }

    pub fn exit_code(&self) -> i32 {
        EXIT_AUDIT_WRITE_FAILED
    }

    /// A secret-free diagnostic object for stderr. It never contains a record
    /// payload and does not require the failed sink to work.
    pub fn diagnostic_json(&self) -> serde_json::Value {
        let mut error = serde_json::json!({
            "code": self.code,
            "message": self.message,
        });
        if let Some(secondary) = &self.secondary_code {
            error["secondary_code"] = serde_json::json!(secondary);
        }
        serde_json::json!({ "error": error })
    }
}

impl std::fmt::Display for AuditFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// Configured bounds for the audit sink (config group `reviewer.audit`). A
/// record that would exceed either bound is a required-audit failure, never a
/// silent truncation or unbounded allocation.
#[derive(Debug, Clone, Copy)]
pub struct AuditLimits {
    pub max_event_bytes: usize,
    pub max_events: u64,
}

/// The five run-configuration values and effective policy recorded in
/// `run_start`. Every field is a name, host, flag, or list — never a secret
/// value, full URL, or body. Unavailable metadata is explicit (`None`), never
/// invented (ADR-026).
#[derive(Debug, Clone, Default)]
pub struct RunAuditMeta {
    pub provider_label: Option<String>,
    pub requested_model: Option<String>,
    /// Only the host of the base URL (userinfo/port/path/query dropped).
    pub base_url_host: Option<String>,
    /// `chat` or `messages`; `None` until WP07 controlled transport supplies it.
    pub wire: Option<String>,
    /// The NAME of the environment variable holding the credential — never its
    /// value.
    pub credential_env: Option<String>,
    pub effective_allowlist: Option<Vec<String>>,
    pub effective_root: Option<String>,
    pub workspace_read_only: bool,
}

/// Reduce a base URL to its host alone, dropping scheme, userinfo, port, path,
/// query, and fragment (ADR-026: "redact credentials and URL
/// userinfo/path/query"). Returns `None` when no host can be extracted.
pub fn base_url_host(url: &str) -> Option<String> {
    let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    // Authority ends at the first '/', '?', or '#'.
    let authority = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme);
    // Drop userinfo (everything up to and including the last '@').
    let host_port = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    let host = if let Some(end) = host_port.strip_prefix('[') {
        // IPv6 literal: keep the bracketed address, drop any :port after it.
        end.split_once(']').map(|(addr, _)| addr).unwrap_or(end)
    } else {
        host_port.split(':').next().unwrap_or(host_port)
    };
    let host = host.trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

struct AuditInner {
    file: std::fs::File,
    sequence: u64,
    events_written: u64,
    /// Set once the first write/flush fails. Every subsequent record is refused
    /// so no further model/tool work proceeds under a broken sink.
    poisoned: Option<AuditFailure>,
}

/// The serialized, append-only JSONL audit stream for one run. A single
/// serialized sink prevents interleaved partial lines from concurrent tool
/// calls.
pub struct RunAudit {
    run_id: String,
    limits: AuditLimits,
    inner: Mutex<AuditInner>,
}

impl RunAudit {
    /// Open the requested destination with private (0600) permissions, refusing
    /// to append to an existing file, then write the terminal-safe `run_start`
    /// event. Any failure here is a required-audit failure that must abort the
    /// run before any model request (startup order item 6).
    ///
    /// `resolved_path` is the already-validated destination (outside evidence,
    /// and under the state root when an explicit state root is selected).
    pub fn open(
        resolved_path: &Path,
        run_id: String,
        limits: AuditLimits,
        meta: &RunAuditMeta,
    ) -> Result<Self, AuditFailure> {
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(parent) = resolved_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    AuditFailure::new(format!(
                        "--audit-path parent directory {} is unusable: {error}",
                        parent.display()
                    ))
                })?;
            }
        }
        // `create_new` => O_CREAT|O_EXCL: never silently append another run to
        // an existing file (ADR-026).
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(resolved_path)
            .map_err(|error| {
                AuditFailure::new(format!(
                    "--audit-path {} cannot be created: {error}",
                    resolved_path.display()
                ))
            })?;
        let audit = Self {
            run_id,
            limits,
            inner: Mutex::new(AuditInner {
                file,
                sequence: 0,
                events_written: 0,
                poisoned: None,
            }),
        };
        let mut start = serde_json::Map::new();
        insert_opt_str(&mut start, "provider_label", &meta.provider_label);
        insert_opt_str(&mut start, "requested_model", &meta.requested_model);
        insert_opt_str(&mut start, "base_url_host", &meta.base_url_host);
        insert_opt_str(&mut start, "wire", &meta.wire);
        insert_opt_str(&mut start, "credential_env", &meta.credential_env);
        if let Some(list) = &meta.effective_allowlist {
            start.insert("effective_allowlist".into(), serde_json::json!(list));
        } else {
            start.insert("effective_allowlist".into(), serde_json::Value::Null);
        }
        insert_opt_str(&mut start, "effective_root", &meta.effective_root);
        start.insert(
            "workspace_read_only".into(),
            serde_json::json!(meta.workspace_read_only),
        );
        audit.record("run_start", start)?;
        Ok(audit)
    }

    /// Accessor for the run correlation id. Exercised through the public test
    /// seam; `allow(dead_code)` because the bin build has no production caller
    /// until an integrator threads it into frontend diagnostics.
    #[allow(dead_code)]
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// True once a write/flush has failed. The run loop checks this to stop
    /// before further model or tool work.
    pub fn poisoned(&self) -> bool {
        self.inner
            .lock()
            .map(|g| g.poisoned.is_some())
            .unwrap_or(true)
    }

    /// Take the recorded failure, if any, for exit-code handling.
    pub fn take_failure(&self) -> Option<AuditFailure> {
        self.inner.lock().ok().and_then(|mut g| g.poisoned.take())
    }

    /// Write the terminal `run_end` event, recording the final status and exit
    /// code. Must be written before publishing run success (ADR-026).
    pub fn finish(
        &self,
        status: &str,
        exit_code: i32,
        reported_model: Option<&str>,
    ) -> Result<(), AuditFailure> {
        let mut end = serde_json::Map::new();
        end.insert("status".into(), serde_json::json!(status));
        end.insert("exit_code".into(), serde_json::json!(exit_code));
        match reported_model {
            Some(model) => {
                end.insert("reported_model".into(), serde_json::json!(model));
            }
            None => {
                end.insert("reported_model".into(), serde_json::Value::Null);
            }
        }
        self.record("run_end", end)
    }

    /// Record one model request. `http_status` and reported `usage` are
    /// recorded only when available; unavailable metadata is explicit `null`,
    /// never invented (ADR-026). No request/response body, header, or error
    /// body is ever recorded.
    #[allow(clippy::too_many_arguments)]
    pub fn record_model_request(
        &self,
        sequence: u64,
        http_status: Option<u16>,
        retry_index: u32,
        probe: bool,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    ) -> Result<(), AuditFailure> {
        let mut fields = serde_json::Map::new();
        fields.insert("request_sequence".into(), serde_json::json!(sequence));
        match http_status {
            Some(status) => {
                fields.insert("http_status".into(), serde_json::json!(status));
            }
            None => {
                fields.insert("http_status".into(), serde_json::Value::Null);
            }
        }
        fields.insert("retry_index".into(), serde_json::json!(retry_index));
        fields.insert("probe".into(), serde_json::json!(probe));
        match (input_tokens, output_tokens) {
            (None, None) => {
                fields.insert("usage".into(), serde_json::Value::Null);
            }
            (input, output) => {
                fields.insert(
                    "usage".into(),
                    serde_json::json!({
                        "input_tokens": input,
                        "output_tokens": output,
                    }),
                );
            }
        }
        self.record("model_request", fields)
    }

    /// Record the single capability-probe outcome (FR-04 reqs 14-17).
    ///
    /// Pending integrator hook: WP09 owns the probe and is not in this base.
    /// The event is defined here and exercised through the public seam so the
    /// integrator can wire WP09's `ProbeOutcome` to it without a schema change.
    #[allow(dead_code)]
    pub fn record_probe_result(
        &self,
        status: &str,
        reason: Option<&str>,
    ) -> Result<(), AuditFailure> {
        let mut fields = serde_json::Map::new();
        fields.insert("status".into(), serde_json::json!(status));
        match reason {
            Some(reason) => {
                fields.insert("reason".into(), serde_json::json!(reason));
            }
            None => {
                fields.insert("reason".into(), serde_json::Value::Null);
            }
        }
        self.record("probe_result", fields)
    }

    /// Record one enforced limit hit (FR-19). Error text is bounded and never
    /// contains raw evidence.
    ///
    /// Pending integrator hook: WP18 owns the budget and is not in this base.
    /// The event is defined here and exercised through the public seam so the
    /// integrator can wire WP18's `LimitFailure` to it without a schema change.
    #[allow(dead_code)]
    pub fn record_limit_hit(
        &self,
        limit: &str,
        tool: Option<&str>,
        configured: Option<u64>,
        consumed: Option<u64>,
    ) -> Result<(), AuditFailure> {
        let mut fields = serde_json::Map::new();
        fields.insert("limit".into(), serde_json::json!(limit));
        match tool {
            Some(tool) => {
                fields.insert("tool".into(), serde_json::json!(tool));
            }
            None => {
                fields.insert("tool".into(), serde_json::Value::Null);
            }
        }
        fields.insert("configured".into(), serde_json::json!(configured));
        fields.insert("consumed".into(), serde_json::json!(consumed));
        self.record("limit_hit", fields)
    }

    /// Record one tool dispatch observed at the admission boundary. A refused
    /// call (`reason` = a policy refusal code) is a `tool_refusal`; an executed
    /// call is a `tool_call`. Arguments and results are recorded only as
    /// SHA-256 digests — their contents never enter the record (ADR-026).
    #[allow(clippy::too_many_arguments)]
    pub fn record_tool(
        &self,
        name: &str,
        route: &str,
        argument_sha256: &str,
        result_sha256: &str,
        reason: Option<&str>,
        duration_ms: u64,
        limit: Option<&str>,
    ) -> Result<(), AuditFailure> {
        let allowed = reason.is_none();
        let kind = if allowed { "tool_call" } else { "tool_refusal" };
        let mut fields = serde_json::Map::new();
        fields.insert("name".into(), serde_json::json!(name));
        fields.insert("route".into(), serde_json::json!(route));
        fields.insert("argument_sha256".into(), serde_json::json!(argument_sha256));
        fields.insert("result_sha256".into(), serde_json::json!(result_sha256));
        fields.insert("allowed".into(), serde_json::json!(allowed));
        match reason {
            Some(code) => {
                fields.insert("reason".into(), serde_json::json!(code));
            }
            None => {
                fields.insert("reason".into(), serde_json::Value::Null);
            }
        }
        fields.insert("duration_ms".into(), serde_json::json!(duration_ms));
        match limit {
            Some(limit) => {
                fields.insert("limit".into(), serde_json::json!(limit));
            }
            None => {
                fields.insert("limit".into(), serde_json::Value::Null);
            }
        }
        self.record(kind, fields)
    }

    /// Assemble one versioned event object, enforce the configured bounds, and
    /// append it as a single JSON line. On any failure the sink is poisoned and
    /// an [`AuditFailure`] is returned.
    fn record(
        &self,
        kind: &str,
        fields: serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), AuditFailure> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| AuditFailure::new("audit sink lock poisoned".into()))?;
        if let Some(existing) = &inner.poisoned {
            return Err(existing.clone());
        }
        let result = (|| -> Result<(), AuditFailure> {
            if inner.events_written >= self.limits.max_events {
                return Err(AuditFailure::new(format!(
                    "audit event count limit reached (reviewer.audit.max_events = {})",
                    self.limits.max_events
                )));
            }
            let mut event = serde_json::Map::new();
            event.insert("audit_version".into(), serde_json::json!(AUDIT_VERSION));
            event.insert("run_id".into(), serde_json::json!(self.run_id));
            event.insert("sequence".into(), serde_json::json!(inner.sequence));
            event.insert("kind".into(), serde_json::json!(kind));
            for (key, value) in fields {
                event.insert(key, value);
            }
            let mut line =
                serde_json::to_string(&serde_json::Value::Object(event)).map_err(|error| {
                    AuditFailure::new(format!("audit event not serializable: {error}"))
                })?;
            if line.len() > self.limits.max_event_bytes {
                return Err(AuditFailure::new(format!(
                    "audit event size {} exceeds reviewer.audit.max_event_bytes = {}",
                    line.len(),
                    self.limits.max_event_bytes
                )));
            }
            line.push('\n');
            inner
                .file
                .write_all(line.as_bytes())
                .map_err(|error| AuditFailure::new(format!("audit append failed: {error}")))?;
            inner
                .file
                .flush()
                .map_err(|error| AuditFailure::new(format!("audit flush failed: {error}")))?;
            inner.sequence += 1;
            inner.events_written += 1;
            Ok(())
        })();
        if let Err(failure) = &result {
            inner.poisoned = Some(failure.clone());
        }
        result
    }
}

/// Tool refusal codes (reviewer-runtime INTERFACES.md). A tool whose boundary
/// result carries one of these in its `code` field was refused by policy and is
/// recorded as a `tool_refusal`; anything else is an executed `tool_call`.
pub const REFUSAL_CODES: &[&str] = &[
    "tool_not_allowed",
    "path_outside_root",
    "workspace_write_forbidden",
    "symlink_refused",
    "tool_policy_unsupported",
    "tool_timeout",
    "tool_input_limit",
    "offline_tool_forbidden",
];

/// SHA-256 of raw bytes, lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// SHA-256 of a JSON value serialized compactly with stable (recursively
/// sorted) key ordering. This crate builds serde_json with `preserve_order`,
/// so `to_string` keeps insertion order; [`canonical_json`] re-sorts keys so
/// the hash is independent of the order the caller supplied (ADR-026).
pub fn hash_json_value(value: &serde_json::Value) -> String {
    sha256_hex(canonical_json(value).as_bytes())
}

/// Serialize `value` compactly with object keys recursively sorted, matching
/// `json.dumps(..., separators=(",",":"), sort_keys=True)`.
fn canonical_json(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).unwrap_or_default());
                out.push(':');
                write_canonical(&map[*k], out);
            }
            out.push('}');
        }
        serde_json::Value::Array(arr) => {
            out.push('[');
            for (i, v) in arr.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(v, out);
            }
            out.push(']');
        }
        other => out.push_str(&serde_json::to_string(other).unwrap_or_default()),
    }
}

/// SHA-256 of a tool result delivered at the dispatch boundary. A JSON result
/// is canonicalized first (so the hash is stable across key order); a non-JSON
/// result is hashed as raw UTF-8. Never a Debug representation (ADR-026).
pub fn hash_result_str(result: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(result) {
        Ok(value) => hash_json_value(&value),
        Err(_) => sha256_hex(result.as_bytes()),
    }
}

/// If the boundary result is a JSON object whose `code` names a known policy
/// refusal, return that code; otherwise `None` (the call executed).
pub fn refusal_code(result: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(result).ok()?;
    let code = value.get("code")?.as_str()?;
    REFUSAL_CODES.contains(&code).then(|| code.to_string())
}

fn insert_opt_str(
    map: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: &Option<String>,
) {
    match value {
        Some(v) => {
            map.insert(key.into(), serde_json::json!(v));
        }
        None => {
            map.insert(key.into(), serde_json::Value::Null);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "run_audit_{tag}_{}_{nanos}.jsonl",
            std::process::id()
        ))
    }

    fn limits() -> AuditLimits {
        AuditLimits {
            max_event_bytes: 65536,
            max_events: 100,
        }
    }

    fn read_events(path: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn every_event_kind_is_versioned_sequenced_and_single_run() {
        let path = tmp_path("kinds");
        let audit = RunAudit::open(
            &path,
            "run-xyz".into(),
            limits(),
            &RunAuditMeta {
                provider_label: Some("anthropic".into()),
                base_url_host: Some("api.example.com".into()),
                credential_env: Some("DBUNK_KEY".into()),
                ..RunAuditMeta::default()
            },
        )
        .unwrap();
        audit
            .record_model_request(0, Some(200), 0, true, Some(10), Some(2))
            .unwrap();
        audit
            .record_probe_result("model_unsupported", Some("text_only"))
            .unwrap();
        audit
            .record_tool("read_file", "native", "a", "b", None, 5, None)
            .unwrap();
        audit
            .record_tool(
                "write_file",
                "native",
                "c",
                "d",
                Some("workspace_write_forbidden"),
                1,
                None,
            )
            .unwrap();
        audit
            .record_limit_hit("tool_input_bytes", Some("search"), Some(1024), Some(2048))
            .unwrap();
        audit.finish("ok", 0, Some("anthropic/some-model")).unwrap();

        let events = read_events(&path);
        let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            vec![
                "run_start",
                "model_request",
                "probe_result",
                "tool_call",
                "tool_refusal",
                "limit_hit",
                "run_end",
            ]
        );
        for (i, ev) in events.iter().enumerate() {
            assert_eq!(ev["audit_version"], AUDIT_VERSION);
            assert_eq!(ev["run_id"], "run-xyz");
            assert_eq!(ev["sequence"], i as u64);
        }
        // No credential value ever appears; only the variable NAME does.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("DBUNK_KEY"));
        assert!(events[3]["allowed"].as_bool().unwrap());
        assert!(!events[4]["allowed"].as_bool().unwrap());
        assert_eq!(events[4]["reason"], "workspace_write_forbidden");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_refuses_to_append_to_an_existing_file() {
        let path = tmp_path("excl");
        std::fs::write(&path, "prior\n").unwrap();
        let err = match RunAudit::open(&path, "r".into(), limits(), &RunAuditMeta::default()) {
            Ok(_) => panic!("must refuse an existing destination"),
            Err(err) => err,
        };
        assert_eq!(err.code, CODE_AUDIT_WRITE_FAILED);
        assert_eq!(err.exit_code(), EXIT_AUDIT_WRITE_FAILED);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "prior\n");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn exceeding_max_events_poisons_the_sink() {
        let path = tmp_path("evcount");
        let audit = RunAudit::open(
            &path,
            "r".into(),
            AuditLimits {
                max_event_bytes: 65536,
                max_events: 1,
            },
            &RunAuditMeta::default(),
        )
        .unwrap();
        // run_start already consumed the single allowed event.
        let err = audit
            .record_tool("x", "native", "a", "b", None, 0, None)
            .unwrap_err();
        assert_eq!(err.code, CODE_AUDIT_WRITE_FAILED);
        assert!(audit.poisoned());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn hashing_is_stable_across_key_order_and_classifies_refusals() {
        let a = hash_result_str(r#"{"b":2,"a":1}"#);
        let b = hash_result_str(r#"{"a":1,"b":2}"#);
        assert_eq!(a, b, "hash must not depend on key order");
        assert_eq!(
            refusal_code(r#"{"code":"workspace_write_forbidden"}"#),
            Some("workspace_write_forbidden".to_string())
        );
        assert_eq!(refusal_code(r#"{"ok":true}"#), None);
        assert_eq!(refusal_code("plain text result"), None);
    }

    #[test]
    fn base_url_host_drops_userinfo_port_path_and_query() {
        assert_eq!(
            base_url_host("https://user:secret@api.example.com:8443/v1/chat?k=v"),
            Some("api.example.com".to_string())
        );
        assert_eq!(
            base_url_host("http://127.0.0.1:52133/v1"),
            Some("127.0.0.1".to_string())
        );
        assert_eq!(
            base_url_host("https://[2001:db8::1]:443/v1"),
            Some("2001:db8::1".to_string())
        );
        assert_eq!(base_url_host(""), None);
    }
}
