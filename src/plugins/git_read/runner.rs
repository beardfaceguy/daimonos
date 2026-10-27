//! Fixed process policy and canonical repository confinement for ADR-017.
use crate::config::{GitReadConfig, ProcessConfig};
use crate::managed_process::{self, ManagedOutput};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(super) struct Failure {
    kind: &'static str,
    message: String,
    truncated: bool,
    details: Option<Value>,
}
impl Failure {
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            truncated: false,
            details: None,
        }
    }
    pub fn truncated(mut self) -> Self {
        self.truncated = true;
        self
    }
    pub fn details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
    pub fn json(self) -> Value {
        json!({"error":{"kind":self.kind,"message":self.message}, "truncated":self.truncated, "metadata":self.details})
    }
}
impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        Self::new("io_error", error.to_string())
    }
}

pub(super) struct ReadRepo {
    pub path: PathBuf,
    pub shallow: bool,
    pub partial: bool,
    pub bare: bool,
    env: HashMap<String, String>,
    process: ProcessConfig,
}

async fn confined(path: &Path, workspace: &Path) -> Result<PathBuf, Failure> {
    let canonical = tokio::fs::canonicalize(path).await?;
    if !canonical.starts_with(workspace) {
        return Err(Failure::new(
            "repo_outside_workspace",
            "repository or object store is outside workspace",
        ));
    }
    Ok(canonical)
}

