//! Caller-owned run-scoped state authority (ADR-021, WP01).
//!
//! Context: docs/contexts/reviewer-runtime/CONTEXT.md and the interface ledger
//! docs/contexts/reviewer-runtime/INTERFACES.md. Decision: ADR-021
//! (docs/adr/021-caller-owned-run-authority.md).
//!
//! When the caller selects an explicit `--state-dir`, every internal writer and
//! private temporary artifact resolves below that single, immutable,
//! run-scoped root — never HOME, XDG, cwd, or a temporary-directory fallback.
//! The authority is resolved once, before any discovery-dependent service or
//! writer starts, and is threaded through constructors as an immutable value.
//! It is deliberately **not** a process-global environment or static redirect,
//! so independent sessions in one process keep independent roots.
//!
//! The root must already exist, be a writable directory, and resolve (after
//! following symlinks) to a location that does not overlap the run's workspace
//! (the evidence tree). An absent, unusable, or overlapping root fails the run
//! before any fallback or artifact creation. When no `--state-dir` is supplied
//! the authority is absent and legacy path resolution (`crate::paths`) applies
//! unchanged.

use std::fmt;
use std::path::{Path, PathBuf};

/// A machine-readable run failure carrying the stable run code, CLI exit code,
/// and a bounded human message. State failures use exit 2
/// (`run_configuration_invalid`) with the reason codes reserved in the
/// interface ledger. Shared across run-authority consumers; owned here because
/// `state_root` is the first authority module to resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunFailure {
    /// Stable run/reason code, e.g. `state_unusable`.
    pub code: &'static str,
    /// CLI exit code associated with the run code.
    pub exit_code: i32,
    /// Bounded, privacy-safe message. Never contains evidence content.
    pub message: String,
}

impl RunFailure {
    /// A requested state root is missing, not a directory, or not writable.
    pub fn state_unusable(message: impl Into<String>) -> Self {
        Self {
            code: "state_unusable",
            exit_code: 2,
            message: message.into(),
        }
    }

    /// A requested state root overlaps the run's workspace (evidence) tree.
    pub fn state_overlaps_workspace(message: impl Into<String>) -> Self {
        Self {
            code: "state_overlaps_workspace",
            exit_code: 2,
            message: message.into(),
        }
    }

    /// A generic `run_configuration_invalid` (exit 2) with an explicit reason.
    pub fn configuration_invalid(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            exit_code: 2,
            message: message.into(),
        }
    }
}

impl fmt::Display for RunFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for RunFailure {}

/// Reject existing files that share an inode with another path (ADR-021).
/// This check does not protect against concurrent filesystem changes.
pub(crate) fn verify_unshared_file(path: &Path) -> std::io::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "state file must not be a symlink",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "state file must not have multiple hard links",
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = metadata;
    Ok(())
}

/// Check a SQLite database and its WAL, shared-memory and journal files.
pub(crate) fn verify_unshared_sqlite_files(path: &Path) -> std::io::Result<()> {
    verify_unshared_file(path)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        verify_unshared_file(Path::new(&sidecar))?;
    }
    Ok(())
}

/// Stable identity for one workspace (evidence) tree, derived from the
/// canonicalized workspace path. The full SHA-256 hex digest is used so two
/// distinct workspaces cannot collide onto one sub-tree. The raw OS bytes of
/// the path are hashed (not a lossy UTF-8 rendering) so two distinct non-UTF-8
/// workspace paths cannot silently collapse onto the same id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceId(String);

