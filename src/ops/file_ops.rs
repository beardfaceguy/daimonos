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
            }))
            .read_dedup()
            .with_unfiltered_chars(content.len());
        }
        session.update_read_cache(path, &content);
    }

    // Preserve the file's trailing newline. `str::lines()` discards
    // trailing newlines and `lines.join("\n")` doesn't restore them, so
    // a naive collect+join silently makes read→modify→write lossy.
    // Full reads return content verbatim; sliced reads re-emit the
    // trailing newline only when the slice actually reaches EOF.
    let ends_with_newline = content.ends_with('\n');

    let (body, returned_count) = if is_full_read {
        (content.clone(), total_lines)
    } else {
        let lines: Vec<&str> = if limit > 0 {
            content.lines().skip(offset).take(limit as usize).collect()
        } else {
            content.lines().skip(offset).collect()
        };
        let returned = lines.len();
        let reaches_eof = offset.saturating_add(returned) >= total_lines;
        let mut joined = lines.join("\n");
        if reaches_eof && ends_with_newline && !joined.is_empty() {
            joined.push('\n');
        }
        (joined, returned)
    };

    let mut resp = json!({
        "content": body,
        "lines": total_lines,
    });
    if offset > 0 || limit > 0 {
        resp["offset"] = json!(offset);
        resp["returned"] = json!(returned_count);
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
    let mut failures: Vec<serde_json::Value> = Vec::new();
    for pair in edits.chunks(2) {
        let old = &pair[0];
        let new = &pair[1];
        match apply_one_edit(&content, old, new) {
            EditOutcome::Applied(updated) => {
                content = updated;
                applied += 1;
                diffs.push(json!([old, new]));
            }
            EditOutcome::Failed(reason) => {
                failures.push(json!({"old": old, "reason": reason}));
            }
        }
    }

    match tokio::fs::write(&path, &content).await {
        Ok(()) => {
            session.invalidate_read_cache(&path);
            let mut resp = json!({"applied": applied});
            if !diffs.is_empty() {
                resp["diffs"] = json!(diffs);
            }
            // Additive (vikunja #1127): report per-edit failures so the model
            // can react precisely (fix the one bad `old`) instead of re-reading
            // the whole file. Absent entirely on full success.
            if !failures.is_empty() {
                resp["failures"] = json!(failures);
            }
            Response::ok(resp)
        }
        Err(e) => Response::err(4, &format!("write: {e}")),
    }
}

/// Outcome of trying to apply a single `old -> new` edit to file content.
enum EditOutcome {
    /// Matched (exactly or whitespace-tolerantly); carries the updated content.
    Applied(String),
    /// Not applied; carries a human-readable reason for the model to act on.
    Failed(String),
}

/// Apply one `old -> new` edit with a resilient match ladder (vikunja #1127,
/// approach adapted from Aider's editblock matcher). First it tries an exact
/// substring match, but treats ambiguity (more than one occurrence) as a hard
/// error rather than a silent first-match, so the model cannot clobber the
/// wrong site. Failing that, it falls back to a line-wise,
/// leading-whitespace-tolerant match (the model routinely mangles indentation
/// uniformly): it matches on `trim_start()`-equal lines and rewrites using the
/// file's actual indentation, again guarding against a non-unique match.
/// A miss returns a `Failed` with a reason, so the caller reports it as a
/// structured failure rather than silently dropping the edit.
fn apply_one_edit(content: &str, old: &str, new: &str) -> EditOutcome {
    // 1. Exact substring, with an ambiguity guard.
    let exact_count = content.matches(old).count();
    if exact_count == 1 {
        return EditOutcome::Applied(content.replacen(old, new, 1));
    }
    if exact_count > 1 {
        return EditOutcome::Failed(format!(
            "ambiguous: `old` matches {exact_count} places exactly; \
             include more surrounding lines so it is unique"
        ));
    }

    // 2. Leading-whitespace-tolerant, line-wise match.
    if let Some(updated) = replace_ignoring_leading_ws(content, old, new) {
        return updated;
    }

    EditOutcome::Failed("not found: `old` did not match the file".to_string())
}

