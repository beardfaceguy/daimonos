//! Central run-admission table for `--workspace-read-only` (FR-07, WP04).
//!
//! Context: docs/adr/022-shared-filesystem-authority.md — read-only is an
//! independent option that applies read-only permission to *all* deliberate
//! workspace mutations and refuses opaque local tools whose effects cannot be
//! mediated. Reviewer-runtime INTERFACES.md reserves this module's public names
//! (`ToolAdmission`, `EffectClass`) and the refusal codes
//! `workspace_write_forbidden` / `tool_policy_unsupported`; INVENTORIES.md's
//! tool-class table is the binding classification source.
//!
//! This is the single place that decides, per tool/opcode, whether a request may
//! proceed when the workspace is read-only. Every dispatch boundary
//! (`ops::dispatch_op`, `mcp::dispatch_tool_inner`, `run_execute_script`) asks
//! this table before any effect begins. It never hides a tool or suppresses an
//! error; it refuses before execution with a named code. An allowlist entry
//! cannot override a refusal here (INVENTORIES tool-class table).
//!
//! Internal state writes are *not* workspace mutations: they are routed under
//! the explicit state root by WP01/WP02 (ADR-021). This table classifies them
//! as `Internal` and admits them — it does not duplicate writer routing.
//!
//! Refusal metadata exemption (R2): the `ToolFailure` `message` strings built in
//! [`ToolAdmission::refuse`] are structured refusal metadata keyed by a reviewed
//! `code` (INTERFACES.md's machine-readable contract), not model-facing prompts,
//! so — exactly like the merged WP03 `path_policy::ToolFailure` messages — they
//! stay in Rust rather than `prompts/` (AGENTS.md's prompt rule governs prompts
//! and top-level tool descriptions, not refusal-code metadata).

use crate::path_policy::ToolFailure;
use crate::protocol::op;
use std::path::{Component, Path, PathBuf};

/// Refusal code for a requested mutation of the read-only evidence workspace.
pub const CODE_WORKSPACE_WRITE_FORBIDDEN: &str = "workspace_write_forbidden";
/// Refusal code for an opaque tool whose effects cannot be mediated read-only.
pub const CODE_TOOL_POLICY_UNSUPPORTED: &str = "tool_policy_unsupported";
/// Refusal code for a tool not on the explicit `--tool-allowlist` (FR-02, WP06).
/// Reserved in reviewer-runtime INTERFACES.md and `run_audit::REFUSAL_CODES`, so
/// a boundary result carrying it is recorded as a `tool_refusal` by WP08.
pub const CODE_TOOL_NOT_ALLOWED: &str = "tool_not_allowed";

/// The explicit run allowlist (FR-02, ADR-023), kept separate from the legacy
/// policy and from the read-only [`ToolAdmission`]. It is the one immutable
/// authority consulted at every direct and delegated dispatch boundary
/// (`mcp::dispatch_tool_inner`, `script::dispatch_tool_by_name`, batch children,
/// the `execute_script` branches, and the remote/worker hook) and at every
/// model-visible listing (tools/list, `list_all_tools`, the agent catalog).
///
/// `None` preserves current behavior (no explicit list — legacy). `Some(set)`
/// is an explicit selection: an empty set grants NO tools (a deliberate
/// deny-all), which is distinct from `None`. Nested composite calls can never
/// reset or widen it, and workspace/environment settings can never add to it
/// (it is built only from the caller's `--tool-allowlist`).
///
/// This allowlist can only *narrow*: it is an additional restrictive filter
/// layered on top of the legacy `SafetyPolicy` restrictions and the read-only /
/// confinement admissions, which remain stronger and may still refuse an
/// otherwise-listed tool (INTERFACES.md). Context: src/run_policy.rs.
#[derive(Debug, Clone, Default)]
pub struct ToolPolicy {
    allowlist: Option<std::collections::BTreeSet<String>>,
}

impl ToolPolicy {
    /// No explicit allowlist: legacy behavior, every tool is allowed.
    pub fn unrestricted() -> Self {
        ToolPolicy { allowlist: None }
    }

