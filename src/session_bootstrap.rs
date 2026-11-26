//! Connect-first bootstrap for the local daemon-owned session runtime.
//!
//! The spawned daemon is intentionally independent of the TUI process: all
//! standard streams are detached, the child owns its own process group, and
//! dropping the frontend does not terminate it.

use std::ffi::OsString;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use crate::client_transport::UnixFrontendTransport;
use sha2::{Digest, Sha256};

pub const BOOTSTRAP_FINGERPRINT_ENV: &str = "DAIMONOS_SESSION_BOOTSTRAP_FINGERPRINT";
const INSTANCE_METADATA_MAX_BYTES: u64 = 16 * 1024;

pub struct BootstrapOptions<'a> {
    pub workspace: &'a Path,
    pub config_path: Option<&'a Path>,
    pub state_dir: Option<&'a Path>,
    pub agent_instructions: Option<&'a Path>,
    pub confine_reference: Option<&'a Path>,
    pub confine_identity: Option<&'a str>,
    /// Explicit-only configuration discovery (ADR-021, FR-09). Pinned into the
    /// daemon launch identity so a daemon started with the mode is never reused
    /// by a frontend that did not request it, and vice versa.
    pub ignore_config_files: bool,
    /// Forward the caller's collection policy and refuse reuse of a collecting daemon.
    pub analytics_off: bool,
    /// Independent no-follow option (WP14). Part of the daemon's launch
    /// identity so a pinned session daemon enforces the same link policy.
    pub no_follow: bool,
    pub debug_tokens: bool,
    /// FR-07 (ADR-022, WP04): whether the client requested `--workspace-read-only`.
    /// This is part of the daemon's immutable launch identity: a read-only client
    /// must spawn a read-only daemon (forwarded in `daemon_args`) and must refuse
    /// to attach to a daemon that was started without it (hashed into
    /// `bootstrap_fingerprint`, treated as an explicit selection in
    /// `launch_identity`). The daemon's `Session::admission` is pinned from the
    /// DAEMON's own config, so reuse across this boundary would otherwise bypass
    /// the read-only gate (R4).
    pub workspace_read_only: bool,
    /// FR-02 (WP06): the client's `--tool-allowlist` file path. Part of the
    /// daemon's immutable launch identity exactly like `workspace_read_only`:
    /// forwarded in `daemon_args`, hashed into `bootstrap_fingerprint`, and an
    /// explicit selection in `launch_identity`, so a client requesting an
    /// allowlist cannot be served by a daemon started without the same one (the
    /// daemon pins `Session::policy` from its OWN config).
    pub tool_allowlist: Option<&'a Path>,
    pub socket_path: &'a Path,
    pub provider: Option<&'a str>,
    pub model: Option<&'a str>,
    pub agent_env: Option<&'a Path>,
    pub max_frame_bytes: usize,
    pub timeout: Duration,
    pub retry_interval: Duration,
    /// WP17 (FR-15): serve/export full self-contained schemas in the daemon.
    /// Part of the launch identity so a frontend requesting it cannot silently
    /// attach to a daemon launched without it.
    pub full_tool_schemas: bool,
    pub read_sha256: bool,
    /// FR-19 (WP18, ADR-031): the caller's cooperative tool-budget options.
    /// Part of the daemon's immutable launch identity exactly like
    /// `workspace_read_only`: forwarded in `daemon_args`, hashed into
    /// `bootstrap_fingerprint`, and treated as an explicit selection in
    /// `launch_identity`, so a client requesting a budget cannot be served by a
    /// daemon started without the same one (the daemon pins its budget from its
    /// OWN parsed flags). All-absent keeps legacy behavior.
    pub tool_budget: crate::tool_budget::ToolBudgetConfig,
}

