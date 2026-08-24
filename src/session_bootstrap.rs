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
    pub socket_path: &'a Path,
    pub provider: Option<&'a str>,
    pub model: Option<&'a str>,
    pub agent_env: Option<&'a Path>,
    pub max_frame_bytes: usize,
    pub timeout: Duration,
    pub retry_interval: Duration,
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
    let mut command = tokio::process::Command::new(&executable);
    command
        .args(daemon_args(options))
        .env(BOOTSTRAP_FINGERPRINT_ENV, bootstrap_fingerprint(options))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(false);
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

fn daemon_args(options: &BootstrapOptions<'_>) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("--workspace"),
        options.workspace.as_os_str().to_owned(),
    ];
    if let Some(path) = options.config_path {
        args.push(OsString::from("--config"));
        args.push(path.as_os_str().to_owned());
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
    if let Some(provider) = options.provider {
        digest.update(provider.as_bytes());
    }
    digest.update([0_u8]);
    format!("{:x}", digest.finalize())
}

fn launch_identity(options: &BootstrapOptions<'_>) -> std::io::Result<LaunchIdentity> {
    if options.config_path.is_none() && options.provider.is_none() && options.agent_env.is_none() {
        return Ok(LaunchIdentity::NotRequested);
    }
    let Some(value) = instance_metadata(options.socket_path)? else {
        return Ok(LaunchIdentity::Unavailable);
    };
    let Some(fingerprint) = value["bootstrap_fingerprint"].as_str() else {
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

    fn options<'a>(
        workspace: &'a Path,
        config: &'a Path,
        socket: &'a Path,
        agent_env: &'a Path,
    ) -> BootstrapOptions<'a> {
        BootstrapOptions {
            workspace,
            config_path: Some(config),
            socket_path: socket,
            provider: Some("openrouter"),
            model: Some("test/model"),
            agent_env: Some(agent_env),
            max_frame_bytes: 1024,
            timeout: Duration::from_secs(1),
            retry_interval: Duration::from_millis(1),
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
}
