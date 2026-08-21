use crate::ops::{exec_filter, ExecProgress, ExecProgressCallback};
use crate::protocol::{Op, Response};
use crate::session::Session;
use crate::tool_runner::ToolRegistry;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;

/// Merge per-call env vars (`op.kv`) on top of the session env, so callers
/// can override session-wide values for a single invocation. Used by `exec`,
/// `bg`, and the plugin-redirect path so they all see the same environment.
fn merge_env(
    session_env: &HashMap<String, String>,
    op_kv: Option<&HashMap<String, String>>,
) -> HashMap<String, String> {
    let mut merged = session_env.clone();
    if let Some(kv) = op_kv {
        for (k, v) in kv {
            merged.insert(k.clone(), v.clone());
        }
    }
    merged
}

/// When args is empty and command contains whitespace, shell-wrap via `sh -c`
/// so models can send `command: "cargo test"` without splitting into args.
fn build_command(cmd: &str, args: &[String]) -> Command {
    let mut command = if args.is_empty() && cmd.contains(' ') {
        let mut c = Command::new("sh");
        c.args(["-c", cmd]);
        c
    } else {
        let mut c = Command::new(cmd);
        c.args(args);
        c
    };
    // Never hand a child our stdin. Under `daimonos acp` fd 0 is the JSON-RPC
    // pipe from the client: an inheriting child can consume protocol bytes, and
    // a child that reads stdin asynchronously (node/libuv, python asyncio) sets
    // O_NONBLOCK on the shared open file description without restoring it, which
    // makes our own transport reads fail with EAGAIN.
    command.stdin(Stdio::null());
    command
}

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

/// Attempt to redirect an exec command through a native plugin for structured output.
/// Returns Some(Response) if the command was handled, None to fall through to raw exec.
async fn try_plugin_redirect(
    full_cmd: &str,
    cwd: &Path,
    env: &HashMap<String, String>,
    registry: &Arc<ToolRegistry>,
) -> Option<Response> {
    let words: Vec<&str> = full_cmd.split_whitespace().collect();
    if words.is_empty() {
        return None;
    }

    let base = words[0].rsplit('/').next().unwrap_or(words[0]);
    let rest = &words[1..];

    match base {
        "cargo" => parse_cargo_redirect(rest, cwd, env, registry).await,
        "git" => parse_git_redirect(rest, cwd, env, registry).await,
        "gh" => parse_gh_redirect(rest, cwd, env, registry).await,
        "docker" => parse_docker_redirect(rest, cwd, env, registry).await,
        _ => None,
    }
}

fn plugin_response(output: serde_json::Value) -> Response {
    let text = serde_json::to_string(&output).unwrap_or_default();
    Response::ok(json!({"exit": 0, "out": text, "via": "plugin"})).redirect_via_plugin()
}

fn plugin_error_response(tool: &str, cmd: &str, err: &str) -> Response {
    Response::ok(json!({"exit": 1, "out": format!("{tool} {cmd}: {err}"), "via": "plugin"}))
        .redirect_via_plugin()
}

// --- cargo redirect ---

async fn parse_cargo_redirect(
    args: &[&str],
    cwd: &Path,
    env: &HashMap<String, String>,
    registry: &Arc<ToolRegistry>,
) -> Option<Response> {
    if args.is_empty() {
        return None;
    }
    let subcommand = args[0];
    let rest = &args[1..];

    match subcommand {
        "test" | "build" | "check" | "clippy" | "fmt" | "add" => {}
        _ => return None,
    }

    let mut plugin_args = json!({});
    let mut i = 0;
    let flag_args = rest;
    while i < flag_args.len() {
        match flag_args[i] {
            "--package" | "-p" if i + 1 < flag_args.len() => {
                plugin_args["package"] = json!(flag_args[i + 1]);
                i += 2;
            }
            "--lib" => {
                plugin_args["lib"] = json!(true);
                i += 1;
            }
            "--check" if subcommand == "fmt" => {
                plugin_args["check"] = json!(true);
                i += 1;
            }
            "--release" if subcommand == "build" => {
                plugin_args["release"] = json!(true);
                i += 1;
            }
            "--" => {
                // Everything after -- is the test filter
                if i + 1 < flag_args.len() {
                    plugin_args["filter"] = json!(flag_args[i + 1..].join(" "));
                }
                break;
            }
            _ => {
                // Unrecognized flag — fall through to raw exec
                return None;
            }
        }
    }

    let extra = if plugin_args.as_object().is_none_or(|o| o.is_empty()) {
        None
    } else {
        Some(plugin_args)
    };

    match registry
        .run("cargo", subcommand, cwd, env, None, extra.as_ref())
        .await
    {
        Ok(result) => Some(plugin_response(result.output)),
        Err(e) => Some(plugin_error_response("cargo", subcommand, &e)),
    }
}

// --- git redirect ---

async fn parse_git_redirect(
    args: &[&str],
    cwd: &Path,
    env: &HashMap<String, String>,
    registry: &Arc<ToolRegistry>,
) -> Option<Response> {
    if args.is_empty() {
        return None;
    }
    let subcommand = args[0];
    let rest = &args[1..];

    match subcommand {
        "status" | "log" | "diff" | "branch" | "add" | "commit" | "push" | "pull" | "checkout" => {}
        _ => return None,
    }

    let mut plugin_args = json!({});
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            "-n" | "--max-count" if i + 1 < rest.len() => {
                if let Ok(n) = rest[i + 1].parse::<i64>() {
                    plugin_args["limit"] = json!(n);
                }
                i += 2;
            }
            "--oneline" => {
                plugin_args["oneline"] = json!(true);
                i += 1;
            }
            "--staged" | "--cached" => {
                plugin_args["mode"] = json!("staged");
                i += 1;
            }
            "-m" if i + 1 < rest.len() => {
                plugin_args["message"] = json!(rest[i + 1]);
                i += 2;
            }
            "-a" | "--all" if subcommand == "commit" => {
                plugin_args["all"] = json!(true);
                i += 1;
            }
            "-b" if subcommand == "checkout" && i + 1 < rest.len() => {
                plugin_args["branch"] = json!(rest[i + 1]);
                plugin_args["create"] = json!(true);
                i += 2;
            }
            "--" => {
                // Path filter for log/diff
                if i + 1 < rest.len() {
                    plugin_args["path"] = json!(rest[i + 1]);
                }
                break;
            }
            arg if !arg.starts_with('-') => {
                // Positional arg: branch name for checkout, path for add/log
                match subcommand {
                    "checkout" => {
                        plugin_args["branch"] = json!(arg);
                    }
                    "add" => {
                        plugin_args["path"] = json!(arg);
                    }
                    _ => return None,
                }
                i += 1;
            }
            _ => return None,
        }
    }

    let extra = if plugin_args.as_object().is_none_or(|o| o.is_empty()) {
        None
    } else {
        Some(plugin_args)
    };

    match registry
        .run("git", subcommand, cwd, env, None, extra.as_ref())
        .await
    {
        Ok(result) => Some(plugin_response(result.output)),
        Err(e) => Some(plugin_error_response("git", subcommand, &e)),
    }
}