pub struct BootstrapConnection {
    pub transport: UnixFrontendTransport,
    pub launch_identity: LaunchIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchIdentity {
    /// No immutable daemon selection was explicitly requested.
    NotRequested,
    Matched,
    Mismatched,
    /// A directly launched or older daemon published no comparable identity.
    Unavailable,
}

pub async fn connect_or_spawn(
    options: &BootstrapOptions<'_>,
) -> anyhow::Result<BootstrapConnection> {
    match tokio::net::UnixStream::connect(options.socket_path).await {
        Ok(stream) => {
            return Ok(BootstrapConnection {
                transport: frontend_transport(stream, options)?,
                launch_identity: launch_identity(options)?,
            });
        }
        Err(error) if should_bootstrap(&error) => {}
        Err(error) => {
            anyhow::bail!(
                "cannot connect to session daemon at {}: {error}",
                options.socket_path.display()
            );
        }
    }

    let executable = std::env::current_exe()
        .map_err(|error| anyhow::anyhow!("resolve daimonos binary: {error}"))?;
    let mut command = daemon_command(&executable, options, &crate::env_file::implicit_keys());
    // SAFETY: this closure runs after Command's fork and before exec. setsid,
    // fork, and _exit are async-signal-safe; do not add allocation, logging, or
    // any other non-async-signal-safe operation here. The grandchild returns to
    // Command so it can exec the daemon, while the intermediate child exits for
    // us to reap immediately. The daemon is then adopted outside the TUI
    // process and cannot become its zombie or die with the TUI runtime.
    unsafe {
        command.as_std_mut().pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let pid = libc::fork();
            if pid < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if pid > 0 {
                libc::_exit(0);
            }
            Ok(())
        });
    }
    let mut intermediate = command
        .spawn()
        .map_err(|error| anyhow::anyhow!("start session daemon: {error}"))?;
    let intermediate_status = intermediate.wait().await?;
    if !intermediate_status.success() {
        anyhow::bail!("session daemon bootstrap process exited with {intermediate_status}");
    }
    let deadline = tokio::time::Instant::now() + options.timeout;

    loop {
        match tokio::net::UnixStream::connect(options.socket_path).await {
            Ok(stream) => {
                return Ok(BootstrapConnection {
                    transport: frontend_transport(stream, options)?,
                    launch_identity: launch_identity(options)?,
                });
            }
            Err(error) if should_bootstrap(&error) => {}
            Err(error) => {
                anyhow::bail!(
                    "cannot connect to bootstrapped session daemon at {}: {error}",
                    options.socket_path.display()
                );
            }
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        tokio::time::sleep(options.retry_interval.min(deadline - now)).await;
    }

    let pid = instance_pid(options.socket_path)?
        .map(|pid| format!("; daemon metadata reports pid {pid}"))
        .unwrap_or_default();
    anyhow::bail!(
        "timed out waiting for session daemon at {}{pid}; run `daimonos \
         --workspace {} session-daemon --socket {}` directly for startup diagnostics",
        options.socket_path.display(),
        options.workspace.display(),
        options.socket_path.display(),
    )
}

fn frontend_transport(
    stream: tokio::net::UnixStream,
    options: &BootstrapOptions<'_>,
) -> anyhow::Result<UnixFrontendTransport> {
    Ok(UnixFrontendTransport::new(
        stream,
        format!("session daemon {}", options.socket_path.display()),
        options.max_frame_bytes,
    )?)
}

fn should_bootstrap(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

/// The command that starts the session daemon, without its `pre_exec` step.
///
/// The daemon does not inherit the variables that this process set from an
/// implicit `agent.env` file. It has the same workspace and state options, so
/// it loads the same files again and knows that these values are implicit.
/// Inherited, they would count as values of the real environment and rank
/// above the `--agent-env` file (ADR-037).
fn daemon_command(
    executable: &Path,
    options: &BootstrapOptions<'_>,
    implicit_keys: &std::collections::BTreeSet<String>,
) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(executable);
    command
        .args(daemon_args(options))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(false);
    for key in implicit_keys {
        command.env_remove(key);
    }
    command.env(BOOTSTRAP_FINGERPRINT_ENV, bootstrap_fingerprint(options));
    command
}

