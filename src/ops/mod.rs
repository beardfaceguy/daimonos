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
    // FR-19 (WP18, ADR-031): install a fresh operation budget for this
    // top-level call. A composite's children re-enter here and inherit the
    // parent's budget (`owned == false`), so delegation cannot multiply the
    // allowance or extend the deadline; the installing call clears it on
    // return so each sequential top-level call starts fresh. When no budget
    // option is set nothing is installed and behavior is unchanged.
    let owned = session.begin_tool_operation();
    let result = match req {
        Request::Single(op) => dispatch_op(session, op, on_exec_progress).await,
        Request::Batch { batch } => {
            let mut results = Vec::with_capacity(batch.len());
            for op in batch {
                results.push(dispatch_op(session, op, on_exec_progress).await);
            }
            Response::ok(serde_json::to_value(results).unwrap())
        }
    };
    session.end_tool_operation(owned);
    result
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
    // FR-07 (ADR-022): refuse workspace mutations and opaque effects before they
    // run when the workspace is read-only. Reads/internal effects pass through.
    if let Err(failure) = session.admission.admit_opcode(op.c) {
        return failure.into_response(&format!("opcode {}", op.c));
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

    // FR-19 F1 (WP18, ADR-031): the index-answered search (mode=files) reads
    // cached index content, not scannable file content, so its logical input is
    // not meterable as logical bytes; refuse an enabled input/entry budget
    // rather than charge "bytes returned" (the owner choice stated in the
    // handoff). Uses the mode-specific refusal because the allow-list admits
    // `search` for its byte-metered content mode (R1/D60). The deadline IS
    // enforced (checked before and after the search).
    if let Some(failure) = session.budget_index_search_refusal() {
        return failure.into_response("search");
    }
    if let Err(failure) = session.budget_check_deadline("search") {
        return failure.into_response("search");
    }
    idx.ensure_populated().await;
    let results = idx
        .search_scoped(&query, max, op.q.as_deref(), file_glob.as_ref())
        .await;
    // A search that ran past the deadline is a timeout, not a late result.
    if let Err(failure) = session.budget_check_deadline("search") {
        return failure.into_response("search");
    }
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
    // FR-19 F1 (WP18, ADR-031): the confined files-mode search ranks index/name
    // content, not scannable file content, so refuse an enabled input/entry
    // budget rather than charge "bytes returned" (same choice as the plain
    // index path). The deadline is enforced below.
    if let Some(failure) = session.budget_index_search_refusal() {
        return failure.into_response("search");
    }
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
    // D08 (ADR-021, FR-09): explicit-only runs read no implicit ignore file,
    // even under confinement, so a hostile ignore file cannot steer the
    // confined filename walk behind `search mode=files`/`find`.
    let apply_ignore_files = session.cfg.discovery_policy.allows_implicit();
    let result = tokio::task::spawn_blocking(move || {
        // WP14: honor no-follow for the explicit scope so a symlink named as
        // the find root is refused with its target, never walked through.
        let start = authority.open_read_policy(&root)?;
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
                // WP14: no-follow never walks through links.
                follow_symlinks: !authority.no_follow(),
                ..WalkOptions::default()
            },
        )?;
        let ignores = if apply_ignore_files {
            crate::confined_ignore::ConfinedIgnore::from_walk(&authority, &scan_root, &walk)
        } else {
            crate::confined_ignore::ConfinedIgnore::disabled(&authority)
        };
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
            // A search that ran past the deadline is a timeout, not a late
            // result (FR-19 F2).
            if let Err(failure) = session.budget_check_deadline("search") {
                return failure.into_response("search");
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::Arc;

    fn files_search(pattern: &str) -> Request {
        Request::Single(Op {
            c: protocol::op::FIND,
            p: Some(pattern.to_string()),
            ..Op::default()
        })
    }

    /// A session with an index over 20 small files and a 1 ms tool deadline.
    fn indexed_session(dir: &std::path::Path) -> Session {
        for i in 0..20 {
            std::fs::write(dir.join(format!("wp18f_{i}.rs")), "x").unwrap();
        }
        let mut cfg = Config::default();
        cfg.tool_budget.timeout_ms = Some(1);
        let cfg = Arc::new(cfg);
        let mut session = Session::new(dir.to_path_buf(), Arc::clone(&cfg));
        session.index = Some(Arc::new(crate::index::WorkspaceIndex::new(
            dir.to_path_buf(),
            &cfg.index,
            false,
        )));
        session
    }

    /// FR-19 (WP18): a top-level files-mode search whose deadline passed is a
    /// timeout with no result. The check is in the search itself: this test
    /// calls `ops::dispatch`, below the general check of the MCP dispatch.
    ///
    /// The budget is installed first and the test then waits, so the deadline
    /// has passed for certain. A deadline that is only short (1 ms) is not
    /// enough: the index search can end before it.
    #[tokio::test]
    async fn a_files_search_after_the_deadline_is_a_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = indexed_session(dir.path());

        let budget = session.tool_budget_config.start().expect("a budget");
        session.set_operation_budget(Some(Arc::clone(&budget)));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(budget.check_deadline().is_err(), "the deadline passed");

        let response = dispatch(&mut session, files_search("wp18f")).await;
        assert!(!response.ok, "{response:?}");
        let failure = response.d.clone().expect("a typed failure");
        assert_eq!(failure["code"], "tool_timeout", "{failure}");
        assert_eq!(failure["tool"], "search", "{failure}");
        assert!(failure.get("results").is_none(), "{failure}");
        assert!(!format!("{response:?}").contains("wp18f_"), "{response:?}");
        // The call did not install this budget, so it does not clear it.
        assert!(session.tool_budget.is_some());
    }

    // Check the native listing below the MCP dispatch guard. Use an existing
    // entry to exercise the actual walk after a verified expired deadline.
    #[tokio::test]
    async fn native_ls_refuses_an_expired_walk() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = indexed_session(dir.path());
        let budget = session.tool_budget_config.start().unwrap();
        session.set_operation_budget(Some(Arc::clone(&budget)));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(budget.check_deadline().is_err());
        let response = dispatch(
            &mut session,
            Request::Single(Op {
                c: protocol::op::LS,
                ..Op::default()
            }),
        )
        .await;
        assert!(!response.ok);
        let data = response.d.expect("typed failure");
        assert_eq!(data["code"], "tool_timeout");
        assert_eq!(data["tool"], "ls");
        assert!(data.get("entries").is_none());
    }

    /// Control: the same search without an installed budget gets a new one and
    /// can succeed. With a long deadline it returns the files.
    #[tokio::test]
    async fn a_files_search_with_time_left_returns_its_results() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = indexed_session(dir.path());
        session.tool_budget_config.timeout_ms = Some(60_000);

        let response = dispatch(&mut session, files_search("wp18f")).await;
        assert!(response.ok, "{response:?}");
        assert!(
            format!("{response:?}").contains("wp18f_0.rs"),
            "{response:?}"
        );
        assert!(session.tool_budget.is_none(), "the call cleared its budget");
    }
}
