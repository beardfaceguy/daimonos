//! Interactive full-screen terminal UI for daimonos agent sessions
//! (Vikunja #1091).
//!
//! The TUI is a *client* of the daemon-owned session core (`session_core`,
//! `session_protocol`, `client_transport`) built for the remote-control track
//! (Vikunja #1090 / ADR-010). It never binds rendering directly to
//! provider/tool hooks. The layering is deliberately split so each layer is
//! independently testable:
//!
//! 1. `session_protocol` — transport-independent canonical session events and
//!    snapshots (already exists, shared with the Android client).
//! 2. [`state`] — a *pure* reducer ([`state::ViewState`]) that folds
//!    `(seq, SessionEvent)` streams and canonical [`SessionSnapshot`]s into a
//!    render-ready view, with explicit duplicate / out-of-order / gap
//!    handling. **No terminal or async dependencies** — 100% unit-testable.
//! 3. rendering + input mapping (ratatui/crossterm) — layered on top in a
//!    later slice; it only ever reads a [`state::ViewState`].
//! 4. a local ACP/UDS client task (`client_transport`) that feeds the reducer.
//!
//! This module currently lands layer 2. See ADR-011 for the full design.
//!
//! [`SessionSnapshot`]: crate::session_protocol::SessionSnapshot

#![allow(dead_code)] // Rendering/input layers consume this in the next slice.
#![allow(unused_imports)] // Re-exports below are the module's public surface.

pub mod agent_mode;
pub mod app;
pub mod commands;
pub mod input;
pub mod render;
mod session;
pub mod state;
pub mod terminal;

pub use agent_mode::{resolve_agent_mode, AgentMode};
pub use app::{run as run_tui, TuiOptions};
pub use commands::{approval_from_key, parse_command, UiCommand};
pub use render::{
    composer_cursor_position, composer_cursor_position_at, render, render_with_options, tui_layout,
    RenderOptions,
};
pub(crate) use session::{ControllerFactory, SwitchPolicy};
pub use state::{ApplyOutcome, ViewLine, ViewState};
pub use terminal::TerminalGuard;