// --- gh redirect ---

async fn parse_gh_redirect(
    args: &[&str],
    cwd: &Path,
    env: &HashMap<String, String>,
    registry: &Arc<ToolRegistry>,
) -> Option<Response> {
    if args.is_empty() {
        return None;
    }

    // Map CLI subcommands to plugin commands
    let (plugin_cmd, rest) = match (args[0], args.get(1).copied()) {
        ("pr", Some("view")) => ("pr_view", &args[2..]),
        ("pr", Some("list")) => ("pr_list", &args[2..]),
        ("pr", Some("diff")) => ("pr_diff", &args[2..]),
        ("pr", Some("checks")) => ("pr_checks", &args[2..]),
        ("pr", Some("create")) => ("pr_create", &args[2..]),
        ("api", _) => ("api", &args[1..]),
        _ => return None,
    };

    let mut plugin_args = json!({});
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            "--state" if i + 1 < rest.len() => {
                plugin_args["state"] = json!(rest[i + 1]);
                i += 2;
            }
            "--limit" if i + 1 < rest.len() => {
                if let Ok(n) = rest[i + 1].parse::<i64>() {
                    plugin_args["limit"] = json!(n);
                }
                i += 2;
            }
            "--author" if i + 1 < rest.len() => {
                plugin_args["author"] = json!(rest[i + 1]);
                i += 2;
            }
            "--method" if i + 1 < rest.len() && plugin_cmd == "api" => {
                plugin_args["method"] = json!(rest[i + 1]);
                i += 2;
            }
            arg if !arg.starts_with('-') => {
                // Positional: PR number or API endpoint
                if plugin_cmd == "api" {
                    plugin_args["endpoint"] = json!(arg);
                } else if let Ok(n) = arg.parse::<i64>() {
                    plugin_args["number"] = json!(n);
                }
                i += 1;
            }
            _ => return None,
        }
    }

    let extra = if plugin_args.as_object().is_none_or(|o| o.is_empty()) {
        None
    } else {
        Some(plugin_args)
    };

    match registry
        .run("gh", plugin_cmd, cwd, env, None, extra.as_ref())
        .await
    {
        Ok(result) => Some(plugin_response(result.output)),
        Err(e) => Some(plugin_error_response("gh", plugin_cmd, &e)),
    }
}

// --- docker redirect ---

async fn parse_docker_redirect(
    args: &[&str],
    cwd: &Path,
    env: &HashMap<String, String>,
    registry: &Arc<ToolRegistry>,
) -> Option<Response> {
    if args.is_empty() {
        return None;
    }

    let (plugin_cmd, rest) = match (args[0], args.get(1).copied()) {
        ("ps", _) => ("ps", &args[1..]),
        ("images", _) => ("images", &args[1..]),
        ("logs", _) => ("logs", &args[1..]),
        ("stop", _) => ("stop", &args[1..]),
        ("inspect", _) => ("inspect", &args[1..]),
        ("compose", Some("up")) => ("compose_up", &args[2..]),
        ("compose", Some("down")) => ("compose_down", &args[2..]),
        ("compose", Some("ps")) => ("compose_ps", &args[2..]),
        _ => return None,
    };

    let mut plugin_args = json!({});
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            "--tail" | "-n" if i + 1 < rest.len() => {
                if let Ok(n) = rest[i + 1].parse::<i64>() {
                    plugin_args["tail"] = json!(n);
                }
                i += 2;
            }
            "-f" if i + 1 < rest.len() && plugin_cmd.starts_with("compose") => {
                plugin_args["file"] = json!(rest[i + 1]);
                i += 2;
            }
            "-d" if plugin_cmd == "compose_up" => {
                plugin_args["detach"] = json!(true);
                i += 1;
            }
            arg if !arg.starts_with('-') => {
                plugin_args["container"] = json!(arg);
                i += 1;
            }
            _ => return None,
        }
    }

    let extra = if plugin_args.as_object().is_none_or(|o| o.is_empty()) {
        None
    } else {
        Some(plugin_args)
    };

    match registry
        .run("docker", plugin_cmd, cwd, env, None, extra.as_ref())
        .await
    {
        Ok(result) => Some(plugin_response(result.output)),
        Err(e) => Some(plugin_error_response("docker", plugin_cmd, &e)),
    }
}

#[cfg(test)]
pub async fn exec(session: &Session, op: &Op) -> Response {
    exec_with_progress(session, op, None).await
}