    /// An explicit allowlist. An empty iterator yields a deny-all policy.
    pub fn explicit<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        ToolPolicy {
            allowlist: Some(names.into_iter().map(Into::into).collect()),
        }
    }

    /// Build from the caller-owned config value (`None` => legacy).
    pub fn from_config(names: Option<&Vec<String>>) -> Self {
        match names {
            None => Self::unrestricted(),
            Some(list) => Self::explicit(list.iter().cloned()),
        }
    }

    /// True when an explicit allowlist is in force (even an empty one).
    pub fn is_active(&self) -> bool {
        self.allowlist.is_some()
    }

    /// Whether `name` is permitted by the explicit allowlist. With no explicit
    /// list every name is allowed (legacy). This never widens another policy:
    /// read-only/confinement admissions are checked separately and are stronger.
    pub fn allows(&self, name: &str) -> bool {
        match &self.allowlist {
            None => true,
            Some(set) => set.contains(name),
        }
    }

    /// Admit (or refuse) a tool by exported name at a dispatch boundary. The
    /// refusal carries the reviewed `tool_not_allowed` code and names the tool
    /// without exposing any denied payload (ADR-023).
    pub fn admit(&self, name: &str) -> Result<(), ToolFailure> {
        if self.allows(name) {
            Ok(())
        } else {
            Err(ToolFailure {
                code: CODE_TOOL_NOT_ALLOWED.to_string(),
                message: format!("tool {name} is not on the explicit tool allowlist"),
                tool: Some(name.to_string()),
                argument: None,
                budget: None,
                retryable: false,
            })
        }
    }

    /// The effective allowlist as a sorted vector, or `None` when no explicit
    /// list is in force. Intersected with the legacy restriction when one is
    /// supplied (the allowlist can only narrow it, never widen it — the
    /// reported set is what the run will actually permit).
    pub fn effective(&self, legacy_allows: Option<&dyn Fn(&str) -> bool>) -> Option<Vec<String>> {
        self.allowlist.as_ref().map(|set| {
            set.iter()
                .filter(|name| legacy_allows.map(|f| f(name)).unwrap_or(true))
                .cloned()
                .collect()
        })
    }
}

/// How a tool/opcode affects the world, for read-only admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectClass {
    /// A mediated read: file reads, search, listing, metadata, diff, git_read,
    /// schema/introspection. Safe under read-only.
    MediatedRead,
    /// An internal effect that is routed under the explicit state root (never
    /// the workspace): coordination, session env, session info, snapshot
    /// listing. Safe under read-only.
    Internal,
    /// A deliberate workspace mutation: writes, edits, appends, snapshot
    /// create, restore, snapshot delete. Refused read-only.
    WorkspaceMutation,
    /// An opaque effect that cannot be mediated: arbitrary exec, background
    /// processes, repair helpers, tool pipelines/registration, external CLI
    /// plugins, and `execute_script`. Refused read-only.
    OpaqueEffect,
}

impl EffectClass {
    /// True when this class may proceed while the workspace is read-only.
    pub fn is_admitted_read_only(self) -> bool {
        matches!(self, EffectClass::MediatedRead | EffectClass::Internal)
    }
}

/// Classify a native protocol opcode. Opcodes reach this table through
/// `tool_facade::invoke` -> `ops::dispatch_op` (MCP ops-backed tools, every
/// batch child, and the agent loop).
pub fn classify_opcode(code: u8) -> EffectClass {
    match code {
        op::READ | op::LS | op::STAT | op::GLOB | op::GREP | op::FIND | op::DIFF | op::SCHEMA => {
            EffectClass::MediatedRead
        }
        // Session-scoped or state-routed effects; never touch the workspace.
        op::SESSION | op::ENV_GET | op::ENV_SET | op::COORD | op::TOOL_LIST | op::SNAP_LIST => {
            EffectClass::Internal
        }
        op::WRITE | op::PATCH | op::APPEND | op::SNAP | op::RESTORE | op::SNAP_DELETE => {
            EffectClass::WorkspaceMutation
        }
        // EXEC/BG/POLL/KILL (arbitrary processes), TOOL_RUN (external plugins),
        // LINT_REPAIR (repair helper), TOOL_PIPELINE, TOOL_REGISTER, and any
        // other/unknown opcode: opaque, cannot be mediated read-only.
        _ => EffectClass::OpaqueEffect,
    }
}

