use crate::protocol::{Op, Response};
use serde_json::json;

pub fn schema(op: &Op) -> Response {
    let specific = op.n.map(|n| n as u8);

    let registry = vec![
        op_schema(0, "read", "Read file contents", &[
            ("p", "path", true), ("n", "offset", false), ("n2", "limit", false),
        ]),
        op_schema(1, "write", "Write file contents", &[
            ("p", "path", true), ("s", "content", true),
        ]),
        op_schema(2, "patch", "Apply string replacements to file", &[
            ("p", "path", true), ("a", "edits [old,new,...]", true),
        ]),
        op_schema(3, "ls", "List directory contents", &[
            ("p", "path", false),
        ]),
        op_schema(4, "stat", "File metadata", &[
            ("p", "path", true),
        ]),
        op_schema(5, "glob", "Find files by pattern", &[
            ("p", "pattern", true), ("q", "root", false),
        ]),
        op_schema(6, "grep", "Search file contents", &[
            ("p", "pattern", true), ("q", "root", false), ("g", "file_glob", false), ("n", "max_results", false),
        ]),
        op_schema(7, "find", "Semantic search (placeholder)", &[
            ("p", "query", true), ("q", "root", false),
        ]),
        op_schema(8, "exec", "Execute command synchronously", &[
            ("s", "cmd", true), ("a", "args", false), ("q", "cwd", false), ("kv", "env", false),
        ]),
        op_schema(9, "bg", "Execute command in background", &[
            ("s", "cmd", true), ("a", "args", false), ("q", "cwd", false),
        ]),
        op_schema(10, "poll", "Check background process status", &[
            ("n", "pid", true),
        ]),
        op_schema(11, "kill", "Kill background process", &[
            ("n", "pid", true),
        ]),
        op_schema(12, "snap", "Create workspace snapshot", &[
            ("p", "tag", false),
        ]),
        op_schema(13, "restore", "Restore workspace snapshot", &[
            ("p", "id", true),
        ]),
        op_schema(14, "diff", "Diff two files", &[
            ("p", "path_a", true), ("q", "path_b", true),
        ]),
        op_schema(15, "git", "Git operation", &[
            ("s", "subcommand", true), ("a", "args", false),
        ]),
        op_schema(16, "env_set", "Set session env var", &[
            ("p", "key", true), ("s", "value", true),
        ]),
        op_schema(17, "env_get", "Get env var", &[
            ("p", "key", true),
        ]),
        op_schema(18, "session", "Session info", &[]),
        op_schema(255, "schema", "Get opcode registry", &[
            ("n", "specific_op", false),
        ]),
    ];

    if let Some(code) = specific {
        match registry.iter().find(|r| r["c"] == code) {
            Some(r) => Response::ok(r.clone()),
            None => Response::err(3, &format!("unknown opcode: {code}")),
        }
    } else {
        Response::ok(json!({
            "version": "0.1.0",
            "ops": registry,
        }))
    }
}

fn op_schema(code: u8, name: &str, desc: &str, args: &[(&str, &str, bool)]) -> serde_json::Value {
    let params: Vec<serde_json::Value> = args
        .iter()
        .map(|(key, desc, required)| {
            json!({"k": key, "desc": desc, "req": required})
        })
        .collect();

    json!({
        "c": code,
        "name": name,
        "desc": desc,
        "params": params,
    })
}
