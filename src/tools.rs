use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;

use crate::protocol::{self, Op, Request};

/// Maps MCP args JSON to a protocol Request. Factored out of `ToolDef` so the
/// field type stays readable (clippy::type_complexity).
pub type ToRequestFn = fn(&Value) -> Result<Request, String>;

// --- Argument extraction helpers (used by to_request fns and mcp.rs) ---

pub fn get_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)?.as_str().map(String::from)
}

pub fn get_i64(args: &Value, key: &str) -> Option<i64> {
    args.get(key)?.as_i64()
}

pub fn get_str_array(args: &Value, key: &str) -> Option<Vec<String>> {
    args.get(key)?
        .as_array()?
        .iter()
        .map(|v| v.as_str().map(String::from))
        .collect()
}

pub fn get_str_map(args: &Value, key: &str) -> Option<std::collections::HashMap<String, String>> {
    let obj = args.get(key)?.as_object()?;
    let mut map = std::collections::HashMap::new();
    for (k, v) in obj {
        if let Some(s) = v.as_str() {
            map.insert(k.clone(), s.to_string());
        }
    }
    Some(map)
}

// --- Tool tier ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolTier {
    /// Always present with full JSON Schema in list_tools.
    Full,
    /// Name + description only in list_tools; full schema via get_tool_schema.
    Terse,
    /// Hidden until activated via list_all_tools.
    OnDemand,
}

// --- Tool definition ---

pub struct ToolDef {
    pub name: &'static str,
    pub tier: ToolTier,
    pub description: &'static str,
    pub schema: Value,
    /// Maps MCP args JSON to a protocol Request. None for special tools
    /// that need session access or custom handling (git, set_cwd, etc.).
    pub to_request: Option<ToRequestFn>,
    /// If set, tool is omitted from list_tools when the check returns false.
    pub context_check: Option<fn(&Path) -> bool>,
}

// --- Registry ---