impl WorkspaceId {
    /// Derive the stable id from a workspace path. Canonicalization is
    /// best-effort: an unresolvable path falls back to its lexical form so the
    /// id is still stable for a given string. The hash is taken over the raw OS
    /// bytes of the path so distinct non-UTF-8 paths stay distinct.
    pub fn from_workspace(workspace: &Path) -> Self {
        use sha2::{Digest, Sha256};
        use std::os::unix::ffi::OsStrExt;
        let canonical =
            std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_os_str().as_bytes());
        let digest = hasher.finalize();
        let key: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        Self(key)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A reserved internal-state destination. Keys are a fixed, reviewed set — not
/// caller-controlled relative paths. Workspace-scoped keys live under
/// `workspaces/<id>/<segment>`; global keys live directly under the root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateKey {
    // Workspace-scoped (require a WorkspaceId).
    Index,
    Snapshots,
    Pipeline,
    Kgl,
    Checkpoints,
    Evidence,
    // Run/global.
    Sessions,
    Catalog,
    Coordination,
    Analytics,
    Logs,
    ToolOutput,
    Oauth,
    Scratch,
    Sockets,
}

impl StateKey {
    /// Reserved path segment for this key. These spellings are fixed by the
    /// interface ledger's state-layout reservation.
    fn segment(self) -> &'static str {
        match self {
            StateKey::Index => "index",
            StateKey::Snapshots => "snapshots",
            StateKey::Pipeline => "pipeline",
            StateKey::Kgl => "kgl",
            StateKey::Checkpoints => "checkpoints",
            StateKey::Evidence => "evidence",
            StateKey::Sessions => "sessions",
            StateKey::Catalog => "catalog",
            StateKey::Coordination => "coordination",
            StateKey::Analytics => "analytics",
            StateKey::Logs => "logs",
            StateKey::ToolOutput => "tool-output",
            StateKey::Oauth => "oauth",
            StateKey::Scratch => "scratch",
            StateKey::Sockets => "sockets",
        }
    }

    /// Whether this destination is partitioned per workspace.
    fn is_workspace_scoped(self) -> bool {
        matches!(
            self,
            StateKey::Index
                | StateKey::Snapshots
                | StateKey::Pipeline
                | StateKey::Kgl
                | StateKey::Checkpoints
                | StateKey::Evidence
        )
    }
}

/// The session store used by a supported frontend.
#[derive(Debug, Clone, Copy)]
pub(crate) enum SessionStoreKind {
    Daemon,
    Acp,
    Chat,
}

/// Immutable, run-scoped authority over internal state placement. Constructed
/// once from an explicit caller `--state-dir`; cloned freely and threaded
/// through constructors. Carries no mutable or process-global state.
#[derive(Debug, Clone)]
pub struct StateAuthority {
    /// Canonicalized, validated root path. This is a path *value*, not an open
    /// descriptor: it is not an fd pin, so a sufficiently privileged concurrent
    /// rename of an ancestor is not guarded against. At construction time the
    /// directory existed, was caller-private, and was effectively writable.
    root: PathBuf,
}