/// Classify a non-opcode tool by exported name. Only tools whose dispatch does
/// NOT go through an opcode reach this path (`tools::build_request` returns
/// `None`): external-CLI plugins and read/meta tools handled directly in
/// `mcp::dispatch_tool_inner`, plus `execute_script`.
pub fn classify_tool(name: &str) -> EffectClass {
    match name {
        // Mediated read-only git access (W13/N19): private scratch is relocated
        // and it performs no mutation or network. Read/meta/introspection tools
        // have no workspace effect. NOTE: `kgl_query`/`kgl_assert` are NOT here
        // — their store open creates `<workspace>/.kgl` unless relocated, so
        // they are classified dynamically in `admit_tool` (G1).
        "git_read"
        | "skill"
        | "get_tool_schema"
        | "list_tool_signatures"
        | "list_all_tools"
        | "session_stats"
        | "set_external_session_id"
        | "set_verbosity"
        | "workspace_info" => EffectClass::MediatedRead,
        // `set_cwd` only changes the session's working directory (no workspace
        // write); subsequent writes/exec from the new cwd are still refused by
        // their own admission. Session-scoped => Internal.
        "set_cwd" => EffectClass::Internal,
        // External-program plugins, the repair helper, and arbitrary scripting:
        // opaque. A pathname check is not a sandbox (ADR-022), so these are
        // refused rather than inspected. Fail closed for any other non-opcode
        // name that reaches here.
        _ => EffectClass::OpaqueEffect,
    }
}

/// The run's read-only admission decision, consulted at each dispatch boundary.
#[derive(Debug, Clone, Copy)]
pub struct ToolAdmission {
    read_only: bool,
    /// True when the KGL store resolves OUTSIDE the workspace (relocated by an
    /// explicit state root, ADR-021). Only then may `kgl_query`/`kgl_assert`
    /// proceed read-only, because opening the store would otherwise create
    /// `<workspace>/.kgl` (G1, INVENTORIES W07/W09).
    kgl_relocated: bool,
    /// True when the snapshot store resolves OUTSIDE the workspace. Only then
    /// may snapshot create/delete proceed read-only: they read the workspace
    /// and write the relocated store, never the workspace itself (G6,
    /// INVENTORIES W03). Restore always mutates the workspace and stays refused.
    snapshots_relocated: bool,
}

impl ToolAdmission {
    /// Build an admission table. `read_only` is the effective
    /// `--workspace-read-only` state for the run; `kgl_relocated` and
    /// `snapshots_relocated` say whether those internal stores resolve outside
    /// the workspace (computed by the caller from the pinned config).
    pub fn new(read_only: bool, kgl_relocated: bool, snapshots_relocated: bool) -> Self {
        ToolAdmission {
            read_only,
            kgl_relocated,
            snapshots_relocated,
        }
    }