pub async fn exec_with_progress(
    session: &Session,
    op: &Op,
    on_progress: Option<&ExecProgressCallback<'_>>,
) -> Response {
    let cmd = match &op.s {
        Some(c) => c.clone(),
        None => return Response::err(3, "exec requires command in 's'"),
    };

    let args = op.a.clone().unwrap_or_default();

    let cwd = match &op.q {
        Some(d) => session.resolve_path(d),
        None => session.cwd.clone(),
    };

    let env = merge_env(&session.env, op.kv.as_ref());

    // Layer 1: try redirecting through a native plugin for structured output.
    // Pass the merged env so per-call kv reaches the plugin too.
    // Native plugins return only their completed structured result. ACP terminal
    // streaming needs subprocess pipes, so observed foreground calls stay raw.
    if on_progress.is_none() && session.cfg.process.exec_plugin_redirect && args.is_empty() {
        if let Some(registry) = &session.tool_registry {
            if let Some(resp) = try_plugin_redirect(&cmd, &cwd, &env, registry).await {
                return resp;
            }
        }
    }

    let mut command = build_command(&cmd, &args);
    command.current_dir(&cwd);
    crate::managed_process::apply_environment(&mut command, &session.cfg.process, &env);
    let progress = on_progress.map(|callback| {
        move |text: String| {
            callback(ExecProgress::Output(text));
        }
    });
    let output = match crate::managed_process::capture(
        &mut command,
        &session.cfg.process,
        progress
            .as_ref()
            .map(|callback| callback as &crate::managed_process::ProgressCallback<'_>),
        None,
    )
    .await
    {
        Ok(output) => output,
        Err(error) => {
            if let Some(callback) = on_progress {
                callback(ExecProgress::Exit {
                    code: None,
                    signal: Some("exec_error".to_string()),
                });
            }
            return Response::err(5, &format!("exec: {error}"));
        }
    };
    if let Some(callback) = on_progress {
        callback(ExecProgress::Exit {
            code: output.status.code(),
            signal: exit_signal(&output.status),
        });
    }
    if output.timed_out {
        return Response::err(
            6,
            &format!(
                "exec timed out after {} seconds",
                session.cfg.process.default_timeout_secs
            ),
        );
    }
    let stdout = output.stdout;
    let stderr = output.stderr;
    let exit = output.status.code().unwrap_or(-1);
    let max = session.cfg.process.exec_output_max_chars;

    if session.cfg.process.exec_output_filters
        && !output.stdout_truncated
        && !output.stderr_truncated
    {
        if let Some(filtered) = exec_filter::filter_exec_output(&cmd, &stdout, &stderr, exit) {
            let raw_chars = stdout.len() + stderr.len();
            let mut resp = json!({
                "exit": exit,
                "out": cap_output(&filtered.out, max),
            });
            if !filtered.err.is_empty() {
                resp["err"] = json!(cap_output(&filtered.err, max));
            }
            return Response::ok(resp)
                .filter_applied()
                .with_unfiltered_chars(raw_chars);
        }
    }

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

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(|signal| signal.to_string())
}

#[cfg(not(unix))]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<String> {
    None
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

    if session.bg_processes.len() >= session.cfg.process.max_background_processes {
        return Response::err(
            5,
            &format!(
                "background process limit reached ({})",
                session.cfg.process.max_background_processes
            ),
        );
    }
    let pid = session.alloc_pid();

    let mut command = build_command(&cmd, &args);
    command.current_dir(&cwd);
    let env = merge_env(&session.env, op.kv.as_ref());
    crate::managed_process::apply_environment(&mut command, &session.cfg.process, &env);
    let process = match crate::managed_process::ManagedBackground::spawn(
        &mut command,
        &session.cfg.process,
        &format!("bg-{pid}"),
    ) {
        Ok(process) => process,
        Err(e) => return Response::err(5, &format!("spawn: {e}")),
    };
    let output_path = process.output_path.clone();
    session.bg_processes.insert(pid, process);

    Response::ok(json!({
        "pid": pid,
        "log": output_path.to_string_lossy(),
    }))
}

pub async fn poll(session: &mut Session, op: &Op) -> Response {
    let pid = match op.n {
        Some(p) if p > 0 && p <= u32::MAX as i64 => p as u32,
        Some(_) => return Response::err(3, "poll pid must be a positive u32"),
        None => return Response::err(3, "poll requires pid in 'n'"),
    };

    let process = match session.bg_processes.get_mut(&pid) {
        Some(process) => process,
        None => return Response::err(7, &format!("no process with pid {pid}")),
    };

    let status = process.try_wait();
    let output_path = process.output_path.clone();
    let tail_n = session.cfg.process.poll_tail_lines;

    match status {
        Ok(Some(exit)) => {
            let code = exit.code().unwrap_or(-1);
            if let Some(mut process) = session.bg_processes.remove(&pid) {
                let _ = process.settle_output().await;
                let tail = crate::managed_process::tail_lines(
                    &output_path,
                    tail_n,
                    session.cfg.process.output_memory_bytes,
                )
                .await
                .ok();
                process.cleanup_artifact();
                return Response::ok(json!({
                    "running": false,
                    "exit": code,
                    "tail": tail,
                }));
            }
            Response::ok(json!({
                "running": false,
                "exit": code,
                "tail": serde_json::Value::Null,
            }))
        }
        Ok(None) => {
            let tail = crate::managed_process::tail_lines(
                &output_path,
                tail_n,
                session.cfg.process.output_memory_bytes,
            )
            .await
            .ok();
            Response::ok(json!({
                "running": true,
                "tail": tail,
            }))
        }
        Err(e) => Response::err(4, &format!("poll: {e}")),
    }
}