impl StateAuthority {
    /// Resolve an explicit caller-supplied state root against the run's
    /// `workspace` (evidence tree).
    ///
    /// Validates that the root exists, is a writable directory, and — after
    /// symlink resolution of both paths — does not overlap the workspace in
    /// either direction. Does **not** create or copy the root: it must already
    /// exist. On any failure returns a [`RunFailure`] (exit 2) and creates no
    /// artifact.
    pub fn explicit(state_dir: &Path, workspace: &Path) -> Result<Self, RunFailure> {
        let root = std::fs::canonicalize(state_dir).map_err(|e| {
            RunFailure::state_unusable(format!(
                "state root {} is unusable: {e}",
                state_dir.display()
            ))
        })?;

        let meta = std::fs::metadata(&root).map_err(|e| {
            RunFailure::state_unusable(format!("state root {} is unusable: {e}", root.display()))
        })?;
        if !meta.is_dir() {
            return Err(RunFailure::state_unusable(format!(
                "state root {} is not a directory",
                root.display()
            )));
        }

        // Caller-private contract: the root must be owned by this effective user
        // and grant no access to group or other (ADR-021).
        Self::enforce_private_ownership(&root, &meta)?;

        // Overlap guard: both paths are canonicalized so a symlinked ancestor or
        // descendant cannot smuggle state into the evidence tree. Requested mode
        // fails closed: if the workspace cannot be canonicalized we cannot prove
        // non-overlap, so we refuse rather than fall back to a lexical compare.
        // This precedes the write probe so a rejected (overlapping) root never
        // receives a probe artifact inside the evidence tree.
        let ws = std::fs::canonicalize(workspace).map_err(|e| {
            RunFailure::state_unusable(format!(
                "workspace {} cannot be resolved to check state overlap: {e}",
                workspace.display()
            ))
        })?;
        if !ws.is_dir() {
            return Err(RunFailure::state_unusable(format!(
                "workspace {} is not a directory",
                ws.display()
            )));
        }
        if root == ws || root.starts_with(&ws) || ws.starts_with(&root) {
            return Err(RunFailure::state_overlaps_workspace(format!(
                "state root {} overlaps the workspace {}",
                root.display(),
                ws.display()
            )));
        }

        // Effective writability: `Permissions::readonly` reflects only the owner
        // write bit, not ACLs, a read-only mount, or a container overlay. Probe
        // with a uniquely named file that is removed immediately. Only reached
        // once the root is proven non-overlapping, so no probe is ever written
        // into the evidence tree.
        Self::probe_writable(&root)?;

        Ok(Self { root })
    }

    /// Reject a root that is not caller-private: it must be owned by the current
    /// effective user and expose no group/other access bit at all. Caller-owned
    /// without group/other *write* is weaker than inaccessible private state;
    /// this is sensitive run control state, so any group/other read/write/exec
    /// bit disqualifies it (ADR-021).
    #[cfg(unix)]
    fn enforce_private_ownership(root: &Path, meta: &std::fs::Metadata) -> Result<(), RunFailure> {
        use std::os::unix::fs::MetadataExt;
        let euid = unsafe { libc::geteuid() };
        if meta.uid() != euid {
            return Err(RunFailure::state_unusable(format!(
                "state root {} is not owned by the current user",
                root.display()
            )));
        }
        if meta.mode() & 0o077 != 0 {
            return Err(RunFailure::state_unusable(format!(
                "state root {} is group- or world-accessible; a private (0700) root is required",
                root.display()
            )));
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn enforce_private_ownership(
        _root: &Path,
        _meta: &std::fs::Metadata,
    ) -> Result<(), RunFailure> {
        Ok(())
    }

    /// Confirm the caller can actually create a file in `root` by writing and
    /// removing a uniquely named probe. Any failure maps to `state_unusable`.
    fn probe_writable(root: &Path) -> Result<(), RunFailure> {
        let probe = root.join(format!(
            ".daimonos-state-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
        {
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
                Ok(())
            }
            Err(e) => Err(RunFailure::state_unusable(format!(
                "state root {} is not writable: {e}",
                root.display()
            ))),
        }
    }

    /// Resolve a caller-configured internal destination under this authority.
    ///
    /// Resolve existing ancestors, including symlinks, before accepting a
    /// not-yet-created suffix. This is not a sandbox against concurrent
    /// renames of caller-owned state. Context: ADR-021.
    pub(crate) fn explicit_destination(
        &self,
        option: &str,
        value: &Path,
    ) -> Result<PathBuf, RunFailure> {
        use std::path::Component;
        let input = value.display();
        let path = value
            .to_str()
            .map(crate::paths::expand_tilde)
            .unwrap_or_else(|| value.to_path_buf());
        let absolute = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .map_err(|error| RunFailure::state_unusable(format!("{option} {input}: {error}")))?
                .join(path)
        };
        if absolute
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(RunFailure::state_unusable(format!(
                "{option} {input} must not contain '..' under --state-dir"
            )));
        }
        let mut ancestor = absolute.as_path();
        let mut suffix = Vec::new();
        let mut resolved = loop {
            match std::fs::symlink_metadata(ancestor) {
                Ok(_) => {
                    break std::fs::canonicalize(ancestor).map_err(|error| {
                        RunFailure::state_unusable(format!("{option} {input}: {error}"))
                    })?
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let name = ancestor.file_name().ok_or_else(|| {
                        RunFailure::state_unusable(format!(
                            "{option} {input} has no existing ancestor"
                        ))
                    })?;
                    suffix.push(name.to_os_string());
                    ancestor = ancestor.parent().ok_or_else(|| {
                        RunFailure::state_unusable(format!(
                            "{option} {input} has no existing ancestor"
                        ))
                    })?;
                }
                Err(error) => {
                    return Err(RunFailure::state_unusable(format!(
                        "{option} {input}: {error}"
                    )))
                }
            }
        };
        for component in suffix.iter().rev() {
            resolved.push(component);
        }
        if !resolved.starts_with(&self.root) {
            return Err(RunFailure::state_unusable(format!(
                "{option} {input} is outside --state-dir {}",
                self.root.display()
            )));
        }
        Ok(resolved)
    }

    /// Verify a future internal directory without creating the directory itself.
    /// Probe the nearest existing ancestor so unused caches stay absent. ADR-021.
    pub(crate) fn verify_writable_directory(
        &self,
        option: &str,
        directory: &Path,
    ) -> Result<(), RunFailure> {
        let resolved = self.explicit_destination(option, directory)?;
        let mut ancestor = resolved.as_path();
        loop {
            match std::fs::symlink_metadata(ancestor) {
                Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                    return Err(RunFailure::state_unusable(format!(
                        "{option} {} has an unusable directory component {}",
                        directory.display(),
                        ancestor.display()
                    )));
                }
                Ok(_) => {
                    return Self::probe_writable(ancestor).map_err(|error| {
                        RunFailure::state_unusable(format!(
                            "{option} {}: {}",
                            directory.display(),
                            error.message
                        ))
                    })
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    ancestor = ancestor.parent().ok_or_else(|| {
                        RunFailure::state_unusable(format!(
                            "{option} {} has no existing ancestor",
                            directory.display()
                        ))
                    })?;
                }
                Err(error) => {
                    return Err(RunFailure::state_unusable(format!(
                        "{option} {}: {error}",
                        directory.display()
                    )))
                }
            }
        }
    }