fn daemon_args(options: &BootstrapOptions<'_>) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("--workspace"),
        options.workspace.as_os_str().to_owned(),
    ];
    if let Some(path) = options.config_path {
        args.push(OsString::from("--config"));
        args.push(path.as_os_str().to_owned());
    }
    if let Some(path) = options.state_dir {
        args.push(OsString::from("--state-dir"));
        args.push(path.as_os_str().to_owned());
    }
    if let Some(path) = options.agent_instructions {
        args.push(OsString::from("--agent-instructions"));
        args.push(path.as_os_str().to_owned());
    }
    if options.analytics_off {
        args.push(OsString::from("--analytics-off"));
    }
    if options.debug_tokens {
        args.push(OsString::from("--debug-tokens"));
    }
    if options.workspace_read_only {
        args.push(OsString::from("--workspace-read-only"));
    }
    if let Some(path) = options.tool_allowlist {
        args.push(OsString::from("--tool-allowlist"));
        args.push(path.as_os_str().to_owned());
    }
    if let Some(reference) = options.confine_reference {
        args.push(OsString::from("--confine-paths"));
        args.push(OsString::from("--confine-root"));
        args.push(reference.as_os_str().to_owned());
    }
    if options.ignore_config_files {
        args.push(OsString::from("--ignore-config-files"));
    }
    if options.no_follow {
        args.push(OsString::from("--no-follow-symlinks"));
    }
    // FR-19 (WP18, ADR-031): forward the caller's budget to the daemon so an
    // interactive session actually enforces it (global flags, before the
    // subcommand). Omitted options add nothing, preserving legacy launch args.
    if let Some(ms) = options.tool_budget.timeout_ms {
        args.push(OsString::from("--tool-timeout-ms"));
        args.push(OsString::from(ms.to_string()));
    }
    if let Some(bytes) = options.tool_budget.input_bytes {
        args.push(OsString::from("--tool-input-bytes"));
        args.push(OsString::from(bytes.to_string()));
    }
    if let Some(entries) = options.tool_budget.max_entries {
        args.push(OsString::from("--tool-max-entries"));
        args.push(OsString::from(entries.to_string()));
    }
    args.push(OsString::from("session-daemon"));
    args.push(OsString::from("--socket"));
    args.push(options.socket_path.as_os_str().to_owned());
    if let Some(provider) = options.provider {
        args.push(OsString::from("--provider"));
        args.push(OsString::from(provider));
    }
    if let Some(model) = options.model {
        args.push(OsString::from("--model"));
        args.push(OsString::from(model));
    }
    if let Some(path) = options.agent_env {
        args.push(OsString::from("--agent-env"));
        args.push(path.as_os_str().to_owned());
    }
    if options.read_sha256 {
        args.push(OsString::from("--read-sha256"));
    }
    if options.full_tool_schemas {
        args.push(OsString::from("--full-tool-schemas"));
    }
    args
}

fn bootstrap_fingerprint(options: &BootstrapOptions<'_>) -> String {
    let mut digest = Sha256::new();
    for path in [
        Some(options.workspace),
        options.config_path,
        options.agent_env,
    ] {
        if let Some(path) = path {
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            digest.update(canonical.as_os_str().as_bytes());
        }
        digest.update([0_u8]);
    }
    if let Some(path) = options.state_dir {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        digest.update(b"selected-state\0");
        digest.update(canonical.as_os_str().as_bytes());
        digest.update([0_u8]);
    }
    if let Some(path) = options.agent_instructions {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        digest.update(b"agent-instructions\0");
        digest.update(canonical.as_os_str().as_bytes());
        digest.update([0_u8]);
    }
    if options.debug_tokens {
        digest.update(b"debug-tokens\0");
    }
    if options.workspace_read_only {
        digest.update(b"workspace-read-only\0");
    }
    if let Some(path) = options.tool_allowlist {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        digest.update(b"tool-allowlist\0");
        digest.update(canonical.as_os_str().as_bytes());
        digest.update([0_u8]);
    }
    if let Some(identity) = options.confine_identity {
        digest.update(b"confined-root\0");
        digest.update(identity.as_bytes());
        digest.update([0_u8]);
    }
    if options.ignore_config_files {
        digest.update(b"ignore-config-files\0");
    }
    if options.no_follow {
        digest.update(b"no-follow\0");
    }
    // FR-19 (WP18, ADR-031): pin the budget into the launch identity so an
    // attach cannot silently run under a different (or absent) budget.
    if let Some(ms) = options.tool_budget.timeout_ms {
        digest.update(b"tool-timeout-ms\0");
        digest.update(ms.to_le_bytes());
    }
    if let Some(bytes) = options.tool_budget.input_bytes {
        digest.update(b"tool-input-bytes\0");
        digest.update(bytes.to_le_bytes());
    }
    if let Some(entries) = options.tool_budget.max_entries {
        digest.update(b"tool-max-entries\0");
        digest.update(entries.to_le_bytes());
    }
    if let Some(provider) = options.provider {
        digest.update(provider.as_bytes());
    }
    digest.update([0_u8]);
    if options.read_sha256 {
        digest.update(b"read-sha256\0");
    }
    if options.analytics_off {
        digest.update(b"analytics-off\0");
    }
    if options.full_tool_schemas {
        digest.update(b"full-tool-schemas\0");
    }
    format!("{:x}", digest.finalize())
}

