use crate::plugins::generic_cli::GenericCliPlugin;
use crate::protocol::{Op, Response};
use crate::session::Session;
use crate::tool_runner::ToolDescriptor;
use serde_json::json;
use std::sync::Arc;

/// Opcode 20: Run a registered tool command.
/// p = tool_id, s = command name, q = cwd override
/// Extra tool-specific args forwarded from n, n2, a, g, kv fields.
pub async fn tool_run(session: &Session, op: &Op) -> Response {
    let tool_id = match &op.p {
        Some(t) => t.as_str(),
        None => return Response::err(3, "tool_run requires tool id in 'p'"),
    };

    let command = match &op.s {
        Some(c) => c.as_str(),
        None => return Response::err(3, "tool_run requires command in 's'"),
    };

    let registry = match &session.tool_registry {
        Some(r) => r,
        None => return Response::err(4, "tool registry not available"),
    };

    let cwd = match &op.q {
        Some(d) => session.resolve_path(d),
        None => session.cwd.clone(),
    };

    let extra = build_tool_args(op);
    let extra_ref = if extra.as_object().map_or(true, |o| o.is_empty()) {
        None
    } else {
        Some(&extra)
    };

    match registry.run(tool_id, command, &cwd, &session.env, None, extra_ref).await {
        Ok(result) => Response::ok(serde_json::to_value(result).unwrap()),
        Err(e) => Response::err(5, &e),
    }
}

fn build_tool_args(op: &Op) -> serde_json::Value {
    let mut args = serde_json::Map::new();
    if let Some(n) = op.n {
        args.insert("n".into(), json!(n));
    }
    if let Some(n2) = op.n2 {
        args.insert("n2".into(), json!(n2));
    }
    if let Some(a) = &op.a {
        args.insert("a".into(), json!(a));
    }
    if let Some(g) = &op.g {
        args.insert("g".into(), json!(g));
    }
    if let Some(kv) = &op.kv {
        args.insert("kv".into(), json!(kv));
    }
    json!(args)
}

/// Opcode 21: Run repair loop (lint -> fix -> re-lint).
/// p = tool_id, n = max_iterations (default 3), q = cwd override
pub async fn tool_repair(session: &Session, op: &Op) -> Response {
    let tool_id = match &op.p {
        Some(t) => t.as_str(),
        None => return Response::err(3, "tool_repair requires tool id in 'p'"),
    };

    let registry = match &session.tool_registry {
        Some(r) => r,
        None => return Response::err(4, "tool registry not available"),
    };

    let cwd = match &op.q {
        Some(d) => session.resolve_path(d),
        None => session.cwd.clone(),
    };

    let max_iter = op.n.unwrap_or(3).max(1) as u32;

    match registry.repair(tool_id, &cwd, &session.env, max_iter).await {
        Ok(result) => Response::ok(serde_json::to_value(result).unwrap()),
        Err(e) => Response::err(5, &e),
    }
}

/// Opcode 22: Run a pipeline of stages, short-circuiting on failure.
/// p = tool_id, a = stage names, q = cwd override
pub async fn tool_pipeline(session: &Session, op: &Op) -> Response {
    let tool_id = match &op.p {
        Some(t) => t.as_str(),
        None => return Response::err(3, "tool_pipeline requires tool id in 'p'"),
    };

    let stages = match &op.a {
        Some(s) if !s.is_empty() => s,
        _ => return Response::err(3, "tool_pipeline requires stage names in 'a'"),
    };

    let registry = match &session.tool_registry {
        Some(r) => r,
        None => return Response::err(4, "tool registry not available"),
    };

    let cwd = match &op.q {
        Some(d) => session.resolve_path(d),
        None => session.cwd.clone(),
    };

    match registry.pipeline(tool_id, stages, &cwd, &session.env).await {
        Ok(result) => Response::ok(serde_json::to_value(result).unwrap()),
        Err(e) => Response::err(5, &e),
    }
}