    /// Validate one frontend's session store before its writers start (ADR-021).
    pub(crate) fn verify_session_store(
        &self,
        kind: SessionStoreKind,
    ) -> Result<PathBuf, RunFailure> {
        let name = match kind {
            SessionStoreKind::Daemon => "daemon",
            SessionStoreKind::Acp => "acp",
            SessionStoreKind::Chat => "chat",
        };
        let directory = self.resolve_trusted(&["sessions", name])?;
        self.verify_writable_directory("session store", &directory)?;
        let database = directory.join(crate::session_store::SESSION_DATABASE_NAME);
        self.explicit_destination("session database", &database)?;
        verify_unshared_sqlite_files(&database).map_err(|error| {
            RunFailure::state_unusable(format!("session database {}: {error}", database.display()))
        })?;
        Ok(directory)
    }

    /// Validate the selected-state approval file before frontend startup (ADR-021).
    pub(crate) fn verify_approvals(&self) -> Result<PathBuf, RunFailure> {
        let directory = self.resolve_trusted(&["sessions", "approvals"])?;
        self.verify_writable_directory("approvals", &directory)?;
        let file = directory.join("agent-approvals");
        self.explicit_destination("approvals", &file)?;
        verify_unshared_file(&file).map_err(|error| {
            RunFailure::state_unusable(format!("approvals {}: {error}", file.display()))
        })?;
        Ok(file)
    }

