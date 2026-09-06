//! Herdr supervisor integration (custom-agent state reporting).
//!
//! [Herdr](https://herdr.dev) is a terminal runtime that supervises coding
//! agents in PTY panes. It marks every pane working / blocked / idle and lets
//! other agents wait on those transitions. For agents it does not natively
//! know, herdr documents a custom-integration contract: a process running
//! inside a herdr pane inherits `HERDR_ENV=1`, `HERDR_PANE_ID`, and
//! `HERDR_BIN_PATH`, and reports semantic state through the herdr CLI:
//!
//! ```text
//! $HERDR_BIN_PATH pane report-agent $HERDR_PANE_ID \
//!     --source custom:daimonos --agent daimonos --state working --seq N
//! ```
//!
//! plus `pane release-agent` on exit to hand lifecycle authority back.
//!
//! This module implements that contract for the `chat` and one-shot `agent`
//! frontends. Outside herdr ([`HerdrReporter::from_env`] returns `None`) the
//! integration is a complete no-op, per herdr's guidance.
//!
//! Dispatch is intentionally *synchronous* (spawn + wait on the herdr CLI):
//! state transitions happen at human-paced boundaries (turn start/end,
//! approval prompts, shutdown), the call is local IPC, and blocking keeps
//! reports strictly ordered — a fire-and-forget `report` racing past the
//! final `release` could re-assert authority on a released source. This also
//! leaves no child processes to reap.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Environment contract inherited from a herdr pane. Names are herdr's
/// published protocol constants, not daimonos tunables.
pub const ENV_ACTIVE: &str = "HERDR_ENV";
pub const ENV_PANE_ID: &str = "HERDR_PANE_ID";
pub const ENV_BIN_PATH: &str = "HERDR_BIN_PATH";

/// Stable integration identity — herdr keys lifecycle authority on the
/// source, so this must never change between reports of one process.
const SOURCE: &str = "custom:daimonos";
const AGENT_NAME: &str = "daimonos";

/// Semantic pane states herdr understands for a custom agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    /// Ready for input at the prompt.
    Idle,
    /// A turn is in flight.
    Working,
    /// Waiting on an operator decision (safety approval).
    Blocked,
}

impl AgentState {
    fn as_str(self) -> &'static str {
        match self {
            AgentState::Idle => "idle",
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
        }
    }
}

/// Reports daimonos agent state to a supervising herdr instance.
///
/// Constructed once per frontend via [`HerdrReporter::from_env`]; all
/// reports share one strictly-increasing `--seq` counter so herdr can drop
/// stale reports if any ever arrive out of order.
pub struct HerdrReporter {
    bin: String,
    pane_id: String,
    seq: AtomicU64,
    /// Session id attached to every report once known (`--agent-session-id`),
    /// so herdr surfaces it through its pane/agent APIs (`chat --resume <id>`
    /// is the manual restore path).
    session_id: Mutex<Option<String>>,
}

impl HerdrReporter {
    /// Build a reporter from the process environment. `None` (no-op mode)
    /// unless all three herdr pane variables are present and `HERDR_ENV=1`,
    /// exactly as herdr's integration guide requires.
    pub fn from_env() -> Option<Self> {
        Self::from_vars(
            std::env::var(ENV_ACTIVE).ok(),
            std::env::var(ENV_PANE_ID).ok(),
            std::env::var(ENV_BIN_PATH).ok(),
        )
    }

    /// Env-injectable constructor for tests.
    fn from_vars(
        active: Option<String>,
        pane_id: Option<String>,
        bin: Option<String>,
    ) -> Option<Self> {
        let active = active?;
        if active.trim() != "1" {
            return None;
        }
        let pane_id = pane_id.filter(|v| !v.trim().is_empty())?;
        let bin = bin.filter(|v| !v.trim().is_empty())?;
        Some(HerdrReporter {
            bin,
            pane_id,
            seq: AtomicU64::new(1),
            session_id: Mutex::new(None),
        })
    }

    /// Attach a session id to all subsequent reports.
    pub fn set_session_id(&self, id: &str) {
        let mut guard = self
            .session_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(id.to_string());
    }

