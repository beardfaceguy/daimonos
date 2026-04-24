use crate::plugins::generic_cli::GenericCliPlugin;
use crate::protocol::{Op, Response};
use crate::session::Session;
use crate::tool_runner::ToolDescriptor;
use serde_json::json;
use std::sync::Arc;

/// Opcode 20: Run a registered tool command.
/// p = tool_id, s = command name, q = cwd override
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

    match registry.run(tool_id, command, &cwd, &session.env, None).await {
        Ok(result) => Response::ok(serde_json::to_value(result).unwrap()),
        Err(e) => Response::err(5, &e),
    }
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