pub async fn kill(session: &mut Session, op: &Op) -> Response {
    let pid = match op.n {
        Some(p) if p > 0 && p <= u32::MAX as i64 => p as u32,
        Some(_) => return Response::err(3, "kill pid must be a positive u32"),
        None => return Response::err(3, "kill requires pid in 'n'"),
    };

    let process = match session.bg_processes.remove(&pid) {
        Some(process) => process,
        None => return Response::err(7, &format!("no process with pid {pid}")),
    };

    let output_path = process.output_path.clone();
    let grace = std::time::Duration::from_millis(session.cfg.process.termination_grace_ms);
    match process.terminate(grace).await {
        Ok(_) => {
            crate::managed_process::remove_artifact(&output_path);
            Response::ok(json!({"ok": true}))
        }
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

    /// Installs a pipe carrying `payload` as fd 0 for the duration of a test,
    /// restoring the original descriptor on drop.
    ///
    /// Without this, asserting "the child did not inherit our stdin" is vacuous
    /// whenever the test harness's own stdin is already `/dev/null` — which is
    /// the common case in CI and under backgrounded runs. Giving the parent a
    /// distinctive stdin makes the assertion discriminating in every environment.
    #[cfg(unix)]
    struct StdinOverride {
        saved: i32,
    }

    #[cfg(unix)]
    impl StdinOverride {
        fn with_payload(payload: &[u8]) -> Self {
            let mut fds = [0i32; 2];
            // SAFETY: `fds` is the two-element array pipe(2) writes its ends into.
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            let (read_fd, write_fd) = (fds[0], fds[1]);
            // SAFETY: both ends are live; `payload` is far below the pipe buffer
            // so the write completes without blocking.
            unsafe {
                assert_eq!(
                    libc::write(write_fd, payload.as_ptr().cast(), payload.len()),
                    payload.len() as isize
                );
                libc::close(write_fd);
            }
            // SAFETY: fd 0 is open; `dup`/`dup2` only manipulate descriptors.
            let saved = unsafe { libc::dup(0) };
            assert!(saved >= 0);
            // SAFETY: `read_fd` is live and fd 0 is a valid target.
            unsafe {
                assert_eq!(libc::dup2(read_fd, 0), 0);
                libc::close(read_fd);
            }
            Self { saved }
        }
    }

    #[cfg(unix)]
    impl Drop for StdinOverride {
        fn drop(&mut self) {
            // SAFETY: `saved` is the descriptor duplicated from fd 0 in `new`.
            unsafe {
                libc::dup2(self.saved, 0);
                libc::close(self.saved);
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exec_does_not_give_children_our_stdin() {
        // Under `daimonos acp` fd 0 is the JSON-RPC pipe from the client. A child
        // that inherits it can consume protocol bytes, and an async-stdin child
        // leaves O_NONBLOCK set on the shared open file description, which makes
        // our own transport reads fail with EAGAIN.
        let _stdin = StdinOverride::with_payload(b"parent-protocol-bytes");
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        // `wc -c` drains stdin to EOF, so a zero byte count proves the child was
        // handed no input — the parent's stdin has 21 bytes waiting.
        let r = exec(
            &s,
            &Op {
                c: 8,
                s: Some("wc -c".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["exit"], 0);
        assert_eq!(d["out"].as_str().unwrap().trim(), "0");

        // Where procfs is available, also assert descriptor identity: an
        // inheriting child would report the parent's pipe, not /dev/null.
        if Path::new("/proc/self/fd/0").exists() {
            let r = exec(
                &s,
                &Op {
                    c: 8,
                    s: Some("readlink /proc/self/fd/0".into()),
                    ..Op::default()
                },
            )
            .await;
            assert!(r.ok);
            let d = r.d.unwrap();
            assert_eq!(d["exit"], 0);
            assert_eq!(d["out"].as_str().unwrap().trim(), "/dev/null");
        }
    }

    #[tokio::test]
    async fn exec_captures_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        let r = exec(
            &s,
            &Op {
                c: 8,
                s: Some("echo".into()),
                a: Some(vec!["hello".into()]),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["exit"], 0);
        assert_eq!(d["out"], "hello");
    }

    #[tokio::test]
    async fn exec_captures_stderr_and_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        let r = exec(
            &s,
            &Op {
                c: 8,
                s: Some("sh".into()),
                a: Some(vec!["-c".into(), "echo err >&2; exit 42".into()]),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["exit"], 42);
        assert_eq!(d["err"], "err");
    }

    #[tokio::test]
    async fn exec_streams_output_chunks_and_exit_before_returning_final_result() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        let events = std::sync::Mutex::new(Vec::new());
        let on_progress = |event| events.lock().unwrap().push(event);

        let r = exec_with_progress(
            &s,
            &Op {
                c: 8,
                s: Some("sh".into()),
                a: Some(vec![
                    "-c".into(),
                    "printf first; sleep 0.05; printf second; printf err >&2; exit 7".into(),
                ]),
                ..Op::default()
            },
            Some(&on_progress),
        )
        .await;

        assert!(r.ok);
        assert_eq!(r.d.as_ref().unwrap()["exit"], 7);
        let events = events.into_inner().unwrap();
        let streamed = events
            .iter()
            .filter_map(|event| match event {
                ExecProgress::Output(data) => Some(data.as_str()),
                ExecProgress::Exit { .. } => None,
            })
            .collect::<String>();
        assert!(streamed.contains("first"));
        assert!(streamed.contains("second"));
        assert!(streamed.contains("err"));
        assert!(matches!(
            events.last(),
            Some(ExecProgress::Exit { code: Some(7), .. })
        ));
    }

    #[tokio::test]
    async fn exec_missing_command_arg() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        let r = exec(
            &s,
            &Op {
                c: 8,
                ..Op::default()
            },
        )
        .await;
        assert!(!r.ok);
        assert_eq!(r.e, Some(3));
    }

    #[tokio::test]
    async fn exec_nonexistent_binary() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        let r = exec(
            &s,
            &Op {
                c: 8,
                s: Some("/nonexistent_binary_xyz".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(!r.ok);
        assert_eq!(r.e, Some(5));
    }

    #[tokio::test]
    async fn exec_with_env_vars() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        s.env.insert("MY_VAR".into(), "session_val".into());

        let r = exec(
            &s,
            &Op {
                c: 8,
                s: Some("sh".into()),
                a: Some(vec!["-c".into(), "echo $MY_VAR".into()]),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        assert_eq!(r.d.unwrap()["out"], "session_val");
    }

    // --- merge_env unit tests (vikunja #244 / #245) ---

    #[test]
    fn merge_env_no_kv_returns_session_env_clone() {
        let mut session = HashMap::new();
        session.insert("A".to_string(), "1".to_string());
        session.insert("B".to_string(), "2".to_string());

        let merged = merge_env(&session, None);
        assert_eq!(merged, session);
    }

    #[test]
    fn merge_env_kv_adds_new_keys() {
        let mut session = HashMap::new();
        session.insert("SESSION_KEY".to_string(), "s".to_string());

        let mut kv = HashMap::new();
        kv.insert("PER_CALL_KEY".to_string(), "p".to_string());

        let merged = merge_env(&session, Some(&kv));
        assert_eq!(merged.get("SESSION_KEY"), Some(&"s".to_string()));
        assert_eq!(merged.get("PER_CALL_KEY"), Some(&"p".to_string()));
    }

    #[test]
    fn merge_env_kv_overrides_session() {
        let mut session = HashMap::new();
        session.insert("SHARED".to_string(), "session_value".to_string());

        let mut kv = HashMap::new();
        kv.insert("SHARED".to_string(), "per_call_value".to_string());

        let merged = merge_env(&session, Some(&kv));
        assert_eq!(merged.get("SHARED"), Some(&"per_call_value".to_string()));
    }

    #[test]
    fn merge_env_empty_session_with_kv() {
        let session = HashMap::new();
        let mut kv = HashMap::new();
        kv.insert("ONLY_KV".to_string(), "x".to_string());

        let merged = merge_env(&session, Some(&kv));
        assert_eq!(merged.len(), 1);
        assert_eq!(merged.get("ONLY_KV"), Some(&"x".to_string()));
    }

    /// Regression for vikunja #244: `bg()` previously iterated only
    /// `session.env` and silently dropped per-call `op.kv`, producing
    /// inconsistent behavior vs `exec()`.
    #[tokio::test]
    async fn bg_passes_op_kv_to_subprocess() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());

        let mut kv = HashMap::new();
        kv.insert("BG_KV_VAR".to_string(), "kv_value".to_string());

        let bg_resp = bg(
            &mut s,
            &Op {
                c: 9,
                s: Some("sh".into()),
                a: Some(vec!["-c".into(), "echo $BG_KV_VAR > out.txt".into()]),
                q: Some(dir.path().to_string_lossy().into()),
                kv: Some(kv),
                ..Op::default()
            },
        )
        .await;
        assert!(bg_resp.ok, "bg failed: {:?}", bg_resp.m);
        let pid = bg_resp.d.unwrap()["pid"].as_u64().unwrap() as i64;
        let log_path = std::env::temp_dir().join(format!("daimonos_bg_{}.log", pid));

        let out_file = dir.path().join("out.txt");
        let mut appeared = false;
        for _ in 0..100 {
            if out_file.exists() {
                appeared = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            appeared,
            "bg subprocess never wrote out.txt within 2s (likely never ran or env not propagated)"
        );

        let written =
            std::fs::read_to_string(&out_file).expect("bg subprocess should have created out.txt");
        assert_eq!(
            written.trim(),
            "kv_value",
            "bg subprocess did not see op.kv env var (got: {written:?})"
        );

        // Wait for the child to actually exit, rather than treating "out.txt
        // appeared" as proof that it did. Writing the file and exiting are two
        // events: the shell still has to close the redirect and return, and
        // under parallel test load that gap is wide enough to observe. `poll`
        // is also what reaps a finished child, so calling it too early both
        // reports running=true and leaves the process in the map -- which is
        // why this failed roughly one full-suite run in three while passing
        // every time in isolation.
        let mut poll_resp = None;
        for _ in 0..100 {
            let resp = poll(
                &mut s,
                &Op {
                    c: 10,
                    n: Some(pid),
                    ..Op::default()
                },
            )
            .await;
            assert!(resp.ok, "poll failed: {:?}", resp.m);
            if resp.d.as_ref().unwrap()["running"] == false {
                poll_resp = Some(resp);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let poll_resp = poll_resp.expect("bg child never reported exit within 2s");
        assert_eq!(poll_resp.d.as_ref().unwrap()["running"], false);

        assert!(
            s.bg_processes.is_empty(),
            "bg_processes map should be empty after poll() reaped completed child"
        );
        assert!(
            !log_path.exists(),
            "bg log file at {} should be deleted after poll() reap",
            log_path.display()
        );
    }

    /// The gap the test above used to assume away, made deterministic.
    ///
    /// `bg_passes_op_kv_to_subprocess` polled once, immediately after its
    /// output file appeared, and asserted the child had exited. A file
    /// appearing does not mean the process is gone, and that race only showed
    /// up under parallel load -- it passed 12/12 in isolation and failed about
    /// one full-suite run in three.
    ///
    /// Here the child writes its file and *then* lingers, so the window is
    /// always open rather than sometimes. A single poll at the moment the file
    /// appears observes `running: true`; only waiting for the real exit gives
    /// `false` and reaps the entry.
    #[tokio::test]
    async fn poll_reports_running_until_the_child_actually_exits() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());

        let bg_resp = bg(
            &mut s,
            &Op {
                c: 9,
                s: Some("sh".into()),
                a: Some(vec!["-c".into(), "echo done > out.txt; sleep 0.5".into()]),
                q: Some(dir.path().to_string_lossy().into()),
                ..Op::default()
            },
        )
        .await;
        assert!(bg_resp.ok, "bg failed: {:?}", bg_resp.m);
        let pid = bg_resp.d.unwrap()["pid"].as_u64().unwrap() as i64;

        let out_file = dir.path().join("out.txt");
        for _ in 0..100 {
            if out_file.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(out_file.exists(), "child never wrote its output file");

        // The file is there and the child is definitely still alive. This is
        // exactly the state the old assertion mistook for "finished".
        let early = poll(
            &mut s,
            &Op {
                c: 10,
                n: Some(pid),
                ..Op::default()
            },
        )
        .await;
        assert!(early.ok);
        assert_eq!(
            early.d.as_ref().unwrap()["running"],
            true,
            "output file appearing must not be read as process exit"
        );
        assert!(
            !s.bg_processes.is_empty(),
            "a still-running child must not be reaped"
        );

        // Now wait for the real exit.
        let mut final_resp = None;
        for _ in 0..150 {
            let resp = poll(
                &mut s,
                &Op {
                    c: 10,
                    n: Some(pid),
                    ..Op::default()
                },
            )
            .await;
            assert!(resp.ok);
            if resp.d.as_ref().unwrap()["running"] == false {
                final_resp = Some(resp);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(final_resp.is_some(), "child never reported exit within 3s");
        assert!(
            s.bg_processes.is_empty(),
            "poll() should reap once the child has actually exited"
        );
    }

    /// Regression for vikunja #244: `op.kv` should override session env.
    #[tokio::test]
    async fn exec_op_kv_overrides_session_env() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        s.env.insert("SHARED".into(), "session_value".into());

        let mut kv = HashMap::new();
        kv.insert("SHARED".to_string(), "per_call_value".to_string());

        let r = exec(
            &s,
            &Op {
                c: 8,
                s: Some("sh".into()),
                a: Some(vec!["-c".into(), "echo $SHARED".into()]),
                kv: Some(kv),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        assert_eq!(r.d.unwrap()["out"], "per_call_value");
    }

    #[tokio::test]
    async fn bg_poll_kill_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());

        let bg_resp = bg(
            &mut s,
            &Op {
                c: 9,
                s: Some("sleep".into()),
                a: Some(vec!["60".into()]),
                ..Op::default()
            },
        )
        .await;
        assert!(bg_resp.ok);
        let pid = bg_resp.d.as_ref().unwrap()["pid"].as_u64().unwrap() as i64;

        let poll_resp = poll(
            &mut s,
            &Op {
                c: 10,
                n: Some(pid),
                ..Op::default()
            },
        )
        .await;
        assert!(poll_resp.ok);
        assert_eq!(poll_resp.d.as_ref().unwrap()["running"], true);

        let kill_resp = kill(
            &mut s,
            &Op {
                c: 11,
                n: Some(pid),
                ..Op::default()
            },
        )
        .await;
        assert!(kill_resp.ok);
    }

    #[tokio::test]
    async fn bg_process_removed_after_completion() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());

        let bg_resp = bg(
            &mut s,
            &Op {
                c: 9,
                s: Some("true".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(bg_resp.ok);
        let pid = bg_resp.d.as_ref().unwrap()["pid"].as_u64().unwrap() as i64;
        assert_eq!(s.bg_processes.len(), 1);

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let poll_resp = poll(
            &mut s,
            &Op {
                c: 10,
                n: Some(pid),
                ..Op::default()
            },
        )
        .await;
        assert!(poll_resp.ok);
        assert_eq!(poll_resp.d.as_ref().unwrap()["running"], false);

        assert!(
            !s.bg_processes.contains_key(&(pid as u32)),
            "completed bg process should be removed from map, but {} entries remain",
            s.bg_processes.len()
        );
    }

    #[tokio::test]
    async fn bg_process_removed_after_kill() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());

        let bg_resp = bg(
            &mut s,
            &Op {
                c: 9,
                s: Some("sleep".into()),
                a: Some(vec!["60".into()]),
                ..Op::default()
            },
        )
        .await;
        assert!(bg_resp.ok);
        let pid = bg_resp.d.as_ref().unwrap()["pid"].as_u64().unwrap() as i64;
        assert_eq!(s.bg_processes.len(), 1);

        let kill_resp = kill(
            &mut s,
            &Op {
                c: 11,
                n: Some(pid),
                ..Op::default()
            },
        )
        .await;
        assert!(kill_resp.ok);

        assert!(
            !s.bg_processes.contains_key(&(pid as u32)),
            "killed bg process should be removed from map, but {} entries remain",
            s.bg_processes.len()
        );
    }

    #[tokio::test]
    async fn bg_log_files_cleaned_up_after_completion() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());

        let bg_resp = bg(
            &mut s,
            &Op {
                c: 9,
                s: Some("true".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(bg_resp.ok);
        let pid = bg_resp.d.as_ref().unwrap()["pid"].as_u64().unwrap() as i64;
        let log_path =
            std::path::PathBuf::from(bg_resp.d.as_ref().unwrap()["log"].as_str().unwrap());
        assert!(log_path.exists(), "log file should exist after bg spawn");

        // Poll in a loop until the process is observed completed (or the loop
        // times out). The earlier fixed 200 ms sleep was racy under parallel
        // `cargo test` load — `true` may genuinely finish in <1 ms, but the
        // tokio scheduler may not get back to this task that quickly when
        // hundreds of sibling tests are saturating the runtime.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let poll_resp = poll(
                &mut s,
                &Op {
                    c: 10,
                    n: Some(pid),
                    ..Op::default()
                },
            )
            .await;
            assert!(poll_resp.ok);
            if poll_resp.d.as_ref().unwrap()["running"] == false {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "bg `true` did not complete within 5 s — runtime is unusably \
                 starved or the bg process tracking is broken"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        assert!(
            !log_path.exists(),
            "log file at {} should be deleted after process completes",
            log_path.display()
        );
    }

    #[tokio::test]
    async fn bg_processes_dont_accumulate() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());

        // Collect the pids this session was assigned. `alloc_pid` is now
        // process-global so the pids are no longer guaranteed to be 1..=10 —
        // a parallel test may have advanced the counter further.
        let mut pids = Vec::with_capacity(10);
        for i in 0..10 {
            let bg_resp = bg(
                &mut s,
                &Op {
                    c: 9,
                    s: Some("true".into()),
                    ..Op::default()
                },
            )
            .await;
            assert!(bg_resp.ok, "bg {i} failed");
            let pid = bg_resp.d.as_ref().unwrap()["pid"].as_u64().unwrap() as i64;
            pids.push(pid);
        }

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        for pid in pids {
            poll(
                &mut s,
                &Op {
                    c: 10,
                    n: Some(pid),
                    ..Op::default()
                },
            )
            .await;
        }

        assert!(
            s.bg_processes.is_empty(),
            "all 10 completed processes should be cleaned up, but {} remain",
            s.bg_processes.len()
        );
    }

    #[tokio::test]
    async fn bg_admission_counts_live_processes() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.process.max_background_processes = 1;
        cfg.process.artifact_directory = Some(dir.path().join("logs").display().to_string());
        let mut session = Session::new(dir.path().to_path_buf(), Arc::new(cfg));
        let first = bg(
            &mut session,
            &Op {
                c: 9,
                s: Some("sleep 30".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(first.ok);
        let second = bg(
            &mut session,
            &Op {
                c: 9,
                s: Some("sleep 30".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(!second.ok);
        assert!(second.m.unwrap().contains("limit reached"));
        let pid = first.d.unwrap()["pid"].as_i64().unwrap();
        assert!(
            kill(
                &mut session,
                &Op {
                    c: 11,
                    n: Some(pid),
                    ..Op::default()
                },
            )
            .await
            .ok
        );
    }

    #[tokio::test]
    async fn poll_and_kill_reject_invalid_synthetic_ids() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_in(dir.path());
        for id in [-1, 0, i64::from(u32::MAX) + 1] {
            assert!(
                !poll(
                    &mut session,
                    &Op {
                        c: 10,
                        n: Some(id),
                        ..Op::default()
                    },
                )
                .await
                .ok
            );
            assert!(
                !kill(
                    &mut session,
                    &Op {
                        c: 11,
                        n: Some(id),
                        ..Op::default()
                    },
                )
                .await
                .ok
            );
        }
    }

    #[tokio::test]
    async fn background_artifact_is_bounded_while_child_keeps_running() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.process.artifact_directory = Some(dir.path().join("logs").display().to_string());
        cfg.process.artifact_max_bytes = 32;
        let mut session = Session::new(dir.path().to_path_buf(), Arc::new(cfg));
        let response = bg(
            &mut session,
            &Op {
                c: 9,
                s: Some(
                    "python3 -c 'print(\"x\" * 4096, flush=True); import time; time.sleep(30)'"
                        .into(),
                ),
                ..Op::default()
            },
        )
        .await;
        assert!(response.ok);
        let pid = response.d.as_ref().unwrap()["pid"].as_i64().unwrap();
        let path = std::path::PathBuf::from(response.d.as_ref().unwrap()["log"].as_str().unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let len = std::fs::metadata(&path).unwrap().len();
            assert!(len <= 32, "artifact exceeded configured bound: {len}");
            if len == 32 {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            kill(
                &mut session,
                &Op {
                    c: 11,
                    n: Some(pid),
                    ..Op::default()
                },
            )
            .await
            .ok
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_shutdown_retires_background_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.process.artifact_directory = Some(dir.path().join("logs").display().to_string());
        cfg.process.termination_grace_ms = 20;
        let mut session = Session::new(dir.path().to_path_buf(), Arc::new(cfg));
        let pid_file = dir.path().join("child.pid");
        let command = format!(
            "sh -c 'sleep 30 & echo $! > \"{}\"; wait'",
            pid_file.display()
        );
        assert!(
            bg(
                &mut session,
                &Op {
                    c: 9,
                    s: Some(command),
                    ..Op::default()
                },
            )
            .await
            .ok
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !pid_file.exists() {
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let child_pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        session.shutdown_processes().await;
        assert!(session.bg_processes.is_empty());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            // SAFETY: signal 0 probes existence without changing process state.
            let rc = unsafe { libc::kill(child_pid, 0) };
            if rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "background descendant {child_pid} was not reaped"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn terminal_poll_waits_for_final_output_drain() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.process.artifact_directory = Some(dir.path().join("logs").display().to_string());
        let mut session = Session::new(dir.path().to_path_buf(), Arc::new(cfg));
        let response = bg(
            &mut session,
            &Op {
                c: 9,
                s: Some("printf start; sleep 0.05; printf end".into()),
                ..Op::default()
            },
        )
        .await;
        let pid = response.d.unwrap()["pid"].as_i64().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let response = poll(
                &mut session,
                &Op {
                    c: 10,
                    n: Some(pid),
                    ..Op::default()
                },
            )
            .await;
            assert!(response.ok);
            let data = response.d.unwrap();
            if data["running"] == false {
                assert_eq!(data["tail"], "startend");
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn poll_nonexistent_pid() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let r = poll(
            &mut s,
            &Op {
                c: 10,
                n: Some(999),
                ..Op::default()
            },
        )
        .await;
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
    async fn exec_unsplit_command_string() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        let r = exec(
            &s,
            &Op {
                c: 8,
                s: Some("echo hello world".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["exit"], 0);
        assert_eq!(d["out"], "hello world");
    }

    #[tokio::test]
    async fn exec_unsplit_with_pipe() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        let r = exec(
            &s,
            &Op {
                c: 8,
                s: Some("echo foo | tr f F".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["exit"], 0);
        assert_eq!(d["out"], "Foo");
    }

    #[tokio::test]
    async fn exec_explicit_args_not_shell_wrapped() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        let r = exec(
            &s,
            &Op {
                c: 8,
                s: Some("echo".into()),
                a: Some(vec!["hello".into(), "world".into()]),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["exit"], 0);
        assert_eq!(d["out"], "hello world");
    }

    #[tokio::test]
    async fn exec_caps_large_output() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.process.exec_output_max_chars = 100;
        let s = Session::new(dir.path().to_path_buf(), Arc::new(cfg));

        let r = exec(
            &s,
            &Op {
                c: 8,
                s: Some("sh".into()),
                a: Some(vec!["-c".into(), "seq 1 1000".into()]),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let out = r.d.unwrap()["out"].as_str().unwrap().to_string();
        assert!(out.contains("truncated"));
        assert!(out.len() < 500);
    }

    // --- Plugin redirect tests ---

    async fn session_with_registry(dir: &std::path::Path) -> Session {
        use crate::plugins;
        use crate::tool_runner::ToolRegistry;

        let cfg = Arc::new(Config::default());
        let mut s = Session::new(dir.to_path_buf(), cfg);

        let r = ToolRegistry::new();
        if plugins::cargo::is_available() {
            r.register(Arc::new(plugins::cargo::CargoPlugin::new()))
                .await;
        }
        if plugins::git::is_available() {
            r.register(Arc::new(plugins::git::GitPlugin::new())).await;
        }

        s.tool_registry = Some(Arc::new(r));
        s
    }

    #[tokio::test]
    async fn redirect_cargo_test_via_exec() {
        if !crate::plugins::cargo::is_available() {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        // Create a minimal Cargo project
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"rtest\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "#[test] fn it_works() { assert!(true); }\n",
        )
        .unwrap();

        let s = session_with_registry(dir.path()).await;

        let r = exec(
            &s,
            &Op {
                c: 8,
                s: Some("cargo test".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["exit"], 0);
        let out = d["out"].as_str().unwrap();
        assert!(d.get("via").is_some(), "should have 'via: plugin' marker");
        assert!(out.contains("passed") || out.contains("ok"), "got: {out}");
        assert!(
            !out.contains("running 1 test"),
            "should not contain raw test output, got: {out}"
        );
    }

    #[tokio::test]
    async fn redirect_cargo_build_via_exec() {
        if !crate::plugins::cargo::is_available() {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"rtest\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();

        let s = session_with_registry(dir.path()).await;

        let r = exec(
            &s,
            &Op {
                c: 8,
                s: Some("cargo build".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["exit"], 0);
        assert_eq!(d["via"], "plugin");
    }

    #[tokio::test]
    async fn redirect_git_status_via_exec() {
        if !crate::plugins::git::is_available() {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        // Init a git repo
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let s = session_with_registry(dir.path()).await;

        let r = exec(
            &s,
            &Op {
                c: 8,
                s: Some("git status".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert_eq!(d["exit"], 0);
        assert_eq!(d["via"], "plugin");
        // Plugin returns structured JSON with clean/modified/untracked fields
        let out = d["out"].as_str().unwrap();
        assert!(
            out.contains("clean") || out.contains("modified") || out.contains("untracked"),
            "got: {out}"
        );
    }

    #[tokio::test]
    async fn redirect_unknown_command_falls_through() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_with_registry(dir.path()).await;

        let r = exec(
            &s,
            &Op {
                c: 8,
                s: Some("echo hello".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        // Should NOT have via:plugin — went through raw exec
        assert!(d.get("via").is_none());
        assert_eq!(d["out"], "hello");
    }

    #[tokio::test]
    async fn redirect_skipped_when_explicit_args() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_with_registry(dir.path()).await;

        let r = exec(
            &s,
            &Op {
                c: 8,
                s: Some("echo".into()),
                a: Some(vec!["raw args".into()]),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        assert!(d.get("via").is_none());
    }

    #[tokio::test]
    async fn redirect_cargo_unrecognized_flags_falls_through() {
        if !crate::plugins::cargo::is_available() {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"rtest\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();

        let s = session_with_registry(dir.path()).await;

        // --some-weird-flag isn't recognized, should fall through to raw exec
        let r = exec(
            &s,
            &Op {
                c: 8,
                s: Some("cargo build --some-weird-flag".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        // Falls through to raw exec (which will fail, but no via:plugin)
        assert!(d.get("via").is_none());
    }

    #[tokio::test]
    async fn redirect_disabled_by_config() {
        if !crate::plugins::cargo::is_available() {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"rtest\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();

        let mut cfg = Config::default();
        cfg.process.exec_plugin_redirect = false;

        use crate::plugins;
        use crate::tool_runner::ToolRegistry;

        let mut s = Session::new(dir.path().to_path_buf(), Arc::new(cfg));
        let registry = Arc::new(ToolRegistry::new());
        registry
            .register(Arc::new(plugins::cargo::CargoPlugin::new()))
            .await;
        s.tool_registry = Some(registry);

        let r = exec(
            &s,
            &Op {
                c: 8,
                s: Some("cargo build".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        let d = r.d.unwrap();
        // Redirect disabled — should go through raw exec
        assert!(d.get("via").is_none());
    }

    // --- Structured ResponseMeta plumbing (vikunja #247) ---
    //
    // The `via:plugin` JSON marker is for human/agent inspection; the
    // analytics layer reads the structured `Response.meta` flag instead.
    // These tests pin the meta plumbing for both sources of the flag in
    // exec_ops: plugin redirects (success + error) and raw-exec passthrough.

    #[tokio::test]
    async fn plugin_redirect_sets_meta_flag() {
        if !crate::plugins::git::is_available() {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let s = session_with_registry(dir.path()).await;

        let r = exec(
            &s,
            &Op {
                c: 8,
                s: Some("git status".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        assert!(
            r.meta.redirect_via_plugin,
            "plugin redirect must set meta.redirect_via_plugin"
        );
        assert!(!r.meta.filter_applied);
        assert!(!r.meta.read_dedup);
    }

    #[tokio::test]
    async fn raw_exec_does_not_set_redirect_meta_flag() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_with_registry(dir.path()).await;

        let r = exec(
            &s,
            &Op {
                c: 8,
                s: Some("echo hello".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        assert!(
            !r.meta.redirect_via_plugin,
            "raw exec must not set meta.redirect_via_plugin"
        );
    }

    #[tokio::test]
    async fn plugin_error_response_still_sets_meta_flag() {
        // A plugin can short-circuit a command and surface a 1-exit error
        // payload (e.g. invalid args). The response is still a redirect
        // and analytics should count it as such.
        let resp = plugin_error_response("git", "bogus", "no such subcommand");
        assert!(resp.ok); // plugin errors are wrapped as ok responses
        assert!(
            resp.meta.redirect_via_plugin,
            "plugin_error_response must still mark the redirect"
        );
    }

    /// Positive handler-level coverage for the third meta flag: when a
    /// recognized command goes through `exec_filter`, the response must
    /// carry `meta.filter_applied = true`. Uses `make` (classified as
    /// Build) with no Makefile so the filter compresses the failure into
    /// a `FAILED (exit N)` summary deterministically.
    #[tokio::test]
    async fn filtered_exec_sets_filter_applied_meta_flag() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());
        assert!(
            s.cfg.process.exec_output_filters,
            "default config must have exec_output_filters enabled for this test"
        );

        let r = exec(
            &s,
            &Op {
                c: 8,
                s: Some("make".into()),
                ..Op::default()
            },
        )
        .await;
        assert!(
            r.ok,
            "exec call itself must succeed (failure is in subprocess)"
        );
        assert!(
            r.meta.filter_applied,
            "recognized command must set meta.filter_applied; meta = {:?}",
            r.meta
        );
        assert!(!r.meta.redirect_via_plugin);
        assert!(!r.meta.read_dedup);
    }

    /// Negative case: an unrecognized command must NOT set
    /// `meta.filter_applied`.
    #[tokio::test]
    async fn unfiltered_exec_does_not_set_filter_applied_meta_flag() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        let r = exec(
            &s,
            &Op {
                c: 8,
                s: Some("echo".into()),
                a: Some(vec!["hi".into()]),
                ..Op::default()
            },
        )
        .await;
        assert!(r.ok);
        assert!(
            !r.meta.filter_applied,
            "unrecognized command must not set meta.filter_applied"
        );
    }
}