pub fn all_tools() -> Vec<ToolDef> {
    vec![
        // ===================== Tier 0: Full schema =====================

        ToolDef {
            name: "read_file",
            tier: ToolTier::Full,
            description: "Read file. Returns {content, lines} or {unchanged:true, lines} if already read and unmodified.",
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative path"},
                    "offset": {"type": "integer", "description": "Start line (0-based)"},
                    "limit": {"type": "integer", "description": "Max lines"}
                },
                "required": ["path"]
            }),
            to_request: Some(|args| Ok(Request::Single(Op {
                c: protocol::op::READ,
                p: get_str(args, "path"),
                n: get_i64(args, "offset"),
                n2: get_i64(args, "limit"),
                ..Default::default()
            }))),
            context_check: None,
        },

        ToolDef {
            name: "write_file",
            tier: ToolTier::Full,
            description: "Write file, creating parent dirs.",
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative path"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
            to_request: Some(|args| Ok(Request::Single(Op {
                c: protocol::op::WRITE,
                p: get_str(args, "path"),
                s: get_str(args, "content"),
                ..Default::default()
            }))),
            context_check: None,
        },

        ToolDef {
            name: "edit_file",
            tier: ToolTier::Full,
            description: "String-replace edits. Returns {applied, diffs} confirming each change.",
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative path"},
                    "edits": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "[old, new, old, new, ...] pairs"
                    }
                },
                "required": ["path", "edits"]
            }),
            to_request: Some(|args| Ok(Request::Single(Op {
                c: protocol::op::PATCH,
                p: get_str(args, "path"),
                a: get_str_array(args, "edits"),
                ..Default::default()
            }))),
            context_check: None,
        },

        ToolDef {
            name: "search",
            tier: ToolTier::Full,
            description: "Regex content search or trigram file-name search.",
            schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "mode": {"type": "string", "enum": ["content", "files"], "description": "Default: content"},
                    "path": {"type": "string", "description": "Scope dir"},
                    "glob": {"type": "string", "description": "e.g. *.rs"},
                    "max_results": {"type": "integer"}
                },
                "required": ["pattern"]
            }),
            to_request: Some(|args| {
                let mode = get_str(args, "mode").unwrap_or_else(|| "content".into());
                if mode == "files" {
                    Ok(Request::Single(Op {
                        c: protocol::op::FIND,
                        p: get_str(args, "pattern"),
                        n: get_i64(args, "max_results"),
                        ..Default::default()
                    }))
                } else {
                    Ok(Request::Single(Op {
                        c: protocol::op::GREP,
                        p: get_str(args, "pattern"),
                        q: get_str(args, "path"),
                        g: get_str(args, "glob"),
                        n: get_i64(args, "max_results"),
                        ..Default::default()
                    }))
                }
            }),
            context_check: None,
        },

        ToolDef {
            name: "workspace_info",
            tier: ToolTier::Terse,
            description: "Detailed workspace info (session state, root listing, index stats). Basic info is already in server instructions — only call if you need index stats or full detail.",
            schema: json!({"type": "object", "properties": {}}),
            to_request: None, // needs session.index access
            context_check: None,
        },

        ToolDef {
            name: "exec",
            tier: ToolTier::Full,
            description: "Run command. Returns {exit, out, err?}. Output auto-truncated if very large.",
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "args": {"type": "array", "items": {"type": "string"}},
                    "cwd": {"type": "string"},
                    "env": {"type": "object", "additionalProperties": {"type": "string"}}
                },
                "required": ["command"]
            }),
            to_request: Some(|args| Ok(Request::Single(Op {
                c: protocol::op::EXEC,
                s: get_str(args, "command"),
                a: get_str_array(args, "args"),
                q: get_str(args, "cwd"),
                kv: get_str_map(args, "env"),
                ..Default::default()
            }))),
            context_check: None,
        },

        ToolDef {
            name: "batch",
            tier: ToolTier::Terse,
            description: "Multiple tools in one call. Always batch when you need 2+ independent reads/searches. E.g. [{tool:\"read_file\",arguments:{path:\"a.rs\"}},{tool:\"search\",arguments:{pattern:\"TODO\"}}].",
            schema: json!({
                "type": "object",
                "properties": {
                    "ops": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "tool": {"type": "string"},
                                "arguments": {"type": "object"}
                            },
                            "required": ["tool"]
                        }
                    }
                },
                "required": ["ops"]
            }),
            to_request: None, // meta-tool handled in mcp.rs
            context_check: None,
        },

        ToolDef {
            name: "get_tool_schema",
            tier: ToolTier::Full,
            description: "Get full inputSchema for tool(s). Call before using a tool whose schema was not in list_tools.",
            schema: json!({
                "type": "object",
                "properties": {
                    "tools": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Tool name(s)"
                    }
                },
                "required": ["tools"]
            }),
            to_request: None, // schema lookup handled in mcp.rs
            context_check: None,
        },

        ToolDef {
            name: "execute_script",
            tier: ToolTier::Full,
            description: "Run a Starlark (Python-subset) script with all tools as built-in functions. Intermediate results stay in the sandbox; only `result` variable is returned. Much cheaper than sequential tool calls.",
            schema: json!({
                "type": "object",
                "properties": {
                    "code": {"type": "string", "description": "Starlark source. Set `result` variable for output."},
                    "timeout": {"type": "integer", "description": "Max seconds (default: 60)"}
                },
                "required": ["code"]
            }),
            to_request: None, // Starlark runtime handled in mcp.rs
            context_check: None,
        },

        ToolDef {
            name: "kgl_query",
            tier: ToolTier::Full,
            description: "Query the KGL knowledge graph to orient in a codebase WITHOUT reading source: find defs by intent/name, trace dependencies and calls, see what state a def reads/mutates, list open questions left by prior agents, and compute blast radius. Action 'index' (re)builds the graph from the workspace; 'check' reports KGL-completeness.",
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "enum": ["index", "orient", "node", "neighbors", "find", "writers_of", "blast_radius", "open_questions", "check"]
                    },
                    "args": {
                        "type": "object",
                        "description": "orient needs {task} (one bundled call: matching defs + intent/open-questions + edges + dependents — prefer this for orientation); node/neighbors/blast_radius need {hash}; find needs {q}; writers_of needs {resource}; neighbors optional {kind,dir}; check optional {mode}."
                    }
                },
                "required": ["query"]
            }),
            to_request: None, // KGL store access handled in mcp.rs
            // #936 prefix diet: heaviest tool in the prefix and niche — only
            // list it where a KGL store exists. Still callable while hidden
            // (dispatch auto-activates), so `kgl_query index` bootstraps fine.
            context_check: Some(|ws| ws.join(".kgl").exists()),
        },

        ToolDef {
            name: "kgl_assert",
            tier: ToolTier::Full,
            description: "Declare the non-derivable KGL layer for a def: its intent/purpose (+ rationale, open questions), authoring provenance, or a typed edge (reads/mutates/calls/depends_on). This is how an authoring agent records WHY code exists and what it touches — the part no derived graph provides. Get the target def 'hash' from kgl_query find/node.",
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["intent", "provenance", "declare_edge"]},
                    "args": {
                        "type": "object",
                        "description": "intent: {hash, purpose, rationale?, open_questions?[]}; provenance: {hash, authored_by, session_id?, assumptions?[], supersedes?[]}; declare_edge: {from, to, kind} where kind is one of calls|depends_on|reads|mutates (snake_case)."
                    }
                },
                "required": ["action"]
            }),
            to_request: None, // KGL store access handled in mcp.rs
            context_check: Some(|ws| ws.join(".kgl").exists()),
        },

        ToolDef {
            name: "list_tool_signatures",
            tier: ToolTier::OnDemand,
            description: "Python-style function signatures for all tool bindings available in execute_script. Already included in server instructions.",
            schema: json!({"type": "object", "properties": {}}),
            to_request: None, // returns static string
            context_check: None,
        },

        // ===================== Tier 1: Terse schema =====================

        ToolDef {
            name: "git",
            tier: ToolTier::Terse,
            description: "Git operations. Commands: status, log, diff, branch, add, commit, push, pull, checkout. All args besides 'command' are passed through.",
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["status", "log", "diff", "branch", "add", "commit", "push", "pull", "checkout"]},
                    "message": {"type": "string", "description": "commit: message"},
                    "all": {"type": "boolean", "description": "commit: auto-stage (-a)"},
                    "limit": {"type": "integer", "description": "log: max commits (default 10)"},
                    "oneline": {"type": "boolean", "description": "log: compact format (hash + subject per line)"},
                    "path": {"type": "string", "description": "log: filter by path"},
                    "mode": {"type": "string", "enum": ["unstaged", "staged"], "description": "diff: scope"},
                    "paths": {"type": "array", "items": {"type": "string"}, "description": "add: files (default [\".\"])"},
                    "branch": {"type": "string", "description": "checkout/push/pull: branch"},
                    "create": {"type": "boolean", "description": "checkout: create new branch (-b)"},
                    "files": {"type": "array", "items": {"type": "string"}, "description": "checkout: restore files"},
                    "remote": {"type": "string", "description": "push/pull: remote (default origin)"},
                    "set_upstream": {"type": "boolean", "description": "push: -u flag"},
                    "rebase": {"type": "boolean", "description": "pull: --rebase"}
                },
                "required": ["command"]
            }),
            to_request: None, // uses ToolRegistry plugin
            context_check: Some(|ws| ws.join(".git").exists()),
        },

        ToolDef {
            name: "cargo",
            tier: ToolTier::Terse,
            description: "Cargo operations. Commands: test, build, check, clippy, fmt, add.",
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["test", "build", "check", "clippy", "fmt", "add"]},
                    "package": {"type": "string", "description": "Target package (--package)"},
                    "filter": {"type": "string", "description": "test: name filter"},
                    "lib": {"type": "boolean", "description": "test: --lib flag"},
                    "release": {"type": "boolean", "description": "build/check: --release flag"},
                    "dev": {"type": "boolean", "description": "add: --dev flag"}
                },
                "required": ["command"]
            }),
            to_request: None, // uses ToolRegistry plugin
            context_check: Some(|ws| ws.join("Cargo.toml").exists()),
        },

        ToolDef {
            name: "pytest",
            tier: ToolTier::Terse,
            description: "Python test runner. Commands: run (passed/failed/skipped + failure ids), collect (--collect-only test ids).",
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["run", "collect"]},
                    "path": {"type": "string", "description": "Test file or directory (default: pytest's auto-discovery)"},
                    "filter": {"type": "string", "description": "run: -k expression (test name selector)"},
                    "markers": {"type": "string", "description": "run: -m expression (marker selector)"},
                    "verbose": {"type": "boolean", "description": "run: -v flag"},
                    "failfast": {"type": "boolean", "description": "run: -x flag (stop on first failure)"}
                },
                "required": ["command"]
            }),
            to_request: None, // uses ToolRegistry plugin
            context_check: Some(|ws| {
                ws.join("pytest.ini").exists()
                    || ws.join("pyproject.toml").exists()
                    || ws.join("setup.py").exists()
                    || ws.join("setup.cfg").exists()
                    || ws.join("tox.ini").exists()
                    || ws.join("conftest.py").exists()
                    || ws.join("tests").is_dir()
            }),
        },

        ToolDef {
            name: "gh",
            tier: ToolTier::Terse,
            description: "GitHub CLI (structured). Commands: pr_view, pr_list, pr_create, pr_diff, pr_checks, pr_merge, pr_checkout, run_list, run_view, issue_list, issue_view, issue_create, issue_comment, api, raw (arbitrary gh args).",
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["pr_view", "pr_list", "pr_create", "pr_diff", "pr_checks", "pr_merge", "pr_checkout", "run_list", "run_view", "issue_list", "issue_view", "issue_create", "issue_comment", "api", "raw"]},
                    "number": {"type": "integer", "description": "pr_view/pr_diff/pr_checks/pr_merge/pr_checkout/issue_view/issue_comment: PR or issue number (PR ops default to current branch)"},
                    "state": {"type": "string", "enum": ["open", "closed", "merged", "all"], "description": "pr_list/issue_list: filter (default open; 'merged' is PR-only)"},
                    "limit": {"type": "integer", "description": "pr_list/run_list/issue_list: max results"},
                    "author": {"type": "string", "description": "pr_list/issue_list: filter by author"},
                    "title": {"type": "string", "description": "pr_create/issue_create: title (required)"},
                    "body": {"type": "string", "description": "pr_create/pr_merge/issue_create/issue_comment: body text"},
                    "base": {"type": "string", "description": "pr_create: base branch"},
                    "draft": {"type": "boolean", "description": "pr_create: create as draft"},
                    "merge_method": {"type": "string", "enum": ["merge", "squash", "rebase"], "description": "pr_merge: merge strategy (default merge)"},
                    "delete_branch": {"type": "boolean", "description": "pr_merge: delete the head branch after merge"},
                    "subject": {"type": "string", "description": "pr_merge: commit subject line"},
                    "branch": {"type": "string", "description": "pr_checkout: branch to check out; run_list: filter by branch"},
                    "workflow": {"type": "string", "description": "run_list: filter by workflow name or file"},
                    "status": {"type": "string", "description": "run_list: filter by run status (e.g. in_progress, completed)"},
                    "run_id": {"type": "integer", "description": "run_view: run databaseId (from run_list)"},
                    "label": {"type": "string", "description": "issue_list/issue_create: label filter or label to add"},
                    "args": {"type": "array", "items": {"type": "string"}, "description": "raw: full gh argv, e.g. [\"pr\", \"merge\", \"6\", \"--squash\"]"},
                    "endpoint": {"type": "string", "description": "api: REST endpoint (e.g. repos/{owner}/{repo}/pulls)"},
                    "method": {"type": "string", "description": "api: HTTP method (default GET)"}
                },
                "required": ["command"]
            }),
            to_request: None,
            context_check: Some(|ws| ws.join(".git").exists()),
        },

        ToolDef {
            name: "docker",
            tier: ToolTier::Terse,
            description: "Docker operations. Commands: ps, logs, exec, images, inspect, stop, compose_up, compose_down, compose_ps.",
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["ps", "logs", "exec", "images", "inspect", "stop", "compose_up", "compose_down", "compose_ps"]},
                    "container": {"type": "string", "description": "logs/exec/inspect/stop: container name or id"},
                    "tail": {"type": "integer", "description": "logs: max lines (default 50)"},
                    "file": {"type": "string", "description": "compose_*: path to compose file (-f)"},
                    "detach": {"type": "boolean", "description": "compose_up: run detached (default true)"}
                },
                "required": ["command"]
            }),
            to_request: None,
            context_check: Some(|ws| {
                ws.join("Dockerfile").exists()
                    || ws.join("docker-compose.yml").exists()
                    || ws.join("docker-compose.yaml").exists()
            }),
        },

        ToolDef {
            name: "curl",
            tier: ToolTier::Terse,
            description: "HTTP request. Returns {status, headers, body (capped 16KB), timing_ms, url, method}. Auto-registered when curl is on PATH.",
            schema: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "Request URL (required)"},
                    "method": {"type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"], "description": "HTTP method (default: GET)"},
                    "headers": {"type": "object", "description": "Request headers as key-value pairs, e.g. Content-Type: application/json"},
                    "body": {"type": "string", "description": "Request body"},
                    "timeout": {"type": "integer", "description": "Timeout in seconds (default: 10)"}
                },
                "required": ["url"]
            }),
            to_request: None, // uses CurlPlugin via ToolRegistry
            context_check: None,
        },

        ToolDef {
            name: "shellcheck",
            tier: ToolTier::Terse,
            description: "Lint shell scripts via shellcheck. Returns {clean, diagnostics:[{file,line,col,level,code,message}]}. Auto-registered when shellcheck is on PATH.",
            schema: json!({
                "type": "object",
                "properties": {
                    "file": {"type": "string", "description": "Path to a single script to check"},
                    "files": {"type": "array", "items": {"type": "string"}, "description": "Paths to multiple scripts to check"},
                    "shell": {"type": "string", "enum": ["bash", "sh", "dash", "ksh"], "description": "Shell dialect (default: bash)"}
                }
            }),
            to_request: None, // uses ShellcheckPlugin via ToolRegistry
            context_check: None,
        },

        ToolDef {
            name: "npm",
            tier: ToolTier::Terse,
            description: "npm package manager. Commands: install, run, test, build, audit. Returns {exit, ok, stdout, stderr} or {clean, vulnerabilities, findings} for audit. Auto-registered when npm is on PATH.",
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["install", "run", "test", "build", "audit"], "description": "npm command to run"},
                    "script": {"type": "string", "description": "Script name for 'run' command (e.g. lint, dev)"}
                },
                "required": ["command"]
            }),
            to_request: None, // uses NpmPlugin via ToolRegistry
            context_check: None,
        },

        ToolDef {
            name: "discord",
            tier: ToolTier::Terse,
            description: "Discord read-only operations. Commands: list_guilds, list_channels, read_messages, search_messages.",
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["list_guilds", "list_channels", "read_messages", "search_messages"]},
                    "guild_id": {"type": "string", "description": "list_channels: allowlisted guild id"},
                    "channel_id": {"type": "string", "description": "read_messages: allowlisted channel id"},
                    "query": {"type": "string", "description": "search_messages: case-insensitive substring query"},
                    "limit": {"type": "integer", "description": "read_messages/search_messages: max messages to fetch (clamped by config)"},
                    "analytics_tag": {"type": "string", "description": "Optional analytics tag suffix for session_stats attribution"}
                },
                "required": ["command"]
            }),
            to_request: None,
            context_check: None,
        },

        ToolDef {
            name: "snapshot",
            tier: ToolTier::Terse,
            description: "Workspace snapshots. Actions: create (returns id), restore (rolls back), list, delete.",
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["create", "restore", "list", "delete"]},
                    "id": {"type": "string", "description": "restore/delete: snapshot id"},
                    "tag": {"type": "string", "description": "create: optional label"}
                },
                "required": ["action"]
            }),
            to_request: Some(|args| {
                let action = get_str(args, "action")
                    .ok_or_else(|| "snapshot requires 'action'".to_string())?;
                let opcode = match action.as_str() {
                    "create" => protocol::op::SNAP,
                    "restore" => protocol::op::RESTORE,
                    "list" => protocol::op::SNAP_LIST,
                    "delete" => protocol::op::SNAP_DELETE,
                    _ => return Err(format!("unknown snapshot action: {action}")),
                };
                let p = match action.as_str() {
                    "create" => get_str(args, "tag"),
                    "restore" | "delete" => get_str(args, "id"),
                    _ => None,
                };
                Ok(Request::Single(Op { c: opcode, p, ..Default::default() }))
            }),
            context_check: None,
        },

        ToolDef {
            name: "set_cwd",
            tier: ToolTier::Terse,
            description: "Change working directory for all subsequent operations. Returns {cwd, previous}.",
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "New working directory (absolute or relative to current cwd)"}
                },
                "required": ["path"]
            }),
            to_request: None, // mutates session directly
            context_check: None,
        },

        ToolDef {
            name: "ls",
            tier: ToolTier::Terse,
            description: "List directory. Returns [{n,d,s}]. Skips .git/node_modules/target/__pycache__. Use glob to filter by filename pattern (e.g. *.rs), type to restrict to files (f) or dirs (d).",
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Dir path (default: cwd)"},
                    "depth": {"type": "integer", "description": "Depth 1-5 (default: 1)"},
                    "glob": {"type": "string", "description": "Filename glob filter, e.g. *.rs or test_*.py"},
                    "type": {"type": "string", "enum": ["f", "d"], "description": "f=files only, d=dirs only"},
                    "all": {"type": "boolean", "description": "Show dotfiles"},
                    "stat": {"type": "boolean", "description": "Add mode+mtime"}
                }
            }),
            to_request: Some(|args| {
                let flag = if args.get("stat").and_then(|v| v.as_bool()).unwrap_or(false) {
                    Some("stat".into())
                } else if args.get("all").and_then(|v| v.as_bool()).unwrap_or(false) {
                    Some("all".into())
                } else {
                    None
                };
                let type_filter = match args.get("type").and_then(|v| v.as_str()) {
                    Some("f") => Some(1),
                    Some("d") => Some(2),
                    _ => None,
                };
                Ok(Request::Single(Op {
                    c: protocol::op::LS,
                    p: get_str(args, "path"),
                    n: get_i64(args, "depth"),
                    q: get_str(args, "glob"),
                    n2: type_filter,
                    g: flag,
                    ..Default::default()
                }))
            }),
            context_check: None,
        },

        ToolDef {
            name: "session_stats",
            tier: ToolTier::Terse,
            description: "Token analytics. Scopes: session (current totals), history (cross-session), daily (trend). Optional external_session_id filters history/daily to a single agent-runtime session id.",
            schema: json!({
                "type": "object",
                "properties": {
                    "scope": {"type": "string", "enum": ["session", "history", "daily"], "description": "Default: session"},
                    "days": {"type": "integer", "description": "history/daily: lookback days (default 30)"},
                    "external_session_id": {"type": "string", "description": "history/daily: restrict to this agent-runtime session id"}
                }
            }),
            to_request: None, // needs session.analytics access
            context_check: None,
        },

        ToolDef {
            name: "set_external_session_id",
            tier: ToolTier::Terse,
            description: "Attach an agent-runtime session id (e.g. claude `--session-id` UUID) to every subsequent analytics row from this connection. Use to correlate daimonos analytics with the agent's own usage logs. Pass an empty string to clear.",
            schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Agent-runtime session identifier; empty string clears."}
                },
                "required": ["id"]
            }),
            to_request: None, // mutates session directly
            context_check: None,
        },

        ToolDef {
            name: "set_verbosity",
            tier: ToolTier::Terse,
            description: "Set this session's output verbosity: full (default, most detail), compact (tighter caps), terse (minimum viable — counts, exit codes, first error). Trades tool-output tokens for detail. Returns {verbosity, previous}.",
            schema: json!({
                "type": "object",
                "properties": {
                    "level": {"type": "string", "enum": ["full", "compact", "terse"], "description": "Target verbosity level."}
                },
                "required": ["level"]
            }),
            to_request: None, // mutates session directly
            context_check: None,
        },

        ToolDef {
            name: "list_all_tools",
            tier: ToolTier::Terse,
            description: "Show all available tools including extended ones (diff, pipelines, repair). Call once to unlock them.",
            schema: json!({"type": "object", "properties": {}}),
            to_request: None, // activates on-demand tools in session
            context_check: None,
        },

        // ===================== Tier 2: On-demand =====================

        ToolDef {
            name: "diff_files",
            tier: ToolTier::OnDemand,
            description: "Structured diff: hunks with =, +, - tagged lines.",
            schema: json!({
                "type": "object",
                "properties": {
                    "path_a": {"type": "string"},
                    "path_b": {"type": "string"},
                    "content_b": {"type": "string", "description": "Alt: diff against string"}
                },
                "required": ["path_a"]
            }),
            to_request: Some(|args| Ok(Request::Single(Op {
                c: protocol::op::DIFF,
                p: get_str(args, "path_a"),
                q: get_str(args, "path_b"),
                s: get_str(args, "content_b"),
                ..Default::default()
            }))),
            context_check: None,
        },

        ToolDef {
            name: "tool_pipeline",
            tier: ToolTier::OnDemand,
            description: "Run tool stages sequentially, abort on failure.",
            schema: json!({
                "type": "object",
                "properties": {
                    "tool_id": {"type": "string"},
                    "stages": {"type": "array", "items": {"type": "string"}},
                    "cwd": {"type": "string"}
                },
                "required": ["tool_id", "stages"]
            }),
            to_request: Some(|args| Ok(Request::Single(Op {
                c: protocol::op::TOOL_PIPELINE,
                p: get_str(args, "tool_id"),
                a: get_str_array(args, "stages"),
                q: get_str(args, "cwd"),
                ..Default::default()
            }))),
            context_check: None,
        },

        ToolDef {
            name: "tool_repair",
            tier: ToolTier::OnDemand,
            description: "Auto lint-fix loop until clean.",
            schema: json!({
                "type": "object",
                "properties": {
                    "tool_id": {"type": "string"},
                    "max_iterations": {"type": "integer", "description": "Default: 3"},
                    "cwd": {"type": "string"}
                },
                "required": ["tool_id"]
            }),
            to_request: Some(|args| Ok(Request::Single(Op {
                c: protocol::op::TOOL_REPAIR,
                p: get_str(args, "tool_id"),
                n: get_i64(args, "max_iterations"),
                q: get_str(args, "cwd"),
                ..Default::default()
            }))),
            context_check: None,
        },
    ]
}

