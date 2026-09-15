//! Translate Zed's JSONC `context_servers` into standalone `mcpServers` JSON.
//! Some proprietary harnesses (notably Cursor ACP) ignore ACP-forwarded MCP
//! servers and only read their own file, so a symlink cannot bridge the
//! incompatible top-level formats. This command provides an atomic adapter.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use crate::cli::{McpConfigArgs, McpConfigCommand};
use crate::mcp_bridge::ServerSpec;

pub fn run(args: McpConfigArgs) -> anyhow::Result<()> {
    match args.command {
        McpConfigCommand::Sync {
            source,
            target,
            dry_run,
            exec,
        } => sync(source, target, dry_run, exec),
    }
}

fn sync(
    source: Option<PathBuf>,
    target: PathBuf,
    dry_run: bool,
    exec: Vec<String>,
) -> anyhow::Result<()> {
    if dry_run && !exec.is_empty() {
        anyhow::bail!("--dry-run cannot be combined with a command after --");
    }
    let source = source
        .map(|p| crate::paths::expand_tilde(p.to_string_lossy().as_ref()))
        .unwrap_or_else(|| crate::zed_config::default_settings_path());
    let target = crate::paths::expand_tilde(target.to_string_lossy().as_ref());
    let specs = crate::zed_config::context_server_specs(Some(source.to_string_lossy().as_ref()))?;
    let document = render_mcp_servers(&specs)?;
    if dry_run {
        print!("{document}");
    } else {
        write_atomic(&target, document.as_bytes())?;
        eprintln!(
            "synced {} enabled MCP server(s) from {} to {}",
            specs.len(),
            source.display(),
            target.display()
        );
    }
    if exec.is_empty() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = std::process::Command::new(&exec[0]).args(&exec[1..]).exec();
        Err(anyhow::anyhow!("exec {}: {error}", exec[0]))
    }
    #[cfg(not(unix))]
    anyhow::bail!("sync-then-exec is supported only on Unix; run the synced command separately")
}

fn render_mcp_servers(specs: &[ServerSpec]) -> anyhow::Result<String> {
    let mut servers = serde_json::Map::new();
    for spec in specs {
        let (name, entry) = match spec {
            ServerSpec::Stdio {
                name,
                command,
                args,
                env,
            } => {
                let mut entry = serde_json::Map::new();
                entry.insert("command".into(), command.clone().into());
                if !args.is_empty() {
                    entry.insert("args".into(), serde_json::to_value(args)?);
                }
                if !env.is_empty() {
                    entry.insert("env".into(), serde_json::to_value(env)?);
                }
                (name, entry)
            }
            ServerSpec::Http { name, url, headers } => {
                let mut entry = serde_json::Map::new();
                entry.insert("url".into(), url.clone().into());
                if !headers.is_empty() {
                    entry.insert("headers".into(), serde_json::to_value(headers)?);
                }
                (name, entry)
            }
        };
        servers.insert(name.clone(), entry.into());
    }
    let mut root = serde_json::Map::new();
    root.insert("mcpServers".into(), servers.into());
    Ok(serde_json::to_string_pretty(&root)? + "\n")
}

fn write_atomic(path: &Path, content: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("target has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.daimonos-sync-{}-{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("mcp.json"),
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&tmp)
        .with_context(|| format!("create private temporary file {}", tmp.display()))?;
    file.write_all(content)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path).map_err(|error| {
        let _ = std::fs::remove_file(&tmp);
        anyhow::anyhow!("replace {} atomically: {error}", path.display())
    })?;
    #[cfg(unix)]
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("secure {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn renders_cursor_compatible_document() {
        let specs = vec![
            ServerSpec::Stdio {
                name: "local".into(),
                command: "server".into(),
                args: vec!["--stdio".into()],
                env: HashMap::from([("TOKEN_FILE".into(), "/secret/path".into())]),
            },
            ServerSpec::Http {
                name: "remote".into(),
                url: "https://example.test/mcp".into(),
                headers: HashMap::from([("Authorization".into(), "Bearer test".into())]),
            },
        ];
        let value: serde_json::Value =
            serde_json::from_str(&render_mcp_servers(&specs).unwrap()).unwrap();
        assert_eq!(value["mcpServers"]["local"]["command"], "server");
        assert_eq!(
            value["mcpServers"]["local"]["env"]["TOKEN_FILE"],
            "/secret/path"
        );
        assert_eq!(
            value["mcpServers"]["remote"]["url"],
            "https://example.test/mcp"
        );
        assert_eq!(
            value["mcpServers"]["remote"]["headers"]["Authorization"],
            "Bearer test"
        );
    }

    #[test]
    fn atomic_write_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(&path, "old").unwrap();
        write_atomic(&path, b"new\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn dry_run_with_exec_is_rejected_before_source_io_or_output() {
        let error = sync(
            Some(PathBuf::from("/definitely/missing/settings.json")),
            PathBuf::from("/unused/mcp.json"),
            true,
            vec!["echo".into()],
        )
        .unwrap_err();
        assert!(error.to_string().contains("--dry-run cannot be combined"));
    }
}
