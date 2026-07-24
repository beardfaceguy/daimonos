//! Agent-to-agent coordination ("agent mail") — native, in-daemon, per-workspace
//! (ADR-009; vikunja #1057). Lets multiple daimonos processes working on one
//! workspace share identity, exchange directed messages, and post advisory file
//! reservations, all over a single dedicated SQLite database.
//!
//! ## Store & sharing model (ADR-009 D1)
//!
//! One dedicated SQLite DB **per workspace**, at
//! `~/.daimonos/coordination/<workspace-key>.db`, opened directly by every
//! daimonos process in WAL mode — no broker, no HTTP, no auth. Processes
//! coordinate purely by reading and writing rows (the same pattern
//! `src/kgl/store.rs` and `src/analytics.rs` already use safely across
//! concurrent processes). The DB never reads from or writes to any other
//! daimonos database.
//!
//! ## Hard constraints (ADR-009 / #1053)
//!
//! - **No unbounded recursion** in any read/traversal path — bounded SQL +
//!   capped loops only (slice 1 has no traversal; the discipline holds anyway).
//! - **Panic-free reads:** every DB call is `Result`-typed and `?`-propagated;
//!   no `unwrap`/`expect` on a read path.
//! - **Fail-open:** callers wrap the store so an unopenable/broken DB yields a
//!   soft error and the agent's turn continues (see `ops::coord`).
//! - **Single source of truth:** this DB only; zero cross-DB reads.

pub mod names;
pub mod store;

pub use store::{
    AgentRecord, CoordinationStore, Importance, InboxEntry, InboxFilter, MessageRecord,
};

use std::path::Path;

/// Compute the per-workspace coordination DB path under the global daimonos
/// state dir: `<base>/coordination/<workspace-key>.db`, where `<workspace-key>`
/// is a stable hash of the *canonicalized* workspace path. This keeps the DB
/// out of the target repo tree (daimonos is an installed binary) and makes two
/// processes on the same workspace share one file (ADR-009 D1).
///
/// `base` is the coordination base dir (default `~/.daimonos/coordination`,
/// overridable via `[coordination] db_dir`). Canonicalization is best-effort:
/// if the path can't be canonicalized (e.g. it doesn't exist yet) we fall back
/// to the lexical path, so the key is still stable for a given string.
pub fn workspace_db_path(base: &Path, workspace: &Path) -> std::path::PathBuf {
    use sha2::{Digest, Sha256};
    let canonical = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    // Full 256-bit digest as hex (64 chars). A truncated key could let two
    // distinct workspaces collide onto one DB and cross-contaminate identity
    // and messages across the trust boundary (ADR-009: the workspace IS the
    // trust boundary), so we keep the whole digest — the filename length is a
    // non-issue for a hidden state file.
    let key: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    base.join(format!("{key}.db"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn workspace_db_path_is_stable_for_same_workspace() {
        let base = PathBuf::from("/home/u/.daimonos/coordination");
        let ws = PathBuf::from("/nonexistent/workspace/alpha");
        let a = workspace_db_path(&base, &ws);
        let b = workspace_db_path(&base, &ws);
        assert_eq!(a, b, "same workspace must map to the same DB path");
        assert!(a.starts_with(&base));
        assert_eq!(a.extension().and_then(|e| e.to_str()), Some("db"));
    }

    #[test]
    fn workspace_db_path_differs_across_workspaces() {
        let base = PathBuf::from("/home/u/.daimonos/coordination");
        let a = workspace_db_path(&base, &PathBuf::from("/nonexistent/alpha"));
        let b = workspace_db_path(&base, &PathBuf::from("/nonexistent/beta"));
        assert_ne!(a, b, "different workspaces must map to different DBs");
    }
}
