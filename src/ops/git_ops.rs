use crate::protocol::{Op, Response};
use crate::session::Session;
use serde_json::json;
use similar::{ChangeTag, TextDiff};

/// Opcode 14: structured diff between two files or between a file and content.
/// - p: path to first file (required)
/// - q: path to second file, OR
/// - s: content to diff against (if q is absent)
pub async fn diff(session: &Session, op: &Op) -> Response {
    let path_a = match &op.p {
        Some(p) => session.resolve_path(p),
        None => return Response::err(3, "diff requires path in 'p'"),
    };

    let content_a = match tokio::fs::read_to_string(&path_a).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Response::err(1, &format!("not found: {}", path_a.display()));
        }
        Err(e) => return Response::err(4, &format!("read: {e}")),
    };

    let content_b = if let Some(q) = &op.q {
        let path_b = session.resolve_path(q);
        match tokio::fs::read_to_string(&path_b).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Response::err(1, &format!("not found: {}", path_b.display()));
            }
            Err(e) => return Response::err(4, &format!("read: {e}")),
        }
    } else if let Some(s) = &op.s {
        s.clone()
    } else {
        return Response::err(3, "diff requires second path in 'q' or content in 's'");
    };

    let diff = TextDiff::from_lines(&content_a, &content_b);
    let mut hunks = Vec::new();

    for group in diff.grouped_ops(3) {
        let mut changes = Vec::new();
        let mut old_start = 0;
        let mut old_len = 0;
        let mut new_start = 0;
        let mut new_len = 0;

        for op in &group {
            if changes.is_empty() {
                old_start = op.old_range().start + 1;
                new_start = op.new_range().start + 1;
            }
            old_len = op.old_range().end - (old_start - 1);
            new_len = op.new_range().end - (new_start - 1);

            for change in diff.iter_changes(op) {
                let tag = match change.tag() {
                    ChangeTag::Equal => "=",
                    ChangeTag::Delete => "-",
                    ChangeTag::Insert => "+",
                };
                changes.push(json!({
                    "t": tag,
                    "v": change.value().trim_end_matches('\n'),
                }));
            }
        }

        hunks.push(json!({
            "old": [old_start, old_len],
            "new": [new_start, new_len],
            "changes": changes,
        }));
    }

    let identical = hunks.is_empty();

    Response::ok(json!({
        "identical": identical,
        "hunks": hunks,
        "count": hunks.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::Arc;

    fn session_in(dir: &std::path::Path) -> Session {
        Session::new(dir.to_path_buf(), Arc::new(Config::default()))
    }

    #[tokio::test]
    async fn diff_identical_files() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "hello\nworld\n").unwrap();

        let r = diff(&s, &Op {
            c: 14,
            p: Some("a.txt".into()),
            q: Some("b.txt".into()),
            ..Op::default()
        }).await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["identical"], true);
        assert_eq!(d["count"], 0);
    }

    #[tokio::test]
    async fn diff_different_files() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        std::fs::write(dir.path().join("a.txt"), "line1\nline2\nline3\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "line1\nchanged\nline3\n").unwrap();

        let r = diff(&s, &Op {
            c: 14,
            p: Some("a.txt".into()),
            q: Some("b.txt".into()),
            ..Op::default()
        }).await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["identical"], false);
        assert!(d["count"].as_u64().unwrap() >= 1);

        let hunks = d["hunks"].as_array().unwrap();
        let changes = hunks[0]["changes"].as_array().unwrap();
        let tags: Vec<&str> = changes.iter().map(|c| c["t"].as_str().unwrap()).collect();
        assert!(tags.contains(&"-"));
        assert!(tags.contains(&"+"));
    }

    #[tokio::test]
    async fn diff_file_vs_content() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        std::fs::write(dir.path().join("orig.txt"), "alpha\nbeta\n").unwrap();

        let r = diff(&s, &Op {
            c: 14,
            p: Some("orig.txt".into()),
            s: Some("alpha\ngamma\n".into()),
            ..Op::default()
        }).await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["identical"], false);
    }

    #[tokio::test]
    async fn diff_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        let r = diff(&s, &Op {
            c: 14,
            p: Some("nope.txt".into()),
            q: Some("also_nope.txt".into()),
            ..Op::default()
        }).await;
        assert!(!r.ok);
        assert_eq!(r.e, Some(1));
    }

    #[tokio::test]
    async fn diff_missing_args() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        let r = diff(&s, &Op { c: 14, ..Op::default() }).await;
        assert!(!r.ok);
        assert_eq!(r.e, Some(3));
    }
}
