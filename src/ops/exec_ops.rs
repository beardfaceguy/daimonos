use crate::protocol::{Op, Response};
use crate::session::{BgProcess, Session};
use serde_json::json;
use std::process::Stdio;
use tokio::process::Command;

/// Truncate output that exceeds `max_chars` by keeping first and last lines
/// with a truncation notice in the middle.
fn cap_output(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars || max_chars == 0 {
        return text.to_string();
    }
    let half = max_chars / 2;
    let lines: Vec<&str> = text.lines().collect();
    let mut head = String::new();
    let mut head_count = 0;
    for line in &lines {
        if head.len() + line.len() + 1 > half {
            break;
        }
        if !head.is_empty() {
            head.push('\n');
        }
        head.push_str(line);
        head_count += 1;
    }
    let mut tail = String::new();
    let mut tail_count = 0;
    for line in lines.iter().rev() {
        if tail.len() + line.len() + 1 > half {
            break;
        }
        if !tail.is_empty() {
            tail.insert(0, '\n');
        }
        tail.insert_str(0, line);
        tail_count += 1;
    }
    let skipped = lines.len().saturating_sub(head_count + tail_count);
    format!(
        "{head}\n\n... [{skipped} lines, {total_chars} chars truncated] ...\n\n{tail}",
        total_chars = text.len()
    )
}

pub async fn exec(session: &Session, op: &Op) -> Response {
    let cmd = match &op.s {
        Some(c) => c.clone(),
        None => return Response::err(3, "exec requires command in 's'"),
    };

    let args = op.a.clone().unwrap_or_default();

    let cwd = match &op.q {
        Some(d) => session.resolve_path(d),
        None => session.cwd.clone(),
    };

    let mut command = Command::new(&cmd);
    command
        .args(&args)
        .current_dir(&cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    for (k, v) in &session.env {
        command.env(k, v);
    }

    if let Some(kv) = &op.kv {
        for (k, v) in kv {
            command.env(k, v);
        }
    }

    let output = match command.output().await {
        Ok(o) => o,
        Err(e) => return Response::err(5, &format!("exec: {e}")),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit = output.status.code().unwrap_or(-1);
    let max = session.cfg.process.exec_output_max_chars;

    let mut resp = json!({
        "exit": exit,
        "out": cap_output(stdout.trim_end(), max),
    });
    let err_trimmed = stderr.trim_end();
    if !err_trimmed.is_empty() {
        resp["err"] = json!(cap_output(err_trimmed, max));
    }
    Response::ok(resp)
}

pub async fn bg(session: &mut Session, op: &Op) -> Response {
    let cmd = match &op.s {
        Some(c) => c.clone(),
        None => return Response::err(3, "bg requires command in 's'"),
    };

    let args = op.a.clone().unwrap_or_default();

    let cwd = match &op.q {
        Some(d) => session.resolve_path(d),
        None => session.cwd.clone(),
    };

    let pid = session.alloc_pid();
    let output_path = std::env::temp_dir().join(format!("daimonos_bg_{pid}.log"));

    let out_file = match std::fs::File::create(&output_path) {
        Ok(f) => f,
        Err(e) => return Response::err(4, &format!("create log: {e}")),
    };

    let err_file = match out_file.try_clone() {
        Ok(f) => f,
        Err(e) => return Response::err(4, &format!("clone log: {e}")),
    };

    let mut command = Command::new(&cmd);
    command
        .args(&args)
        .current_dir(&cwd)
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file));

    for (k, v) in &session.env {
        command.env(k, v);
    }

    let child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return Response::err(5, &format!("spawn: {e}")),
    };

    session.bg_processes.insert(
        pid,
        BgProcess {
            child,
            output_path: output_path.clone(),
        },
    );

    Response::ok(json!({
        "pid": pid,
        "log": output_path.to_string_lossy(),
    }))
}

pub async fn poll(session: &mut Session, op: &Op) -> Response {
    let pid = match op.n {
        Some(p) => p as u32,
        None => return Response::err(3, "poll requires pid in 'n'"),
    };

    let proc = match session.bg_processes.get_mut(&pid) {
        Some(p) => p,
        None => return Response::err(7, &format!("no process with pid {pid}")),
    };

    let status = proc.child.try_wait();
    let tail = tokio::fs::read_to_string(&proc.output_path)
        .await
        .ok()
        .map(|s| {
            let lines: Vec<&str> = s.lines().collect();
            let tail_n = session.cfg.process.poll_tail_lines;
            let start = if lines.len() > tail_n { lines.len() - tail_n } else { 0 };
            lines[start..].join("\n")
        });

    match status {
        Ok(Some(exit)) => {
            let code = exit.code().unwrap_or(-1);
            Response::ok(json!({
                "running": false,
                "exit": code,
                "tail": tail,
            }))
        }
        Ok(None) => Response::ok(json!({
            "running": true,
            "tail": tail,
        })),
        Err(e) => Response::err(4, &format!("poll: {e}")),
    }
}

