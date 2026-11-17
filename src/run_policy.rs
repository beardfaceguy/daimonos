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

use crate::path_policy::ToolFailure;
use crate::protocol::op;
use std::path::{Component, Path, PathBuf};

/// Refusal code for a requested mutation of the read-only evidence workspace.
pub const CODE_WORKSPACE_WRITE_FORBIDDEN: &str = "workspace_write_forbidden";
/// Refusal code for an opaque tool whose effects cannot be mediated read-only.
pub const CODE_TOOL_POLICY_UNSUPPORTED: &str = "tool_policy_unsupported";

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

/// Under read-only, return the first internal-writer destination that would
/// land inside the workspace. The caller refuses startup with
/// `CODE_WORKSPACE_WRITE_FORBIDDEN`, naming the option (G3). Returns `None` when
/// read-only is off or every destination is outside the workspace. The check is
/// independent of `--state-dir`: an explicit state root simply relocates the
/// destinations so they fall outside the workspace and pass.
pub fn first_internal_destination_inside_workspace<'a>(
    read_only: bool,
    workspace: &Path,
    destinations: &'a [(&'a str, PathBuf)],
) -> Option<(&'a str, &'a Path)> {
    if !read_only {
        return None;
    }
    destinations
        .iter()
        .find(|(_, path)| resolves_inside(workspace, path))
        .map(|(label, path)| (*label, path.as_path()))
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

    // G3: the startup destination guard flags a destination inside the
    // workspace only while read-only, independent of state relocation.
    #[test]
    fn startup_destination_guard_flags_inside_workspace() {
        let ws = Path::new("/work/evidence");
        let inside = ("logging.directory", ws.join("logs"));
        let outside = ("logging.directory", PathBuf::from("/state/logs"));
        assert!(first_internal_destination_inside_workspace(
            true,
            ws,
            std::slice::from_ref(&outside)
        )
        .is_none());
        let hit =
            first_internal_destination_inside_workspace(true, ws, std::slice::from_ref(&inside));
        assert_eq!(hit, Some(("logging.directory", ws.join("logs").as_path())));
        // Off => never flagged, even for an inside destination.
        assert!(first_internal_destination_inside_workspace(false, ws, &[inside]).is_none());
        // A `..` escape out of the workspace is not flagged.
        let escape = ("x", ws.join("../elsewhere"));
        assert!(first_internal_destination_inside_workspace(true, ws, &[escape]).is_none());
    }
}