fn launch_identity(options: &BootstrapOptions<'_>) -> std::io::Result<LaunchIdentity> {
    let bare = options.config_path.is_none()
        && options.provider.is_none()
        && options.agent_env.is_none()
        && options.state_dir.is_none()
        && options.agent_instructions.is_none()
        && !options.debug_tokens
        && !options.workspace_read_only
        && options.tool_allowlist.is_none()
        && options.confine_identity.is_none()
        && !options.ignore_config_files
        && !options.read_sha256
        && !options.analytics_off
        && !options.no_follow
        && !options.tool_budget.any_enabled()
        && !options.full_tool_schemas;
    let metadata = instance_metadata(options.socket_path)?;
    let value = metadata.as_ref();
    let selected = value.and_then(|v| v["selected_evidence_profile"].as_bool()) == Some(true);
    if bare && !selected {
        // Keep bare attachment semantics for other profiles and older/manual daemons.
        return Ok(LaunchIdentity::NotRequested);
    }
    let Some(fingerprint) = value.and_then(|v| v["bootstrap_fingerprint"].as_str()) else {
        return Ok(LaunchIdentity::Unavailable);
    };
    Ok(if fingerprint == bootstrap_fingerprint(options) {
        LaunchIdentity::Matched
    } else {
        LaunchIdentity::Mismatched
    })
}

fn instance_pid(socket_path: &Path) -> std::io::Result<Option<u32>> {
    Ok(instance_metadata(socket_path)?
        .and_then(|value| value["pid"].as_u64())
        .and_then(|pid| u32::try_from(pid).ok()))
}