    /// Whether the workspace is read-only for this run.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    fn refuse(class: EffectClass, subject: &str) -> ToolFailure {
        let (code, message) = match class {
            EffectClass::WorkspaceMutation => (
                CODE_WORKSPACE_WRITE_FORBIDDEN,
                format!("{subject} would mutate the read-only workspace"),
            ),
            _ => (
                CODE_TOOL_POLICY_UNSUPPORTED,
                format!("{subject} cannot be mediated while the workspace is read-only"),
            ),
        };
        ToolFailure {
            code: code.to_string(),
            message,
            tool: None,
            argument: None,
            budget: None,
            retryable: false,
        }
    }

    /// Admit (or refuse) a native opcode under the read-only policy.
    pub fn admit_opcode(&self, code: u8) -> Result<(), ToolFailure> {
        if !self.read_only {
            return Ok(());
        }
        // Snapshot create/delete only read the workspace and write the snapshot
        // store; admit them when that store is relocated outside the workspace
        // (G6, INVENTORIES W03). Restore always mutates the workspace.
        if matches!(code, op::SNAP | op::SNAP_DELETE) && self.snapshots_relocated {
            return Ok(());
        }
        let class = classify_opcode(code);
        if class.is_admitted_read_only() {
            Ok(())
        } else {
            Err(Self::refuse(class, &format!("opcode {code}")))
        }
    }

    /// Admit (or refuse) a non-opcode tool by name under the read-only policy.
    pub fn admit_tool(&self, name: &str) -> Result<(), ToolFailure> {
        if !self.read_only {
            return Ok(());
        }
        // G1: KGL tools open `<workspace>/.kgl` unless the store is relocated.
        // `kgl_assert` writes internal state; `kgl_query`'s open still creates
        // the store directory and tables. Admit (Internal) only when relocated;
        // otherwise refuse the workspace write with the named code.
        if matches!(name, "kgl_query" | "kgl_assert") {
            if self.kgl_relocated {
                return Ok(());
            }
            return Err(Self::refuse(
                EffectClass::WorkspaceMutation,
                &format!("tool {name}"),
            ));
        }
        let class = classify_tool(name);
        if class.is_admitted_read_only() {
            Ok(())
        } else {
            Err(Self::refuse(class, &format!("tool {name}")))
        }
    }
}

/// Lexically normalize a path (resolving `.`/`..` without touching the
/// filesystem — the check itself must never write under a read-only
/// workspace). Symlinks are deliberately not followed; this is a conservative,
/// fail-safe pathname test, not a sandbox (ADR-022).
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether `candidate` (made absolute against `workspace` when relative) would
/// resolve inside `workspace`. Used by the startup destination guard (G3) and
/// by the KGL/snapshot relocation checks.
pub fn resolves_inside(workspace: &Path, candidate: &Path) -> bool {
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        workspace.join(candidate)
    };
    lexical_normalize(&absolute).starts_with(lexical_normalize(workspace))
}

/// How `--workspace-read-only` accounts for one internal writer catalogued in
/// reviewer-runtime INVENTORIES.md (the W01..W27 table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterGuard {
    /// Refused by the main startup destination guard when its configured
    /// destination resolves inside the workspace. [`StartupDestinations`]
    /// produces exactly these, and `main` consumes that list (R3).
    StartupDestination,
    /// KGL autoindex/observation: refused by `main`'s dedicated KGL startup
    /// block (it creates `<workspace>/.kgl` unless a state root relocates it).
    StartupKgl,
    /// Mediated at a dispatch boundary (`admit_opcode`/`admit_tool`) or
    /// relocated under the state root on use; never a startup destination.
    OnUse,
    /// Writes only to HOME/state/in-memory; cannot land inside the workspace.
    NonWorkspace,
}

/// The single registry of every internal writer in INVENTORIES.md (W01..W27)
/// with its read-only disposition. The startup guard consumes the
/// [`WriterGuard::StartupDestination`] rows (via [`StartupDestinations`]); the
/// completeness tests cross-check this list against INVENTORIES.md so a future
/// workspace-capable writer cannot be added to the inventory without an
/// explicit disposition here (R3 — W11 was previously omitted by hand).
pub const INTERNAL_WRITERS: &[(&str, WriterGuard)] = &[
    ("W01", WriterGuard::NonWorkspace),       // HOME token-debug log
    ("W02", WriterGuard::NonWorkspace),       // in-memory index + watcher
    ("W03", WriterGuard::OnUse),              // snapshot store (admit SNAP/RESTORE/DELETE)
    ("W04", WriterGuard::StartupDestination), // analytics db path
    ("W05", WriterGuard::StartupDestination), // coordination db dir
    ("W06", WriterGuard::NonWorkspace),       // in-memory pipeline cache
    ("W07", WriterGuard::StartupKgl),         // kgl store (on use: admit_tool)
    ("W08", WriterGuard::StartupKgl),         // kgl autoindex watcher
    ("W09", WriterGuard::StartupKgl),         // kgl observe after file ops
    ("W10", WriterGuard::StartupDestination), // logging directory
    ("W11", WriterGuard::StartupDestination), // providers/request_trace directory
    ("W12", WriterGuard::StartupDestination), // tool_output directory
    ("W13", WriterGuard::StartupDestination), // git_read status scratch
    ("W14", WriterGuard::NonWorkspace),       // daemon socket/catalog under state/HOME
    ("W15", WriterGuard::NonWorkspace),       // agent histories under state/HOME
    ("W16", WriterGuard::NonWorkspace),       // chat histories under state/HOME
    ("W17", WriterGuard::NonWorkspace),       // acp sessions under state/HOME
    ("W18", WriterGuard::NonWorkspace),       // session store under state/HOME
    ("W19", WriterGuard::NonWorkspace),       // daemon stores under state/HOME
    ("W20", WriterGuard::NonWorkspace),       // session catalog under state/HOME
    ("W21", WriterGuard::StartupDestination), // checkpoint shadow repo (workspace .daimonos)
    ("W22", WriterGuard::NonWorkspace),       // evidence under HOME
    ("W23", WriterGuard::NonWorkspace),       // mcp_oauth under HOME
    ("W24", WriterGuard::OnUse),              // provisioning aggregates (producer admission)
    ("W25", WriterGuard::OnUse),              // managed_process env (exec is refused)
    ("W26", WriterGuard::OnUse),              // session resolve_path user writes (admit_opcode)
    ("W27", WriterGuard::StartupDestination), // managed_process artifact directory
];

