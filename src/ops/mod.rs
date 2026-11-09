mod coord;
mod diff_ops;
pub mod exec_filter;
mod exec_ops;
mod file_ops;
mod schema;
mod snap_ops;
mod tool_ops;

use crate::protocol::{self, Op, Request, Response};
use crate::session::Session;
pub(crate) use file_ops::SKIP_INDEX_OBSERVATION;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecProgress {
    Output(String),
    Exit {
        code: Option<i32>,
        signal: Option<String>,
    },
}

pub type ExecProgressCallback<'a> = dyn Fn(ExecProgress) + Send + Sync + 'a;

pub async fn dispatch(session: &mut Session, req: Request) -> Response {
    dispatch_with_progress(session, req, None).await
}

pub async fn dispatch_with_progress(
    session: &mut Session,
    req: Request,
    on_exec_progress: Option<&ExecProgressCallback<'_>>,
) -> Response {
    match req {
        Request::Single(op) => dispatch_op(session, op, on_exec_progress).await,
        Request::Batch { batch } => {
            let mut results = Vec::with_capacity(batch.len());
            for op in batch {
                results.push(dispatch_op(session, op, on_exec_progress).await);
            }
            Response::ok(serde_json::to_value(results).unwrap())
        }
    }
}

async fn dispatch_op(
    session: &mut Session,
    op: Op,
    on_exec_progress: Option<&ExecProgressCallback<'_>>,
) -> Response {
    if session.authority.is_some()
        && !matches!(
            op.c,
            protocol::op::READ
                | protocol::op::LS
                | protocol::op::GREP
                | protocol::op::FIND
                | protocol::op::STAT
                | protocol::op::GLOB
                | protocol::op::DIFF
                | protocol::op::SESSION
                | protocol::op::SCHEMA
                // ENV_GET can return process variables. This is a documented
                // native-protocol note, not a filesystem path (docs/configuration.md).
                | protocol::op::ENV_GET
                | protocol::op::ENV_SET
        )
    {
        return crate::path_policy::ToolFailure::unsupported(format!(
            "operation {} has no confined adapter",
            op.c
        ))
        .into_response(&format!("opcode {}", op.c));
    }
    match op.c {
        protocol::op::READ => file_ops::read(session, &op).await,
        protocol::op::WRITE => file_ops::write(session, &op).await,
        protocol::op::PATCH => file_ops::patch(session, &op).await,
        protocol::op::APPEND => file_ops::append(session, &op).await,
        protocol::op::LS => file_ops::ls(session, &op).await,
        protocol::op::STAT => file_ops::stat(session, &op).await,
        protocol::op::GLOB => file_ops::glob(session, &op).await,
        protocol::op::GREP => file_ops::grep(session, &op).await,
        protocol::op::EXEC => {
            if let Some(cmd) = &op.s {
                session.record_exec_usage(cmd.clone());
            }
            exec_ops::exec_with_progress(session, &op, on_exec_progress).await
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
        protocol::op::LINT_REPAIR => tool_ops::lint_repair(session, &op).await,
        protocol::op::TOOL_PIPELINE => tool_ops::tool_pipeline(session, &op).await,
        protocol::op::TOOL_REGISTER => tool_ops::tool_register(session, &op).await,
        protocol::op::TOOL_LIST => tool_ops::tool_list(session, &op).await,
        protocol::op::SNAP_LIST => snap_ops::snap_list(session).await,
        protocol::op::SNAP_DELETE => snap_ops::snap_delete(session, &op).await,
        protocol::op::ENV_SET => env_set(session, &op),
        protocol::op::ENV_GET => env_get(session, &op),
        protocol::op::SESSION => session_info(session),
        protocol::op::COORD => coord::coord(session, &op),
        protocol::op::SCHEMA => schema::schema(&op),
        _ => Response::err(3, &format!("unknown opcode: {}", op.c)),
    }
}

async fn find(session: &Session, op: &Op) -> Response {
    if session.authority.is_some() {
        return find_confined(session, op).await;
    }
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
    let file_glob = match op.g.as_deref().map(glob::Pattern::new) {
        Some(Ok(pattern)) => Some(pattern),
        Some(Err(error)) => return Response::err(3, &format!("invalid glob: {error}")),
        None => None,
    };

    idx.ensure_populated().await;
    let results = idx
        .search_scoped(&query, max, op.q.as_deref(), file_glob.as_ref())
        .await;
    let stats = idx.stats().await;

    Response::ok(serde_json::json!({
        "results": results,
        "index": stats,
    }))
}

/// Rank only names returned by the confined walker (ADR-022).
async fn find_confined(session: &Session, op: &Op) -> Response {
    use crate::path_policy::WalkOptions;
    let Some(query) = &op.p else {
        return Response::err(3, "find requires query in 'p'");
    };
    let glob = match op.g.as_deref().map(glob::Pattern::new) {
        Some(Ok(pattern)) => Some(pattern),
        Some(Err(error)) => return Response::err(3, &format!("invalid glob: {error}")),
        None => None,
    };
    let authority = session
        .authority
        .as_ref()
        .expect("confined find requires authority")
        .clone();
    let root =
        op.q.as_deref()
            .map(|path| session.resolve_path(path))
            .unwrap_or_else(|| {
                if session.workspace.starts_with(authority.root()) {
                    session.workspace.clone()
                } else {
                    authority.root().to_path_buf()
                }
            });
    let workspace = session.workspace.clone();
    let cfg = session.cfg.index.clone();
    let query = query.clone();
    let max =
        op.n.unwrap_or(session.cfg.search.default_find_max as i64)
            .max(1) as usize;
    let result = tokio::task::spawn_blocking(move || {
        let start = authority.open_read(&root)?;
        let only_file = start
            .metadata()
            .is_ok_and(|metadata| metadata.is_file())
            .then(|| start.relative_path().to_path_buf());
        let scan_root = if only_file.is_some() {
            root.parent().unwrap_or(&root).to_path_buf()
        } else {
            root.clone()
        };
        let walk = authority.walk(
            &scan_root,
            WalkOptions {
                include_hidden: false,
                ..WalkOptions::default()
            },
        )?;
        let ignores =
            crate::confined_ignore::ConfinedIgnore::from_walk(&authority, &scan_root, &walk);
        let skip_extensions = cfg.skip_set();
        let skipped = walk.skipped.len();
        let paths = walk
            .entries
            .into_iter()
            .filter_map(|entry| {
                if !entry.metadata.is_file()
                    || only_file
                        .as_ref()
                        .is_some_and(|path| path != &entry.relative_path)
                    || ignores.is_ignored(&entry.relative_path, false)
                {
                    return None;
                }
                let ext = entry
                    .relative_path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                if skip_extensions.contains(&ext) {
                    return None;
                }
                let display = authority.root().join(&entry.relative_path);
                Some(
                    display
                        .strip_prefix(&workspace)
                        .unwrap_or(&display)
                        .to_string_lossy()
                        .to_string(),
                )
            })
            .collect();
        let (results, stats) =
            crate::index::search_authorized_paths(paths, &cfg, &query, max, glob.as_ref());
        Ok::<_, crate::path_policy::ToolFailure>((results, stats, skipped))
    })
    .await;
    match result {
        Ok(Ok((results, stats, skipped))) => {
            let mut value = serde_json::json!({"results": results, "index": stats});
            if skipped > 0 {
                value["skipped"] = serde_json::json!(skipped)
            }
            Response::ok(value)
        }
        Ok(Err(error)) => error
            .with_argument(op.q.as_deref().unwrap_or("."))
            .into_response("search"),
        Err(error) => Response::err(4, &format!("find task: {error}")),
    }
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