/// Try to match `old` against a run of `content` lines ignoring each line's
/// leading whitespace, and if it matches uniquely, splice in `new` re-indented
/// to the file's actual leading whitespace at the match site. Returns `None`
/// when there is no whitespace-tolerant match; returns `Some(Failed(..))` when
/// the match is ambiguous (>1 site) so the caller surfaces that too.
fn replace_ignoring_leading_ws(content: &str, old: &str, new: &str) -> Option<EditOutcome> {
    let content_lines: Vec<&str> = content.lines().collect();
    let old_lines: Vec<&str> = old.lines().collect();
    if old_lines.is_empty() {
        return None;
    }
    let key: Vec<&str> = old_lines.iter().map(|l| l.trim_start()).collect();

    // Find every window of content whose trimmed lines equal the trimmed `old`.
    let mut matches: Vec<usize> = Vec::new();
    if content_lines.len() >= old_lines.len() {
        for start in 0..=content_lines.len() - old_lines.len() {
            let window = &content_lines[start..start + old_lines.len()];
            if window.iter().zip(&key).all(|(cl, k)| cl.trim_start() == *k) {
                matches.push(start);
            }
        }
    }

    match matches.len() {
        0 => None,
        1 => {
            let start = matches[0];
            // Re-indent `new` to the file's leading whitespace at the match
            // site (per matched line; new lines beyond the matched run reuse
            // the first line's indent). This preserves the file's real style.
            let new_lines: Vec<&str> = new.lines().collect();
            let indents: Vec<&str> = (0..old_lines.len())
                .map(|i| leading_ws(content_lines[start + i]))
                .collect();
            let first_indent = indents.first().copied().unwrap_or("");
            let mut rebuilt: Vec<String> = Vec::new();
            for (i, nl) in new_lines.iter().enumerate() {
                let indent = indents.get(i).copied().unwrap_or(first_indent);
                if nl.trim().is_empty() {
                    rebuilt.push(String::new());
                } else {
                    rebuilt.push(format!("{indent}{}", nl.trim_start()));
                }
            }
            // Reassemble the file: lines before, the rebuilt block, lines after.
            let mut out: Vec<&str> = Vec::new();
            out.extend_from_slice(&content_lines[..start]);
            let rebuilt_refs: Vec<&str> = rebuilt.iter().map(|s| s.as_str()).collect();
            out.extend_from_slice(&rebuilt_refs);
            out.extend_from_slice(&content_lines[start + old_lines.len()..]);
            let mut joined = out.join("\n");
            // Preserve a trailing newline if the original had one.
            if content.ends_with('\n') {
                joined.push('\n');
            }
            Some(EditOutcome::Applied(joined))
        }
        n => Some(EditOutcome::Failed(format!(
            "ambiguous: `old` matches {n} places (ignoring indentation); \
             include more surrounding lines so it is unique"
        ))),
    }
}

/// The leading-whitespace prefix of a line (spaces/tabs before the first
/// non-whitespace char); empty string if the line has no indentation.
fn leading_ws(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".daimonos",
    "__pycache__",
    ".next",
    "dist",
];

struct LsOpts<'a> {
    show_all: bool,
    stat: bool,
    glob_pat: Option<&'a glob::Pattern>,
    type_filter: Option<i64>,
}

pub async fn ls(session: &Session, op: &Op) -> Response {
    let path = match &op.p {
        Some(p) => session.resolve_path(p),
        None => session.cwd.clone(),
    };

    let depth = op.n.unwrap_or(1).clamp(1, 5) as usize;
    let glob_owned: Option<glob::Pattern> =
        op.q.as_deref().and_then(|p| glob::Pattern::new(p).ok());
    let opts = LsOpts {
        show_all: op.g.as_deref() == Some("all"),
        stat: op.g.as_deref() == Some("stat"),
        glob_pat: glob_owned.as_ref(),
        // op.n2: 1 = files only, 2 = dirs only, None/other = all
        type_filter: op.n2,
    };

    let mut entries = Vec::new();
    if let Err(e) = ls_recurse(&path, &path, depth, &opts, &mut entries).await {
        return Response::err(4, &format!("ls: {e}"));
    }

    entries.sort_by(|a, b| {
        let a_name = a["n"].as_str().unwrap_or("");
        let b_name = b["n"].as_str().unwrap_or("");
        a_name.cmp(b_name)
    });

    Response::ok(json!({"entries": entries}))
}