    /// Report a state transition. Best-effort: a missing or failing herdr
    /// binary must never break the agent frontend.
    pub fn report(&self, state: AgentState, message: Option<&str>) {
        let args = self.report_args(state, message);
        self.dispatch(&args);
    }

    /// Release lifecycle authority for this source — call once on frontend
    /// exit, per herdr's contract.
    pub fn release(&self) {
        let args = vec![
            "pane".to_string(),
            "release-agent".to_string(),
            self.pane_id.clone(),
            "--source".to_string(),
            SOURCE.to_string(),
            "--agent".to_string(),
            AGENT_NAME.to_string(),
        ];
        self.dispatch(&args);
    }

    fn report_args(&self, state: AgentState, message: Option<&str>) -> Vec<String> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let mut args = vec![
            "pane".to_string(),
            "report-agent".to_string(),
            self.pane_id.clone(),
            "--source".to_string(),
            SOURCE.to_string(),
            "--agent".to_string(),
            AGENT_NAME.to_string(),
            "--state".to_string(),
            state.as_str().to_string(),
            "--seq".to_string(),
            seq.to_string(),
        ];
        if let Some(message) = message {
            args.push("--message".to_string());
            args.push(message.to_string());
        }
        let session = self
            .session_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(id) = session.as_deref() {
            args.push("--agent-session-id".to_string());
            args.push(id.to_string());
        }
        args
    }

    fn dispatch(&self, args: &[String]) {
        let status = std::process::Command::new(&self.bin)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if let Err(error) = status {
            tracing::debug!(
                target: "daimonos::herdr",
                event = "herdr_report_failed",
                error = %error,
                "herdr state report failed; continuing without supervision"
            );
        }
    }
}

