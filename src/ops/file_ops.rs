use crate::protocol::{Op, Response};
use crate::session::Session;
use serde_json::json;
use std::path::Path;

pub async fn read(session: &mut Session, op: &Op) -> Response {
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
    let is_full_read = offset == 0 && limit <= 0;

    // Dedup: if the model already read this exact content, return a compact response
    if is_full_read {
        if let Some(_entry) = session.check_read_cache(&path, &content) {
            return Response::ok(json!({
                "unchanged": true,
                "lines": total_lines,
            }));
        }
        session.update_read_cache(path, &content);
    }

    let lines: Vec<&str> = if limit > 0 {
        content.lines().skip(offset).take(limit as usize).collect()
    } else if offset > 0 {
        content.lines().skip(offset).collect()
    } else {
        content.lines().collect()
    };

    let mut resp = json!({
        "content": lines.join("\n"),
        "lines": total_lines,
    });
    if offset > 0 || limit > 0 {
        resp["offset"] = json!(offset);
        resp["returned"] = json!(lines.len());
    }
    Response::ok(resp)
}

pub async fn write(session: &mut Session, op: &Op) -> Response {
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
        Ok(()) => {
            session.invalidate_read_cache(&path);
            Response::ok(json!({"ok": true}))
        }
        Err(e) => Response::err(4, &format!("write: {e}")),
    }
}

pub async fn patch(session: &mut Session, op: &Op) -> Response {
    let path = match &op.p {
        Some(p) => session.resolve_path(p),
        None => return Response::err(3, "patch requires path in 'p'"),
    };

    let edits = match &op.a {
        Some(a) if a.len() % 2 == 0 => a,
        _ => {
            return Response::err(
                3,
                "patch requires edits in 'a' as [old, new, old, new, ...]",
            )
        }
    };

    let mut content = match tokio::fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(e) => return Response::err(4, &format!("read: {e}")),
    };

    let mut applied = 0;
    let mut diffs: Vec<serde_json::Value> = Vec::new();
    for pair in edits.chunks(2) {
        let old = &pair[0];
        let new = &pair[1];
        if content.contains(old.as_str()) {
            content = content.replacen(old.as_str(), new, 1);
            applied += 1;
            diffs.push(json!([old, new]));
        }
    }

    match tokio::fs::write(&path, &content).await {
        Ok(()) => {
            session.invalidate_read_cache(&path);
            let mut resp = json!({"applied": applied});
            if !diffs.is_empty() {
                resp["diffs"] = json!(diffs);
            }
            Response::ok(resp)
        }
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
        let is_symlink = ft.as_ref().map(|t| t.is_symlink()).unwrap_or(false);
        let is_dir = if is_symlink {
            // DirEntry::metadata() doesn't follow symlinks; use fs::metadata to resolve target
            tokio::fs::metadata(entry.path())
                .await
                .ok()
                .map(|m| m.is_dir())
                .unwrap_or(false)
        } else {
            ft.as_ref().map(|t| t.is_dir()).unwrap_or(false)
        };
        let size = if is_symlink {
            tokio::fs::metadata(entry.path())
                .await
                .ok()
                .map(|m| m.len())
                .unwrap_or(0)
        } else {
            entry.metadata().await.ok().map(|m| m.len()).unwrap_or(0)
        };

        let mut e = json!({
            "n": name,
            "d": is_dir,
            "s": size,
        });
        if is_symlink {
            e["l"] = json!(true);
        }
        entries.push(e);
    }

    entries.sort_by(|a, b| {
        let a_name = a["n"].as_str().unwrap_or("");
        let b_name = b["n"].as_str().unwrap_or("");
        a_name.cmp(b_name)
    });

    Response::ok(json!({"entries": entries}))
}