    /// The validated root path (a canonical value, not an open descriptor).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve the directory for an internal destination below the validated
    /// root. Creates nothing; child directories are created by their writer
    /// when needed. Each fixed layout component between the root and the
    /// destination is checked with `symlink_metadata` (an `lstat`, never
    /// following targets): if a pre-existing component is a symlink the resolve
    /// is rejected, so a planted child such as `logs -> /outside` cannot
    /// redirect an internal writer out of the caller-private root. Only the
    /// root itself may be a (already canonicalized) resolved path. This is a
    /// per-call read of the caller's own control state — no global state and no
    /// claim of adversarial-race sandboxing. Workspace-scoped keys require a
    /// [`WorkspaceId`].
    pub fn resolve(
        &self,
        key: StateKey,
        workspace: Option<&WorkspaceId>,
    ) -> Result<PathBuf, RunFailure> {
        if key.is_workspace_scoped() {
            let id = workspace.ok_or_else(|| {
                RunFailure::configuration_invalid(
                    "state_unusable",
                    format!("state key {key:?} requires a workspace id"),
                )
            })?;
            self.resolve_trusted(&["workspaces", id.as_str(), key.segment()])
        } else {
            self.resolve_trusted(&[key.segment()])
        }
    }

    /// Join `segments` below the validated root, rejecting any pre-existing
    /// component that is a symlink (checked without following its target). A
    /// not-yet-created component ends the walk: nothing below it exists to
    /// redirect, and the writer will create it under the trusted root.
    fn resolve_trusted(&self, segments: &[&str]) -> Result<PathBuf, RunFailure> {
        let mut path = self.root.clone();
        let mut checking = true;
        for seg in segments {
            path.push(seg);
            if !checking {
                continue;
            }
            match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(RunFailure::state_unusable(format!(
                        "state layout component {} is a symlink; the internal layout \
                         must not be redirected outside the state root",
                        path.display()
                    )));
                }
                Ok(meta) if !meta.is_dir() => {
                    return Err(RunFailure::state_unusable(format!(
                        "state layout component {} is not a directory",
                        path.display()
                    )));
                }
                Ok(_) => {}
                // First non-existent component: nothing below it can exist to
                // redirect, but keep assembling the full destination path.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => checking = false,
                Err(error) => {
                    return Err(RunFailure::state_unusable(format!(
                        "cannot inspect state layout component {}: {error}",
                        path.display()
                    )))
                }
            }
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TestDir(tempfile::TempDir);

    impl std::ops::Deref for TestDir {
        type Target = Path;

        fn deref(&self) -> &Path {
            self.0.path()
        }
    }

    impl AsRef<Path> for TestDir {
        fn as_ref(&self) -> &Path {
            self.0.path()
        }
    }

    fn tmp(label: &str) -> TestDir {
        let dir = tempfile::Builder::new()
            .prefix(&format!("daimonos-state-test-{label}-"))
            .tempdir()
            .unwrap();
        // Owned roots are private control state: 0700, no group/other access.
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        TestDir(dir)
    }