/// Wrap an approval callback so herdr sees the pane as `blocked` while the
/// operator decision is pending, and `working` again once it resolves (the
/// turn resumes either way — an approved tool runs, a denied one returns a
/// block result to the model).
pub fn wrap_approve_fn(
    inner: crate::safety::ApproveFn,
    reporter: std::sync::Arc<HerdrReporter>,
) -> crate::safety::ApproveFn {
    Box::new(move |name: &str, input: &serde_json::Value| {
        reporter.report(
            AgentState::Blocked,
            Some(&format!("waiting for approval: {name}")),
        );
        let decision = inner(name, input);
        reporter.report(AgentState::Working, None);
        decision
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn reporter(bin: &str) -> HerdrReporter {
        HerdrReporter::from_vars(
            Some("1".to_string()),
            Some("%7".to_string()),
            Some(bin.to_string()),
        )
        .expect("complete herdr env should build a reporter")
    }

    /// Stub herdr binary: appends each invocation's args as one line to a log
    /// file, so tests can assert exact CLI calls and their order.
    fn stub_bin(dir: &std::path::Path) -> (String, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let log = dir.join("calls.log");
        let bin = dir.join("herdr-stub");
        std::fs::write(
            &bin,
            format!("#!/bin/sh\necho \"$@\" >> {}\n", log.display()),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        (bin.display().to_string(), log)
    }

    // --- env gating ---

    #[test]
    fn absent_env_is_a_noop() {
        assert!(HerdrReporter::from_vars(None, None, None).is_none());
    }

    #[test]
    fn env_flag_must_be_exactly_one() {
        assert!(HerdrReporter::from_vars(
            Some("0".into()),
            Some("%1".into()),
            Some("/bin/true".into())
        )
        .is_none());
        assert!(HerdrReporter::from_vars(
            Some("true".into()),
            Some("%1".into()),
            Some("/bin/true".into())
        )
        .is_none());
    }

    #[test]
    fn missing_pane_or_bin_is_a_noop() {
        assert!(
            HerdrReporter::from_vars(Some("1".into()), None, Some("/bin/true".into())).is_none()
        );
        assert!(HerdrReporter::from_vars(Some("1".into()), Some("%1".into()), None).is_none());
        assert!(HerdrReporter::from_vars(
            Some("1".into()),
            Some("  ".into()),
            Some("/bin/true".into())
        )
        .is_none());
    }

    #[test]
    fn complete_env_builds_a_reporter() {
        let r = reporter("/bin/true");
        assert_eq!(r.pane_id, "%7");
    }

    // --- report argument construction ---

    #[test]
    fn report_args_follow_the_herdr_contract() {
        let r = reporter("/bin/true");
        let args = r.report_args(AgentState::Working, None);
        assert_eq!(
            args,
            vec![
                "pane",
                "report-agent",
                "%7",
                "--source",
                "custom:daimonos",
                "--agent",
                "daimonos",
                "--state",
                "working",
                "--seq",
                "1",
            ]
        );
    }

    #[test]
    fn seq_increases_monotonically_across_reports() {
        let r = reporter("/bin/true");
        let first = r.report_args(AgentState::Working, None);
        let second = r.report_args(AgentState::Idle, None);
        let seq_of = |args: &[String]| {
            let at = args.iter().position(|a| a == "--seq").unwrap();
            args[at + 1].parse::<u64>().unwrap()
        };
        assert!(seq_of(&second) > seq_of(&first));
    }

    #[test]
    fn blocked_report_carries_the_message() {
        let r = reporter("/bin/true");
        let args = r.report_args(AgentState::Blocked, Some("waiting for approval: exec"));
        let at = args.iter().position(|a| a == "--message").unwrap();
        assert_eq!(args[at + 1], "waiting for approval: exec");
        assert!(args.contains(&"blocked".to_string()));
    }

    #[test]
    fn session_id_is_attached_once_set() {
        let r = reporter("/bin/true");
        assert!(!r
            .report_args(AgentState::Idle, None)
            .contains(&"--agent-session-id".to_string()));
        r.set_session_id("sess-42");
        let args = r.report_args(AgentState::Idle, None);
        let at = args.iter().position(|a| a == "--agent-session-id").unwrap();
        assert_eq!(args[at + 1], "sess-42");
    }

    // --- full lifecycle through the stub binary ---

    #[test]
    fn full_lifecycle_reports_then_releases_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, log) = stub_bin(dir.path());
        let r = reporter(&bin);

        r.set_session_id("sess-1");
        r.report(AgentState::Idle, None);
        r.report(AgentState::Working, None);
        r.release();

        let calls = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = calls.lines().collect();
        assert_eq!(lines.len(), 3, "two reports + one release: {calls:?}");
        assert!(lines[0].contains("report-agent") && lines[0].contains("--state idle"));
        assert!(lines[0].contains("--agent-session-id sess-1"));
        assert!(lines[1].contains("--state working"));
        assert!(
            lines[2].contains("release-agent"),
            "exit must release lifecycle authority: {calls:?}"
        );
        assert!(
            !lines[2].contains("--state"),
            "release carries no state: {calls:?}"
        );
    }

    #[test]
    fn missing_binary_never_panics() {
        let r = reporter("/nonexistent/herdr-binary");
        r.report(AgentState::Working, None);
        r.release();
    }

    // --- approval wrapper ---

    #[test]
    fn approve_wrapper_reports_blocked_then_working_around_the_decision() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, log) = stub_bin(dir.path());
        let r = Arc::new(reporter(&bin));

        let inner: crate::safety::ApproveFn =
            Box::new(|_, _| crate::safety::ApprovalDecision::Once);
        let wrapped = wrap_approve_fn(inner, Arc::clone(&r));
        let decision = wrapped("exec", &serde_json::json!({"command": "ls"}));

        assert_eq!(decision, crate::safety::ApprovalDecision::Once);
        let calls = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = calls.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("--state blocked"));
        assert!(lines[0].contains("waiting for approval: exec"));
        assert!(lines[1].contains("--state working"));
    }

    #[test]
    fn approve_wrapper_preserves_the_inner_decision_on_deny() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _log) = stub_bin(dir.path());
        let r = Arc::new(reporter(&bin));
        let inner: crate::safety::ApproveFn =
            Box::new(|_, _| crate::safety::ApprovalDecision::Deny);
        let wrapped = wrap_approve_fn(inner, r);
        assert_eq!(
            wrapped("git", &serde_json::json!({})),
            crate::safety::ApprovalDecision::Deny
        );
    }
}