// --- Derived queries ---

/// Build the initial set of exposed tool names (Tier 0 + Tier 1).
pub fn initial_exposed_tools() -> HashSet<String> {
    all_tools()
        .iter()
        .filter(|t| t.tier == ToolTier::Full || t.tier == ToolTier::Terse)
        .map(|t| t.name.to_string())
        .collect()
}

/// Names of on-demand tools (activated by list_all_tools).
pub fn on_demand_names() -> Vec<&'static str> {
    all_tools()
        .iter()
        .filter(|t| t.tier == ToolTier::OnDemand)
        .map(|t| t.name)
        .collect()
}

/// Returns true if this tool is Full-tier (always gets schema in list_tools).
pub fn has_full_schema(name: &str) -> bool {
    all_tools()
        .iter()
        .any(|t| t.name == name && t.tier == ToolTier::Full)
}

/// Whether `list_tools` should include the full inputSchema for this tool.
pub fn expose_full_schema_in_list(name: &str, full_tool_schemas: bool, already_used: bool) -> bool {
    if full_tool_schemas {
        return all_tools()
            .iter()
            .any(|t| t.name == name && (t.tier == ToolTier::Full || t.tier == ToolTier::Terse));
    }
    has_full_schema(name) && !already_used
}

/// A shorter description for `name`, used at verbosity levels below `Full` to
/// shrink the list_tools prefix (vikunja #936 lever 3). Returns `None` for tools
/// whose full description is already terse — callers fall back to the full one.
///
/// Terse variants deliberately KEEP the subcommand list for multiplexer tools
/// (git/cargo/gh/docker/...): at low verbosity the inputSchema is stripped from
/// list_tools, so dropping the command names would just push the agent to call
/// `get_tool_schema` more (extra activation round-trips), defeating the saving.
pub fn terse_description(name: &str) -> Option<&'static str> {
    Some(match name {
        "read_file" => "Read file → {content, lines}.",
        "workspace_info" => "Workspace info: session, root listing, index stats.",
        "exec" => "Run command → {exit, out, err?}.",
        "batch" => "Run 2+ independent tools in one call; get_tool_schema for shape.",
        "execute_script" => {
            "Run a Starlark script; all tools as functions; returns `result`. Cheaper than sequential calls."
        }
        "get_tool_schema" => "Get full inputSchema for tool(s) before use.",
        "git" => "Git: status, log, diff, branch, add, commit, push, pull, checkout.",
        "cargo" => "Cargo: test, build, check, clippy, fmt, add.",
        "pytest" => "Python tests: run, collect.",
        "gh" => {
            "GitHub: pr_view/list/create/diff/checks/merge/checkout, run_list/view, issue_list/view/create/comment, api, raw."
        }
        "docker" => "Docker: ps, logs, exec, images, inspect, stop, compose_up/down/ps.",
        "npm" => "npm: install, run, test, build, audit.",
        "curl" => "HTTP request → {status, headers, body, timing_ms}.",
        "shellcheck" => "Lint shell scripts → {clean, diagnostics}.",
        "discord" => "Discord read-only: list_guilds, list_channels, read_messages, search_messages.",
        "snapshot" => "Workspace snapshots: create, restore, list, delete.",
        "ls" => "List dir → [{n,d,s}]; skips vcs/build dirs; glob/type filters.",
        "session_stats" => "Token analytics: session, history, daily.",
        "set_external_session_id" => "Attach agent-runtime session id to analytics rows.",
        "set_verbosity" => "Set output verbosity: full|compact|terse.",
        "list_all_tools" => "Unlock extended tools (diff, pipelines, repair).",
        "kgl_query" => {
            "Query KGL graph: find defs, deps, calls, state, open questions, blast radius; index/check."
        }
        "kgl_assert" => "Declare KGL layer for a def: intent, provenance, or typed edge.",
        _ => return None,
    })
}

