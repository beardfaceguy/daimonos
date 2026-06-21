#![allow(dead_code)]

use serde_json::Value;

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

/// Injectable approval callback: `(tool_name, formatted_input) -> approved`.
pub type ApproveFn = Box<dyn Fn(&str, &Value) -> bool + Send + Sync>;

pub struct SafetyPolicy {
    pub approval_mode: ApprovalMode,
    /// Non-empty: only tools in this list may run (denylist still wins).
    pub allowed_commands: Vec<String>,
    /// Tools listed here are always blocked, regardless of other settings.
    pub denied_commands: Vec<String>,
    /// Approval callback (stdin prompt in production, mock in tests).
    /// `None` is equivalent to always-approve.
    pub approve_fn: Option<ApproveFn>,
}

impl Default for SafetyPolicy {
    fn default() -> Self {
        SafetyPolicy {
            approval_mode: ApprovalMode::Auto,
            allowed_commands: Vec::new(),
            denied_commands: Vec::new(),
            approve_fn: None,
        }
    }
}

impl SafetyPolicy {
    /// Consume the policy and return a `BeforeHook` closure for the agent loop.
    pub fn into_before_hook(self) -> BeforeHook {
        Box::new(move |info: &ToolCallInfo| {
            let name = info.name.as_str();

            // Denylist — hard block, no override.
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

            // Approval gate.
            let needs_approval = match self.approval_mode {
                ApprovalMode::Auto => false,
                ApprovalMode::Interactive => DESTRUCTIVE_TOOLS.contains(&name),
                ApprovalMode::Paranoid => true,
            };

            if needs_approval {
                let approved = self
                    .approve_fn
                    .as_ref()
                    .map(|f| f(name, &info.input))
                    .unwrap_or(true); // no approve_fn → default allow
                if !approved {
                    return BeforeHookResult::Block(format!(
                        "blocked: operator declined approval for '{name}'"
                    ));
                }
            }

            BeforeHookResult::Allow
        })
    }

    /// Convenience: stdin-backed approval prompt for production use.
    pub fn stdin_approve_fn() -> ApproveFn {
        Box::new(|name: &str, input: &Value| {
            eprint!("\n[safety] approve '{name}' with args {}? [y/N] ", input);
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).unwrap_or(0);
            matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
        })
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
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c = called.clone();
        let policy = SafetyPolicy {
            approval_mode: ApprovalMode::Auto,
            approve_fn: Some(Box::new(move |_, _| {
                c.store(true, std::sync::atomic::Ordering::SeqCst);
                true
            })),
            ..SafetyPolicy::default()
        };
        let hook = policy.into_before_hook();
        hook(&info("exec"));
        assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn interactive_mode_prompts_for_destructive_tool() {
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c = called.clone();
        let policy = SafetyPolicy {
            approval_mode: ApprovalMode::Interactive,
            approve_fn: Some(Box::new(move |_, _| {
                c.store(true, std::sync::atomic::Ordering::SeqCst);
                true
            })),
            ..SafetyPolicy::default()
        };
        policy.into_before_hook()(&info("exec"));
        assert!(called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn interactive_mode_skips_prompt_for_safe_tool() {
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c = called.clone();
        let policy = SafetyPolicy {
            approval_mode: ApprovalMode::Interactive,
            approve_fn: Some(Box::new(move |_, _| {
                c.store(true, std::sync::atomic::Ordering::SeqCst);
                true
            })),
            ..SafetyPolicy::default()
        };
        policy.into_before_hook()(&info("read_file"));
        assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn paranoid_mode_prompts_for_safe_tool() {
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c = called.clone();
        let policy = SafetyPolicy {
            approval_mode: ApprovalMode::Paranoid,
            approve_fn: Some(Box::new(move |_, _| {
                c.store(true, std::sync::atomic::Ordering::SeqCst);
                true
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
            approve_fn: Some(Box::new(|_, _| false)),
            ..SafetyPolicy::default()
        };
        let reason = block_reason(policy, "exec");
        assert!(reason.contains("declined"), "{reason}");
    }

    #[test]
    fn approve_fn_allow_permits_call() {
        let policy = SafetyPolicy {
            approval_mode: ApprovalMode::Interactive,
            approve_fn: Some(Box::new(|_, _| true)),
            ..SafetyPolicy::default()
        };
        assert!(allow(policy, "exec"));
    }

    #[test]
    fn no_approve_fn_defaults_to_allow() {
        // approve_fn = None → always approve (non-interactive use)
        let policy = SafetyPolicy {
            approval_mode: ApprovalMode::Paranoid,
            approve_fn: None,
            ..SafetyPolicy::default()
        };
        assert!(allow(policy, "read_file"));
    }

    #[test]
    fn approve_fn_receives_tool_name_and_input() {
        use std::sync::{Arc, Mutex};
        let captured = Arc::new(Mutex::new(("".to_string(), json!(null))));
        let cap = captured.clone();
        let policy = SafetyPolicy {
            approval_mode: ApprovalMode::Interactive,
            approve_fn: Some(Box::new(move |name, input| {
                *cap.lock().unwrap() = (name.to_string(), input.clone());
                true
            })),
            ..SafetyPolicy::default()
        };
        let i = info_with("exec", json!({"command": "ls"}));
        policy.into_before_hook()(&i);
        let (name, input) = &*captured.lock().unwrap();
        assert_eq!(name, "exec");
        assert_eq!(input["command"], "ls");
    }
}
