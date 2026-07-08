#![allow(dead_code)]

use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::agent::{BeforeHook, BeforeHookResult, ToolCallInfo};

/// Tools that mutate the filesystem, run processes, or talk to external services.
/// In `Interactive` mode only these require approval; in `Paranoid` mode all tools do.
const DESTRUCTIVE_TOOLS: &[&str] = &[
    "exec", "write_file", "edit_file", "git", "docker", "cargo", "gh",
];

#[derive(Debug, Clone, PartialEq, Default)]
pub enum ApprovalMode {
    /// No prompts — only denylist/allowlist enforcement.
    #[default]
    Auto,
    /// Prompt before destructive tools; safe tools run freely.
    Interactive,
    /// Prompt before every tool call.
    Paranoid,
}

/// Operator's response to an approval prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Reject this call.
    Deny,
    /// Allow this one call only.
    Once,
    /// Allow this call and every future call of the same tool — recorded on the
    /// session's auto-approve set and persisted so later runs skip the prompt.
    Always,
}

/// Injectable approval callback: `(tool_name, formatted_input) -> decision`.
pub type ApproveFn = Box<dyn Fn(&str, &Value) -> ApprovalDecision + Send + Sync>;

pub struct SafetyPolicy {
    pub approval_mode: ApprovalMode,
    /// Non-empty: only tools in this list may run at all (denylist still wins).
    /// This is a *restrictive allowlist*, distinct from `auto_approve`.
    pub allowed_commands: Vec<String>,
    /// Tools listed here are always blocked, regardless of other settings.
    pub denied_commands: Vec<String>,
    /// Approval callback (stdin prompt in production, mock in tests).
    /// `None` is equivalent to always-approve (headless).
    pub approve_fn: Option<ApproveFn>,
    /// Tools the operator chose "always" for — seeded from the persisted
    /// approvals file at startup and grown at runtime when a prompt returns
    /// `Always`. Members skip the approval prompt. Shared + interior-mutable so
    /// an `Always` decision takes effect for the rest of the session.
    pub auto_approve: Arc<Mutex<HashSet<String>>>,
    /// Where to persist `Always` approvals (append-only, one tool name per
    /// line). `None` disables persistence (e.g. in tests / headless).
    pub approvals_path: Option<PathBuf>,
}

impl Default for SafetyPolicy {
    fn default() -> Self {
        SafetyPolicy {
            approval_mode: ApprovalMode::Auto,
            allowed_commands: Vec::new(),
            denied_commands: Vec::new(),
            approve_fn: None,
            auto_approve: Arc::new(Mutex::new(HashSet::new())),
            approvals_path: None,
        }
    }
}

impl SafetyPolicy {
    /// Consume the policy and return a `BeforeHook` closure for the agent loop.
    pub fn into_before_hook(self) -> BeforeHook {
        Box::new(move |info: &ToolCallInfo| {
            let name = info.name.as_str();

            // Denylist — hard block, no override (beats auto-approve).
            if self.denied_commands.iter().any(|d| d == name) {
                return BeforeHookResult::Block(format!(
                    "blocked by policy: '{name}' is in the denied-commands list"
                ));
            }

            // Allowlist — if non-empty, tool must be present.
            if !self.allowed_commands.is_empty()
                && !self.allowed_commands.iter().any(|a| a == name)
            {
                return BeforeHookResult::Block(format!(
                    "blocked by policy: '{name}' is not in the allowed-commands list"
                ));
            }

            let needs_approval = match self.approval_mode {
                ApprovalMode::Auto => false,
                ApprovalMode::Interactive => DESTRUCTIVE_TOOLS.contains(&name),
                ApprovalMode::Paranoid => true,
            };

            if needs_approval {
                // Previously "always"-approved (persisted or earlier this
                // session) → skip the prompt.
                if self
                    .auto_approve
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .contains(name)
                {
                    return BeforeHookResult::Allow;
                }

                let decision = match self.approve_fn.as_ref() {
                    Some(f) => f(name, &info.input),
                    None => ApprovalDecision::Once, // no approve_fn → default allow
                };
                match decision {
                    ApprovalDecision::Deny => {
                        return BeforeHookResult::Block(format!(
                            "blocked: operator declined approval for '{name}'"
                        ));
                    }
                    ApprovalDecision::Once => {}
                    ApprovalDecision::Always => {
                        self.auto_approve
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .insert(name.to_string());
                        if let Some(path) = &self.approvals_path {
                            persist_approval(path, name);
                        }
                    }
                }
            }

            BeforeHookResult::Allow
        })
    }