async fn bounded_file(path: &Path, limit: usize) -> Result<String, Failure> {
    use tokio::io::AsyncReadExt;
    if !tokio::fs::metadata(path).await?.is_file() {
        return Err(Failure::new(
            "invalid_repository",
            "repository metadata must be a regular file",
        ));
    }
    let file = tokio::fs::File::open(path).await?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > limit {
        return Err(Failure::new(
            "output_limit",
            "repository metadata exceeds git_read.max_input_bytes",
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| Failure::new("invalid_repository", "non-UTF-8 repository metadata"))
}

impl ReadRepo {
    pub async fn open(
        workspace: &Path,
        input: &str,
        limits: &GitReadConfig,
        process: &ProcessConfig,
    ) -> Result<Self, Failure> {
        if Path::new(input).is_absolute() {
            return Err(Failure::new(
                "rejected_argument",
                "repo must be workspace-relative",
            ));
        }
        let workspace = tokio::fs::canonicalize(workspace).await?;
        let path = confined(&workspace.join(input), &workspace).await?;
        let dotgit = path.join(".git");
        let gitdir = if dotgit.is_dir() {
            confined(&dotgit, &workspace).await?
        } else if dotgit.is_file() {
            let pointer = confined(&dotgit, &workspace).await?;
            let content = bounded_file(&pointer, limits.max_input_bytes).await?;
            let target = content
                .trim()
                .strip_prefix("gitdir: ")
                .ok_or_else(|| Failure::new("invalid_repository", "invalid .git file"))?;
            confined(&path.join(target), &workspace).await?
        } else if path.join("HEAD").is_file() && path.join("objects").is_dir() {
            path.clone()
        } else {
            return Err(Failure::new(
                "not_repository",
                "repo must name a repository root",
            ));
        };
        let common = if gitdir.join("commondir").exists() {
            let pointer = confined(&gitdir.join("commondir"), &workspace).await?;
            let content = bounded_file(&pointer, limits.max_input_bytes).await?;
            confined(&gitdir.join(content.trim()), &workspace).await?
        } else {
            gitdir.clone()
        };
        // Alternates can chain. Validate every reachable store, including symlinks.
        let mut pending = vec![common.join("objects")];
        let mut seen = HashSet::new();
        while let Some(objects) = pending.pop() {
            let objects = confined(&objects, &workspace).await?;
            if !seen.insert(objects.clone()) {
                continue;
            }
            if seen.len() > limits.max_alternate_dirs {
                return Err(Failure::new(
                    "output_limit",
                    "too many alternate object stores",
                ));
            }
            let alternates = objects.join("info/alternates");
            if alternates.exists() {
                let alternates = confined(&alternates, &workspace).await?;
                let content = bounded_file(&alternates, limits.max_input_bytes).await?;
                for line in content.lines().filter(|line| !line.is_empty()) {
                    if pending.len() >= limits.max_alternate_dirs {
                        return Err(Failure::new(
                            "output_limit",
                            "too many alternate object stores",
                        ));
                    }
                    // Quoted alternate paths need Git's C-style decoder. Fail closed.
                    if line.starts_with('"') {
                        return Err(Failure::new(
                            "rejected_argument",
                            "quoted alternate paths are unsupported",
                        ));
                    }
                    pending.push(objects.join(line));
                }
            }
        }
        let mut env: HashMap<String, String> = super::super::git::NONINTERACTIVE_ENV
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect();
        for (key, value) in [
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("GIT_NO_LAZY_FETCH", "1"),
            ("GIT_PAGER", "cat"),
            ("GIT_ALLOW_PROTOCOL", ""),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_SYSTEM", "/dev/null"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_ATTR_NOSYSTEM", "1"),
            ("GIT_NO_REPLACE_OBJECTS", "1"),
            ("LC_ALL", "C"),
        ] {
            env.insert(key.into(), value.into());
        }
        env.insert("GIT_DIR".into(), gitdir.to_string_lossy().into_owned());
        // A bare repository has no worktree. Never reinterpret its object database as one.
        let bare = gitdir == path;
        if !bare {
            env.insert("GIT_WORK_TREE".into(), path.to_string_lossy().into_owned());
        }
        let mut process = process.clone();
        // Inherit no Git controls, even if process.inherit_env_prefixes includes GIT_.
        process
            .inherit_env
            .retain(|key| !key.starts_with("GIT_") && key != "PAGER");
        process
            .inherit_env_prefixes
            .retain(|prefix| !"GIT_".starts_with(prefix.as_str()) && !prefix.starts_with("GIT_"));
        process.output_memory_bytes = limits.max_output_bytes;
        let mut repo = Self {
            path,
            shallow: false,
            partial: false,
            bare,
            env,
            process,
        };
        repo.shallow = repo
            .text(&["rev-parse", "--is-shallow-repository"])
            .await?
            .trim()
            == "true";
        // --get-regexp has exit 1 for no match. This command only reads config.
        let partial = repo
            .output(&[
                "config",
                "--local",
                "--get-regexp",
                "^(extensions\\.partialclone|remote\\..*\\.promisor)$",
            ])
            .await?;
        if partial.status.code() != Some(1) {
            repo.check(&partial)?;
        }
        if partial.stdout_truncated || partial.stderr_truncated {
            return Err(
                Failure::new("output_limit", "partial-clone metadata exceeds byte cap").truncated(),
            );
        }
        repo.partial = partial.stdout.lines().any(|line| {
            let (key, value) = line.split_once(' ').unwrap_or((line, ""));
            if key == "extensions.partialclone" {
                !value.is_empty()
            } else {
                matches!(
                    value.to_ascii_lowercase().as_str(),
                    "" | "true" | "yes" | "on" | "1"
                )
            }
        });
        Ok(repo)
    }

    pub async fn output(&self, arguments: &[&str]) -> Result<ManagedOutput, Failure> {
        let mut argv: Vec<String> = [
            "--no-pager",
            "--no-lazy-fetch",
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.untrackedCache=false",
            "-c",
            "submodule.recurse=false",
            "-c",
            "diff.submodule=short",
            "-c",
            "color.ui=false",
            "-c",
            "log.showSignature=false",
        ]
        .iter()
        .map(|s| (*s).into())
        .collect();
        argv.extend(arguments.iter().map(|s| (*s).into()));
        managed_process::run("git", &argv, &self.path, &self.env, &self.process, None)
            .await
            .map_err(Into::into)
    }
    pub async fn text(&self, arguments: &[&str]) -> Result<String, Failure> {
        let output = self.output(arguments).await?;
        self.check(&output)?;
        if output.stdout_truncated || output.stderr_truncated {
            return Err(Failure::new(
                "output_limit",
                "git output exceeds git_read.max_output_bytes",
            )
            .truncated());
        }
        Ok(output.stdout)
    }
    pub fn check(&self, output: &ManagedOutput) -> Result<(), Failure> {
        if output.status.success() {
            return Ok(());
        }
        let diagnostic = output.stderr.to_ascii_lowercase();
        let kind = if [
            "missing",
            "bad object",
            "unable to read",
            "could not read",
            "not a valid object",
            "could not get object info",
            "not a tree object",
        ]
        .iter()
        .any(|message| diagnostic.contains(message))
        {
            "object_missing"
        } else {
            "git_error"
        };
        Err(Failure::new(kind, output.stderr.trim()))
    }
    pub async fn file_object(&self, commit: &str, path: &str) -> Result<String, Failure> {
        self.path_object(commit, path, "blob").await
    }
    pub async fn path_object(
        &self,
        commit: &str,
        path: &str,
        expected: &str,
    ) -> Result<String, Failure> {
        let path = path.trim_end_matches('/');
        let listing = self.text(&["ls-tree", "-z", commit, "--", path]).await?;
        let header = listing
            .split_once('\t')
            .map(|(header, _)| header)
            .ok_or_else(|| Failure::new("path_not_found", "path is absent at revision"))?;
        let fields: Vec<_> = header.split_whitespace().collect();
        if fields.len() != 3 {
            return Err(Failure::new("git_error", "invalid tree entry"));
        }
        if fields[1] != expected {
            return Err(Failure::new(
                "path_not_found",
                "path has the wrong object type",
            ));
        }
        // ls-tree can name a promised object that is not present locally.
        self.text(&["cat-file", "-t", fields[2]]).await?;
        Ok(fields[2].into())
    }
    pub async fn object(&self, revision: &str) -> Result<String, Failure> {
        if revision.starts_with('-') || revision.contains('\0') {
            return Err(Failure::new(
                "rejected_argument",
                "revision must not begin with a hyphen or contain NUL",
            ));
        }
        self.resolve(revision, "object").await
    }
    pub async fn commit(&self, revision: &str) -> Result<String, Failure> {
        self.resolve(revision, "commit").await
    }
    async fn resolve(&self, revision: &str, kind: &str) -> Result<String, Failure> {
        if revision.starts_with('-') || revision.contains('\0') {
            return Err(Failure::new(
                "rejected_argument",
                "revision must not begin with a hyphen or contain NUL",
            ));
        }
        let name = format!("{revision}^{{{kind}}}");
        let out = self
            .output(&["rev-parse", "--verify", "--end-of-options", &name])
            .await?;
        if !out.status.success() {
            let incomplete = (self.shallow || self.partial)
                && (revision.contains(['~', '^'])
                    || revision.chars().all(|c| c.is_ascii_hexdigit()));
            return Err(Failure::new(
                if incomplete {
                    "object_missing"
                } else {
                    "unknown_revision"
                },
                format!("revision is not available locally: {revision}"),
            ));
        }
        let sha = out.stdout.trim();
        if !matches!(sha.len(), 40 | 64) || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Failure::new(
                "unknown_revision",
                "revision did not resolve to a full object ID",
            ));
        }
        Ok(sha.into())
    }
}