    /// Create a private (0700) directory at `path`.
    fn mkdir_private(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::create_dir_all(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn workspace_id_is_stable_and_distinct() {
        let a = WorkspaceId::from_workspace(Path::new("/nonexistent/alpha"));
        let b = WorkspaceId::from_workspace(Path::new("/nonexistent/alpha"));
        let c = WorkspaceId::from_workspace(Path::new("/nonexistent/beta"));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.as_str().len(), 64);
    }

    #[test]
    fn explicit_root_must_exist() {
        let ws = tmp("ws-missing");
        let missing = ws.join("does-not-exist");
        let err = StateAuthority::explicit(&missing, &ws).unwrap_err();
        assert_eq!(err.code, "state_unusable");
        assert_eq!(err.exit_code, 2);
    }

    #[test]
    fn explicit_root_rejects_non_directory() {
        let root = tmp("root-file");
        let file = root.join("f");
        fs::write(&file, b"x").unwrap();
        let ws = tmp("ws-file");
        let err = StateAuthority::explicit(&file, &ws).unwrap_err();
        assert_eq!(err.code, "state_unusable");
    }

    #[test]
    fn explicit_root_rejects_overlap_with_workspace() {
        // Root inside the workspace.
        let ws = tmp("ws-overlap");
        let inside = ws.join("state");
        mkdir_private(&inside);
        let err = StateAuthority::explicit(&inside, &ws).unwrap_err();
        assert_eq!(err.code, "state_overlaps_workspace");

        // Workspace inside the root.
        let root = tmp("root-overlap");
        let nested_ws = root.join("ws");
        fs::create_dir_all(&nested_ws).unwrap();
        let err = StateAuthority::explicit(&root, &nested_ws).unwrap_err();
        assert_eq!(err.code, "state_overlaps_workspace");

        // Identical paths.
        let same = tmp("same");
        let err = StateAuthority::explicit(&same, &same).unwrap_err();
        assert_eq!(err.code, "state_overlaps_workspace");
    }

    #[test]
    fn overlap_guard_follows_symlinks() {
        // A symlinked root that resolves inside the workspace must be rejected.
        let ws = tmp("ws-symlink");
        let real_inside = ws.join("real-state");
        mkdir_private(&real_inside);
        let link_parent = tmp("link-root");
        let link_root = link_parent.join("link");
        std::os::unix::fs::symlink(&real_inside, &link_root).unwrap();
        let err = StateAuthority::explicit(&link_root, &ws).unwrap_err();
        assert_eq!(err.code, "state_overlaps_workspace");
    }

    #[test]
    fn explicit_fails_closed_when_workspace_cannot_be_canonicalized() {
        // Requested mode must refuse rather than fall back to a lexical compare
        // when the workspace path does not resolve: non-overlap is unprovable.
        let root = tmp("root-ws-missing");
        let bogus_ws = std::env::temp_dir().join(format!(
            "daimonos-no-such-ws-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let err = StateAuthority::explicit(&root, &bogus_ws).unwrap_err();
        assert_eq!(err.code, "state_unusable");
        assert_eq!(err.exit_code, 2);
    }

    #[test]
    fn explicit_rejects_group_or_world_writable_root() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("root-shared");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();
        let ws = tmp("ws-shared");
        let err = StateAuthority::explicit(&root, &ws).unwrap_err();
        assert_eq!(err.code, "state_unusable");
    }

    #[test]
    fn explicit_rejects_group_or_world_readable_root() {
        // Caller-owned with no group/other *write* is weaker than inaccessible
        // private state. A merely group-readable root (0750, no group write)
        // must still be refused: sensitive control state demands 0700.
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("root-grp-read");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o750)).unwrap();
        let ws = tmp("ws-grp-read");
        let err = StateAuthority::explicit(&root, &ws).unwrap_err();
        assert_eq!(err.code, "state_unusable");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn explicit_rejects_effectively_unwritable_root() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("root-ro");
        // Owner r-x only: `readonly()` on the Permissions alone would still allow
        // this through on some checks, but the write probe fails.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o500)).unwrap();
        let ws = tmp("ws-ro");
        let err = StateAuthority::explicit(&root, &ws).unwrap_err();
        assert_eq!(err.code, "state_unusable");
        // Restore so the tempdir can be cleaned up.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn valid_root_resolves_reserved_layout() {
        let root = tmp("root-ok");
        let ws = tmp("ws-ok");
        let auth = StateAuthority::explicit(&root, &ws).unwrap();
        let canonical_root = fs::canonicalize(&root).unwrap();
        assert_eq!(auth.root(), canonical_root.as_path());

        let wsid = WorkspaceId::from_workspace(&ws);

        // Global keys resolve directly under the root.
        assert_eq!(
            auth.resolve(StateKey::Analytics, None).unwrap(),
            canonical_root.join("analytics")
        );
        assert_eq!(
            auth.resolve(StateKey::Coordination, None).unwrap(),
            canonical_root.join("coordination")
        );
        assert_eq!(
            auth.resolve(StateKey::ToolOutput, None).unwrap(),
            canonical_root.join("tool-output")
        );

        // Workspace-scoped keys partition under workspaces/<id>/.
        assert_eq!(
            auth.resolve(StateKey::Snapshots, Some(&wsid)).unwrap(),
            canonical_root
                .join("workspaces")
                .join(wsid.as_str())
                .join("snapshots")
        );
        assert_eq!(
            auth.resolve(StateKey::Evidence, Some(&wsid)).unwrap(),
            canonical_root
                .join("workspaces")
                .join(wsid.as_str())
                .join("evidence")
        );
    }