pub async fn kill(session: &mut Session, op: &Op) -> Response {
    let pid = match op.n {
        Some(p) => p as u32,
        None => return Response::err(3, "kill requires pid in 'n'"),
    };

    let proc = match session.bg_processes.get_mut(&pid) {
        Some(p) => p,
        None => return Response::err(7, &format!("no process with pid {pid}")),
    };

    match proc.child.kill().await {
        Ok(()) => Response::ok(json!({"ok": true})),
        Err(e) => Response::err(4, &format!("kill: {e}")),
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

    #[tokio::test]
    async fn exec_captures_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        let r = exec(&s, &Op {
            c: 8,
            s: Some("echo".into()),
            a: Some(vec!["hello".into()]),
            ..Op::default()
        }).await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["exit"], 0);
        assert_eq!(d["out"], "hello");
    }

    #[tokio::test]
    async fn exec_captures_stderr_and_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        let r = exec(&s, &Op {
            c: 8,
            s: Some("sh".into()),
            a: Some(vec!["-c".into(), "echo err >&2; exit 42".into()]),
            ..Op::default()
        }).await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["exit"], 42);
        assert_eq!(d["err"], "err");
    }

    #[tokio::test]
    async fn exec_missing_command_arg() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        let r = exec(&s, &Op { c: 8, ..Op::default() }).await;
        assert!(!r.ok);
        assert_eq!(r.e, Some(3));
    }

    #[tokio::test]
    async fn exec_nonexistent_binary() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        let r = exec(&s, &Op {
            c: 8,
            s: Some("/nonexistent_binary_xyz".into()),
            ..Op::default()
        }).await;
        assert!(!r.ok);
        assert_eq!(r.e, Some(5));
    }

    #[tokio::test]
    async fn exec_with_env_vars() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        s.env.insert("MY_VAR".into(), "session_val".into());

        let r = exec(&s, &Op {
            c: 8,
            s: Some("sh".into()),
            a: Some(vec!["-c".into(), "echo $MY_VAR".into()]),
            ..Op::default()
        }).await;
        assert!(r.ok);
        assert_eq!(r.d.unwrap()["out"], "session_val");
    }

    #[tokio::test]
    async fn bg_poll_kill_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());

        let bg_resp = bg(&mut s, &Op {
            c: 9,
            s: Some("sleep".into()),
            a: Some(vec!["60".into()]),
            ..Op::default()
        }).await;
        assert!(bg_resp.ok);
        let pid = bg_resp.d.as_ref().unwrap()["pid"].as_u64().unwrap() as i64;

        let poll_resp = poll(&mut s, &Op {
            c: 10,
            n: Some(pid),
            ..Op::default()
        }).await;
        assert!(poll_resp.ok);
        assert_eq!(poll_resp.d.as_ref().unwrap()["running"], true);

        let kill_resp = kill(&mut s, &Op {
            c: 11,
            n: Some(pid),
            ..Op::default()
        }).await;
        assert!(kill_resp.ok);
    }

    #[tokio::test]
    async fn poll_nonexistent_pid() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let r = poll(&mut s, &Op {
            c: 10,
            n: Some(999),
            ..Op::default()
        }).await;
        assert!(!r.ok);
        assert_eq!(r.e, Some(7));
    }

    #[test]
    fn cap_output_short_text_unchanged() {
        let text = "hello world";
        assert_eq!(cap_output(text, 100), text);
    }

    #[test]
    fn cap_output_zero_max_unchanged() {
        let text = "hello world";
        assert_eq!(cap_output(text, 0), text);
    }

    #[test]
    fn cap_output_truncates_large_text() {
        let lines: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
        let text = lines.join("\n");
        let capped = cap_output(&text, 200);
        assert!(capped.contains("truncated"));
        assert!(capped.contains("line 0"));
        assert!(capped.contains("line 99"));
        assert!(capped.len() < text.len());
    }

    #[tokio::test]
    async fn exec_caps_large_output() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.process.exec_output_max_chars = 100;
        let s = Session::new(dir.path().to_path_buf(), Arc::new(cfg));

        let r = exec(&s, &Op {
            c: 8,
            s: Some("sh".into()),
            a: Some(vec!["-c".into(), "seq 1 1000".into()]),
            ..Op::default()
        }).await;
        assert!(r.ok);
        let out = r.d.unwrap()["out"].as_str().unwrap().to_string();
        assert!(out.contains("truncated"));
        assert!(out.len() < 500);
    }
}
