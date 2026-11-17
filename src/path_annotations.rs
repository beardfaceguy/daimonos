//! WP16 (FR-14): owned path annotations for the effective tool contract.
//!
//! Context: docs/adr/029-effective-tool-contract.md and the reviewer-runtime
//! INTERFACES.md WP16 rows. ADR-029 requires that Daimonos-owned filesystem
//! path, glob, and walk arguments — including array elements — be marked in the
//! served and exported tool schemas with a standard annotation, so a caller
//! (dbunk) can gate every path a tool receives without a hand-written per-tool
//! list. The marker means the same thing everywhere it appears (FR-14 R2).
//!
//! A path annotation describes **semantics, not spelling** (ADR-029 rejects
//! inferring path fields by argument name). This module therefore keeps one
//! explicit, reviewed table of which arguments of which Daimonos-owned (native)
//! tools denote filesystem locations. It deliberately does **not** mark:
//!
//! - opaque command/argv/script-code strings (`exec.command`/`exec.args`,
//!   `execute_script.code`, raw `git.args`) — under ADR-022 those tools are
//!   refused in confined mode anyway;
//! - agent-mail reservation glob patterns (`reserve_paths.paths`,
//!   `release_reservations.paths`, `check_conflicts.paths`) — they are opaque
//!   reservation keys, never resolved against the filesystem;
//! - URLs, revisions, search patterns, identifiers, names, and enums.
//!
//! Caller-provided MCP worker tools and the caller's final-answer schema are out
//! of scope (ADR-029): this module only knows native registry tool names.
//!
//! The annotation is applied once in the serving pipeline
//! ([`crate::tools::tool_definitions`]) and the agent catalog
//! ([`crate::tool_facade::active_schemas_for_policy`]), so the deterministic
//! `daimonos tool-contract` dump and the live `tools/list` carry identical
//! marks by construction (AC-14.1). ADR-022 performs the actual enforcement and
//! reads the same table via [`path_arguments`].

use serde_json::Value;

/// The standard JSON Schema annotation key marking an argument (or array
/// element) whose value is a Daimonos-owned filesystem path, glob, or walk root
/// (FR-14). Its value is always the boolean `true`.
pub const PATH_ANNOTATION_KEY: &str = "x-daimonos-path";

/// How a path argument carries its value in the tool's input schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathArgKind {
    /// A scalar string property whose value is a filesystem path/glob/walk root.
    /// The property schema node itself is marked.
    Scalar,
    /// An array property whose string *elements* are each a filesystem path.
    /// The array's `items` schema node is marked (FR-14: "path elements inside
    /// arrays are marked too").
    ArrayItems,
}

/// One owned path argument of a native tool: the property name and where its
/// path-valued string lives.
#[derive(Debug, Clone, Copy)]
pub struct PathArg {
    pub name: &'static str,
    pub kind: PathArgKind,
}

const fn scalar(name: &'static str) -> PathArg {
    PathArg {
        name,
        kind: PathArgKind::Scalar,
    }
}

const fn array(name: &'static str) -> PathArg {
    PathArg {
        name,
        kind: PathArgKind::ArrayItems,
    }
}