/// Render one tool definition for a list_tools response under the session's
/// verbosity + schema policy: swap in a terse description below `Full`, then
/// strip the inputSchema unless this tool should advertise it.
pub fn render_list_tool(
    mut t: rust_mcp_sdk::schema::Tool,
    verbosity: crate::verbosity::Verbosity,
    full_tool_schemas: bool,
    already_used: bool,
) -> rust_mcp_sdk::schema::Tool {
    if verbosity != crate::verbosity::Verbosity::Full {
        if let Some(d) = terse_description(&t.name) {
            t.description = Some(d.to_string());
        }
    }
    if expose_full_schema_in_list(&t.name, full_tool_schemas, already_used) {
        t
    } else {
        rust_mcp_sdk::schema::Tool {
            input_schema: serde_json::from_value(json!({"type": "object"}))
                .unwrap_or(t.input_schema),
            ..t
        }
    }
}

/// Returns false if the tool has a context check that fails for this workspace.
pub fn passes_context_check(name: &str, workspace: &Path) -> bool {
    all_tools()
        .iter()
        .find(|t| t.name == name)
        .and_then(|t| t.context_check.map(|f| f(workspace)))
        .unwrap_or(true)
}

/// Try to build a protocol Request from MCP tool args.
/// Returns None if the tool is unknown or has no opcode mapping (special tool).
/// Returns Some(Err) if args are invalid.
pub fn build_request(name: &str, args: &Value) -> Option<Result<Request, String>> {
    let tools = all_tools();
    let tool = tools.iter().find(|t| t.name == name)?;
    let to_request = tool.to_request?;
    Some(to_request(args))
}