fn instance_metadata(socket_path: &Path) -> std::io::Result<Option<serde_json::Value>> {
    let path = crate::session_daemon::instance_metadata_path(socket_path);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    // SAFETY: geteuid has no preconditions and dereferences no pointers.
    let effective_uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_file()
        || metadata.uid() != effective_uid
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "session daemon metadata is not an owner-only regular file",
        ));
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    let opened = file.metadata()?;
    if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
        return Err(std::io::Error::other(
            "session daemon metadata changed while opening",
        ));
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(INSTANCE_METADATA_MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > INSTANCE_METADATA_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "session daemon metadata exceeds its internal size limit",
        ));
    }
    let value = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-037: the daemon loads the implicit `agent.env` files itself. It
    /// does not inherit their values as values of the real environment.
    #[test]
    fn daemon_command_does_not_pass_on_implicit_agent_env_values() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let socket = directory.path().join("daemon.sock");
        let agent_env = directory.path().join("agent.env");
        let selected = options(directory.path(), &config, &socket, &agent_env);
        let implicit = ["DAIMONOS_AGENT_MODEL", "DAIMONOS_AGENT_BASE_URL"]
            .into_iter()
            .map(String::from)
            .collect();

        let command = daemon_command(Path::new("/bin/daimonos"), &selected, &implicit);
        let envs: std::collections::HashMap<_, _> = command
            .as_std()
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.map(|value| value.to_owned())))
            .collect();
        // `None` is a removal from the inherited environment.
        assert_eq!(envs[std::ffi::OsStr::new("DAIMONOS_AGENT_MODEL")], None);
        assert_eq!(envs[std::ffi::OsStr::new("DAIMONOS_AGENT_BASE_URL")], None);
        assert!(envs[std::ffi::OsStr::new(BOOTSTRAP_FINGERPRINT_ENV)].is_some());
        assert_eq!(envs.len(), 3);

        let none = daemon_command(Path::new("/bin/daimonos"), &selected, &Default::default());
        assert_eq!(none.as_std().get_envs().count(), 1);
    }

    fn options<'a>(
        workspace: &'a Path,
        config: &'a Path,
        socket: &'a Path,
        agent_env: &'a Path,
    ) -> BootstrapOptions<'a> {
        BootstrapOptions {
            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
            workspace,
            tool_allowlist: None,
            config_path: Some(config),
            state_dir: None,
            agent_instructions: None,
            confine_reference: None,
            confine_identity: None,
            ignore_config_files: false,
            analytics_off: false,
            no_follow: false,
            debug_tokens: false,
            workspace_read_only: false,
            socket_path: socket,
            provider: Some("openrouter"),
            model: Some("test/model"),
            agent_env: Some(agent_env),
            max_frame_bytes: 1024,
            timeout: Duration::from_secs(1),
            retry_interval: Duration::from_millis(1),
            full_tool_schemas: false,
            read_sha256: false,
        }
    }

    #[test]
    fn daemon_arguments_preserve_explicit_frontend_selection() {
        let args = daemon_args(&options(
            Path::new("/workspace"),
            Path::new("/config.toml"),
            Path::new("/runtime/session.sock"),
            Path::new("/agent.env"),
        ));
        assert_eq!(
            args,
            vec![
                "--workspace",
                "/workspace",
                "--config",
                "/config.toml",
                "session-daemon",
                "--socket",
                "/runtime/session.sock",
                "--provider",
                "openrouter",
                "--model",
                "test/model",
                "--agent-env",
                "/agent.env",
            ]
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn daemon_arguments_and_identity_include_selected_state() {
        let mut first = options(
            Path::new("/workspace"),
            Path::new("/config.toml"),
            Path::new("/runtime/session.sock"),
            Path::new("/agent.env"),
        );
        first.state_dir = Some(Path::new("/state/one"));
        let args = daemon_args(&first);
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--state-dir", "/state/one"]));
        let mut second = options(
            Path::new("/workspace"),
            Path::new("/config.toml"),
            Path::new("/runtime/session.sock"),
            Path::new("/agent.env"),
        );
        second.state_dir = Some(Path::new("/state/two"));
        assert_ne!(
            bootstrap_fingerprint(&first),
            bootstrap_fingerprint(&second)
        );
    }

    // WP18 (FR-19, ADR-031, req 1): the interactive agent runs through the
    // session daemon, so each budget option must be a daemon launch argument
    // AND part of the launch identity. If it were not in the identity, an
    // attach could silently reuse a daemon with a different (or absent) budget;
    // the client compares fingerprints and refuses on mismatch.
    #[test]
    fn tool_budget_is_a_daemon_argument_and_pins_the_launch_identity() {
        let base = options(
            Path::new("/workspace"),
            Path::new("/config.toml"),
            Path::new("/runtime/session.sock"),
            Path::new("/agent.env"),
        );
        let budgeted = BootstrapOptions {
            tool_budget: crate::tool_budget::ToolBudgetConfig {
                timeout_ms: Some(1500),
                input_bytes: Some(4096),
                max_entries: Some(32),
            },
            ..options(
                Path::new("/workspace"),
                Path::new("/config.toml"),
                Path::new("/runtime/session.sock"),
                Path::new("/agent.env"),
            )
        };
        let args = daemon_args(&budgeted);
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--tool-timeout-ms", "1500"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--tool-input-bytes", "4096"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--tool-max-entries", "32"]));
        // The unbudgeted daemon carries none of these arguments.
        assert!(!daemon_args(&base)
            .iter()
            .any(|arg| arg == "--tool-timeout-ms"));
        // A budget changes the launch identity, so an attach to a daemon that
        // lacks it (or has a different one) is refused.
        assert_ne!(
            bootstrap_fingerprint(&base),
            bootstrap_fingerprint(&budgeted)
        );
        let other = BootstrapOptions {
            tool_budget: crate::tool_budget::ToolBudgetConfig {
                timeout_ms: Some(500),
                input_bytes: None,
                max_entries: None,
            },
            ..options(
                Path::new("/workspace"),
                Path::new("/config.toml"),
                Path::new("/runtime/session.sock"),
                Path::new("/agent.env"),
            )
        };
        assert_ne!(
            bootstrap_fingerprint(&budgeted),
            bootstrap_fingerprint(&other)
        );
    }

    // WP11 (FR-09, ADR-021): explicit-only mode is pinned into the daemon's
    // launch identity, the same way --confine-paths is.
    #[test]
    fn explicit_only_is_pinned_into_daemon_launch_identity() {
        let base = options(
            Path::new("/workspace"),
            Path::new("/config.toml"),
            Path::new("/runtime/session.sock"),
            Path::new("/agent.env"),
        );
        let explicit = BootstrapOptions {
            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
            ignore_config_files: true,
            ..options(
                Path::new("/workspace"),
                Path::new("/config.toml"),
                Path::new("/runtime/session.sock"),
                Path::new("/agent.env"),
            )
        };
        let args = daemon_args(&explicit);
        assert!(args.iter().any(|arg| arg == "--ignore-config-files"));
        assert!(!daemon_args(&base)
            .iter()
            .any(|arg| arg == "--ignore-config-files"));
        assert_ne!(
            bootstrap_fingerprint(&base),
            bootstrap_fingerprint(&explicit)
        );
    }

    #[test]
    fn explicit_only_alone_requests_a_pinned_identity() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.path().join("session.sock");
        let config = directory.path().join("config.toml");
        let agent_env = directory.path().join("agent.env");
        let pinned = BootstrapOptions {
            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
            config_path: None,
            provider: None,
            agent_env: None,
            ignore_config_files: true,
            ..options(directory.path(), &config, &socket, &agent_env)
        };
        assert_ne!(
            launch_identity(&pinned).unwrap(),
            LaunchIdentity::NotRequested,
            "explicit-only mode must request a pinned daemon identity"
        );
    }

    #[test]
    fn bare_client_compares_a_bootstrapped_daemons_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("test.sock");
        let mut opts = options(dir.path(), dir.path(), &socket, dir.path());
        opts.config_path = None;
        opts.provider = None;
        opts.agent_env = None;
        let path = crate::session_daemon::instance_metadata_path(&socket);
        std::fs::write(&path, br#"{"pid":1,"selected_evidence_profile":true}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(launch_identity(&opts).unwrap(), LaunchIdentity::Unavailable);
        for (digest, analytics, expected) in [
            (true, false, LaunchIdentity::Mismatched),
            (false, true, LaunchIdentity::Mismatched),
            (false, false, LaunchIdentity::NotRequested),
        ] {
            opts.read_sha256 = digest;
            opts.analytics_off = analytics;
            std::fs::write(
                &path,
                serde_json::json!({"pid":1,"bootstrap_fingerprint":bootstrap_fingerprint(&opts),"selected_evidence_profile":digest || analytics})
                    .to_string(),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            opts.read_sha256 = false;
            opts.analytics_off = false;
            assert_eq!(launch_identity(&opts).unwrap(), expected);
        }
    }

    #[test]
    fn read_digest_changes_daemon_identity_and_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("d.sock");
        let mut opts = options(dir.path(), dir.path(), &socket, dir.path());
        opts.config_path = None;
        opts.agent_env = None;
        opts.provider = None;
        let plain = bootstrap_fingerprint(&opts);
        assert!(!daemon_args(&opts).contains(&OsString::from("--read-sha256")));
        opts.read_sha256 = true;
        assert_ne!(plain, bootstrap_fingerprint(&opts));
        assert!(daemon_args(&opts).contains(&OsString::from("--read-sha256")));
        assert_eq!(launch_identity(&opts).unwrap(), LaunchIdentity::Unavailable);
    }

    #[test]
    fn daemon_args_include_full_tool_schemas_when_requested() {
        let mut opts = options(
            Path::new("/workspace"),
            Path::new("/config.toml"),
            Path::new("/runtime/session.sock"),
            Path::new("/agent.env"),
        );
        assert!(!daemon_args(&opts)
            .iter()
            .any(|a| a == "--full-tool-schemas"));
        opts.full_tool_schemas = true;
        assert!(daemon_args(&opts)
            .iter()
            .any(|a| a == "--full-tool-schemas"));
    }

    #[test]
    fn bootstrap_fingerprint_changes_with_full_tool_schemas() {
        // WP17 (FR-15): the full-schema mode is part of the daemon launch
        // identity, so an interactive frontend requesting it cannot silently
        // attach to a daemon launched without it (frontend coverage rule).
        let first = options(
            Path::new("/workspace"),
            Path::new("/config.toml"),
            Path::new("/runtime/session.sock"),
            Path::new("/agent.env"),
        );
        let mut second = options(
            Path::new("/workspace"),
            Path::new("/config.toml"),
            Path::new("/runtime/session.sock"),
            Path::new("/agent.env"),
        );
        second.full_tool_schemas = true;
        assert_ne!(
            bootstrap_fingerprint(&first),
            bootstrap_fingerprint(&second)
        );
    }

    // ADR-032: a client that disables analytics must not reuse a collecting daemon.
    #[test]
    fn analytics_off_is_forwarded_and_requires_a_matching_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let mut opts = options(dir.path(), dir.path(), &socket, dir.path());
        opts.config_path = None;
        opts.provider = None;
        opts.agent_env = None;
        let plain = bootstrap_fingerprint(&opts);
        assert!(!daemon_args(&opts).contains(&OsString::from("--analytics-off")));
        assert_eq!(
            launch_identity(&opts).unwrap(),
            LaunchIdentity::NotRequested
        );

        opts.analytics_off = true;
        assert!(daemon_args(&opts).contains(&OsString::from("--analytics-off")));
        assert_ne!(bootstrap_fingerprint(&opts), plain);
        assert_eq!(launch_identity(&opts).unwrap(), LaunchIdentity::Unavailable);
    }

    #[test]
    fn daemon_arguments_and_identity_include_no_follow() {
        // WP14: the independent no-follow option must ride the daemon launch
        // identity so a pinned session daemon enforces the same link policy.
        let base = options(
            Path::new("/workspace"),
            Path::new("/config.toml"),
            Path::new("/runtime/session.sock"),
            Path::new("/agent.env"),
        );
        assert!(!daemon_args(&base).contains(&OsString::from("--no-follow-symlinks")));
        let mut nofollow = options(
            Path::new("/workspace"),
            Path::new("/config.toml"),
            Path::new("/runtime/session.sock"),
            Path::new("/agent.env"),
        );
        nofollow.no_follow = true;
        assert!(daemon_args(&nofollow).contains(&OsString::from("--no-follow-symlinks")));
        assert_ne!(
            bootstrap_fingerprint(&base),
            bootstrap_fingerprint(&nofollow)
        );
    }

    #[test]
    fn bootstrap_only_handles_absent_or_stale_local_socket() {
        assert!(should_bootstrap(&std::io::Error::from(
            std::io::ErrorKind::NotFound
        )));
        assert!(should_bootstrap(&std::io::Error::from(
            std::io::ErrorKind::ConnectionRefused
        )));
        assert!(!should_bootstrap(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
    }

    #[test]
    fn bootstrap_fingerprint_changes_with_daemon_owned_selection() {
        let first = options(
            Path::new("/workspace"),
            Path::new("/config.toml"),
            Path::new("/runtime/session.sock"),
            Path::new("/agent.env"),
        );
        let mut second = options(
            Path::new("/workspace"),
            Path::new("/config.toml"),
            Path::new("/runtime/session.sock"),
            Path::new("/agent.env"),
        );
        assert_eq!(
            bootstrap_fingerprint(&first),
            bootstrap_fingerprint(&second)
        );
        second.provider = Some("anthropic");
        assert_ne!(
            bootstrap_fingerprint(&first),
            bootstrap_fingerprint(&second)
        );
        second.provider = first.provider;
        second.model = Some("another/runtime-model");
        assert_eq!(
            bootstrap_fingerprint(&first),
            bootstrap_fingerprint(&second)
        );
    }

    #[test]
    fn launch_identity_distinguishes_match_mismatch_and_unavailable() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.path().join("session.sock");
        let config = directory.path().join("config.toml");
        let agent_env = directory.path().join("agent.env");
        let selected = options(directory.path(), &config, &socket, &agent_env);
        let metadata_path = crate::session_daemon::instance_metadata_path(&socket);

        std::fs::write(&metadata_path, br#"{"pid":1,"bootstrap_fingerprint":null}"#).unwrap();
        std::fs::set_permissions(&metadata_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            launch_identity(&selected).unwrap(),
            LaunchIdentity::Unavailable
        );
        let bare = BootstrapOptions {
            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
            config_path: None,
            provider: None,
            agent_env: None,
            ..options(directory.path(), &config, &socket, &agent_env)
        };
        assert_eq!(
            launch_identity(&bare).unwrap(),
            LaunchIdentity::NotRequested
        );

        std::fs::write(
            &metadata_path,
            serde_json::to_vec(&serde_json::json!({
                "pid": 1,
                "bootstrap_fingerprint": bootstrap_fingerprint(&selected),
            }))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(launch_identity(&selected).unwrap(), LaunchIdentity::Matched);

        std::fs::write(
            &metadata_path,
            br#"{"pid":1,"bootstrap_fingerprint":"different"}"#,
        )
        .unwrap();
        assert_eq!(
            launch_identity(&selected).unwrap(),
            LaunchIdentity::Mismatched
        );
    }

    #[test]
    fn instance_metadata_reader_rejects_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.path().join("session.sock");
        let target = directory.path().join("target.json");
        std::fs::write(&target, br#"{"pid":1}"#).unwrap();
        std::os::unix::fs::symlink(
            &target,
            crate::session_daemon::instance_metadata_path(&socket),
        )
        .unwrap();
        assert_eq!(
            instance_metadata(&socket).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn fingerprint_normalizes_equivalent_path_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let config = directory.path().join("config.toml");
        let agent_env = directory.path().join("agent.env");
        std::fs::write(&config, "").unwrap();
        std::fs::write(&agent_env, "").unwrap();
        let config_alias = directory.path().join("config-alias.toml");
        let env_alias = directory.path().join("env-alias");
        std::os::unix::fs::symlink(&config, &config_alias).unwrap();
        std::os::unix::fs::symlink(&agent_env, &env_alias).unwrap();
        let socket = directory.path().join("session.sock");
        let direct = options(&workspace, &config, &socket, &agent_env);
        let aliased = options(&workspace, &config_alias, &socket, &env_alias);
        assert_eq!(
            bootstrap_fingerprint(&direct),
            bootstrap_fingerprint(&aliased)
        );
    }

    // R4 (FR-07, ADR-022): `--workspace-read-only` is part of the daemon's
    // launch identity. A read-only client (a) spawns a read-only daemon,
    // (b) hashes read-only into the fingerprint, and (c) treats read-only as an
    // explicit selection so attaching to a daemon with a different (e.g.
    // non-read-only) identity is Mismatched, never a silent NotRequested join.
    #[test]
    fn read_only_is_forwarded_to_the_spawned_daemon() {
        let mut opts = options(
            Path::new("/workspace"),
            Path::new("/config.toml"),
            Path::new("/runtime/session.sock"),
            Path::new("/agent.env"),
        );
        assert!(!daemon_args(&opts).contains(&OsString::from("--workspace-read-only")));
        opts.workspace_read_only = true;
        assert!(daemon_args(&opts).contains(&OsString::from("--workspace-read-only")));
    }

    #[test]
    fn read_only_changes_the_bootstrap_fingerprint() {
        let plain = options(
            Path::new("/workspace"),
            Path::new("/config.toml"),
            Path::new("/runtime/session.sock"),
            Path::new("/agent.env"),
        );
        let mut read_only = options(
            Path::new("/workspace"),
            Path::new("/config.toml"),
            Path::new("/runtime/session.sock"),
            Path::new("/agent.env"),
        );
        read_only.workspace_read_only = true;
        assert_ne!(
            bootstrap_fingerprint(&plain),
            bootstrap_fingerprint(&read_only)
        );
    }

    #[test]
    fn read_only_client_refuses_a_non_read_only_daemon() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.path().join("session.sock");
        let config = directory.path().join("config.toml");
        let agent_env = directory.path().join("agent.env");
        let metadata_path = crate::session_daemon::instance_metadata_path(&socket);

        // A read-only client whose ONLY selection is read-only is still an
        // explicit selection (not NotRequested), so it is compared against the
        // daemon's published identity.
        let read_only = BootstrapOptions {
            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
            config_path: None,
            provider: None,
            agent_env: None,
            workspace_read_only: true,
            ..options(directory.path(), &config, &socket, &agent_env)
        };

        // The running daemon published a NON-read-only fingerprint (its own
        // bare identity). The client must NOT silently attach.
        let non_read_only = BootstrapOptions {
            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
            config_path: None,
            provider: None,
            agent_env: None,
            ..options(directory.path(), &config, &socket, &agent_env)
        };
        std::fs::write(
            &metadata_path,
            serde_json::to_vec(&serde_json::json!({
                "pid": 1,
                "bootstrap_fingerprint": bootstrap_fingerprint(&non_read_only),
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::set_permissions(&metadata_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            launch_identity(&read_only).unwrap(),
            LaunchIdentity::Mismatched
        );

        // A read-only client attaching to a read-only daemon Matches.
        std::fs::write(
            &metadata_path,
            serde_json::to_vec(&serde_json::json!({
                "pid": 1,
                "bootstrap_fingerprint": bootstrap_fingerprint(&read_only),
            }))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            launch_identity(&read_only).unwrap(),
            LaunchIdentity::Matched
        );
    }
}