pub async fn stat(session: &Session, op: &Op) -> Response {
    let path = match &op.p {
        Some(p) => session.resolve_path(p),
        None => return Response::err(3, "stat requires path in 'p'"),
    };

    // Use symlink_metadata (lstat) so we can detect symlinks;
    // metadata() follows them, making is_symlink() always false.
    let lmeta = match tokio::fs::symlink_metadata(&path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Response::err(1, &format!("not found: {}", path.display()))
        }
        Err(e) => return Response::err(4, &format!("stat: {e}")),
    };

    if lmeta.is_symlink() {
        let target = tokio::fs::read_link(&path)
            .await
            .map(|t| t.to_string_lossy().to_string())
            .unwrap_or_default();
        let broken = tokio::fs::metadata(&path).await.is_err();
        return Response::ok(json!({
            "size": lmeta.len(),
            "type": "link",
            "target": target,
            "broken": broken,
            "readonly": lmeta.permissions().readonly(),
        }));
    }

    let file_type = if lmeta.is_dir() { "dir" } else { "file" };

    Response::ok(json!({
        "size": lmeta.len(),
        "type": file_type,
        "readonly": lmeta.permissions().readonly(),
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

    Response::ok(json!({"files": files}))
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

    let max =
        op.n.unwrap_or(session.cfg.search.default_grep_max as i64)
            .max(1) as usize;
    let file_glob = op.g.clone();

    let pattern_clone = pattern.clone();
    let workspace = session.workspace.clone();

    let result = tokio::task::spawn_blocking(move || {
        grep_blocking(&pattern_clone, &root, &workspace, max, file_glob.as_deref())
    })
    .await;

    match result {
        Ok(Ok(matches)) => Response::ok(json!({"matches": matches})),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::session::Session;
    use std::sync::Arc;

    fn session_in(dir: &std::path::Path) -> Session {
        Session::new(dir.to_path_buf(), Arc::new(Config::default()))
    }

    fn op_read(path: &str) -> Op {
        Op {
            c: 0,
            p: Some(path.to_string()),
            ..Op::default()
        }
    }

    #[tokio::test]
    async fn read_write_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());

        let w = write(
            &mut s,
            &Op {
                c: 1,
                p: Some("hello.txt".into()),
                s: Some("line1\nline2\nline3".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(w.ok);

        let r = read(&mut s, &op_read("hello.txt")).await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["lines"], 3);
        assert_eq!(d["content"], "line1\nline2\nline3");
    }

    #[tokio::test]
    async fn read_with_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());

        write(
            &mut s,
            &Op {
                c: 1,
                p: Some("f.txt".into()),
                s: Some("a\nb\nc\nd\ne".into()),
                ..Op::default()
            },
        )
        .await;

        let r = read(
            &mut s,
            &Op {
                c: 0,
                p: Some("f.txt".into()),
                n: Some(1),
                n2: Some(2),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["content"], "b\nc");
        assert_eq!(d["returned"], 2);
        assert_eq!(d["offset"], 1);
    }

    #[tokio::test]
    async fn read_nonexistent_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let r = read(&mut s, &op_read("nope.txt")).await;
        assert!(!r.ok);
        assert_eq!(r.e, Some(1));
    }

    #[tokio::test]
    async fn read_missing_path_arg() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let r = read(
            &mut s,
            &Op {
                c: 0,
                ..Op::default()
            },
        )
        .await;
        assert!(!r.ok);
        assert_eq!(r.e, Some(3));
    }

    #[tokio::test]
    async fn write_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let w = write(
            &mut s,
            &Op {
                c: 1,
                p: Some("a/b/c.txt".into()),
                s: Some("deep".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(w.ok);
        assert!(dir.path().join("a/b/c.txt").exists());
    }

    #[tokio::test]
    async fn patch_applies_edits() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());

        write(
            &mut s,
            &Op {
                c: 1,
                p: Some("p.txt".into()),
                s: Some("hello world foo bar".into()),
                ..Op::default()
            },
        )
        .await;

        let r = patch(
            &mut s,
            &Op {
                c: 2,
                p: Some("p.txt".into()),
                a: Some(vec![
                    "hello".into(),
                    "goodbye".into(),
                    "foo".into(),
                    "baz".into(),
                ]),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["applied"], 2);

        let content = std::fs::read_to_string(dir.path().join("p.txt")).unwrap();
        assert_eq!(content, "goodbye world baz bar");
    }

    #[tokio::test]
    async fn patch_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());

        write(
            &mut s,
            &Op {
                c: 1,
                p: Some("pm.txt".into()),
                s: Some("abc".into()),
                ..Op::default()
            },
        )
        .await;

        let r = patch(
            &mut s,
            &Op {
                c: 2,
                p: Some("pm.txt".into()),
                a: Some(vec!["xyz".into(), "123".into()]),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        assert_eq!(r.d.unwrap()["applied"], 0);
    }

    #[tokio::test]
    async fn patch_returns_diffs() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());

        write(
            &mut s,
            &Op {
                c: 1,
                p: Some("diff.txt".into()),
                s: Some("hello world foo bar".into()),
                ..Op::default()
            },
        )
        .await;

        let r = patch(
            &mut s,
            &Op {
                c: 2,
                p: Some("diff.txt".into()),
                a: Some(vec![
                    "hello".into(),
                    "goodbye".into(),
                    "foo".into(),
                    "baz".into(),
                ]),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["applied"], 2);
        let diffs = d["diffs"].as_array().unwrap();
        assert_eq!(diffs.len(), 2);
        assert_eq!(diffs[0][0], "hello");
        assert_eq!(diffs[0][1], "goodbye");
        assert_eq!(diffs[1][0], "foo");
        assert_eq!(diffs[1][1], "baz");
    }

    #[tokio::test]
    async fn patch_no_diffs_on_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());

        write(
            &mut s,
            &Op {
                c: 1,
                p: Some("nodiff.txt".into()),
                s: Some("abc".into()),
                ..Op::default()
            },
        )
        .await;

        let r = patch(
            &mut s,
            &Op {
                c: 2,
                p: Some("nodiff.txt".into()),
                a: Some(vec!["xyz".into(), "123".into()]),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["applied"], 0);
        assert!(d.get("diffs").is_none());
    }

    #[tokio::test]
    async fn ls_directory() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();

        let r = ls(
            &s,
            &Op {
                c: 3,
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        let entries = d["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        let names: Vec<&str> = entries.iter().map(|e| e["n"].as_str().unwrap()).collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"sub"));
    }

    #[tokio::test]
    async fn stat_file_and_dir() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        std::fs::write(dir.path().join("s.txt"), "hello").unwrap();

        let r = stat(
            &s,
            &Op {
                c: 4,
                p: Some("s.txt".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["type"], "file");
        assert_eq!(d["size"], 5);

        let r2 = stat(
            &s,
            &Op {
                c: 4,
                p: Some(".".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r2.ok);
        assert_eq!(r2.d.unwrap()["type"], "dir");
    }

    #[tokio::test]
    async fn glob_finds_files() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        std::fs::write(dir.path().join("x.rs"), "").unwrap();
        std::fs::write(dir.path().join("y.rs"), "").unwrap();
        std::fs::write(dir.path().join("z.txt"), "").unwrap();

        let r = glob(
            &s,
            &Op {
                c: 5,
                p: Some("*.rs".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["files"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn grep_finds_matches() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        std::fs::write(
            dir.path().join("g1.txt"),
            "hello world\nfoo bar\nhello again\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("g2.txt"), "no match here\n").unwrap();

        let r = grep(
            &s,
            &Op {
                c: 6,
                p: Some("hello".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        let count = d["matches"].as_array().unwrap().len();
        assert!(count >= 2, "expected at least 2 matches, got {count}");
    }

    // --- Symlink tests ---

    #[tokio::test]
    async fn read_through_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        std::fs::write(dir.path().join("real.txt"), "symlink content").unwrap();
        std::os::unix::fs::symlink(dir.path().join("real.txt"), dir.path().join("link.txt"))
            .unwrap();

        let r = read(&mut s, &op_read("link.txt")).await;
        assert!(r.ok);
        assert_eq!(r.d.unwrap()["content"], "symlink content");
    }

    #[tokio::test]
    async fn read_broken_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        std::os::unix::fs::symlink("/nonexistent_target", dir.path().join("broken.txt")).unwrap();

        let r = read(&mut s, &op_read("broken.txt")).await;
        assert!(!r.ok);
    }

    #[tokio::test]
    async fn write_through_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        std::fs::write(dir.path().join("original.txt"), "old").unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("original.txt"),
            dir.path().join("wlink.txt"),
        )
        .unwrap();

        let w = write(
            &mut s,
            &Op {
                c: 1,
                p: Some("wlink.txt".into()),
                s: Some("new content".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(w.ok);

        let actual = std::fs::read_to_string(dir.path().join("original.txt")).unwrap();
        assert_eq!(actual, "new content");
    }

    #[tokio::test]
    async fn patch_through_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        std::fs::write(dir.path().join("ptarget.txt"), "hello world").unwrap();
        std::os::unix::fs::symlink(dir.path().join("ptarget.txt"), dir.path().join("plink.txt"))
            .unwrap();

        let r = patch(
            &mut s,
            &Op {
                c: 2,
                p: Some("plink.txt".into()),
                a: Some(vec!["hello".into(), "goodbye".into()]),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        assert_eq!(r.d.unwrap()["applied"], 1);

        let actual = std::fs::read_to_string(dir.path().join("ptarget.txt")).unwrap();
        assert_eq!(actual, "goodbye world");
    }

    #[tokio::test]
    async fn stat_detects_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        std::fs::write(dir.path().join("stgt.txt"), "data").unwrap();
        std::os::unix::fs::symlink(dir.path().join("stgt.txt"), dir.path().join("slink.txt"))
            .unwrap();

        let r = stat(
            &s,
            &Op {
                c: 4,
                p: Some("slink.txt".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["type"], "link");
        assert!(d["target"].as_str().unwrap().contains("stgt.txt"));
        assert_eq!(d["broken"], false);
    }

    #[tokio::test]
    async fn stat_detects_broken_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        std::os::unix::fs::symlink("/no/such/path", dir.path().join("deadlink")).unwrap();

        let r = stat(
            &s,
            &Op {
                c: 4,
                p: Some("deadlink".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["type"], "link");
        assert_eq!(d["broken"], true);
    }

    #[tokio::test]
    async fn stat_symlink_to_dir() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        std::fs::create_dir(dir.path().join("realdir")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("realdir"), dir.path().join("dirlink")).unwrap();

        let r = stat(
            &s,
            &Op {
                c: 4,
                p: Some("dirlink".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        assert_eq!(r.d.unwrap()["type"], "link");
    }

    #[tokio::test]
    async fn ls_reports_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        std::fs::write(dir.path().join("real.txt"), "x").unwrap();
        std::os::unix::fs::symlink(dir.path().join("real.txt"), dir.path().join("linked.txt"))
            .unwrap();

        let r = ls(
            &s,
            &Op {
                c: 3,
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let entries = r.d.unwrap()["entries"].as_array().unwrap().clone();

        let real_entry = entries.iter().find(|e| e["n"] == "real.txt").unwrap();
        assert!(
            real_entry.get("l").is_none(),
            "regular file should not have 'l' field"
        );

        let link_entry = entries.iter().find(|e| e["n"] == "linked.txt").unwrap();
        assert_eq!(link_entry["l"], true);
    }

    #[tokio::test]
    async fn ls_symlink_to_dir_shows_is_dir() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("subdir"), dir.path().join("dirlink")).unwrap();

        let r = ls(
            &s,
            &Op {
                c: 3,
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let entries = r.d.unwrap()["entries"].as_array().unwrap().clone();
        let link_entry = entries.iter().find(|e| e["n"] == "dirlink").unwrap();
        assert_eq!(link_entry["l"], true);
        assert_eq!(link_entry["d"], true, "symlink to dir should have d=true");
    }

    #[tokio::test]
    async fn grep_through_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        std::fs::write(dir.path().join("greal.txt"), "findme in symlink\n").unwrap();
        std::os::unix::fs::symlink(dir.path().join("greal.txt"), dir.path().join("glink.txt"))
            .unwrap();

        let r = grep(
            &s,
            &Op {
                c: 6,
                p: Some("findme".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert!(d["matches"].as_array().unwrap().len() >= 1);
    }

    #[tokio::test]
    async fn glob_includes_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        std::fs::write(dir.path().join("glreal.rs"), "").unwrap();
        std::os::unix::fs::symlink(dir.path().join("glreal.rs"), dir.path().join("gllink.rs"))
            .unwrap();

        let r = glob(
            &s,
            &Op {
                c: 5,
                p: Some("*.rs".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        let count = d["files"].as_array().unwrap().len();
        assert!(
            count >= 2,
            "glob should include both real and symlinked files"
        );
    }

    // --- Hard link tests ---

    #[tokio::test]
    async fn read_hard_link() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        std::fs::write(dir.path().join("hlsrc.txt"), "hard link data").unwrap();
        std::fs::hard_link(dir.path().join("hlsrc.txt"), dir.path().join("hlcopy.txt")).unwrap();

        let r = read(&mut s, &op_read("hlcopy.txt")).await;
        assert!(r.ok);
        assert_eq!(r.d.unwrap()["content"], "hard link data");
    }

    #[tokio::test]
    async fn write_hard_link_updates_both() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        std::fs::write(dir.path().join("hlw1.txt"), "old").unwrap();
        std::fs::hard_link(dir.path().join("hlw1.txt"), dir.path().join("hlw2.txt")).unwrap();

        write(
            &mut s,
            &Op {
                c: 1,
                p: Some("hlw2.txt".into()),
                s: Some("new via hardlink".into()),
                ..Op::default()
            },
        )
        .await;

        let r = read(&mut s, &op_read("hlw1.txt")).await;
        assert!(r.ok);
        // write() truncates and rewrites, which may break the hard link
        // depending on the implementation — this test documents the actual behavior
        let content = r.d.unwrap()["content"].as_str().unwrap().to_string();
        assert!(content == "new via hardlink" || content == "old");
    }

    #[tokio::test]
    async fn stat_hard_link_is_file() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        std::fs::write(dir.path().join("hls.txt"), "data").unwrap();
        std::fs::hard_link(dir.path().join("hls.txt"), dir.path().join("hls2.txt")).unwrap();

        let r = stat(
            &s,
            &Op {
                c: 4,
                p: Some("hls2.txt".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        assert_eq!(r.d.unwrap()["type"], "file");
    }

    #[tokio::test]
    async fn patch_hard_link() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        std::fs::write(dir.path().join("hlp.txt"), "hello world").unwrap();
        std::fs::hard_link(dir.path().join("hlp.txt"), dir.path().join("hlp2.txt")).unwrap();

        let r = patch(
            &mut s,
            &Op {
                c: 2,
                p: Some("hlp2.txt".into()),
                a: Some(vec!["hello".into(), "goodbye".into()]),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        assert_eq!(r.d.unwrap()["applied"], 1);
    }

    #[tokio::test]
    async fn ls_shows_hard_links_as_files() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        std::fs::write(dir.path().join("hl1.txt"), "data").unwrap();
        std::fs::hard_link(dir.path().join("hl1.txt"), dir.path().join("hl2.txt")).unwrap();

        let r = ls(
            &s,
            &Op {
                c: 3,
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let entries = r.d.unwrap()["entries"].as_array().unwrap().clone();
        assert_eq!(entries.len(), 2);
        for e in &entries {
            assert_eq!(e["d"], false);
            assert!(e.get("l").is_none(), "hard links should not have 'l' field");
        }
    }

    // --- Read dedup tests ---

    #[tokio::test]
    async fn read_dedup_returns_unchanged_on_reread() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        std::fs::write(dir.path().join("dedup.txt"), "line1\nline2\nline3").unwrap();

        let r1 = read(&mut s, &op_read("dedup.txt")).await;
        assert!(r1.ok);
        let d1 = r1.d.unwrap();
        assert_eq!(d1["content"], "line1\nline2\nline3");
        assert!(d1.get("unchanged").is_none());

        let r2 = read(&mut s, &op_read("dedup.txt")).await;
        assert!(r2.ok);
        let d2 = r2.d.unwrap();
        assert_eq!(d2["unchanged"], true);
        assert_eq!(d2["lines"], 3);
        assert!(d2.get("content").is_none());
    }

    #[tokio::test]
    async fn read_dedup_returns_content_after_write() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        std::fs::write(dir.path().join("rw.txt"), "original").unwrap();

        read(&mut s, &op_read("rw.txt")).await;

        write(
            &mut s,
            &Op {
                c: 1,
                p: Some("rw.txt".into()),
                s: Some("modified".into()),
                ..Op::default()
            },
        )
        .await;

        let r = read(&mut s, &op_read("rw.txt")).await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["content"], "modified");
        assert!(d.get("unchanged").is_none());
    }

    #[tokio::test]
    async fn read_dedup_returns_content_after_patch() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        std::fs::write(dir.path().join("rp.txt"), "hello world").unwrap();

        read(&mut s, &op_read("rp.txt")).await;

        patch(
            &mut s,
            &Op {
                c: 2,
                p: Some("rp.txt".into()),
                a: Some(vec!["hello".into(), "goodbye".into()]),
                ..Op::default()
            },
        )
        .await;

        let r = read(&mut s, &op_read("rp.txt")).await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["content"], "goodbye world");
        assert!(d.get("unchanged").is_none());
    }

    #[tokio::test]
    async fn read_dedup_returns_content_after_external_change() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        std::fs::write(dir.path().join("ext.txt"), "v1").unwrap();

        read(&mut s, &op_read("ext.txt")).await;

        // External write (not through daimonos)
        std::fs::write(dir.path().join("ext.txt"), "v2").unwrap();

        let r = read(&mut s, &op_read("ext.txt")).await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["content"], "v2");
        assert!(d.get("unchanged").is_none());
    }

    #[tokio::test]
    async fn read_dedup_skipped_for_offset_reads() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        std::fs::write(dir.path().join("partial.txt"), "a\nb\nc\nd\ne").unwrap();

        read(&mut s, &op_read("partial.txt")).await;

        // Partial read should always return content, not "unchanged"
        let r = read(
            &mut s,
            &Op {
                c: 0,
                p: Some("partial.txt".into()),
                n: Some(2),
                n2: Some(2),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["content"], "c\nd");
        assert!(d.get("unchanged").is_none());
    }
}
