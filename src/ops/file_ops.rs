use crate::protocol::{Op, Response};
use crate::session::Session;
use serde_json::json;
use std::path::Path;

pub async fn read(session: &Session, op: &Op) -> Response {
    let path = match &op.p {
        Some(p) => session.resolve_path(p),
        None => return Response::err(3, "read requires path in 'p'"),
    };

    let content = match tokio::fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Response::err(1, &format!("not found: {}", path.display()))
        }
        Err(e) => return Response::err(4, &format!("io: {e}")),
    };

    let total_lines = content.lines().count();
    let offset = op.n.unwrap_or(0).max(0) as usize;
    let limit = op.n2.unwrap_or(0);

    let lines: Vec<&str> = if limit > 0 {
        content.lines().skip(offset).take(limit as usize).collect()
    } else if offset > 0 {
        content.lines().skip(offset).collect()
    } else {
        content.lines().collect()
    };

    Response::ok(json!({
        "content": lines.join("\n"),
        "lines": total_lines,
        "size": content.len(),
        "offset": offset,
        "returned": lines.len(),
    }))
}

pub async fn write(session: &Session, op: &Op) -> Response {
    let path = match &op.p {
        Some(p) => session.resolve_path(p),
        None => return Response::err(3, "write requires path in 'p'"),
    };

    let content = match &op.s {
        Some(c) => c,
        None => return Response::err(3, "write requires content in 's'"),
    };

    if let Some(parent) = path.parent() {
        if !parent.exists() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return Response::err(4, &format!("mkdir: {e}"));
            }
        }
    }

    match tokio::fs::write(&path, content).await {
        Ok(()) => Response::ok(json!({"size": content.len()})),
        Err(e) => Response::err(4, &format!("write: {e}")),
    }
}

pub async fn patch(session: &Session, op: &Op) -> Response {
    let path = match &op.p {
        Some(p) => session.resolve_path(p),
        None => return Response::err(3, "patch requires path in 'p'"),
    };

    let edits = match &op.a {
        Some(a) if a.len() % 2 == 0 => a,
        _ => return Response::err(3, "patch requires edits in 'a' as [old, new, old, new, ...]"),
    };

    let mut content = match tokio::fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(e) => return Response::err(4, &format!("read: {e}")),
    };

    let mut applied = 0;
    for pair in edits.chunks(2) {
        let old = &pair[0];
        let new = &pair[1];
        if content.contains(old.as_str()) {
            content = content.replacen(old.as_str(), new, 1);
            applied += 1;
        }
    }

    match tokio::fs::write(&path, &content).await {
        Ok(()) => Response::ok(json!({"applied": applied, "size": content.len()})),
        Err(e) => Response::err(4, &format!("write: {e}")),
    }
}

pub async fn ls(session: &Session, op: &Op) -> Response {
    let path = match &op.p {
        Some(p) => session.resolve_path(p),
        None => session.cwd.clone(),
    };

    let mut entries = Vec::new();
    let mut dir = match tokio::fs::read_dir(&path).await {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Response::err(1, &format!("not found: {}", path.display()))
        }
        Err(e) => return Response::err(4, &format!("ls: {e}")),
    };

    while let Ok(Some(entry)) = dir.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        let ft = entry.file_type().await.ok();
        let is_dir = ft.as_ref().map(|t| t.is_dir()).unwrap_or(false);
        let size = entry.metadata().await.ok().map(|m| m.len()).unwrap_or(0);

        entries.push(json!({
            "n": name,
            "d": is_dir,
            "s": size,
        }));
    }

    entries.sort_by(|a, b| {
        let a_name = a["n"].as_str().unwrap_or("");
        let b_name = b["n"].as_str().unwrap_or("");
        a_name.cmp(b_name)
    });

    Response::ok(json!({"entries": entries, "count": entries.len()}))
}