    /// Convenience: stdin-backed approval prompt for production use.
    /// `Y` = always (persist), `y` = just this once, anything else = deny.
    pub fn stdin_approve_fn() -> ApproveFn {
        Box::new(|name: &str, input: &Value| {
            eprint!("\n[safety] approve '{name}' with args {input}? [Y=always / y=once / N=no] ");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).unwrap_or(0);
            // Case-sensitive: capital Y means "always", lowercase y "once".
            match line.trim() {
                "Y" => ApprovalDecision::Always,
                "y" => ApprovalDecision::Once,
                _ => ApprovalDecision::Deny,
            }
        })
    }
}

/// Load persisted "always" approvals: one tool name per line, `#` comments and
/// blanks ignored. Missing/unreadable file → empty set.
pub fn load_approvals(path: &Path) -> HashSet<String> {
    std::fs::read_to_string(path)
        .map(|c| {
            c.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Append a tool name to the approvals file (idempotent, best-effort — never
/// fails the run if the file can't be written).
fn persist_approval(path: &Path, name: &str) {
    if load_approvals(path).contains(name) {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{name}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn info(name: &str) -> ToolCallInfo {
        ToolCallInfo { id: "t1".into(), name: name.into(), input: json!({}) }
    }

    fn info_with(name: &str, input: Value) -> ToolCallInfo {
        ToolCallInfo { id: "t1".into(), name: name.into(), input }
    }

    fn allow(policy: SafetyPolicy, name: &str) -> bool {
        matches!(policy.into_before_hook()(&info(name)), BeforeHookResult::Allow)
    }

    fn block_reason(policy: SafetyPolicy, name: &str) -> String {
        match policy.into_before_hook()(&info(name)) {
            BeforeHookResult::Block(r) => r,
            BeforeHookResult::Allow => panic!("expected Block, got Allow"),
        }
    }

    // --- denylist ---

    #[test]
    fn denied_tool_is_blocked() {
        let policy = SafetyPolicy {
            denied_commands: vec!["exec".into()],
            ..SafetyPolicy::default()
        };
        let reason = block_reason(policy, "exec");
        assert!(reason.contains("denied-commands"), "{reason}");
    }

    #[test]
    fn non_denied_tool_is_allowed() {
        let policy = SafetyPolicy {
            denied_commands: vec!["exec".into()],
            ..SafetyPolicy::default()
        };
        assert!(allow(policy, "read_file"));
    }

    #[test]
    fn denied_overrides_allowlist() {
        let policy = SafetyPolicy {
            allowed_commands: vec!["exec".into()],
            denied_commands: vec!["exec".into()],
            ..SafetyPolicy::default()
        };
        let reason = block_reason(policy, "exec");
        assert!(reason.contains("denied-commands"), "{reason}");
    }

    // --- allowlist ---

    #[test]
    fn allowlist_blocks_unlisted_tool() {
        let policy = SafetyPolicy {
            allowed_commands: vec!["read_file".into()],
            ..SafetyPolicy::default()
        };
        let reason = block_reason(policy, "exec");
        assert!(reason.contains("allowed-commands"), "{reason}");
    }

    #[test]
    fn allowlist_permits_listed_tool() {
        let policy = SafetyPolicy {
            allowed_commands: vec!["read_file".into()],
            ..SafetyPolicy::default()
        };
        assert!(allow(policy, "read_file"));
    }

    #[test]
    fn empty_allowlist_permits_any_tool() {
        let policy = SafetyPolicy::default();
        assert!(allow(policy, "exec"));
    }

    // --- approval modes ---

    #[test]
    fn auto_mode_never_calls_approve_fn() {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c = called.clone();
        let policy = SafetyPolicy {
            approval_mode: ApprovalMode::Auto,
            approve_fn: Some(Box::new(move |_, _| {
                c.store(true, std::sync::atomic::Ordering::SeqCst);
                ApprovalDecision::Once
            })),
            ..SafetyPolicy::default()
        };
        let hook = policy.into_before_hook();
        hook(&info("exec"));
        assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn interactive_mode_prompts_for_destructive_tool() {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c = called.clone();
        let policy = SafetyPolicy {
            approval_mode: ApprovalMode::Interactive,
            approve_fn: Some(Box::new(move |_, _| {
                c.store(true, std::sync::atomic::Ordering::SeqCst);
                ApprovalDecision::Once
            })),
            ..SafetyPolicy::default()
        };
        policy.into_before_hook()(&info("exec"));
        assert!(called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn interactive_mode_skips_prompt_for_safe_tool() {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c = called.clone();
        let policy = SafetyPolicy {
            approval_mode: ApprovalMode::Interactive,
            approve_fn: Some(Box::new(move |_, _| {
                c.store(true, std::sync::atomic::Ordering::SeqCst);
                ApprovalDecision::Once
            })),
            ..SafetyPolicy::default()
        };
        policy.into_before_hook()(&info("read_file"));
        assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn paranoid_mode_prompts_for_safe_tool() {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c = called.clone();
        let policy = SafetyPolicy {
            approval_mode: ApprovalMode::Paranoid,
            approve_fn: Some(Box::new(move |_, _| {
                c.store(true, std::sync::atomic::Ordering::SeqCst);
                ApprovalDecision::Once
            })),
            ..SafetyPolicy::default()
        };
        policy.into_before_hook()(&info("read_file"));
        assert!(called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn approve_fn_deny_blocks_call() {
        let policy = SafetyPolicy {
            approval_mode: ApprovalMode::Interactive,
            approve_fn: Some(Box::new(|_, _| ApprovalDecision::Deny)),
            ..SafetyPolicy::default()
        };
        let reason = block_reason(policy, "exec");
        assert!(reason.contains("declined"), "{reason}");
    }

    #[test]
    fn approve_fn_once_permits_call() {
        let policy = SafetyPolicy {
            approval_mode: ApprovalMode::Interactive,
            approve_fn: Some(Box::new(|_, _| ApprovalDecision::Once)),
            ..SafetyPolicy::default()
        };
        assert!(allow(policy, "exec"));
    }

    #[test]
    fn no_approve_fn_defaults_to_allow() {
        let policy = SafetyPolicy {
            approval_mode: ApprovalMode::Paranoid,
            approve_fn: None,
            ..SafetyPolicy::default()
        };
        assert!(allow(policy, "read_file"));
    }

    #[test]
    fn approve_fn_receives_tool_name_and_input() {
        let captured = Arc::new(Mutex::new((String::new(), json!(null))));
        let cap = captured.clone();
        let policy = SafetyPolicy {
            approval_mode: ApprovalMode::Interactive,
            approve_fn: Some(Box::new(move |name, input| {
                *cap.lock().unwrap() = (name.to_string(), input.clone());
                ApprovalDecision::Once
            })),
            ..SafetyPolicy::default()
        };
        let i = info_with("exec", json!({"command": "ls"}));
        policy.into_before_hook()(&i);
        let (name, input) = &*captured.lock().unwrap();
        assert_eq!(name, "exec");
        assert_eq!(input["command"], "ls");
    }

    // --- "always" approvals (three-way prompt + persistence) ---

    #[test]
    fn always_skips_next_prompt_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-approvals");
        let auto = Arc::new(Mutex::new(HashSet::new()));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = calls.clone();
        let policy = SafetyPolicy {
            approval_mode: ApprovalMode::Interactive,
            approve_fn: Some(Box::new(move |_, _| {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                ApprovalDecision::Always
            })),
            auto_approve: auto.clone(),
            approvals_path: Some(path.clone()),
            ..SafetyPolicy::default()
        };
        let hook = policy.into_before_hook();
        assert!(matches!(hook(&info("write_file")), BeforeHookResult::Allow));
        // Second call: auto-approved now, prompt must NOT fire again.
        assert!(matches!(hook(&info("write_file")), BeforeHookResult::Allow));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(auto.lock().unwrap().contains("write_file"));
        assert!(load_approvals(&path).contains("write_file"), "must persist to file");
    }

    #[test]
    fn seeded_auto_approve_skips_prompt() {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c = called.clone();
        let policy = SafetyPolicy {
            approval_mode: ApprovalMode::Interactive,
            auto_approve: Arc::new(Mutex::new(HashSet::from(["exec".to_string()]))),
            approve_fn: Some(Box::new(move |_, _| {
                c.store(true, std::sync::atomic::Ordering::SeqCst);
                ApprovalDecision::Deny
            })),
            ..SafetyPolicy::default()
        };
        assert!(allow(policy, "exec"));
        assert!(!called.load(std::sync::atomic::Ordering::SeqCst), "prompt must be skipped");
    }

    #[test]
    fn denied_beats_auto_approve() {
        let policy = SafetyPolicy {
            approval_mode: ApprovalMode::Interactive,
            denied_commands: vec!["exec".into()],
            auto_approve: Arc::new(Mutex::new(HashSet::from(["exec".to_string()]))),
            ..SafetyPolicy::default()
        };
        let reason = block_reason(policy, "exec");
        assert!(reason.contains("denied-commands"), "{reason}");
    }

    #[test]
    fn persist_approval_is_idempotent_and_load_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-approvals");
        persist_approval(&path, "write_file");
        persist_approval(&path, "write_file"); // dup — must not double-write
        persist_approval(&path, "exec");
        let loaded = load_approvals(&path);
        assert!(loaded.contains("write_file") && loaded.contains("exec"));
        let lines = std::fs::read_to_string(&path).unwrap();
        assert_eq!(lines.lines().filter(|l| *l == "write_file").count(), 1);
    }
}
