//! Pure input mapping for the interactive TUI (Vikunja #1091, phases 4-5).
//!
//! This layer turns raw human input into intent, with **no I/O and no session
//! coupling**, so it is exhaustively unit-testable:
//!
//! * [`parse_command`] parses a composer line into a [`UiCommand`] — either a
//!   prompt to send or a slash command driving session / local control.
//! * [`approval_from_key`] maps a keypress to an [`ApprovalDecision`] while
//!   honouring the host policy gate on *allow always* (ADR-011: the local TUI
//!   retains approval authority; *allow always* is separately gated).
//!
//! Mapping a [`UiCommand`] onto the wire ([`ClientMessage`]) belongs to the
//! client task (next slice); keeping parsing pure means that task stays a thin
//! adapter.
//!
//! [`ClientMessage`]: crate::session_protocol::ClientMessage

#![allow(dead_code)] // Consumed by the input/event loop in the next slice.

use crate::session_protocol::ApprovalDecision;

/// A parsed line of composer input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    /// Ordinary text to send as a prompt (trimmed; may be empty for a blank
    /// line, which the caller ignores).
    Prompt(String),
    /// Show the in-TUI help / command palette.
    Help,
    /// Reset the conversation transcript (cumulative usage is kept).
    Clear,
    /// Show cumulative token usage / cost for the session.
    Usage,
    /// Switch model. `None` opens the picker; `Some(id)` selects directly.
    Model(Option<String>),
    /// Force a context compaction pass now.
    Compact,
    /// Interrupt the in-flight turn without quitting.
    Interrupt,
    /// Detach the TUI while leaving the daemon-owned session running.
    Quit,
    /// End the current daemon-owned session and exit.
    StopSession,
    /// Detach the TUI while leaving the daemon-owned session running.
    Detach,
    /// An unrecognized `/command` (echoed back so the UI can hint).
    Unknown(String),
}

/// Human-facing help text for the command palette.
pub const HELP_TEXT: &str = "\
Commands:
  /help, /?          show this help
  /clear             reset the conversation (usage kept)
  /usage             show token usage and cost
  /model [id]        switch model (no id opens the picker)
  /compact           compact context now
  /interrupt, /stop  interrupt the in-flight turn
  /quit, /exit       detach the TUI; the daemon session keeps running
  /detach            detach the TUI; the daemon session keeps running
  /stop-session      end the current session and exit
Anything else is sent to the agent as a prompt.
Enter sends · Ctrl-C interrupts the current turn.
Up/Down browse prompt history · PageUp/PageDown scroll · Home/End jump.
Esc enters scroll mode (vim keys: j/k, Ctrl-D/U, Ctrl-F/B, gg/G; Esc or i returns).";

/// Parse one composer line into a [`UiCommand`].
///
/// A line that does not begin with `/` is a [`UiCommand::Prompt`]. Slash
/// commands are matched case-insensitively on the first whitespace-delimited
/// token; the remainder (trimmed) is the argument.
pub fn parse_command(line: &str) -> UiCommand {
    let trimmed = line.trim();
    if !trimmed.starts_with('/') {
        return UiCommand::Prompt(trimmed.to_string());
    }

    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or("").to_ascii_lowercase();
    let rest = parts.next().map(str::trim).filter(|s| !s.is_empty());

    match head.as_str() {
        "/help" | "/?" | "/h" => UiCommand::Help,
        "/clear" => UiCommand::Clear,
        "/usage" => UiCommand::Usage,
        "/model" => UiCommand::Model(rest.map(str::to_string)),
        "/compact" => UiCommand::Compact,
        "/interrupt" | "/stop" | "/cancel" => UiCommand::Interrupt,
        "/quit" | "/exit" | "/q" => UiCommand::Quit,
        "/detach" => UiCommand::Detach,
        "/stop-session" | "/kill" => UiCommand::StopSession,
        other => UiCommand::Unknown(other.to_string()),
    }
}