/// One labelled startup destination the read-only guard checks against the
/// workspace. `w_id` ties it to its INVENTORIES.md row; `label` is the
/// operator-facing destination name in the refusal.
pub struct StartupDestinationEntry {
    pub w_id: &'static str,
    pub label: &'static str,
    pub path: PathBuf,
}

/// Resolved destinations of every workspace-capable startup writer, filled by
/// `main` from the pinned config and environment. Each present field is one
/// INVENTORIES.md [`WriterGuard::StartupDestination`] row; [`Self::entries`] is
/// the single list the read-only startup guard iterates. A `None` field means
/// that writer is disabled or unconfigured for this run.
#[derive(Debug, Default, Clone)]
pub struct StartupDestinations {
    /// W10 logging directory (when logging is enabled).
    pub logging_dir: Option<PathBuf>,
    /// W04 analytics db path (when analytics is enabled).
    pub analytics_db: Option<PathBuf>,
    /// W05 coordination db dir (when coordination is enabled).
    pub coordination_db_dir: Option<PathBuf>,
    /// W12 tool-output directory (always resolved).
    pub tool_output_dir: Option<PathBuf>,
    /// W27 background-process artifact directory (when explicitly configured).
    pub process_artifact_dir: Option<PathBuf>,
    /// W13 git_read private status scratch directory (when explicitly set).
    pub git_read_status_dir: Option<PathBuf>,
    /// W21 per-turn checkpoint shadow repository (when checkpoints are enabled).
    pub checkpoint_repo: Option<PathBuf>,
    /// W11 structural request-trace directory (DAIMONOS_OPENROUTER_TRACE_DIR).
    pub request_trace_dir: Option<PathBuf>,
}

impl StartupDestinations {
    /// The labelled `(w_id, label, path)` list for the enabled writers, driven
    /// by the [`INTERNAL_WRITERS`] registry: it iterates every
    /// [`WriterGuard::StartupDestination`] row and emits its resolved
    /// destination when configured. `main` consumes this and refuses the first
    /// path inside the read-only workspace. A StartupDestination row without a
    /// resolver arm here is dropped and caught by the completeness test.
    pub fn entries(&self) -> Vec<StartupDestinationEntry> {
        INTERNAL_WRITERS
            .iter()
            .filter(|(_, guard)| *guard == WriterGuard::StartupDestination)
            .filter_map(|(w_id, _)| {
                let (label, path) = match *w_id {
                    "W04" => ("analytics.db_path", &self.analytics_db),
                    "W05" => ("coordination.db_dir", &self.coordination_db_dir),
                    "W10" => ("logging.directory", &self.logging_dir),
                    "W11" => (
                        "providers.request_trace (DAIMONOS_OPENROUTER_TRACE_DIR)",
                        &self.request_trace_dir,
                    ),
                    "W12" => ("tool_output.directory", &self.tool_output_dir),
                    "W13" => ("git_read.status_directory", &self.git_read_status_dir),
                    "W21" => ("checkpoint shadow repository", &self.checkpoint_repo),
                    "W27" => ("process.artifact_directory", &self.process_artifact_dir),
                    _ => return None,
                };
                path.clone()
                    .map(|path| StartupDestinationEntry { w_id, label, path })
            })
            .collect()
    }

