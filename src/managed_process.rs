//! Shared subprocess lifecycle, environment, and bounded-output primitives.
//!
//! The caller retains response parsing and long-lived job ownership. This
//! module owns the dangerous mechanics: process groups, cancellation cleanup,
//! environment isolation, private artifacts, and streaming-time memory bounds.

use crate::config::ProcessConfig;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

pub type ProgressCallback<'a> = dyn Fn(String) + Send + Sync + 'a;

/// Captured process output. Each stream is bounded while it is read.
pub struct ManagedOutput {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub timed_out: bool,
}

/// A spawned child whose Unix process group remains owned until settlement.
pub struct ManagedChild {
    child: Child,
    #[cfg(unix)]
    process_group: Option<i32>,
}

impl ManagedChild {
    /// Spawn a child in a fresh process group and arm emergency drop cleanup.
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        command.kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let child = command.spawn()?;
        #[cfg(unix)]
        let process_group = child.id().and_then(|pid| i32::try_from(pid).ok());
        Ok(Self {
            child,
            #[cfg(unix)]
            process_group,
        })
    }

    pub fn stdout_mut(&mut self) -> &mut Option<tokio::process::ChildStdout> {
        &mut self.child.stdout
    }

    pub fn stderr_mut(&mut self) -> &mut Option<tokio::process::ChildStderr> {
        &mut self.child.stderr
    }

    pub fn stdin_mut(&mut self) -> &mut Option<tokio::process::ChildStdin> {
        &mut self.child.stdin
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
            self.finish_group();
        }
        Ok(status)
    }

    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.wait().await?;
        self.finish_group();
        Ok(status)
    }

    /// TERM the owned group, allow a grace period, then KILL and reap.
    pub async fn terminate(&mut self, grace: Duration) -> io::Result<ExitStatus> {
        if let Some(status) = self.try_wait()? {
            return Ok(status);
        }
        self.signal_group(term_signal())?;
        match tokio::time::timeout(grace, self.child.wait()).await {
            Ok(status) => {
                let status = status?;
                // The leader may exit before descendants. Remove anything still
                // in the owned group before releasing its identity.
                let _ = self.signal_group(kill_signal());
                self.finish_group();
                Ok(status)
            }
            Err(_) => {
                self.signal_group(kill_signal())?;
                let status = self.child.wait().await?;
                self.finish_group();
                Ok(status)
            }
        }
    }

    #[cfg(unix)]
    fn signal_group(&self, signal: i32) -> io::Result<()> {
        let Some(group) = self.process_group else {
            return Ok(());
        };
        // SAFETY: negative pid addresses the process group created at spawn.
        let rc = unsafe { libc::kill(-group, signal) };
        if rc == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }

    #[cfg(not(unix))]
    fn signal_group(&mut self, _signal: i32) -> io::Result<()> {
        self.child.start_kill()
    }

    fn finish_group(&mut self) {
        #[cfg(unix)]
        {
            if self.process_group.is_some() {
                let _ = self.signal_group(kill_signal());
                self.process_group = None;
            }
        }
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        // Async cleanup must happen through `terminate`/`wait`. This emergency
        // path prevents a cancelled future from abandoning its process group.
        let _ = self.signal_group(kill_signal());
    }
}

/// Long-lived background child plus its bounded output-drain tasks.
pub struct ManagedBackground {
    pub child: ManagedChild,
    pub output_path: PathBuf,
    drains: Vec<tokio::task::JoinHandle<io::Result<()>>>,
}