/// Opcode 23: Register a new tool at runtime.
/// s = JSON descriptor
pub async fn tool_register(session: &Session, op: &Op) -> Response {
    let desc_json = match &op.s {
        Some(j) => j,
        None => return Response::err(3, "tool_register requires descriptor JSON in 's'"),
    };

    let descriptor: ToolDescriptor = match serde_json::from_str(desc_json) {
        Ok(d) => d,
        Err(e) => return Response::err(3, &format!("invalid descriptor: {e}")),
    };

    let registry = match &session.tool_registry {
        Some(r) => r,
        None => return Response::err(4, "tool registry not available"),
    };

    let id = descriptor.id.clone();
    let plugin = Arc::new(GenericCliPlugin::new(descriptor));
    registry.register(plugin).await;

    Response::ok(json!({"registered": id}))
}

/// Opcode 24: List all registered tools.
pub async fn tool_list(session: &Session, _op: &Op) -> Response {
    let registry = match &session.tool_registry {
        Some(r) => r,
        None => return Response::err(4, "tool registry not available"),
    };

    let tools = registry.list().await;
    Response::ok(json!({
        "tools": tools.iter().map(|t| json!({
            "id": t.id,
            "commands": t.commands.keys().collect::<Vec<_>>(),
            "source_pattern": t.source_pattern,
            "supports_quickfix": t.supports_quickfix,
        })).collect::<Vec<_>>(),
        "count": tools.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::protocol::Op;
    use crate::tool_runner::ToolRegistry;
    use std::collections::HashMap;

    fn test_session_no_registry() -> (tempfile::TempDir, Session) {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(Config::default());
        let mut session = Session::new(dir.path().to_path_buf(), config);
        session.tool_registry = None;
        (dir, session)
    }

    fn test_session_with_registry() -> (tempfile::TempDir, Session) {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(Config::default());
        let mut session = Session::new(dir.path().to_path_buf(), config);
        session.tool_registry = Some(Arc::new(ToolRegistry::new()));
        (dir, session)
    }

    #[tokio::test]
    async fn tool_run_missing_tool_id() {
        let (_dir, session) = test_session_with_registry();
        let op = Op { c: 20, ..Default::default() };
        let resp = tool_run(&session, &op).await;
        assert!(!resp.ok);
        assert!(resp.m.unwrap().contains("tool id"));
    }

    #[tokio::test]
    async fn tool_run_missing_command() {
        let (_dir, session) = test_session_with_registry();
        let op = Op { c: 20, p: Some("x07".into()), ..Default::default() };
        let resp = tool_run(&session, &op).await;
        assert!(!resp.ok);
        assert!(resp.m.unwrap().contains("command"));
    }

    #[tokio::test]
    async fn tool_run_no_registry() {
        let (_dir, session) = test_session_no_registry();
        let op = Op { c: 20, p: Some("x07".into()), s: Some("build".into()), ..Default::default() };
        let resp = tool_run(&session, &op).await;
        assert!(!resp.ok);
        assert!(resp.m.unwrap().contains("registry not available"));
    }

    #[tokio::test]
    async fn tool_run_unknown_tool() {
        let (_dir, session) = test_session_with_registry();
        let op = Op { c: 20, p: Some("nonexistent".into()), s: Some("build".into()), ..Default::default() };
        let resp = tool_run(&session, &op).await;
        assert!(!resp.ok);
    }

    #[test]
    fn build_tool_args_empty() {
        let op = Op::default();
        let args = build_tool_args(&op);
        assert_eq!(args, json!({}));
    }

    #[test]
    fn build_tool_args_with_fields() {
        let op = Op {
            n: Some(10),
            n2: Some(20),
            a: Some(vec!["stage1".into(), "stage2".into()]),
            g: Some("*.rs".into()),
            ..Default::default()
        };
        let args = build_tool_args(&op);
        assert_eq!(args["n"], 10);
        assert_eq!(args["n2"], 20);
        assert_eq!(args["a"], json!(["stage1", "stage2"]));
        assert_eq!(args["g"], "*.rs");
    }

    #[test]
    fn build_tool_args_with_kv() {
        let mut kv = HashMap::new();
        kv.insert("key1".into(), "val1".into());
        let op = Op {
            kv: Some(kv),
            ..Default::default()
        };
        let args = build_tool_args(&op);
        assert_eq!(args["kv"]["key1"], "val1");
    }

    #[tokio::test]
    async fn tool_repair_missing_tool_id() {
        let (_dir, session) = test_session_with_registry();
        let op = Op { c: 21, ..Default::default() };
        let resp = tool_repair(&session, &op).await;
        assert!(!resp.ok);
        assert!(resp.m.unwrap().contains("tool id"));
    }

    #[tokio::test]
    async fn tool_repair_no_registry() {
        let (_dir, session) = test_session_no_registry();
        let op = Op { c: 21, p: Some("x07".into()), ..Default::default() };
        let resp = tool_repair(&session, &op).await;
        assert!(!resp.ok);
    }

    #[tokio::test]
    async fn tool_pipeline_missing_tool_id() {
        let (_dir, session) = test_session_with_registry();
        let op = Op { c: 22, ..Default::default() };
        let resp = tool_pipeline(&session, &op).await;
        assert!(!resp.ok);
    }

    #[tokio::test]
    async fn tool_pipeline_missing_stages() {
        let (_dir, session) = test_session_with_registry();
        let op = Op { c: 22, p: Some("x07".into()), ..Default::default() };
        let resp = tool_pipeline(&session, &op).await;
        assert!(!resp.ok);
        assert!(resp.m.unwrap().contains("stage names"));
    }

    #[tokio::test]
    async fn tool_pipeline_empty_stages() {
        let (_dir, session) = test_session_with_registry();
        let op = Op { c: 22, p: Some("x07".into()), a: Some(vec![]), ..Default::default() };
        let resp = tool_pipeline(&session, &op).await;
        assert!(!resp.ok);
    }

    #[tokio::test]
    async fn tool_register_missing_descriptor() {
        let (_dir, session) = test_session_with_registry();
        let op = Op { c: 23, ..Default::default() };
        let resp = tool_register(&session, &op).await;
        assert!(!resp.ok);
        assert!(resp.m.unwrap().contains("descriptor JSON"));
    }

    #[tokio::test]
    async fn tool_register_invalid_json() {
        let (_dir, session) = test_session_with_registry();
        let op = Op { c: 23, s: Some("{bad json".into()), ..Default::default() };
        let resp = tool_register(&session, &op).await;
        assert!(!resp.ok);
        assert!(resp.m.unwrap().contains("invalid descriptor"));
    }

    #[tokio::test]
    async fn tool_register_and_list() {
        let (_dir, session) = test_session_with_registry();
        let desc = json!({
            "id": "my_tool",
            "commands": {
                "run": {"bin": "/usr/bin/echo", "args": ["hello"]}
            }
        });
        let op = Op { c: 23, s: Some(desc.to_string()), ..Default::default() };
        let resp = tool_register(&session, &op).await;
        assert!(resp.ok);
        assert_eq!(resp.d.unwrap()["registered"], "my_tool");

        let list_op = Op { c: 24, ..Default::default() };
        let list_resp = tool_list(&session, &list_op).await;
        assert!(list_resp.ok);
        let data = list_resp.d.unwrap();
        assert_eq!(data["count"], 1);
    }

    #[tokio::test]
    async fn tool_list_no_registry() {
        let (_dir, session) = test_session_no_registry();
        let op = Op { c: 24, ..Default::default() };
        let resp = tool_list(&session, &op).await;
        assert!(!resp.ok);
    }
}
