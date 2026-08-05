//! Frontend selection for the `daimonos agent` CLI (Vikunja #1091, phase 7).
//!
//! Pure decision layer that maps the parsed CLI flags plus terminal capability
//! to the frontend the runtime should launch. Kept free of I/O so the whole
//! decision matrix is unit-tested; the caller supplies `tty` from
//! [`std::io::IsTerminal`] and dispatches on the result.
//!
//! Compatibility contract (ADR-011): the historical `daimonos agent <task>`
//! one-shot **print** behaviour is preserved as the default. The interactive
//! full-screen TUI is *opt-in* via `-i`/`--interactive` for now, and only when
//! attached to a real TTY; `--print` forces the one-shot path even on a TTY
//! (for scripts/CI), and a non-TTY stdout always falls back to print so piped
//! and benchmarked invocations are byte-identical to today.

/// Which agent frontend the runtime should launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMode {
    /// Full-screen interactive TUI (opt-in, TTY only).
    Interactive,
    /// One-shot: run the task, print assistant text, exit. The default.
    Print,
    /// `--dry-run`: print task + tool count, never call the provider.
    DryRun,
}

impl AgentMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Print => "print",
            Self::DryRun => "dry-run",
        }
    }
}

/// Resolve the frontend from CLI flags and terminal capability.
///
/// Precedence: `--dry-run` wins over everything (it never touches a provider);
/// otherwise interactive requires the opt-in flag, the absence of `--print`,
/// and a real TTY; everything else is the one-shot print path.
pub fn resolve_agent_mode(
    interactive_flag: bool,
    print_flag: bool,
    dry_run: bool,
    tty: bool,
) -> AgentMode {
    if dry_run {
        return AgentMode::DryRun;
    }
    if interactive_flag && !print_flag && tty {
        return AgentMode::Interactive;
    }
    AgentMode::Print
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_wins_over_all_other_flags() {
        // Even with -i on a TTY, --dry-run must never launch the TUI.
        assert_eq!(
            resolve_agent_mode(true, false, true, true),
            AgentMode::DryRun
        );
        assert_eq!(
            resolve_agent_mode(false, true, true, false),
            AgentMode::DryRun
        );
    }

    #[test]
    fn interactive_requires_flag_tty_and_no_print() {
        assert_eq!(
            resolve_agent_mode(true, false, false, true),
            AgentMode::Interactive
        );
    }

    #[test]
    fn default_without_interactive_flag_is_print() {
        // The historical `daimonos agent <task>` behaviour on a TTY.
        assert_eq!(
            resolve_agent_mode(false, false, false, true),
            AgentMode::Print
        );
    }

    #[test]
    fn print_flag_forces_print_even_with_interactive_on_tty() {
        assert_eq!(
            resolve_agent_mode(true, true, false, true),
            AgentMode::Print
        );
    }

    #[test]
    fn non_tty_never_goes_interactive() {
        // Piped / benchmarked / CI invocations stay on the one-shot path.
        assert_eq!(
            resolve_agent_mode(true, false, false, false),
            AgentMode::Print
        );
    }
}