impl ManagedBackground {
    pub fn spawn(command: &mut Command, cfg: &ProcessConfig, label: &str) -> io::Result<Self> {
        let (output_path, file) = create_artifact(cfg, label)?;
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = match ManagedChild::spawn(command) {
            Ok(child) => child,
            Err(error) => {
                remove_artifact(&output_path);
                return Err(error);
            }
        };
        let stdout = child
            .stdout_mut()
            .take()
            .ok_or_else(|| io::Error::other("spawned process has no stdout pipe"))?;
        let stderr = child
            .stderr_mut()
            .take()
            .ok_or_else(|| io::Error::other("spawned process has no stderr pipe"))?;
        let file = Arc::new(Mutex::new(tokio::fs::File::from_std(file)));
        let written = Arc::new(AtomicU64::new(0));
        let max = cfg.artifact_max_bytes;
        let stdout_drain = tokio::spawn(drain_to_artifact(
            stdout,
            Arc::clone(&file),
            Arc::clone(&written),
            max,
        ));
        let stderr_drain = tokio::spawn(drain_to_artifact(stderr, file, written, max));
        Ok(Self {
            child,
            output_path,
            drains: vec![stdout_drain, stderr_drain],
        })
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    pub async fn terminate(mut self, grace: Duration) -> io::Result<ExitStatus> {
        let status = self.child.terminate(grace).await?;
        self.join_drains().await?;
        Ok(status)
    }

    async fn join_drains(&mut self) -> io::Result<()> {
        for drain in self.drains.drain(..) {
            match drain.await {
                Ok(result) => result?,
                Err(error) => return Err(io::Error::other(format!("output drain: {error}"))),
            }
        }
        Ok(())
    }

    pub async fn settle_output(&mut self) -> io::Result<()> {
        self.join_drains().await
    }

    pub fn cleanup_artifact(&self) {
        remove_artifact(&self.output_path);
    }
}

impl Drop for ManagedBackground {
    fn drop(&mut self) {
        for drain in &self.drains {
            drain.abort();
        }
        remove_artifact(&self.output_path);
    }
}

async fn drain_to_artifact<R: AsyncRead + Unpin>(
    mut reader: R,
    file: Arc<Mutex<tokio::fs::File>>,
    written: Arc<AtomicU64>,
    max: u64,
) -> io::Result<()> {
    let mut chunk = vec![0; 8192];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        let start = written.fetch_add(read as u64, Ordering::Relaxed);
        if start >= max {
            continue;
        }
        let retained = usize::try_from((max - start).min(read as u64)).unwrap_or(read);
        let mut file = file.lock().await;
        file.write_all(&chunk[..retained]).await?;
        file.flush().await?;
    }
}

/// Read only a bounded suffix of a background artifact and return its last
/// `line_count` lines.
pub async fn tail_lines(
    path: &Path,
    line_count: usize,
    max_read_bytes: usize,
) -> io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let len = file.metadata().await?.len();
    let read_len = len.min(max_read_bytes as u64);
    if read_len < len {
        file.seek(std::io::SeekFrom::End(-(read_len as i64)))
            .await?;
    }
    let mut bytes = Vec::with_capacity(read_len as usize);
    file.read_to_end(&mut bytes).await?;
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(line_count);
    Ok(lines[start..].join("\n"))
}

#[cfg(unix)]
const fn term_signal() -> i32 {
    libc::SIGTERM
}

#[cfg(not(unix))]
const fn term_signal() -> i32 {
    0
}

#[cfg(unix)]
const fn kill_signal() -> i32 {
    libc::SIGKILL
}

#[cfg(not(unix))]
const fn kill_signal() -> i32 {
    0
}

/// Clear ambient state, inherit only reviewed parent variables, then overlay
/// session/tool/per-call values.
pub fn apply_environment(
    command: &mut Command,
    cfg: &ProcessConfig,
    overlays: &HashMap<String, String>,
) {
    command.env_clear();
    for (name, value) in std::env::vars() {
        if cfg.inherit_env.iter().any(|allowed| allowed == &name)
            || cfg
                .inherit_env_prefixes
                .iter()
                .any(|prefix| name.starts_with(prefix))
        {
            command.env(&name, value);
        }
    }
    for (name, value) in overlays {
        command.env(name, value);
    }
}

/// Convenience entry point for CLI plugins that only need argv/cwd/env/stdin.
pub async fn run(
    program: &str,
    args: &[String],
    cwd: &Path,
    overlays: &HashMap<String, String>,
    cfg: &ProcessConfig,
    stdin_data: Option<&[u8]>,
) -> io::Result<ManagedOutput> {
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd);
    apply_environment(&mut command, cfg, overlays);
    let output = capture(&mut command, cfg, None, stdin_data).await?;
    if output.timed_out {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "{program} timed out after {} seconds",
                cfg.default_timeout_secs
            ),
        ));
    }
    Ok(output)
}