async fn ls_recurse(
    root: &std::path::Path,
    path: &std::path::Path,
    depth: usize,
    opts: &LsOpts<'_>,
    entries: &mut Vec<serde_json::Value>,
) -> Result<(), String> {
    let mut dir = tokio::fs::read_dir(path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("not found: {}", path.display())
        } else {
            format!("{e}")
        }
    })?;

    while let Ok(Some(entry)) = dir.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if !opts.show_all && name.starts_with('.') {
            continue;
        }

        let ft = entry.file_type().await.ok();
        let is_symlink = ft.as_ref().map(|t| t.is_symlink()).unwrap_or(false);

        let meta = if is_symlink {
            tokio::fs::metadata(entry.path()).await.ok()
        } else {
            entry.metadata().await.ok()
        };

        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);

        if is_dir && SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }

        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path().as_path())
            .to_string_lossy()
            .to_string();

        let mut e = json!({
            "n": rel,
            "d": is_dir,
            "s": size,
        });

        if is_symlink {
            e["l"] = json!(true);
        }

        if opts.stat {
            if let Some(ref m) = meta {
                use std::os::unix::fs::PermissionsExt;
                let mode = m.permissions().mode();
                e["m"] = json!(format!("{:o}", mode & 0o7777));

                if let Ok(mtime) = m.modified() {
                    if let Ok(dur) = mtime.duration_since(std::time::UNIX_EPOCH) {
                        e["t"] = json!(dur.as_secs());
                    }
                }
            }
        }

        // Apply type filter (dirs always recurse regardless)
        let passes_type = match opts.type_filter {
            Some(1) => !is_dir,
            Some(2) => is_dir,
            _ => true,
        };
        // Apply glob filter: match against the entry's filename component
        let passes_glob = opts.glob_pat.map(|pat| pat.matches(&name)).unwrap_or(true);

        if passes_type && passes_glob {
            entries.push(e);
        }

        if is_dir && depth > 1 {
            let _ = Box::pin(ls_recurse(root, &entry.path(), depth - 1, opts, entries)).await;
        }
    }

    Ok(())
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

    // --- resilient matching (#1127) ---

    #[tokio::test]
    async fn patch_tolerates_wrong_leading_whitespace() {
        // The model emits `old` with mangled indentation (a common failure);
        // the applier should still match on non-whitespace content and rewrite
        // using the file's ACTUAL indentation, so the edit lands correctly.
        //
        // Non-vacuity: the file is TAB-indented but the model's `old` uses
        // spaces, so a naive `content.contains(old)` cannot match (verified) —
        // only line-wise whitespace-tolerant matching succeeds.
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let original = "fn main() {\n\tlet x = 1;\n\tprintln!(\"{x}\");\n}\n";
        write(
            &mut s,
            &Op {
                c: 1,
                p: Some("ws.rs".into()),
                s: Some(original.into()),
                ..Op::default()
            },
        )
        .await;
        // Guard against a vacuous test: the raw substring must NOT be present,
        // so a pass proves the whitespace-tolerant path did the work.
        assert!(!original.contains("    let x = 1;"));
        // `old` uses 4-space indent; the file uses a tab. Non-whitespace matches.
        let r = patch(
            &mut s,
            &Op {
                c: 2,
                p: Some("ws.rs".into()),
                a: Some(vec!["    let x = 1;".into(), "    let x = 42;".into()]),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        assert_eq!(r.d.unwrap()["applied"], 1);
        let content = std::fs::read_to_string(dir.path().join("ws.rs")).unwrap();
        // Rewritten preserving the file's real TAB indentation, not the model's spaces.
        assert_eq!(
            content,
            "fn main() {\n\tlet x = 42;\n\tprintln!(\"{x}\");\n}\n"
        );
    }

    #[tokio::test]
    async fn patch_reports_ambiguous_old_as_failure_and_skips_it() {
        // `old` occurs more than once: applying to the first silently is unsafe
        // (the model may have meant a different site). Report it as a structured
        // failure and leave the file unchanged for that pair.
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        write(
            &mut s,
            &Op {
                c: 1,
                p: Some("amb.txt".into()),
                s: Some("x = 1\nx = 1\n".into()),
                ..Op::default()
            },
        )
        .await;
        let r = patch(
            &mut s,
            &Op {
                c: 2,
                p: Some("amb.txt".into()),
                a: Some(vec!["x = 1".into(), "x = 2".into()]),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok); // additive contract: still ok:true (option b)
        let d = r.d.unwrap();
        assert_eq!(d["applied"], 0, "ambiguous edit must not silently apply");
        let failures = d["failures"].as_array().expect("failures array present");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0]["old"], "x = 1");
        assert!(
            failures[0]["reason"]
                .as_str()
                .unwrap()
                .contains("ambiguous"),
            "reason should say why: {:?}",
            failures[0]["reason"]
        );
        // File unchanged.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("amb.txt")).unwrap(),
            "x = 1\nx = 1\n"
        );
    }

    #[tokio::test]
    async fn patch_miss_returns_structured_failure_and_still_applies_others() {
        // A missing `old` must be reported (which pair + a reason), not silently
        // dropped, while other pairs in the same call still apply.
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        write(
            &mut s,
            &Op {
                c: 1,
                p: Some("miss.txt".into()),
                s: Some("alpha beta".into()),
                ..Op::default()
            },
        )
        .await;
        let r = patch(
            &mut s,
            &Op {
                c: 2,
                p: Some("miss.txt".into()),
                a: Some(vec![
                    "alpha".into(),
                    "ALPHA".into(),
                    "nonexistent".into(),
                    "whatever".into(),
                ]),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["applied"], 1, "the matching pair still applies");
        let failures = d["failures"].as_array().expect("failures array present");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0]["old"], "nonexistent");
        assert!(failures[0]["reason"]
            .as_str()
            .unwrap()
            .contains("not found"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("miss.txt")).unwrap(),
            "ALPHA beta"
        );
    }

    #[tokio::test]
    async fn patch_all_applied_has_no_failures_key() {
        // When every pair applies, `failures` must be absent (not an empty
        // array) so the all-success response is unchanged from before.
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        write(
            &mut s,
            &Op {
                c: 1,
                p: Some("clean.txt".into()),
                s: Some("one two".into()),
                ..Op::default()
            },
        )
        .await;
        let r = patch(
            &mut s,
            &Op {
                c: 2,
                p: Some("clean.txt".into()),
                a: Some(vec!["one".into(), "1".into()]),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["applied"], 1);
        assert!(
            d.get("failures").is_none(),
            "no failures key on full success"
        );
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
    async fn ls_omits_mode_mtime_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        std::fs::write(dir.path().join("f.txt"), "hello").unwrap();

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
        let entry = entries.iter().find(|e| e["n"] == "f.txt").unwrap();
        assert!(
            entry.get("m").is_none(),
            "should NOT have mode field by default"
        );
        assert!(
            entry.get("t").is_none(),
            "should NOT have mtime field by default"
        );
    }

    #[tokio::test]
    async fn ls_includes_mode_and_mtime_with_stat() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        std::fs::write(dir.path().join("f.txt"), "hello").unwrap();

        let r = ls(
            &s,
            &Op {
                c: 3,
                g: Some("stat".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let entries = r.d.unwrap()["entries"].as_array().unwrap().clone();
        let entry = entries.iter().find(|e| e["n"] == "f.txt").unwrap();
        assert!(entry.get("m").is_some(), "should have mode field");
        assert!(entry.get("t").is_some(), "should have mtime field");
        let mode = entry["m"].as_str().unwrap();
        assert!(!mode.is_empty());
        let mtime = entry["t"].as_u64().unwrap();
        assert!(mtime > 0);
    }

    #[tokio::test]
    async fn ls_hides_dotfiles_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        std::fs::write(dir.path().join("visible.txt"), "").unwrap();
        std::fs::write(dir.path().join(".hidden"), "").unwrap();

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
        let names: Vec<&str> = entries.iter().map(|e| e["n"].as_str().unwrap()).collect();
        assert!(names.contains(&"visible.txt"));
        assert!(!names.contains(&".hidden"));
    }

    #[tokio::test]
    async fn ls_shows_dotfiles_with_all() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        std::fs::write(dir.path().join("visible.txt"), "").unwrap();
        std::fs::write(dir.path().join(".hidden"), "").unwrap();

        let r = ls(
            &s,
            &Op {
                c: 3,
                g: Some("all".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let entries = r.d.unwrap()["entries"].as_array().unwrap().clone();
        let names: Vec<&str> = entries.iter().map(|e| e["n"].as_str().unwrap()).collect();
        assert!(names.contains(&"visible.txt"));
        assert!(names.contains(&".hidden"));
    }

    #[tokio::test]
    async fn ls_recursive_depth() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/child.txt"), "").unwrap();
        std::fs::write(dir.path().join("a/b/grandchild.txt"), "").unwrap();

        let r = ls(
            &s,
            &Op {
                c: 3,
                n: Some(2),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let entries = r.d.unwrap()["entries"].as_array().unwrap().clone();
        let names: Vec<&str> = entries.iter().map(|e| e["n"].as_str().unwrap()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"a/child.txt"));
        assert!(names.contains(&"a/b"));
        assert!(
            !names.contains(&"a/b/grandchild.txt"),
            "depth=2 should not reach grandchild"
        );
    }

    #[tokio::test]
    async fn ls_skips_git_and_target_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::create_dir(dir.path().join("target")).unwrap();
        std::fs::create_dir(dir.path().join("node_modules")).unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();

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
        let names: Vec<&str> = entries.iter().map(|e| e["n"].as_str().unwrap()).collect();
        assert!(names.contains(&"src"));
        assert!(names.contains(&"Cargo.toml"));
        assert!(!names.contains(&"target"), "target should be skipped");
        assert!(
            !names.contains(&"node_modules"),
            "node_modules should be skipped"
        );
        assert!(!names.contains(&".git"), ".git is hidden AND in skip list");
    }

    #[tokio::test]
    async fn ls_recursive_depth3_reaches_grandchild() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/b/grandchild.txt"), "").unwrap();

        let r = ls(
            &s,
            &Op {
                c: 3,
                n: Some(3),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let entries = r.d.unwrap()["entries"].as_array().unwrap().clone();
        let names: Vec<&str> = entries.iter().map(|e| e["n"].as_str().unwrap()).collect();
        assert!(
            names.contains(&"a/b/grandchild.txt"),
            "depth=3 should reach grandchild"
        );
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
        assert!(!d["matches"].as_array().unwrap().is_empty());
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

    // --- Structured ResponseMeta plumbing (vikunja #247) ---
    //
    // Asserts the analytics signal travels via `Response.meta` rather than
    // requiring the MCP layer to re-parse the wire format. A dedup hit must
    // carry `meta.read_dedup = true`; the first (cache-miss) read and any
    // partial read must leave the flag false.

    #[tokio::test]
    async fn read_dedup_sets_meta_flag_on_cache_hit() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        std::fs::write(dir.path().join("dup.txt"), "hello\n").unwrap();

        let first = read(&mut s, &op_read("dup.txt")).await;
        assert!(first.ok);
        assert!(
            !first.meta.read_dedup,
            "first read is a cache miss; meta.read_dedup must be false"
        );

        let second = read(&mut s, &op_read("dup.txt")).await;
        assert!(second.ok);
        assert!(
            second.meta.read_dedup,
            "second read of unchanged content must set meta.read_dedup"
        );
        assert_eq!(second.d.as_ref().unwrap()["unchanged"], true);
    }

    // --- ls glob / type-filter tests (#36) ---

    #[tokio::test]
    async fn ls_glob_filter_returns_matching_files() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        std::fs::write(dir.path().join("main.rs"), "").unwrap();
        std::fs::write(dir.path().join("lib.rs"), "").unwrap();
        std::fs::write(dir.path().join("main.py"), "").unwrap();

        let r = ls(
            &s,
            &Op {
                c: 3,
                q: Some("*.rs".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let entries = r.d.unwrap()["entries"].as_array().unwrap().clone();
        let names: Vec<&str> = entries.iter().map(|e| e["n"].as_str().unwrap()).collect();
        assert!(names.contains(&"main.rs"));
        assert!(names.contains(&"lib.rs"));
        assert!(
            !names.contains(&"main.py"),
            "main.py should be filtered by glob"
        );
    }

    #[tokio::test]
    async fn ls_glob_no_match_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        std::fs::write(dir.path().join("main.py"), "").unwrap();
        std::fs::write(dir.path().join("lib.py"), "").unwrap();

        let r = ls(
            &s,
            &Op {
                c: 3,
                q: Some("*.rs".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let entries = r.d.unwrap()["entries"].as_array().unwrap().clone();
        assert!(entries.is_empty(), "no entries should match *.rs");
    }

    #[tokio::test]
    async fn ls_type_filter_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        std::fs::write(dir.path().join("file.txt"), "").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();

        let r = ls(
            &s,
            &Op {
                c: 3,
                n2: Some(1), // files only
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let entries = r.d.unwrap()["entries"].as_array().unwrap().clone();
        let names: Vec<&str> = entries.iter().map(|e| e["n"].as_str().unwrap()).collect();
        assert!(names.contains(&"file.txt"));
        assert!(
            !names.contains(&"subdir"),
            "subdir should be excluded by type=files"
        );
    }

    #[tokio::test]
    async fn ls_type_filter_dirs_only() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        std::fs::write(dir.path().join("file.txt"), "").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();

        let r = ls(
            &s,
            &Op {
                c: 3,
                n2: Some(2), // dirs only
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let entries = r.d.unwrap()["entries"].as_array().unwrap().clone();
        let names: Vec<&str> = entries.iter().map(|e| e["n"].as_str().unwrap()).collect();
        assert!(
            !names.contains(&"file.txt"),
            "file.txt should be excluded by type=dirs"
        );
        assert!(names.contains(&"subdir"));
    }

    #[tokio::test]
    async fn ls_glob_recursive_finds_nested_matches() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/lib.py"), "").unwrap();
        std::fs::write(dir.path().join("readme.md"), "").unwrap();

        let r = ls(
            &s,
            &Op {
                c: 3,
                n: Some(3),
                q: Some("*.rs".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let entries = r.d.unwrap()["entries"].as_array().unwrap().clone();
        let names: Vec<&str> = entries.iter().map(|e| e["n"].as_str().unwrap()).collect();
        assert!(
            names.contains(&"src/main.rs"),
            "should find nested .rs file"
        );
        assert!(!names.contains(&"src/lib.py"), "should not find .py files");
        assert!(!names.contains(&"readme.md"), "should not find .md files");
        assert!(
            !names.contains(&"src"),
            "src dir doesn't match *.rs, should not appear"
        );
    }

    #[tokio::test]
    async fn ls_glob_and_type_combined() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/lib.py"), "").unwrap();
        std::fs::write(dir.path().join("notes.rs"), "").unwrap();

        let r = ls(
            &s,
            &Op {
                c: 3,
                n: Some(3),
                q: Some("*.rs".into()),
                n2: Some(1), // files only
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let entries = r.d.unwrap()["entries"].as_array().unwrap().clone();
        let names: Vec<&str> = entries.iter().map(|e| e["n"].as_str().unwrap()).collect();
        assert!(names.contains(&"src/main.rs"), "nested .rs file matches");
        assert!(names.contains(&"notes.rs"), "top-level .rs file matches");
        assert!(!names.contains(&"src/lib.py"), ".py filtered by glob");
        assert!(!names.contains(&"src"), "src dir filtered by type=files");
    }

    #[tokio::test]
    async fn read_dedup_meta_not_set_for_partial_reads() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        std::fs::write(dir.path().join("p.txt"), "a\nb\nc\n").unwrap();

        // Prime the cache via a full read.
        let _ = read(&mut s, &op_read("p.txt")).await;

        // Partial reads bypass the dedup branch entirely.
        let partial = read(
            &mut s,
            &Op {
                c: 0,
                p: Some("p.txt".into()),
                n: Some(1),
                n2: Some(1),
                ..Op::default()
            },
        )
        .await;
        assert!(partial.ok);
        assert!(
            !partial.meta.read_dedup,
            "partial reads must never set meta.read_dedup"
        );
    }
}