/// The reviewed set of owned path arguments for a native tool, by name. An
/// empty slice means the tool has no owned path argument. This is the single
/// source of truth both the schema annotator and ADR-022 enforcement consult.
pub fn path_arguments(tool: &str) -> &'static [PathArg] {
    // Reviewed tables, held as `'static` constants so the function can hand out
    // borrowed slices (ADR-022 enforcement and the schema annotator share them).
    const PATH_ONLY: [PathArg; 1] = [scalar("path")];
    const SEARCH_ARGS: [PathArg; 2] = [scalar("path"), scalar("glob")];
    // `git_read` (pinned local read plugin): workspace-relative repo/path and a
    // paths array; `rev`/`a`/`b` are revisions and `pattern` is search text.
    const GIT_READ_ARGS: [PathArg; 3] = [scalar("repo"), scalar("path"), array("paths")];
    // `git`: structured pathspec arguments. The raw `args` argv is opaque and is
    // left unmarked (like exec argv).
    const GIT_ARGS: [PathArg; 3] = [scalar("path"), array("paths"), array("files")];
    // `cwd`: the working directory a shell/subprocess tool runs in. `command`,
    // `args`, `code`, and `stages` are opaque command/script text, not paths.
    const CWD_ONLY: [PathArg; 1] = [scalar("cwd")];
    // `file`: a single filesystem file path (compose file, script file).
    const FILE_ONLY: [PathArg; 1] = [scalar("file")];
    const SHELLCHECK_ARGS: [PathArg; 2] = [scalar("file"), array("files")];
    const DIFF_ARGS: [PathArg; 2] = [scalar("path_a"), scalar("path_b")];
    match tool {
        // Core file operations: the single `path` each resolves against the
        // run root. `pytest.path` and `set_cwd.path` are the same single path.
        "read_file" | "write_file" | "append_file" | "edit_file" | "pytest" | "set_cwd" => {
            &PATH_ONLY
        }
        // Search / ls: `path` is the scope directory to walk; `glob` is a
        // filesystem glob. `pattern` is regex/trigram search text, not a path.
        "search" | "ls" => &SEARCH_ARGS,
        "git_read" => &GIT_READ_ARGS,
        "git" => &GIT_ARGS,
        // Subprocess tools: only the working directory is a path. `exec.command`
        // /`args` are the opaque command; `tool_pipeline.stages` are opaque
        // stage specs.
        "exec" | "tool_pipeline" | "lint_repair" => &CWD_ONLY,
        "docker" => &FILE_ONLY,
        "shellcheck" => &SHELLCHECK_ARGS,
        // On-demand diff tool: both compared sides are file paths (`content_b`
        // is literal text). Absent from the initial dump, annotated for when it
        // is activated and for ADR-022 enforcement.
        "diff_files" => &DIFF_ARGS,
        _ => &[],
    }
}

/// Apply the owned path annotations to a tool's input schema in place.
///
/// Additive only: it inserts [`PATH_ANNOTATION_KEY`] on the path-valued schema
/// nodes and changes nothing else, so an un-owned tool (empty table entry) or a
/// stripped/absent `properties` block is left byte-identical.
pub fn annotate(tool: &str, schema: &mut Value) {
    let args = path_arguments(tool);
    if args.is_empty() {
        return;
    }
    let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) else {
        return;
    };
    for arg in args {
        let Some(property) = properties.get_mut(arg.name).and_then(Value::as_object_mut) else {
            continue;
        };
        match arg.kind {
            PathArgKind::Scalar => {
                property.insert(PATH_ANNOTATION_KEY.to_string(), Value::Bool(true));
            }
            PathArgKind::ArrayItems => {
                if let Some(items) = property.get_mut("items").and_then(Value::as_object_mut) {
                    items.insert(PATH_ANNOTATION_KEY.to_string(), Value::Bool(true));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scalar_path_property_is_marked_and_others_are_not() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"}
            }
        });
        annotate("write_file", &mut schema);
        let props = &schema["properties"];
        assert_eq!(props["path"][PATH_ANNOTATION_KEY], json!(true));
        assert!(props["content"].get(PATH_ANNOTATION_KEY).is_none());
    }

    #[test]
    fn unowned_tool_schema_is_unchanged() {
        let before = json!({"type": "object", "properties": {"limit": {"type": "integer"}}});
        let mut after = before.clone();
        annotate("list_agents", &mut after);
        assert_eq!(before, after);
    }

    #[test]
    fn array_path_marks_items_not_the_array_node() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "files": {"type": "array", "items": {"type": "string"}}
            }
        });
        annotate("shellcheck", &mut schema);
        let files = &schema["properties"]["files"];
        assert_eq!(files["items"][PATH_ANNOTATION_KEY], json!(true));
        assert!(files.get(PATH_ANNOTATION_KEY).is_none());
    }

    #[test]
    fn on_demand_tools_have_reviewed_path_arguments() {
        // Excluded from the initial dump by tier, but annotated for activation
        // and ADR-022 enforcement.
        let names: Vec<&str> = super::path_arguments("diff_files")
            .iter()
            .map(|a| a.name)
            .collect();
        assert_eq!(names, vec!["path_a", "path_b"]);
        assert_eq!(super::path_arguments("tool_pipeline")[0].name, "cwd");
        assert_eq!(super::path_arguments("lint_repair")[0].name, "cwd");
    }
}