/// Spawn and capture a foreground process without buffering unbounded streams.
pub async fn capture(
    command: &mut Command,
    cfg: &ProcessConfig,
    progress: Option<&ProgressCallback<'_>>,
    stdin_data: Option<&[u8]>,
) -> io::Result<ManagedOutput> {
    command
        .stdin(if stdin_data.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ManagedChild::spawn(command)?;
    let stdout = child
        .stdout_mut()
        .take()
        .ok_or_else(|| io::Error::other("spawned process has no stdout pipe"))?;
    let stderr = child
        .stderr_mut()
        .take()
        .ok_or_else(|| io::Error::other("spawned process has no stderr pipe"))?;
    let stdin = child.stdin_mut().take();
    let timeout =
        (cfg.default_timeout_secs > 0).then(|| Duration::from_secs(cfg.default_timeout_secs));
    let grace = Duration::from_millis(cfg.termination_grace_ms);

    let wait = async {
        if let Some(timeout) = timeout {
            match tokio::time::timeout(timeout, child.wait()).await {
                Ok(status) => status.map(|status| (status, false)),
                Err(_) => child.terminate(grace).await.map(|status| (status, true)),
            }
        } else {
            child.wait().await.map(|status| (status, false))
        }
    };
    let write_stdin = async {
        if let (Some(mut stdin), Some(data)) = (stdin, stdin_data) {
            stdin.write_all(data).await?;
            stdin.shutdown().await?;
        }
        Ok::<(), io::Error>(())
    };
    let (stdout, stderr, status, stdin) = tokio::join!(
        read_bounded(
            stdout,
            cfg.exec_stream_chunk_bytes,
            cfg.output_memory_bytes,
            progress
        ),
        read_bounded(
            stderr,
            cfg.exec_stream_chunk_bytes,
            cfg.output_memory_bytes,
            progress
        ),
        wait,
        write_stdin
    );
    let stdout = stdout?;
    let stderr = stderr?;
    let (status, timed_out) = status?;
    stdin?;
    Ok(ManagedOutput {
        status,
        stdout: stdout.render(),
        stderr: stderr.render(),
        stdout_truncated: stdout.truncated(),
        stderr_truncated: stderr.truncated(),
        timed_out,
    })
}

struct BoundedStream {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: u64,
    limit: usize,
}

impl BoundedStream {
    fn new(limit: usize) -> Self {
        Self {
            head: Vec::with_capacity(limit / 2),
            tail: VecDeque::with_capacity(limit.saturating_sub(limit / 2)),
            total: 0,
            limit,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len() as u64);
        let head_limit = self.limit / 2;
        let take = head_limit.saturating_sub(self.head.len()).min(bytes.len());
        self.head.extend_from_slice(&bytes[..take]);
        let tail_limit = self.limit.saturating_sub(head_limit);
        for byte in &bytes[take..] {
            if self.tail.len() == tail_limit {
                self.tail.pop_front();
            }
            if tail_limit > 0 {
                self.tail.push_back(*byte);
            }
        }
    }

    fn truncated(&self) -> bool {
        self.total > self.limit as u64
    }

    fn render(&self) -> String {
        if !self.truncated() {
            let mut bytes = self.head.clone();
            bytes.extend(self.tail.iter().copied());
            return String::from_utf8_lossy(&bytes).into_owned();
        }
        let omitted = self
            .total
            .saturating_sub((self.head.len() + self.tail.len()) as u64);
        format!(
            "{}\n\n... [{omitted} bytes truncated while reading] ...\n\n{}",
            String::from_utf8_lossy(&self.head),
            String::from_utf8_lossy(&self.tail.iter().copied().collect::<Vec<_>>())
        )
    }
}

async fn read_bounded<R: AsyncRead + Unpin>(
    mut reader: R,
    chunk_bytes: usize,
    limit: usize,
    progress: Option<&ProgressCallback<'_>>,
) -> io::Result<BoundedStream> {
    let mut retained = BoundedStream::new(limit);
    let mut pending_utf8 = Vec::new();
    let mut chunk = vec![0; chunk_bytes];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            emit_progress(progress, decode_stream_utf8(&mut pending_utf8, true));
            return Ok(retained);
        }
        retained.push(&chunk[..read]);
        if progress.is_some() {
            pending_utf8.extend_from_slice(&chunk[..read]);
            emit_progress(progress, decode_stream_utf8(&mut pending_utf8, false));
        }
    }
}

fn emit_progress(progress: Option<&ProgressCallback<'_>>, text: String) {
    if let Some(progress) = progress {
        if !text.is_empty() {
            progress(text);
        }
    }
}

fn decode_stream_utf8(pending: &mut Vec<u8>, eof: bool) -> String {
    let mut rendered = String::new();
    let mut consumed = 0;
    while consumed < pending.len() {
        match std::str::from_utf8(&pending[consumed..]) {
            Ok(text) => {
                rendered.push_str(text);
                consumed = pending.len();
            }
            Err(error) => {
                let valid = error.valid_up_to();
                rendered.push_str(
                    std::str::from_utf8(&pending[consumed..consumed + valid])
                        .expect("valid_up_to always identifies valid UTF-8"),
                );
                consumed += valid;
                match error.error_len() {
                    Some(invalid_bytes) => {
                        rendered.push('\u{FFFD}');
                        consumed += invalid_bytes;
                    }
                    None => break,
                }
            }
        }
    }
    if consumed > 0 {
        pending.drain(..consumed);
    }
    if eof && !pending.is_empty() {
        rendered.push_str(&String::from_utf8_lossy(pending));
        pending.clear();
    }
    rendered
}

