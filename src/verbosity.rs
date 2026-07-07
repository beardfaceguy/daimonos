//! Per-session output verbosity level (vikunja #181, #936 lever design).
//!
//! A runtime-switchable dial that trades tool-output detail for tokens. The
//! level is stored on the per-connection [`crate::session::Session`] and can be
//! changed mid-session via the `set_verbosity` MCP tool. It is intended to
//! parameterize *existing* output knobs (output caps, diagnostic list lengths,
//! schema tier, tool exposure) rather than introduce new per-level formatting
//! paths — that wiring lands with the individual levers; this module only
//! defines the level itself.
//!
//! Note: [`Verbosity::Terse`] is a distinct axis from
//! [`crate::tools::ToolTier::Terse`]. The tier governs *schema-exposure*
//! strategy in `list_tools`; the verbosity level governs *output detail* across
//! a session. They are namespaced and independent.

use serde::Deserialize;

/// Session output verbosity, ordered most-verbose → least-verbose. `Full` is
/// today's default behavior; lower levels trade detail for tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verbosity {
    /// Today's default: full output, every knob at its standard value.
    #[default]
    Full,
    /// Tighter output caps and more aggressive filtering.
    Compact,
    /// Minimum viable output: counts, exit codes, first error only. Must stay
    /// lossless on error signals (exit codes, first error line, truncation
    /// notices always survive).
    Terse,
}

impl Verbosity {
    /// Canonical lowercase name — used in config, env, tool args, and surfacing.
    pub fn as_str(self) -> &'static str {
        match self {
            Verbosity::Full => "full",
            Verbosity::Compact => "compact",
            Verbosity::Terse => "terse",
        }
    }

    /// Parse a user/config/env-supplied level, case-insensitively and ignoring
    /// surrounding whitespace. Returns `None` for unrecognized input so callers
    /// can surface the valid set.
    pub fn from_input(s: &str) -> Option<Verbosity> {
        match s.trim().to_ascii_lowercase().as_str() {
            "full" => Some(Verbosity::Full),
            "compact" => Some(Verbosity::Compact),
            "terse" => Some(Verbosity::Terse),
            _ => None,
        }
    }

    /// The valid level names, for error messages.
    pub fn valid_names() -> &'static [&'static str] {
        &["full", "compact", "terse"]
    }
}

impl std::fmt::Display for Verbosity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_full() {
        assert_eq!(Verbosity::default(), Verbosity::Full);
    }

    #[test]
    fn as_str_round_trips_through_from_input() {
        for v in [Verbosity::Full, Verbosity::Compact, Verbosity::Terse] {
            assert_eq!(Verbosity::from_input(v.as_str()), Some(v));
            assert_eq!(v.to_string(), v.as_str());
        }
    }

    #[test]
    fn from_input_is_case_and_whitespace_insensitive() {
        assert_eq!(Verbosity::from_input("  FULL "), Some(Verbosity::Full));
        assert_eq!(Verbosity::from_input("Compact"), Some(Verbosity::Compact));
        assert_eq!(Verbosity::from_input("TERSE"), Some(Verbosity::Terse));
    }

    #[test]
    fn from_input_rejects_unknown() {
        assert_eq!(Verbosity::from_input("loud"), None);
        assert_eq!(Verbosity::from_input(""), None);
    }

    #[test]
    fn deserializes_from_lowercase_string() {
        // Mirrors how `[mcp] default_verbosity = "terse"` is parsed.
        let v: Verbosity = serde_json::from_str("\"terse\"").unwrap();
        assert_eq!(v, Verbosity::Terse);
        assert!(serde_json::from_str::<Verbosity>("\"nope\"").is_err());
    }
}
