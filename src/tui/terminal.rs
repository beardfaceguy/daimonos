//! RAII terminal guard (Vikunja #1091, layer 3).
//!
//! Owns the host terminal's raw-mode / alternate-screen / bracketed-paste
//! state and **always** restores it — on normal exit, on `?`-propagated
//! error, and on panic (via [`install_panic_hook`]). ADR-011 makes this a
//! hard requirement: a full-screen TUI that leaves the terminal in raw mode
//! renders the user's shell unusable.
//!
//! Restoration is idempotent, so an explicit [`TerminalGuard::restore`]
//! followed by `Drop` (or a panic hook firing before `Drop`) is safe.

use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
    LeaveAlternateScreen,
};

static KEYBOARD_ENHANCEMENT_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether the Kitty keyboard enhancement is currently active — i.e. the
/// terminal probed as supporting it and the push succeeded. The composer hint
/// reads this to advertise Shift-Enter only where it can actually be delivered
/// (vikunja #1424).
pub fn keyboard_enhancement_active() -> bool {
    KEYBOARD_ENHANCEMENT_ACTIVE.load(Ordering::Acquire)
}

/// Guards the terminal's raw/alternate-screen state for the lifetime of a TUI
/// session. Construct with [`TerminalGuard::enter`]; drop (or call
/// [`restore`](Self::restore)) to return the terminal to canonical mode.
pub struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    /// Enter raw mode + alternate screen + bracketed paste.
    ///
    /// On any failure the partial state is rolled back before returning the
    /// error, so a failed `enter` never leaves the terminal half-configured.
    pub fn enter() -> io::Result<Self> {
        // Gate stderr diagnostics first: from this point raw log lines would
        // be drawn over the UI. A failed enter rolls back through
        // `restore_terminal`, which lifts the gate again.
        crate::logging::suppress_stderr_logs();
        if let Err(err) = enable_raw_mode() {
            crate::logging::restore_stderr_logs();
            return Err(err);
        }
        let mut out = io::stdout();
        if let Err(err) = execute!(out, EnterAlternateScreen, EnableBracketedPaste) {
            // `execute!` may have applied EnterAlternateScreen before failing
            // on EnableBracketedPaste, so roll back the *full* terminal state
            // (alt screen + bracketed paste + cursor + raw mode), not just the
            // raw-mode change, or a partial enter leaves the terminal wedged.
            restore_terminal(&mut out);
            return Err(err);
        }
        // Only push the Kitty enhancement when the terminal actually reports it
        // (vikunja #1424). Pushing unconditionally puts terminals with partial
        // support — notably Konsole 25.12.3 (KDE bug 519627) — into a state
        // where modified Enter (Shift-Enter/Alt-Enter) is swallowed, leaving no
        // newline gesture but Ctrl-J. Gating restores legacy modified-Enter on
        // those terminals and still enables Shift-Enter where it works. A probe
        // error is treated as "unsupported" — never worse than not pushing.
        let enhanced = supports_keyboard_enhancement().unwrap_or(false);
        // Record the probe outcome so terminal-specific newline reports (e.g.
        // KDE bug 519627) are diagnosable from logs without a live repro.
        tracing::debug!(
            target: "daimonos::tui",
            keyboard_enhancement_supported = enhanced,
            "kitty keyboard enhancement probe"
        );
        if enhanced {
            if let Err(err) = execute!(
                out,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            ) {
                restore_terminal(&mut out);
                return Err(err);
            }
            KEYBOARD_ENHANCEMENT_ACTIVE.store(true, Ordering::Release);
        }
        Ok(Self { active: true })
    }

    /// Restore canonical terminal state. Idempotent: safe to call more than
    /// once and safe to call from both the panic hook and `Drop`.
    pub fn restore(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        restore_terminal(&mut io::stdout());
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Best-effort terminal restoration used by both [`TerminalGuard::restore`]
/// and [`install_panic_hook`]. Every step is attempted even if an earlier one
/// fails, and all errors are swallowed: during teardown (especially a panic)
/// there is nowhere useful to report them and leaving *some* state restored is
/// strictly better than bailing on the first failure.
fn restore_terminal(out: &mut Stdout) {
    if KEYBOARD_ENHANCEMENT_ACTIVE.swap(false, Ordering::AcqRel) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, DisableBracketedPaste, LeaveAlternateScreen, Show);
    let _ = disable_raw_mode();
    let _ = out.flush();
    // Lift the stderr-log gate last, once the terminal is back in canonical
    // mode: every restore path funnels through here (explicit restore, Drop,
    // panic hook), so a post-panic backtrace and later diagnostics print
    // normally instead of vanishing into the sink.
    crate::logging::restore_stderr_logs();
}

/// Install a panic hook that restores the terminal *before* delegating to the
/// previously installed hook, so a panic inside the TUI still prints a legible
/// backtrace to a cooked terminal instead of a garbled raw-mode screen.
///
/// Call once, immediately after [`TerminalGuard::enter`].
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal(&mut io::stdout());
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_enhancement_requests_modified_enter_reporting() {
        let mut push = Vec::new();
        execute!(
            push,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .unwrap();
        assert_eq!(push, b"\x1b[>1u");

        let mut pop = Vec::new();
        execute!(pop, PopKeyboardEnhancementFlags).unwrap();
        assert!(pop.starts_with(b"\x1b[<"));
    }

    // A full raw-mode enter/exit needs a real TTY, which CI does not provide;
    // those paths are covered by the PTY integration tests in a later phase
    // (ADR-011 gate). Here we assert the pure, TTY-independent guarantees.

    #[test]
    fn restore_is_idempotent_when_inactive() {
        // A guard that never entered raw mode (active = false) must be a no-op
        // to drop/restore — this is the path a non-TTY fallback relies on.
        let mut guard = TerminalGuard { active: false };
        guard.restore();
        guard.restore();
        // Reaching here without touching the terminal is the assertion.
    }

    #[test]
    fn dropping_inactive_guard_does_not_panic() {
        let guard = TerminalGuard { active: false };
        drop(guard);
    }

    #[test]
    fn install_panic_hook_is_callable() {
        // Installing the hook must not itself panic or require a TTY. Restore
        // the default hook afterwards so we don't perturb the test harness.
        let original = std::panic::take_hook();
        install_panic_hook();
        let _ = std::panic::take_hook();
        std::panic::set_hook(original);
    }
}