    #[test]
    fn workspace_key_without_id_fails() {
        let root = tmp("root-noid");
        let ws = tmp("ws-noid");
        let auth = StateAuthority::explicit(&root, &ws).unwrap();
        let err = auth.resolve(StateKey::Index, None).unwrap_err();
        assert_eq!(err.exit_code, 2);
    }

    #[test]
    fn resolve_has_no_filesystem_side_effects() {
        let root = tmp("root-pure");
        let ws = tmp("ws-pure");
        let auth = StateAuthority::explicit(&root, &ws).unwrap();
        let wsid = WorkspaceId::from_workspace(&ws);
        let p = auth.resolve(StateKey::Index, Some(&wsid)).unwrap();
        assert!(!p.exists(), "resolve must not create the directory");
    }

    #[test]
    fn overlap_rejection_leaves_root_untouched() {
        // An overlapping (forbidden) root must be rejected BEFORE the write
        // probe, so no probe artifact is created inside the evidence tree and
        // the directory's mtime is unchanged.
        let ws = tmp("ws-notouch");
        let inside = ws.join("state");
        mkdir_private(&inside);
        let before = fs::metadata(&inside).unwrap().modified().unwrap();

        let err = StateAuthority::explicit(&inside, &ws).unwrap_err();
        assert_eq!(err.code, "state_overlaps_workspace");

        let after = fs::metadata(&inside).unwrap().modified().unwrap();
        assert_eq!(
            before, after,
            "overlap rejection must not write a probe into the state root"
        );
        let entries: Vec<_> = fs::read_dir(&inside).unwrap().collect();
        assert!(
            entries.is_empty(),
            "overlap rejection must leave the root empty (no probe artifact)"
        );
    }

    #[test]
    fn resolve_rejects_symlinked_global_component() {
        // A planted child such as `logs -> /outside` must not redirect a
        // writer out of the root. resolve rejects the symlinked component
        // without following its target; an untouched sibling key still works.
        let root = tmp("root-sym-global");
        let ws = tmp("ws-sym-global");
        let auth = StateAuthority::explicit(&root, &ws).unwrap();
        let outside = tmp("outside-logs");
        std::os::unix::fs::symlink(&outside, auth.root().join("logs")).unwrap();

        let err = auth.resolve(StateKey::Logs, None).unwrap_err();
        assert_eq!(err.code, "state_unusable");
        assert_eq!(err.exit_code, 2);
        assert!(auth.resolve(StateKey::Analytics, None).is_ok());
    }

    #[test]
    fn resolve_rejects_symlinked_workspace_component() {
        let root = tmp("root-sym-ws");
        let ws = tmp("ws-sym-ws");
        let auth = StateAuthority::explicit(&root, &ws).unwrap();
        let wsid = WorkspaceId::from_workspace(&ws);
        let outside = tmp("outside-workspaces");
        std::os::unix::fs::symlink(&outside, auth.root().join("workspaces")).unwrap();

        let err = auth.resolve(StateKey::Index, Some(&wsid)).unwrap_err();
        assert_eq!(err.code, "state_unusable");
    }
}
