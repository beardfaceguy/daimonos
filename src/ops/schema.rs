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
        op_schema(12, "snap", "Create workspace snapshot (copies tracked files)", &[
            ("p", "tag", false),
        ]),
        op_schema(13, "restore", "Restore workspace snapshot (replaces workspace files)", &[
            ("p", "id", true),
        ]),
        op_schema(14, "diff", "Structured diff between files or file and content", &[
            ("p", "path_a", true), ("q", "path_b", false), ("s", "content_b", false),
        ]),
        op_schema(16, "env_set", "Set session env var", &[
            ("p", "key", true), ("s", "value", true),
        ]),
        op_schema(17, "env_get", "Get env var", &[
            ("p", "key", true),
        ]),
        op_schema(18, "session", "Session info", &[]),
        op_schema(20, "tool_run", "Run registered tool command", &[
            ("p", "tool_id", true), ("s", "command", true), ("q", "cwd", false),
        ]),
        op_schema(21, "tool_repair", "Repair loop: lint, fix, re-lint", &[
            ("p", "tool_id", true), ("n", "max_iterations", false), ("q", "cwd", false),
        ]),
        op_schema(22, "tool_pipeline", "Run tool pipeline stages", &[
            ("p", "tool_id", true), ("a", "stages", true), ("q", "cwd", false),
        ]),
        op_schema(23, "tool_register", "Register tool at runtime", &[
            ("s", "descriptor_json", true),
        ]),
        op_schema(24, "tool_list", "List registered tools", &[]),
        op_schema(25, "snap_list", "List all workspace snapshots", &[]),
        op_schema(26, "snap_delete", "Delete a workspace snapshot", &[
            ("p", "id", true),
        ]),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Op;

    #[test]
    fn full_registry_returns_all_ops() {
        let op = Op { c: 255, ..Default::default() };
        let resp = schema(&op);
        assert!(resp.ok);
        let data = resp.d.unwrap();
        let ops = data["ops"].as_array().unwrap();
        assert!(ops.len() >= 20, "should list at least 20 opcodes, got {}", ops.len());
        assert_eq!(data["version"], "0.1.0");
    }

    #[test]
    fn specific_op_returns_single() {
        let op = Op { c: 255, n: Some(0), ..Default::default() };
        let resp = schema(&op);
        assert!(resp.ok);
        let data = resp.d.unwrap();
        assert_eq!(data["name"], "read");
        assert_eq!(data["c"], 0);
    }

    #[test]
    fn unknown_specific_op_returns_error() {
        let op = Op { c: 255, n: Some(199), ..Default::default() };
        let resp = schema(&op);
        assert!(!resp.ok);
        assert!(resp.m.unwrap().contains("unknown opcode"));
    }

    #[test]
    fn op_schema_helper_required_params() {
        let s = op_schema(42, "test_op", "A test", &[("p", "path", true), ("n", "count", false)]);
        assert_eq!(s["c"], 42);
        assert_eq!(s["name"], "test_op");
        assert_eq!(s["desc"], "A test");
        let params = s["params"].as_array().unwrap();
        assert_eq!(params.len(), 2);
        assert_eq!(params[0]["k"], "p");
        assert_eq!(params[0]["req"], true);
        assert_eq!(params[1]["k"], "n");
        assert_eq!(params[1]["req"], false);
    }

    #[test]
    fn each_op_has_required_fields() {
        let op = Op { c: 255, ..Default::default() };
        let resp = schema(&op);
        let data = resp.d.unwrap();
        for entry in data["ops"].as_array().unwrap() {
            assert!(entry.get("c").is_some(), "op missing 'c' field");
            assert!(entry.get("name").is_some(), "op missing 'name' field");
            assert!(entry.get("desc").is_some(), "op missing 'desc' field");
            assert!(entry.get("params").is_some(), "op missing 'params' field");
        }
    }

    #[test]
    fn known_opcodes_present() {
        let op = Op { c: 255, ..Default::default() };
        let resp = schema(&op);
        let data = resp.d.unwrap();
        let ops = data["ops"].as_array().unwrap();
        let names: Vec<&str> = ops.iter().filter_map(|o| o["name"].as_str()).collect();
        for expected in &["read", "write", "patch", "exec", "snap", "restore", "diff", "schema"] {
            assert!(names.contains(expected), "missing opcode: {expected}");
        }
    }
}
