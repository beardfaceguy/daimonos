mod file_ops;
mod exec_ops;
mod schema;

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
        protocol::op::EXEC => exec_ops::exec(session, &op).await,
        protocol::op::BG => exec_ops::bg(session, &op).await,
        protocol::op::POLL => exec_ops::poll(session, &op).await,
        protocol::op::KILL => exec_ops::kill(session, &op).await,
        protocol::op::FIND => find(session, &op).await,
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

    let max = op.n.unwrap_or(session.cfg.search.default_find_max as i64).max(1) as usize;

    let idx = match &session.index {
        Some(i) => i,
        None => return Response::err(4, "index not available"),
    };

    let results = idx.search(&query, max).await;
    let stats = idx.stats().await;

    Response::ok(serde_json::json!({
        "results": results,
        "count": results.len(),
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
    Response::ok(serde_json::json!({
        "workspace": session.workspace.to_string_lossy(),
        "cwd": session.cwd.to_string_lossy(),
        "env_keys": session.env.keys().collect::<Vec<_>>(),
        "bg_count": session.bg_processes.len(),
    }))
}