pub async fn stat(session: &Session, op: &Op) -> Response {
    let path = match &op.p {
        Some(p) => session.resolve_path(p),
        None => return Response::err(3, "stat requires path in 'p'"),
    };

    let meta = match tokio::fs::metadata(&path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Response::err(1, &format!("not found: {}", path.display()))
        }
        Err(e) => return Response::err(4, &format!("stat: {e}")),
    };

    let file_type = if meta.is_dir() {
        "dir"
    } else if meta.is_symlink() {
        "link"
    } else {
        "file"
    };

    Response::ok(json!({
        "size": meta.len(),
        "type": file_type,
        "readonly": meta.permissions().readonly(),
    }))
}

pub async fn glob(session: &Session, op: &Op) -> Response {
    let pattern = match &op.p {
        Some(p) => p.clone(),
        None => return Response::err(3, "glob requires pattern in 'p'"),
    };

    let root = match &op.q {
        Some(r) => session.resolve_path(r),
        None => session.cwd.clone(),
    };

    let full_pattern = root.join(&pattern).to_string_lossy().to_string();

    let files: Vec<String> = match ::glob::glob(&full_pattern) {
        Ok(paths) => paths
            .filter_map(|p| p.ok())
            .filter(|p| p.is_file())
            .map(|p| {
                p.strip_prefix(&session.workspace)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .to_string()
            })
            .collect(),
        Err(e) => return Response::err(3, &format!("glob: {e}")),
    };

    Response::ok(json!({"files": files, "count": files.len()}))
}

pub async fn grep(session: &Session, op: &Op) -> Response {
    let pattern = match &op.p {
        Some(p) => p.clone(),
        None => return Response::err(3, "grep requires pattern in 'p'"),
    };

    let root = match &op.q {
        Some(r) => session.resolve_path(r),
        None => session.cwd.clone(),
    };

    let max = op.n.unwrap_or(session.cfg.search.default_grep_max as i64).max(1) as usize;
    let file_glob = op.g.clone();

    let pattern_clone = pattern.clone();
    let workspace = session.workspace.clone();

    let result = tokio::task::spawn_blocking(move || {
        grep_blocking(&pattern_clone, &root, &workspace, max, file_glob.as_deref())
    })
    .await;

    match result {
        Ok(Ok(matches)) => {
            let count = matches.len();
            Response::ok(json!({"matches": matches, "count": count}))
        }
        Ok(Err(e)) => Response::err(4, &format!("grep: {e}")),
        Err(e) => Response::err(4, &format!("grep task: {e}")),
    }
}

fn grep_blocking(
    pattern: &str,
    root: &Path,
    workspace: &Path,
    max: usize,
    file_glob: Option<&str>,
) -> Result<Vec<serde_json::Value>, String> {
    use grep_regex::RegexMatcher;
    use grep_searcher::sinks::UTF8;
    use grep_searcher::Searcher;
    use ignore::WalkBuilder;

    let matcher = RegexMatcher::new(pattern).map_err(|e| format!("regex: {e}"))?;
    let mut searcher = Searcher::new();
    let mut matches = Vec::new();

    let mut walk = WalkBuilder::new(root);
    walk.hidden(true).git_ignore(true);

    let _ = file_glob; // glob filtering handled per-file below

    for entry in walk.build().flatten() {
        if matches.len() >= max {
            break;
        }
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        if let Some(g) = file_glob {
            let name = path.to_string_lossy();
            if !glob_match_simple(g, &name) {
                continue;
            }
        }

        let rel = path
            .strip_prefix(workspace)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        let _ = searcher.search_path(
            &matcher,
            path,
            UTF8(|line_num, line| {
                if matches.len() < max {
                    matches.push(serde_json::json!({
                        "f": rel,
                        "l": line_num,
                        "t": line.trim_end(),
                    }));
                }
                Ok(matches.len() < max)
            }),
        );
    }

    Ok(matches)
}

fn glob_match_simple(pattern: &str, path: &str) -> bool {
    if let Some(ext) = pattern.strip_prefix("*.") {
        path.ends_with(&format!(".{ext}"))
    } else {
        path.contains(pattern)
    }
}
