//! Frozen metadata prevents repository-config races from executing status filters.
//! Context: docs/contexts/git-read/CONTEXT.md and ADR-019.

use super::runner::Failure;
use crate::config::GitReadConfig;
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;

// These data-only settings preserve built-in status normalization. Executable settings,
// includes, and filter sections never enter the private repository.
pub(super) const STATUS_CONFIG_PATTERN: &str = r"^(core\.(filemode|ignorecase|symlinks|precomposeunicode|trustctime|checkstat|ignorestat|autocrlf|eol|safecrlf|attributesfile|excludesfile|checkroundtripencoding|sparsecheckout|sparsecheckoutcone|protectntfs|protecthfs)|index\.(sparse|version))$";

pub(super) struct StatusView {
    pub path: PathBuf,
    remaining: u64,
}

impl Drop for StatusView {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.path) {
            tracing::warn!(target:"daimonos::git_read",event="status_metadata_cleanup_failed",%error);
        }
    }
}

impl StatusView {
    pub async fn create(
        workspace: &Path,
        gitdir: &Path,
        common: &Path,
        commit: &str,
        config: &str,
        limits: &GitReadConfig,
    ) -> Result<Self, Failure> {
        let directory = limits
            .status_directory
            .as_deref()
            .map(crate::paths::expand_tilde)
            .unwrap_or_else(std::env::temp_dir);
        let directory = if directory.is_absolute() {
            directory
        } else {
            workspace.join(directory)
        };
        let directory = tokio::fs::canonicalize(&directory).await.map_err(|error| {
            Failure::new(
                "io_error",
                format!("git_read.status_directory {}: {error}", directory.display()),
            )
        })?;
        if directory.starts_with(workspace) {
            return Err(Failure::new(
                "rejected_argument",
                format!(
                    "git_read.status_directory must be outside the workspace: {}",
                    directory.display()
                ),
            ));
        }
        let path = directory.join(format!("daimonos-git-status-{}", uuid::Uuid::new_v4()));
        // Acquire directory ownership without an await between creation and the drop guard.
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path).map_err(|error| {
            Failure::new(
                "io_error",
                format!("git_read.status_directory {}: {error}", directory.display()),
            )
        })?;
        let mut view = Self {
            path,
            remaining: limits.max_status_metadata_bytes as u64,
        };
        tokio::fs::create_dir(view.path.join("refs")).await?;
        tokio::fs::create_dir(view.path.join("info")).await?;
        let format = match commit.len() {
            40 => "[core]\nrepositoryformatversion=0\nbare=false\n",
            64 => {
                "[core]\nrepositoryformatversion=1\nbare=false\n[extensions]\nobjectformat=sha256\n"
            }
            _ => {
                return Err(Failure::new(
                    "git_error",
                    "unsupported commit identifier format",
                ))
            }
        };
        let mut settings = format.to_string();
        for record in config.split_terminator('\0') {
            let (key, value) = record.split_once('\n').unwrap_or((record, "true"));
            let (section, name) = key
                .split_once('.')
                .ok_or_else(|| Failure::new("git_error", "invalid status configuration"))?;
            let allowed = match section {
                "core" => matches!(
                    name,
                    "filemode"
                        | "ignorecase"
                        | "symlinks"
                        | "precomposeunicode"
                        | "trustctime"
                        | "checkstat"
                        | "ignorestat"
                        | "autocrlf"
                        | "eol"
                        | "safecrlf"
                        | "attributesfile"
                        | "excludesfile"
                        | "checkroundtripencoding"
                        | "sparsecheckout"
                        | "sparsecheckoutcone"
                        | "protectntfs"
                        | "protecthfs"
                ),
                "index" => matches!(name, "sparse" | "version"),
                _ => false,
            };
            if !allowed {
                return Err(Failure::new(
                    "git_error",
                    "unexpected status configuration key",
                ));
            }
            let value = value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\t', "\\t");
            settings.push_str(&format!("[{section}]\n{name}=\"{value}\"\n"));
        }
        view.write("config", settings.as_bytes()).await?;
        // Detached HEAD and porcelain v1 without --branch do not need upstream refs.
        view.write("HEAD", format!("{commit}\n").as_bytes()).await?;
        view.copy(&gitdir.join("index"), "index", workspace).await?;
        for name in ["attributes", "exclude"] {
            view.copy(
                &common.join("info").join(name),
                &format!("info/{name}"),
                workspace,
            )
            .await?;
        }
        view.copy(
            &gitdir.join("info/sparse-checkout"),
            "info/sparse-checkout",
            workspace,
        )
        .await?;
        // Git keeps a linked worktree's shared indexes next to its index. The combined
        // worktree/split-index regression verifies rev-parse --shared-index-path.
        // Charge top-level names to bound the scan. Do not recurse into refs or objects.
        let mut entries = tokio::fs::read_dir(gitdir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            view.charge(name.len() as u64)?;
            if name.to_string_lossy().starts_with("sharedindex.") {
                let name = name
                    .to_str()
                    .ok_or_else(|| Failure::new("git_error", "invalid shared-index name"))?;
                view.copy(&entry.path(), name, workspace).await?;
            }
        }
        Ok(view)
    }

    fn charge(&mut self, bytes: u64) -> Result<(), Failure> {
        self.remaining = self.remaining.checked_sub(bytes).ok_or_else(|| {
            Failure::new(
                "output_limit",
                "status metadata exceeds git_read.max_status_metadata_bytes",
            )
        })?;
        Ok(())
    }
    async fn write(&mut self, name: &str, bytes: &[u8]) -> Result<(), Failure> {
        self.charge(bytes.len() as u64)?;
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(self.path.join(name)).await?;
        tokio::io::AsyncWriteExt::write_all(&mut file, bytes).await?;
        // Finish queued writes before a separate Git process opens the metadata.
        tokio::io::AsyncWriteExt::flush(&mut file).await?;
        Ok(())
    }
    async fn copy(&mut self, source: &Path, name: &str, workspace: &Path) -> Result<(), Failure> {
        match tokio::fs::symlink_metadata(source).await {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let source = tokio::fs::canonicalize(source).await?;
        if !source.starts_with(workspace) {
            return Err(Failure::new(
                "repo_outside_workspace",
                "status metadata points outside workspace",
            ));
        }
        let mut source_options = tokio::fs::OpenOptions::new();
        source_options.read(true);
        #[cfg(unix)]
        source_options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
        let file = source_options.open(&source).await?;
        let metadata = file.metadata().await?;
        if !metadata.is_file() {
            return Err(Failure::new(
                "invalid_repository",
                "status metadata is not a regular file",
            ));
        }
        let mut input = file.take(self.remaining.saturating_add(1));
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut output = options.open(self.path.join(name)).await?;
        let bytes = tokio::io::copy(&mut input, &mut output).await?;
        self.charge(bytes)?;
        // Git's racy-index check depends on the original index timestamp.
        let output = output.into_std().await;
        output.set_times(std::fs::FileTimes::new().set_modified(metadata.modified()?))?;
        Ok(())
    }
}