/// Resolve and create the private directory used for background output.
pub fn artifact_directory(cfg: &ProcessConfig) -> PathBuf {
    if let Some(configured) = &cfg.artifact_directory {
        if configured == "~" {
            return crate::paths::home_dir().unwrap_or_else(std::env::temp_dir);
        }
        if let Some(rest) = configured.strip_prefix("~/") {
            if let Some(home) = crate::paths::home_dir() {
                return home.join(rest);
            }
        }
        return PathBuf::from(configured);
    }
    crate::paths::home_dir()
        .map(|home| home.join(".daimonos/process-output"))
        .unwrap_or_else(|| std::env::temp_dir().join("daimonos-process-output"))
}

pub fn create_artifact(cfg: &ProcessConfig, label: &str) -> io::Result<(PathBuf, std::fs::File)> {
    let directory = artifact_directory(cfg);
    if let Ok(metadata) = std::fs::symlink_metadata(&directory) {
        if metadata.file_type().is_symlink() {
            return Err(io::Error::other(
                "process artifact directory must not be a symlink",
            ));
        }
        if !metadata.is_dir() {
            return Err(io::Error::other(
                "process artifact path exists and is not a directory",
            ));
        }
    }
    std::fs::create_dir_all(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    }
    let safe_label: String = label
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .take(24)
        .collect();
    let path = directory.join(format!("{safe_label}-{}.log", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path)?;
    Ok((path, file))
}

pub fn remove_artifact(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn capture_bounds_output_while_reading() {
        let mut cfg = Config::default().process;
        cfg.output_memory_bytes = 64;
        cfg.exec_output_max_chars = 64;
        let mut command = Command::new("sh");
        command.args(["-c", "printf 'head'; yes x | head -c 4096; printf 'tail'"]);
        command.stdin(Stdio::null());
        apply_environment(&mut command, &cfg, &HashMap::new());
        let output = capture(&mut command, &cfg, None, None).await.unwrap();
        assert!(output.stdout_truncated);
        assert!(output.stdout.contains("bytes truncated while reading"));
        assert!(output.stdout.starts_with("head"));
        assert!(output.stdout.ends_with("tail"));
    }

    #[tokio::test]
    async fn progress_preserves_utf8_split_across_reads() {
        let mut cfg = Config::default().process;
        cfg.exec_stream_chunk_bytes = 1;
        let seen = Arc::new(Mutex::new(String::new()));
        let capture_seen = Arc::clone(&seen);
        let progress = move |text: String| capture_seen.lock().unwrap().push_str(&text);
        let mut command = Command::new("printf");
        command.arg("€");
        command.stdin(Stdio::null());
        apply_environment(&mut command, &cfg, &HashMap::new());
        let output = capture(&mut command, &cfg, Some(&progress), None)
            .await
            .unwrap();
        assert_eq!(output.stdout, "€");
        assert_eq!(&*seen.lock().unwrap(), "€");
    }

    #[test]
    fn environment_excludes_unlisted_secrets_and_keeps_overrides() {
        let cfg = Config::default().process;
        // SAFETY: this test is single-threaded with respect to this unique key.
        unsafe { std::env::set_var("DAIMONOS_TEST_SECRET_TOKEN", "hidden") };
        let mut overlays = HashMap::new();
        overlays.insert("EXPLICIT_VALUE".into(), "visible".into());
        let mut command = Command::new("env");
        apply_environment(&mut command, &cfg, &overlays);
        let debug = format!("{command:?}");
        assert!(!debug.contains("DAIMONOS_TEST_SECRET_TOKEN"));
        assert!(debug.contains("EXPLICIT_VALUE"));
        unsafe { std::env::remove_var("DAIMONOS_TEST_SECRET_TOKEN") };
    }

    #[test]
    fn artifacts_are_private_and_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default().process;
        cfg.artifact_directory = Some(dir.path().join("private").display().to_string());
        let (path, _file) = create_artifact(&cfg, "bg").unwrap();
        assert!(path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        remove_artifact(&path);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn artifact_directory_rejects_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        symlink(&target, &link).unwrap();
        let mut cfg = Config::default().process;
        cfg.artifact_directory = Some(link.display().to_string());
        let error = create_artifact(&cfg, "bg").unwrap_err();
        assert!(error.to_string().contains("must not be a symlink"));
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminate_retires_process_group_descendants() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30 & wait"]);
        command.stdin(Stdio::null());
        let mut child = ManagedChild::spawn(&mut command).unwrap();
        let group = child.process_group.unwrap();
        child.terminate(Duration::from_millis(50)).await.unwrap();
        // SAFETY: signal 0 probes existence without changing process state.
        let rc = unsafe { libc::kill(-group, 0) };
        assert_eq!(rc, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
}
