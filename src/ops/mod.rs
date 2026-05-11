mod exec_ops;
pub mod exec_filter;
mod file_ops;
mod diff_ops;
mod schema;
mod snap_ops;
mod tool_ops;

use crate::protocol::{self, Op, Request, Response};
use crate::session::Session;

pub async fn dispatch(session: &mut Session, req: Request) -> Response {
    match req {
        Request::Single(op) => dispatch_op(session, op).await,
        Request::Batch { batch } => {
            let mut results = Vec::with_capacity(batch.len());
            for op in batch {
                results.push(dispatch_op(session, op).await);
            }
            Response::ok(serde_json::to_value(results).unwrap())
        }
    }
}

async fn dispatch_op(session: &mut Session, op: Op) -> Response {
    match op.c {
        protocol::op::READ => file_ops::read(session, &op).await,
        protocol::op::WRITE => file_ops::write(session, &op).await,
        protocol::op::PATCH => file_ops::patch(session, &op).await,
        protocol::op::LS => file_ops::ls(session, &op).await,
        protocol::op::STAT => file_ops::stat(session, &op).await,
        protocol::op::GLOB => file_ops::glob(session, &op).await,
        protocol::op::GREP => file_ops::grep(session, &op).await,
        protocol::op::EXEC => {
            if let Some(cmd) = &op.s {
                session.record_exec_usage(cmd.clone());
            }
            exec_ops::exec(session, &op).await
        }
        protocol::op::BG => {
            if let Some(cmd) = &op.s {
                session.record_exec_usage(cmd.clone());
            }
            exec_ops::bg(session, &op).await
        }
        protocol::op::POLL => exec_ops::poll(session, &op).await,
        protocol::op::KILL => exec_ops::kill(session, &op).await,
        protocol::op::SNAP => snap_ops::snap(session, &op).await,
        protocol::op::RESTORE => snap_ops::restore(session, &op).await,
        protocol::op::DIFF => diff_ops::diff(session, &op).await,
        protocol::op::FIND => find(session, &op).await,
        protocol::op::TOOL_RUN => tool_ops::tool_run(session, &op).await,
        protocol::op::TOOL_REPAIR => tool_ops::tool_repair(session, &op).await,
        protocol::op::TOOL_PIPELINE => tool_ops::tool_pipeline(session, &op).await,
        protocol::op::TOOL_REGISTER => tool_ops::tool_register(session, &op).await,
        protocol::op::TOOL_LIST => tool_ops::tool_list(session, &op).await,
        protocol::op::SNAP_LIST => snap_ops::snap_list(session).await,
        protocol::op::SNAP_DELETE => snap_ops::snap_delete(session, &op).await,
        protocol::op::ENV_SET => env_set(session, &op),
        protocol::op::ENV_GET => env_get(session, &op),
        protocol::op::SESSION => session_info(session),
        protocol::op::SCHEMA => schema::schema(&op),
        _ => Response::err(3, &format!("unknown opcode: {}", op.c)),
    }
}

async fn find(session: &Session, op: &Op) -> Response {
    let query = match &op.p {
        Some(q) => q.clone(),
        None => return Response::err(3, "find requires query in 'p'"),
    };

    let max =
        op.n.unwrap_or(session.cfg.search.default_find_max as i64)
            .max(1) as usize;

    let idx = match &session.index {
        Some(i) => i,
        None => return Response::err(4, "index not available"),
    };

    let results = idx.search(&query, max).await;
    let stats = idx.stats().await;

    Response::ok(serde_json::json!({
        "results": results,
        "index": stats,
    }))
}

fn env_set(session: &mut Session, op: &Op) -> Response {
    let key = match &op.p {
        Some(k) => k.clone(),
        None => return Response::err(3, "env_set requires key in 'p'"),
    };
    let val = op.s.clone().unwrap_or_default();
    session.env.insert(key, val);
    Response::ok(serde_json::json!({"ok": true}))
}

fn env_get(session: &Session, op: &Op) -> Response {
    let key = match &op.p {
        Some(k) => k,
        None => return Response::err(3, "env_get requires key in 'p'"),
    };
    let val = session
        .env
        .get(key)
        .cloned()
        .or_else(|| std::env::var(key).ok());
    Response::ok(serde_json::json!({"v": val}))
}

fn session_info(session: &Session) -> Response {
    let mut top_cmds: Vec<_> = session.exec_usage.iter().collect();
    top_cmds.sort_by(|a, b| b.1.cmp(a.1));
    top_cmds.truncate(20);

    let mut info = serde_json::json!({
        "workspace": session.workspace.to_string_lossy(),
        "cwd": session.cwd.to_string_lossy(),
        "env_keys": session.env.keys().collect::<Vec<_>>(),
        "bg_count": session.bg_processes.len(),
        "exec_usage": top_cmds.into_iter()
            .map(|(cmd, count)| serde_json::json!({"cmd": cmd, "n": count}))
            .collect::<Vec<_>>(),
    });

    if let Some(analytics) = &session.analytics {
        let stats = analytics.session_summary();
        info["analytics"] = serde_json::json!({
            "calls": stats.total_calls,
            "req_tokens": stats.total_request_tokens,
            "resp_tokens": stats.total_response_tokens,
            "redirects": stats.redirect_hits,
            "filters": stats.filter_hits,
            "dedup_hits": stats.dedup_hits,
        });
    }

    Response::ok(info)
}