/// Build MCP Tool objects from the registry for list_tools responses.
pub fn tool_definitions() -> Vec<rust_mcp_sdk::schema::Tool> {
    all_tools()
        .into_iter()
        .map(|td| {
            serde_json::from_value(json!({
                "name": td.name,
                "description": td.description,
                "inputSchema": td.schema,
            }))
            .expect("valid tool definition")
        })
        .collect()
}

/// All tool names in the registry.
pub fn all_tool_names() -> Vec<&'static str> {
    all_tools().iter().map(|t| t.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_tools_has_entries() {
        let tools = all_tools();
        assert!(!tools.is_empty());
        let names: Vec<&str> = tools.iter().map(|t| t.name).collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"edit_file"));
        assert!(names.contains(&"exec"));
        assert!(names.contains(&"search"));
        assert!(names.contains(&"batch"));
        assert!(names.contains(&"get_tool_schema"));
        assert!(names.contains(&"execute_script"));
        assert!(names.contains(&"git"));
        assert!(names.contains(&"discord"));
        assert!(names.contains(&"snapshot"));
    }

    #[test]
    fn all_tools_no_duplicates() {
        let tools = all_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name).collect();
        let unique: HashSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len(), "duplicate tool names");
    }

    #[test]
    fn all_tools_have_descriptions() {
        for tool in all_tools() {
            assert!(
                !tool.description.is_empty(),
                "tool '{}' has no description",
                tool.name
            );
        }
    }

    #[test]
    fn tier_classification() {
        let tools = all_tools();
        let full: Vec<&str> = tools
            .iter()
            .filter(|t| t.tier == ToolTier::Full)
            .map(|t| t.name)
            .collect();
        let terse: Vec<&str> = tools
            .iter()
            .filter(|t| t.tier == ToolTier::Terse)
            .map(|t| t.name)
            .collect();
        let on_demand: Vec<&str> = tools
            .iter()
            .filter(|t| t.tier == ToolTier::OnDemand)
            .map(|t| t.name)
            .collect();

        assert!(full.contains(&"read_file"));
        assert!(full.contains(&"exec"));
        assert!(full.contains(&"execute_script"));
        assert!(terse.contains(&"git"));
        assert!(terse.contains(&"snapshot"));
        assert!(on_demand.contains(&"diff_files"));
        assert!(on_demand.contains(&"tool_pipeline"));
    }

    #[test]
    fn initial_exposed_excludes_on_demand() {
        let exposed = initial_exposed_tools();
        assert!(exposed.contains("read_file"));
        assert!(exposed.contains("git"));
        assert!(!exposed.contains("diff_files"));
        assert!(!exposed.contains("tool_pipeline"));
    }

    #[test]
    fn has_full_schema_correct() {
        assert!(has_full_schema("read_file"));
        assert!(has_full_schema("exec"));
        assert!(has_full_schema("get_tool_schema"));
        assert!(!has_full_schema("git"));
        assert!(!has_full_schema("snapshot"));
        assert!(!has_full_schema("ls"));
    }

    #[test]
    fn expose_full_schema_in_list_respects_mode() {
        assert!(expose_full_schema_in_list("read_file", false, false));
        assert!(!expose_full_schema_in_list("read_file", false, true));
        assert!(!expose_full_schema_in_list("git", false, false));
        assert!(expose_full_schema_in_list("git", true, false));
        assert!(expose_full_schema_in_list("git", true, true));
        assert!(!expose_full_schema_in_list("diff_files", true, false));
    }

    #[test]
    fn build_request_for_opcode_tools() {
        let args = json!({"path": "foo.txt"});
        let req = build_request("read_file", &args);
        assert!(req.is_some());
        assert!(req.unwrap().is_ok());
    }

    #[test]
    fn build_request_none_for_special_tools() {
        let args = json!({});
        assert!(build_request("git", &args).is_none());
        assert!(build_request("docker", &args).is_none());
        assert!(build_request("set_cwd", &args).is_none());
        assert!(build_request("batch", &args).is_none());
    }

    #[test]
    fn build_request_none_for_unknown() {
        assert!(build_request("nonexistent", &json!({})).is_none());
    }

    #[test]
    fn snapshot_request_validates_action() {
        let ok = build_request("snapshot", &json!({"action": "create"}));
        assert!(ok.unwrap().is_ok());

        let bad = build_request("snapshot", &json!({"action": "nope"}));
        assert!(bad.unwrap().is_err());

        let missing = build_request("snapshot", &json!({}));
        assert!(missing.unwrap().is_err());
    }

    #[test]
    fn context_check_git() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!passes_context_check("git", dir.path()));
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        assert!(passes_context_check("git", dir.path()));
    }

    #[test]
    fn context_check_no_check_always_passes() {
        let dir = tempfile::tempdir().unwrap();
        assert!(passes_context_check("read_file", dir.path()));
        assert!(passes_context_check("nonexistent_tool", dir.path()));
    }

    #[test]
    fn context_check_kgl_gated_on_store_dir() {
        // #936 prefix diet: the kgl pair is the heaviest prefix content
        // (1.6k chars) and niche — only list it where a store exists.
        let dir = tempfile::tempdir().unwrap();
        assert!(!passes_context_check("kgl_query", dir.path()));
        assert!(!passes_context_check("kgl_assert", dir.path()));
        std::fs::create_dir(dir.path().join(".kgl")).unwrap();
        assert!(passes_context_check("kgl_query", dir.path()));
        assert!(passes_context_check("kgl_assert", dir.path()));
    }

    #[test]
    fn tool_definitions_roundtrip() {
        let defs = tool_definitions();
        assert!(!defs.is_empty());
        for tool in &defs {
            assert!(!tool.name.is_empty());
            assert!(tool.description.is_some());
        }
    }

    // --- verbosity-parameterized descriptions (#936 lever 3) ---

    #[test]
    fn terse_descriptions_are_shorter_and_curated_only() {
        for t in all_tools() {
            if let Some(terse) = terse_description(t.name) {
                assert!(
                    terse.len() < t.description.len(),
                    "terse desc for {} not shorter ({} >= {})",
                    t.name,
                    terse.len(),
                    t.description.len()
                );
            }
        }
        assert!(terse_description("does_not_exist").is_none());
        // Already-short descriptions need no terse variant.
        assert!(terse_description("write_file").is_none());
    }

    #[test]
    fn terse_multiplexer_descriptions_keep_subcommands() {
        // Dropping command names at low verbosity would force extra
        // get_tool_schema round-trips; guard against over-trimming.
        assert!(terse_description("git").unwrap().contains("commit"));
        assert!(terse_description("git").unwrap().contains("push"));
        assert!(terse_description("cargo").unwrap().contains("clippy"));
        assert!(terse_description("docker").unwrap().contains("compose"));
        let gh = terse_description("gh").unwrap();
        assert!(gh.contains("merge") && gh.contains("raw"));
    }

    #[test]
    fn render_list_tool_swaps_description_below_full() {
        use crate::verbosity::Verbosity;
        let git = || {
            tool_definitions()
                .into_iter()
                .find(|t| t.name == "git")
                .unwrap()
        };

        let orig = git().description.clone();
        let full = render_list_tool(git(), Verbosity::Full, false, false);
        assert_eq!(
            full.description, orig,
            "Full verbosity must keep the full description"
        );

        let terse = render_list_tool(git(), Verbosity::Terse, false, false);
        assert_eq!(terse.description.as_deref(), terse_description("git"));
        assert!(terse.description.unwrap().len() < full.description.unwrap().len());

        // A tool without a terse variant is unchanged even below Full.
        let wf = || {
            tool_definitions()
                .into_iter()
                .find(|t| t.name == "write_file")
                .unwrap()
        };
        let wf_full = render_list_tool(wf(), Verbosity::Full, false, false);
        let wf_terse = render_list_tool(wf(), Verbosity::Terse, false, false);
        assert_eq!(wf_full.description, wf_terse.description);
    }

    #[test]
    fn description_block_shrinks_meaningfully_at_terse() {
        let full_bytes: usize = tool_definitions()
            .iter()
            .map(|t| t.description.as_deref().unwrap_or("").len())
            .sum();
        let terse_bytes: usize = tool_definitions()
            .iter()
            .map(|t| {
                terse_description(&t.name)
                    .map(|s| s.len())
                    .unwrap_or_else(|| t.description.as_deref().unwrap_or("").len())
            })
            .sum();
        let reduction = 100.0 * (1.0 - terse_bytes as f64 / full_bytes as f64);
        eprintln!(
            "description block: full {full_bytes} B, terse {terse_bytes} B ({reduction:.1}% smaller)"
        );
        assert!(
            reduction > 15.0,
            "terse description block should be >15% smaller, got {reduction:.1}%"
        );
    }
}
