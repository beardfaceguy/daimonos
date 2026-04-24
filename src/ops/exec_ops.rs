use crate::protocol::{Op, Response};
use crate::session::{BgProcess, Session};
use serde_json::json;
use std::process::Stdio;
use tokio::process::Command;

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

    Response::ok(json!({
        "exit": exit,
        "out": stdout.trim_end(),
        "err": stderr.trim_end(),
    }))
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