/// Map an approval keypress to a decision, honouring the *allow always* gate.
///
/// * `y` → allow once
/// * `n` / `d` → deny
/// * `a` → allow always, **only** when `allow_always_available` (otherwise the
///   key is inert — a host that forbids allow-always must never let it through)
///
/// Any other key returns `None` so the caller keeps the modal open.
pub fn approval_from_key(key: char, allow_always_available: bool) -> Option<ApprovalDecision> {
    match key.to_ascii_lowercase() {
        'y' => Some(ApprovalDecision::AllowOnce),
        'n' | 'd' => Some(ApprovalDecision::Deny),
        'a' if allow_always_available => Some(ApprovalDecision::AllowAlways),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_a_prompt() {
        assert_eq!(
            parse_command("hello world"),
            UiCommand::Prompt("hello world".to_string())
        );
    }

    #[test]
    fn prompt_is_trimmed_and_may_be_empty() {
        assert_eq!(
            parse_command("   spaced  "),
            UiCommand::Prompt("spaced".to_string())
        );
        assert_eq!(parse_command("   "), UiCommand::Prompt(String::new()));
    }

    #[test]
    fn a_leading_slash_word_that_is_prompt_like_stays_command() {
        // Only a recognized head is special; everything else is Unknown, never
        // silently sent as a prompt (which could be surprising/destructive).
        assert_eq!(
            parse_command("/frobnicate now"),
            UiCommand::Unknown("/frobnicate".to_string())
        );
    }

    #[test]
    fn slash_commands_parse_case_insensitively() {
        assert_eq!(parse_command("/HELP"), UiCommand::Help);
        assert_eq!(parse_command("/Clear"), UiCommand::Clear);
        assert_eq!(parse_command("/usage"), UiCommand::Usage);
        assert_eq!(parse_command("/compact"), UiCommand::Compact);
    }

    #[test]
    fn interrupt_quit_and_stop_have_aliases() {
        assert_eq!(parse_command("/interrupt"), UiCommand::Interrupt);
        assert_eq!(parse_command("/stop"), UiCommand::Interrupt);
        assert_eq!(parse_command("/quit"), UiCommand::Quit);
        assert_eq!(parse_command("/exit"), UiCommand::Quit);
        assert_eq!(parse_command("/stop-session"), UiCommand::StopSession);
        assert_eq!(parse_command("/kill"), UiCommand::StopSession);
    }

    #[test]
    fn detach_is_available_for_daemon_owned_sessions() {
        assert_eq!(parse_command("/detach"), UiCommand::Detach);
        assert!(HELP_TEXT.contains("/detach"));
    }

    #[test]
    fn model_takes_optional_argument() {
        assert_eq!(parse_command("/model"), UiCommand::Model(None));
        assert_eq!(
            parse_command("/model opus-5"),
            UiCommand::Model(Some("opus-5".to_string()))
        );
        assert_eq!(
            parse_command("/model   opus-5  "),
            UiCommand::Model(Some("opus-5".to_string()))
        );
    }

    #[test]
    fn approval_keys_map_to_decisions() {
        assert_eq!(
            approval_from_key('y', false),
            Some(ApprovalDecision::AllowOnce)
        );
        assert_eq!(
            approval_from_key('Y', false),
            Some(ApprovalDecision::AllowOnce)
        );
        assert_eq!(approval_from_key('n', false), Some(ApprovalDecision::Deny));
        assert_eq!(approval_from_key('d', true), Some(ApprovalDecision::Deny));
    }

    #[test]
    fn allow_always_is_gated_by_host_policy() {
        // When the host forbids allow-always, 'a' must be inert, not a decision.
        assert_eq!(approval_from_key('a', false), None);
        assert_eq!(
            approval_from_key('a', true),
            Some(ApprovalDecision::AllowAlways)
        );
    }

    #[test]
    fn unmapped_keys_return_none() {
        assert_eq!(approval_from_key('x', true), None);
        assert_eq!(approval_from_key('1', true), None);
    }
}