    /// Under read-only, the first enabled startup destination that resolves
    /// inside `workspace`, as `(w_id, label, path)`. `main` refuses startup
    /// (exit 2) naming the inventory row and its label.
    pub fn first_inside_workspace(
        &self,
        read_only: bool,
        workspace: &Path,
    ) -> Option<(&'static str, &'static str, PathBuf)> {
        if !read_only {
            return None;
        }
        self.entries()
            .into_iter()
            .find(|entry| resolves_inside(workspace, &entry.path))
            .map(|entry| (entry.w_id, entry.label, entry.path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admission(read_only: bool) -> ToolAdmission {
        ToolAdmission::new(read_only, false, false)
    }

    #[test]
    fn disabled_admission_admits_everything() {
        let admission = admission(false);
        assert!(admission.admit_opcode(op::WRITE).is_ok());
        assert!(admission.admit_opcode(op::EXEC).is_ok());
        assert!(admission.admit_tool("cargo").is_ok());
        assert!(admission.admit_tool("execute_script").is_ok());
        assert!(admission.admit_tool("kgl_assert").is_ok());
    }

    #[test]
    fn read_only_refuses_mutations_with_named_code() {
        let admission = admission(true);
        for code in [
            op::WRITE,
            op::PATCH,
            op::APPEND,
            op::SNAP,
            op::RESTORE,
            op::SNAP_DELETE,
        ] {
            let failure = admission.admit_opcode(code).unwrap_err();
            assert_eq!(
                failure.code, CODE_WORKSPACE_WRITE_FORBIDDEN,
                "opcode {code}"
            );
        }
    }

    #[test]
    fn read_only_refuses_opaque_opcodes_and_tools() {
        let admission = admission(true);
        for code in [
            op::EXEC,
            op::BG,
            op::TOOL_RUN,
            op::LINT_REPAIR,
            op::TOOL_PIPELINE,
        ] {
            let failure = admission.admit_opcode(code).unwrap_err();
            assert_eq!(failure.code, CODE_TOOL_POLICY_UNSUPPORTED, "opcode {code}");
        }
        for name in [
            "cargo",
            "git",
            "docker",
            "npm",
            "execute_script",
            "unknown_plugin",
        ] {
            let failure = admission.admit_tool(name).unwrap_err();
            assert_eq!(failure.code, CODE_TOOL_POLICY_UNSUPPORTED, "tool {name}");
        }
    }

    #[test]
    fn read_only_admits_reads_internal_and_git_read() {
        let admission = admission(true);
        for code in [
            op::READ,
            op::LS,
            op::GREP,
            op::FIND,
            op::DIFF,
            op::SESSION,
            op::COORD,
            op::SNAP_LIST,
        ] {
            assert!(admission.admit_opcode(code).is_ok(), "opcode {code}");
        }
        for name in [
            "git_read",
            "get_tool_schema",
            "session_stats",
            "workspace_info",
        ] {
            assert!(admission.admit_tool(name).is_ok(), "tool {name}");
        }
    }

    // G1: KGL tools are refused as a workspace write unless the store is
    // relocated outside the workspace; then they are admitted.
    #[test]
    fn read_only_kgl_tools_depend_on_relocation() {
        let not_relocated = ToolAdmission::new(true, false, false);
        for name in ["kgl_query", "kgl_assert"] {
            let failure = not_relocated.admit_tool(name).unwrap_err();
            assert_eq!(failure.code, CODE_WORKSPACE_WRITE_FORBIDDEN, "tool {name}");
        }
        let relocated = ToolAdmission::new(true, true, false);
        for name in ["kgl_query", "kgl_assert"] {
            assert!(relocated.admit_tool(name).is_ok(), "tool {name}");
        }
    }

    // G6: snapshot create/delete depend on the store being relocated; restore
    // is always refused as a workspace mutation.
    #[test]
    fn read_only_snapshot_create_delete_depend_on_relocation() {
        let not_relocated = ToolAdmission::new(true, false, false);
        for code in [op::SNAP, op::SNAP_DELETE] {
            let failure = not_relocated.admit_opcode(code).unwrap_err();
            assert_eq!(
                failure.code, CODE_WORKSPACE_WRITE_FORBIDDEN,
                "opcode {code}"
            );
        }
        let relocated = ToolAdmission::new(true, false, true);
        for code in [op::SNAP, op::SNAP_DELETE] {
            assert!(relocated.admit_opcode(code).is_ok(), "opcode {code}");
        }
        // Restore mutates the workspace regardless of store relocation.
        assert_eq!(
            relocated.admit_opcode(op::RESTORE).unwrap_err().code,
            CODE_WORKSPACE_WRITE_FORBIDDEN
        );
    }

    // G9: every `protocol::op` constant has a deliberate class. A new opcode
    // that is not added here fails the test (and defaults fail-closed to
    // `OpaqueEffect` in `classify_opcode`).
    #[test]
    fn every_opcode_constant_is_classified() {
        use EffectClass::*;
        let expected: &[(u8, EffectClass)] = &[
            (op::READ, MediatedRead),
            (op::WRITE, WorkspaceMutation),
            (op::PATCH, WorkspaceMutation),
            (op::APPEND, WorkspaceMutation),
            (op::LS, MediatedRead),
            (op::STAT, MediatedRead),
            (op::GLOB, MediatedRead),
            (op::GREP, MediatedRead),
            (op::FIND, MediatedRead),
            (op::EXEC, OpaqueEffect),
            (op::BG, OpaqueEffect),
            (op::POLL, OpaqueEffect),
            (op::KILL, OpaqueEffect),
            (op::SNAP, WorkspaceMutation),
            (op::RESTORE, WorkspaceMutation),
            (op::SNAP_LIST, Internal),
            (op::SNAP_DELETE, WorkspaceMutation),
            (op::DIFF, MediatedRead),
            // op::GIT (15) is deprecated; it is intentionally omitted here and
            // covered by the unknown-opcode fail-closed sweep below.
            (op::ENV_SET, Internal),
            (op::ENV_GET, Internal),
            (op::SESSION, Internal),
            (op::COORD, Internal),
            (op::TOOL_RUN, OpaqueEffect),
            (op::LINT_REPAIR, OpaqueEffect),
            (op::TOOL_PIPELINE, OpaqueEffect),
            (op::TOOL_REGISTER, OpaqueEffect),
            (op::TOOL_LIST, Internal),
            (op::SCHEMA, MediatedRead),
        ];
        for (code, class) in expected {
            assert_eq!(classify_opcode(*code), *class, "opcode {code}");
        }
        // Guard against an unclassified future opcode: any unused code in the
        // u8 space must fall through to the fail-closed OpaqueEffect default.
        let known: std::collections::HashSet<u8> = expected.iter().map(|(c, _)| *c).collect();
        for code in 0u8..=255 {
            if !known.contains(&code) {
                assert_eq!(
                    classify_opcode(code),
                    OpaqueEffect,
                    "unknown opcode {code} must fail closed"
                );
            }
        }
    }

    // G3: the read-only startup destination guard flags a destination inside
    // the workspace only while read-only, independent of state relocation.
    #[test]
    fn startup_destination_guard_flags_inside_workspace() {
        let ws = Path::new("/work/evidence");
        let outside = StartupDestinations {
            logging_dir: Some(PathBuf::from("/state/logs")),
            ..StartupDestinations::default()
        };
        assert!(outside.first_inside_workspace(true, ws).is_none());
        let inside = StartupDestinations {
            logging_dir: Some(ws.join("logs")),
            ..StartupDestinations::default()
        };
        assert_eq!(
            inside.first_inside_workspace(true, ws),
            Some(("W10", "logging.directory", ws.join("logs")))
        );
        // Off => never flagged, even for an inside destination.
        assert!(inside.first_inside_workspace(false, ws).is_none());
        // A `..` escape out of the workspace is not flagged.
        let escape = StartupDestinations {
            logging_dir: Some(ws.join("../elsewhere")),
            ..StartupDestinations::default()
        };
        assert!(escape.first_inside_workspace(true, ws).is_none());
    }

    // R2: the refusal `message` strings are structured metadata keyed by a
    // reviewed `code` (see the module-doc exemption), kept in Rust like
    // `path_policy::ToolFailure`. Pin both templates so a future edit that
    // drifts the machine-readable contract fails the build rather than a
    // review.
    #[test]
    fn refusal_metadata_is_pinned_contract() {
        let admission = admission(true);
        let mutation = admission.admit_opcode(op::WRITE).unwrap_err();
        assert_eq!(mutation.code, CODE_WORKSPACE_WRITE_FORBIDDEN);
        assert_eq!(
            mutation.message,
            format!("opcode {} would mutate the read-only workspace", op::WRITE)
        );
        let opaque = admission.admit_tool("execute_script").unwrap_err();
        assert_eq!(opaque.code, CODE_TOOL_POLICY_UNSUPPORTED);
        assert_eq!(
            opaque.message,
            "tool execute_script cannot be mediated while the workspace is read-only"
        );
    }

    // R3: the startup guard consumes `StartupDestinations::entries`; assert it
    // yields exactly the registry's StartupDestination rows so the guard and
    // the registry cannot drift (e.g. a field added without a registry row, or
    // vice versa).
    #[test]
    fn startup_destinations_cover_every_startup_writer() {
        let all = StartupDestinations {
            logging_dir: Some(PathBuf::from("/x/log")),
            analytics_db: Some(PathBuf::from("/x/a.db")),
            coordination_db_dir: Some(PathBuf::from("/x/coord")),
            tool_output_dir: Some(PathBuf::from("/x/out")),
            process_artifact_dir: Some(PathBuf::from("/x/proc")),
            git_read_status_dir: Some(PathBuf::from("/x/status")),
            checkpoint_repo: Some(PathBuf::from("/x/ckpt")),
            request_trace_dir: Some(PathBuf::from("/x/trace")),
        };
        let produced: std::collections::BTreeSet<&str> =
            all.entries().iter().map(|entry| entry.w_id).collect();
        let expected: std::collections::BTreeSet<&str> = INTERNAL_WRITERS
            .iter()
            .filter(|(_, guard)| *guard == WriterGuard::StartupDestination)
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(
            produced, expected,
            "StartupDestinations::entries must produce exactly the registry's \
             StartupDestination rows"
        );
        // Sanity: the W11 request_trace writer (the row R3 added) is covered.
        assert!(
            produced.contains("W11"),
            "W11 request_trace must be guarded"
        );
    }

    // R3: every internal writer catalogued in INVENTORIES.md must be classified
    // in `INTERNAL_WRITERS`. A new W-row added to the inventory without a
    // disposition here (as W11 was missing from the hand-maintained list) fails
    // this test rather than slipping past review.
    #[test]
    fn every_inventory_w_row_is_classified() {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/docs/contexts/reviewer-runtime/INVENTORIES.md"
        ))
        .expect("INVENTORIES.md must be readable from the crate root");
        let mut in_doc = std::collections::BTreeSet::new();
        for line in text.lines() {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix("| W") {
                // First table cell, e.g. "| W11 | ..." -> "W11".
                let id_digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                if id_digits.len() == 2 {
                    in_doc.insert(format!("W{id_digits}"));
                }
            }
        }
        let in_registry: std::collections::BTreeSet<String> = INTERNAL_WRITERS
            .iter()
            .map(|(id, _)| id.to_string())
            .collect();
        assert!(
            !in_doc.is_empty(),
            "failed to parse any W-row from INVENTORIES.md"
        );
        assert_eq!(
            in_doc, in_registry,
            "INVENTORIES.md W-rows and INTERNAL_WRITERS must match exactly"
        );
    }
}
