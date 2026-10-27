//! Pinned git reads with no writes, network, or repository-controlled programs.
//! Context: docs/adr/017-git-read-tool.md. The caller supplies the workspace,
//! not the mutable session cwd. All errors stay structured across frontends.

mod runner;
#[cfg(test)]
mod tests;
use crate::config::{GitReadConfig, ProcessConfig};
use crate::tool_runner::{ToolCommand, ToolDescriptor, ToolPlugin, ToolResult};
use runner::{Failure, ReadRepo};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;

const OPERATIONS: &[&str] = &[
    "resolve",
    "show_file",
    "list_tree",
    "grep",
    "log",
    "show_commit",
    "diff",
    "blame",
    "refs",
    "ancestry",
    "worktree_status",
];

pub struct GitReadPlugin {
    descriptor: ToolDescriptor,
    limits: GitReadConfig,
    response_cap: usize,
}

impl GitReadPlugin {
    pub fn with_response_cap(mut self, cap: usize) -> Self {
        self.response_cap = self.response_cap.min(cap);
        self
    }
    pub fn new(limits: GitReadConfig) -> Self {
        let commands = OPERATIONS
            .iter()
            .map(|name| {
                (
                    (*name).into(),
                    ToolCommand {
                        bin: "git".into(),
                        args: vec![(*name).into()],
                        output: "structured".into(),
                    },
                )
            })
            .collect();
        Self {
            response_cap: limits.max_output_bytes,
            limits,
            descriptor: ToolDescriptor {
                id: "git_read".into(),
                commands,
                source_pattern: None,
                manifest: None,
                diagnostics_format: "none".into(),
                supports_quickfix: false,
                quickfix_format: None,
            },
        }
    }
}

/// Only sufficiently recent git can guarantee that partial objects stay local.
pub fn is_available() -> bool {
    std::process::Command::new("git")
        .args(["--no-pager", "--no-lazy-fetch", "--version"])
        .env_clear()
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_PAGER", "cat")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .stdin(std::process::Stdio::null())
        .output()
        .is_ok_and(|out| out.status.success())
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Arguments {
    command: Option<String>,
    repo: Option<String>,
    rev: Option<String>,
    path: Option<String>,
    paths: Option<Vec<String>>,
    pattern: Option<String>,
    a: Option<String>,
    b: Option<String>,
    mode: Option<String>,
    offset: usize,
    limit: Option<usize>,
    recursive: bool,
    follow: bool,
    patch: bool,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

#[async_trait::async_trait]
impl ToolPlugin for GitReadPlugin {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    async fn run_command_with_config(
        &self,
        command: &str,
        workspace: &Path,
        _env: &HashMap<String, String>,
        _stdin: Option<&[u8]>,
        args: Option<&Value>,
        process: &ProcessConfig,
    ) -> Result<ToolResult, String> {
        // No session environment enters git. The runner supplies a fixed overlay.
        let result = async {
            let input = args.cloned().unwrap_or_else(|| json!({}));
            if input.to_string().len() > self.limits.max_input_bytes {
                return Err(Failure::new(
                    "rejected_argument",
                    "arguments exceed git_read.max_input_bytes",
                ));
            }
            let args: Arguments = serde_json::from_value(input)
                .map_err(|e| Failure::new("rejected_argument", e.to_string()))?;
            args.validate(command)?;
            let repo = ReadRepo::open(
                workspace,
                args.repo.as_deref().unwrap_or("."),
                &self.limits,
                process,
            )
            .await?;
            let mut output = self.read(&repo, command, &args).await?;
            output["repo"] = json!(repo.path);
            output["shallow"] = json!(repo.shallow);
            output["partial"] = json!(repo.partial);
            Ok(output)
        }
        .await;
        let (mut output, mut exit_code) = match result {
            Ok(output) => (output, 0),
            Err(failure) => (failure.json(), 1),
        };
        if output.to_string().len() > self.response_cap {
            output = output_limit_response();
            exit_code = 1;
        }
        Ok(ToolResult {
            tool: "git_read".into(),
            command: command.into(),
            exit_code,
            output,
            stderr: String::new(),
        })
    }
}

/// Config validation ensures that even the smallest response cap fits this error.
pub(crate) fn output_limit_response() -> Value {
    json!({"error":{"kind":"output_limit","message":"response exceeds byte cap"},"truncated":true})
}

impl Arguments {
    fn validate(&self, command: &str) -> Result<(), Failure> {
        if !OPERATIONS.contains(&command) || self.command.as_deref().is_some_and(|c| c != command) {
            return Err(Failure::new(
                "rejected_argument",
                "unknown or inconsistent operation",
            ));
        }
        for value in [
            self.repo.as_ref(),
            self.rev.as_ref(),
            self.path.as_ref(),
            self.pattern.as_ref(),
            self.a.as_ref(),
            self.b.as_ref(),
        ]
        .into_iter()
        .flatten()
        .chain(self.paths.iter().flatten())
        {
            if value.starts_with('-') || value.contains('\0') {
                return Err(Failure::new(
                    "rejected_argument",
                    "arguments cannot start with '-' or contain NUL",
                ));
            }
        }
        for path in self.path.iter().chain(self.paths.iter().flatten()) {
            if Path::new(path).is_absolute()
                || Path::new(path)
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(Failure::new(
                    "rejected_argument",
                    "paths must be repository-relative without '..'",
                ));
            }
        }
        if self.limit == Some(0) {
            return Err(Failure::new("rejected_argument", "limit must be positive"));
        }
        Ok(())
    }
    fn paths(&self) -> Vec<String> {
        let mut paths = self.paths.clone().unwrap_or_default();
        if let Some(path) = &self.path {
            paths.push(path.clone());
        }
        paths
    }
    fn rev(&self) -> &str {
        self.rev.as_deref().unwrap_or("HEAD")
    }
    fn path(&self) -> Result<&str, Failure> {
        self.path
            .as_deref()
            .filter(|p| !p.is_empty())
            .ok_or_else(|| Failure::new("rejected_argument", "path is required"))
    }
    fn limit(&self, cap: usize) -> usize {
        self.limit.unwrap_or(cap).min(cap)
    }
}

impl GitReadPlugin {
    async fn read(
        &self,
        repo: &ReadRepo,
        command: &str,
        args: &Arguments,
    ) -> Result<Value, Failure> {
        match command {
            "resolve" => {
                let original = repo.object(args.rev()).await?;
                let kind = repo.text(&["cat-file", "-t", &original]).await?;
                let commit = repo.commit(&original).await?;
                Ok(json!({"commits":[commit], "object_sha":original, "object_type":kind.trim()}))
            }
            "show_file" => {
                let commit = repo.commit(args.rev()).await?;
                let path = args.path()?;
                let object = repo.file_object(&commit, path).await?;
                let size = repo
                    .text(&["cat-file", "-s", &object])
                    .await?
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| Failure::new("git_error", "invalid blob size"))?;
                if size > self.limits.max_file_bytes {
                    return Err(Failure::new(
                        "output_limit",
                        "file exceeds git_read.max_file_bytes",
                    )
                    .truncated());
                }
                let content = repo
                    .text(&[
                        "show",
                        "--format=",
                        "--no-ext-diff",
                        "--no-textconv",
                        &object,
                    ])
                    .await?;
                if content.contains(['\0', '\u{fffd}']) {
                    return Err(Failure::new("binary_file", "binary content is not returned")
                        .details(json!({"repo":repo.path,"commits":[commit],"path":path,"object_sha":object,"size":size})));
                }
                let lines: Vec<_> = content.split_inclusive('\n').collect();
                let end = args
                    .offset
                    .saturating_add(args.limit(self.limits.max_lines))
                    .min(lines.len());
                let start = args.offset.min(lines.len());
                Ok(
                    json!({"commits":[commit],"path":path,"object_sha":object,"size":size,
                    "content":lines[start..end].concat(),"offset":args.offset,"lines":end-start,
                    "truncated":end<lines.len(),"next_offset":(end<lines.len()).then_some(end)}),
                )
            }
            "list_tree" => {
                let commit = repo.commit(args.rev()).await?;
                let tree = if let Some(path) =
                    args.path.as_deref().filter(|p| !p.is_empty() && *p != ".")
                {
                    repo.path_object(&commit, path, "tree").await?
                } else {
                    commit.clone()
                };
                let mut argv = vec!["ls-tree", "-l", "-z"];
                if args.recursive {
                    argv.extend(["-r", "-t"]);
                }
                argv.extend([tree.as_str(), "--"]);
                let listing = repo.text(&argv).await?;
                let mut entries = Vec::new();
                for record in listing.split_terminator('\0') {
                    let (header, name) = record
                        .split_once('\t')
                        .ok_or_else(|| Failure::new("git_error", "invalid tree entry"))?;
                    let fields: Vec<_> = header.split_whitespace().collect();
                    if fields.len() != 4 {
                        return Err(Failure::new("git_error", "invalid tree metadata"));
                    }
                    entries.push(json!({"name":name,"mode":fields[0],"type":fields[1],"sha":fields[2],"size":fields[3].parse::<u64>().ok()}));
                }
                Ok(page(
                    entries,
                    &commit,
                    args.offset,
                    args.limit(self.limits.max_entries),
                    "entries",
                ))
            }
            "grep" => {
                let commit = repo.commit(args.rev()).await?;
                let pattern = args
                    .pattern
                    .as_deref()
                    .ok_or_else(|| Failure::new("rejected_argument", "grep requires pattern"))?;
                let mut argv = vec![
                    "grep",
                    "--no-textconv",
                    "--no-recurse-submodules",
                    "-I",
                    "-E",
                    "-n",
                    "-z",
                    "--full-name",
                    "-e",
                    pattern,
                    commit.as_str(),
                    "--",
                ];
                let paths = args.paths();
                argv.extend(paths.iter().map(String::as_str));
                let out = repo.output(&argv).await?;
                if out.status.code() != Some(1) || !out.stderr.is_empty() {
                    repo.check(&out)?;
                }
                if out.stdout_truncated || out.stderr_truncated {
                    return Err(Failure::new(
                        "output_limit",
                        "grep output exceeds git_read.max_output_bytes",
                    )
                    .truncated());
                }
                // A warning can mean an incomplete search. Do not report it as a clean result.
                if !out.stderr.trim().is_empty() {
                    return Err(Failure::new("git_error", out.stderr.trim()));
                }
                let mut matches = Vec::new();
                let mut remaining = out.stdout.as_str();
                while !remaining.is_empty() {
                    let (path, tail) = remaining
                        .split_once('\0')
                        .ok_or_else(|| Failure::new("git_error", "invalid grep path"))?;
                    let (number, tail) = tail
                        .split_once('\0')
                        .ok_or_else(|| Failure::new("git_error", "invalid grep line number"))?;
                    let (line, tail) = tail.split_once('\n').unwrap_or((tail, ""));
                    remaining = tail;
                    let path = path.strip_prefix(&format!("{commit}:")).unwrap_or(path);
                    matches.push(json!({"path":path,"line_number":number.parse::<usize>().map_err(|_| Failure::new("git_error", "invalid grep line number"))?,"line":line}));
                }
                Ok(page(
                    matches,
                    &commit,
                    args.offset,
                    args.limit(self.limits.max_results),
                    "matches",
                ))
            }
            "log" => {
                let (range, commits) = pinned_range(repo, args.rev()).await?;
                let entries = history(repo, &range, args, self.limits.max_commits).await?;
                let limit = args.limit(self.limits.max_commits);
                let more = entries.len() > limit;
                Ok(
                    json!({"commits":commits,"entries":entries.into_iter().take(limit).collect::<Vec<_>>(),
                    "truncated":more,"next_offset":more.then_some(args.offset.saturating_add(limit))}),
                )
            }
            "diff" => {
                let commits = if let (Some(a), Some(b)) = (&args.a, &args.b) {
                    vec![repo.commit(a).await?, repo.commit(b).await?]
                } else {
                    pinned_range(repo, args.rev()).await?.1
                };
                if commits.len() != 2 {
                    return Err(Failure::new(
                        "rejected_argument",
                        "diff requires a and b, or rev=a..b",
                    ));
                }
                let paths = args.paths();
                let mut argv = vec![
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    "--no-renames",
                    "--ignore-submodules=all",
                    "--numstat",
                    "-z",
                    commits[0].as_str(),
                    commits[1].as_str(),
                    "--",
                ];
                argv.extend(paths.iter().map(String::as_str));
                let files = numstat(&repo.text(&argv).await?)?;
                let mut output = page(
                    files,
                    &commits[1],
                    args.offset,
                    args.limit(self.limits.max_entries),
                    "files",
                );
                if args.mode.as_deref().unwrap_or("stat") == "patch" {
                    argv.retain(|arg| !matches!(*arg, "--numstat" | "-z"));
                    let (patch, truncated) =
                        patch_preview(repo, &argv, self.limits.max_patch_bytes).await?;
                    output["patch"] = json!(patch);
                    output["truncated"] = json!(output["truncated"] == true || truncated);
                } else if args.mode.as_deref().is_some_and(|m| m != "stat") {
                    return Err(Failure::new(
                        "rejected_argument",
                        "mode must be stat or patch",
                    ));
                }
                output["commits"] = json!(commits);
                Ok(output)
            }
            "show_commit" => {
                let commit = repo.commit(args.rev()).await?;
                let metadata = history(
                    repo,
                    &commit,
                    &Arguments {
                        limit: Some(1),
                        ..Default::default()
                    },
                    1,
                )
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| Failure::new("object_missing", "commit metadata is absent"))?;
                let paths = args.paths();
                let mut argv = vec![
                    "show",
                    "--format=",
                    "--first-parent",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    "--no-renames",
                    "--ignore-submodules=all",
                    "--numstat",
                    "-z",
                    commit.as_str(),
                    "--",
                ];
                argv.extend(paths.iter().map(String::as_str));
                let files = numstat(&repo.text(&argv).await?)?;
                let mut output = page(
                    files,
                    &commit,
                    args.offset,
                    args.limit(self.limits.max_entries),
                    "files",
                );
                output["commit"] = metadata;
                if args.patch {
                    argv.retain(|arg| !matches!(*arg, "--numstat" | "-z"));
                    argv.insert(1, "--patch");
                    let (patch, truncated) =
                        patch_preview(repo, &argv, self.limits.max_patch_bytes).await?;
                    output["patch"] = json!(patch);
                    output["truncated"] = json!(output["truncated"] == true || truncated);
                }
                Ok(output)
            }
            "refs" => {
                let listing = repo
                    .text(&[
                        "for-each-ref",
                        "--format=%(refname)%00%(objectname)%00%(objecttype)",
                        "--",
                        "refs/heads/",
                        "refs/tags/",
                    ])
                    .await?;
                let entries: Vec<_> = listing.lines().collect();
                let end = args
                    .offset
                    .saturating_add(args.limit(self.limits.max_entries))
                    .min(entries.len());
                let start = args.offset.min(entries.len());
                let mut refs = Vec::new();
                let mut commits = Vec::new();
                for entry in &entries[start..end] {
                    let fields: Vec<_> = entry.split('\0').collect();
                    if fields.len() != 3 {
                        return Err(Failure::new("git_error", "invalid ref record"));
                    }
                    let out = repo
                        .output(&[
                            "rev-parse",
                            "--verify",
                            "--end-of-options",
                            &format!("{}^{{commit}}", fields[1]),
                        ])
                        .await?;
                    let commit = if out.status.success() {
                        Some(out.stdout.trim().to_string())
                    } else {
                        None
                    };
                    if let Some(commit) = &commit {
                        if !commits.contains(commit) {
                            commits.push(commit.clone());
                        }
                    }
                    refs.push(json!({"name":fields[0],"sha":fields[1],"object_type":fields[2],"commit_sha":commit}));
                }
                Ok(
                    json!({"refs":refs,"commits":commits,"truncated":end<entries.len(),"next_offset":(end<entries.len()).then_some(end)}),
                )
            }
            "ancestry" => {
                let a = repo
                    .commit(
                        args.a.as_deref().ok_or_else(|| {
                            Failure::new("rejected_argument", "ancestry requires a")
                        })?,
                    )
                    .await?;
                let b = repo
                    .commit(
                        args.b.as_deref().ok_or_else(|| {
                            Failure::new("rejected_argument", "ancestry requires b")
                        })?,
                    )
                    .await?;
                let ancestor = repo
                    .output(&["merge-base", "--is-ancestor", &a, &b])
                    .await?;
                if ancestor.status.code() != Some(1) || !ancestor.stderr.is_empty() {
                    repo.check(&ancestor)?;
                }
                let base = repo.output(&["merge-base", &a, &b]).await?;
                if base.status.code() != Some(1) || !base.stderr.is_empty() {
                    repo.check(&base)?;
                }
                Ok(
                    json!({"commits":[a,b],"is_ancestor":ancestor.status.success(),
                    "merge_base":base.status.success().then(|| base.stdout.trim().to_string())}),
                )
            }
            "worktree_status" => {
                if repo.bare {
                    return Err(Failure::new(
                        "bare_repository",
                        "worktree_status requires a working tree",
                    ));
                }
                let commit = repo.commit("HEAD").await?;
                let listing = repo
                    .text(&[
                        "status",
                        "--porcelain=v1",
                        "-z",
                        "--no-renames",
                        "--untracked-files=all",
                        "--ignore-submodules=all",
                    ])
                    .await?;
                let mut entries = Vec::new();
                for record in listing.split_terminator('\0') {
                    if record.len() < 3
                        || !record.is_char_boundary(2)
                        || !record.is_char_boundary(3)
                    {
                        return Err(Failure::new("git_error", "invalid status record"));
                    }
                    entries.push(json!({"status":&record[..2],"path":&record[3..]}));
                }
                let clean = entries.is_empty();
                let mut output = page(
                    entries,
                    &commit,
                    args.offset,
                    args.limit(self.limits.max_entries),
                    "entries",
                );
                output["clean"] = json!(clean);
                Ok(output)
            }
            "blame" => {
                let commit = repo.commit(args.rev()).await?;
                let path = args.path()?;
                let object = repo.file_object(&commit, path).await?;
                let size = repo
                    .text(&["cat-file", "-s", &object])
                    .await?
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| Failure::new("git_error", "invalid blob size"))?;
                if size > self.limits.max_file_bytes {
                    return Err(Failure::new(
                        "output_limit",
                        "file exceeds git_read.max_file_bytes",
                    )
                    .truncated());
                }
                let content = repo
                    .text(&[
                        "show",
                        "--format=",
                        "--no-ext-diff",
                        "--no-textconv",
                        &object,
                    ])
                    .await?;
                if content.contains(['\0', '\u{fffd}']) {
                    return Err(
                        Failure::new("binary_file", "binary content cannot be blamed")
                            .details(json!({"size":size,"path":path,"object_sha":object})),
                    );
                }
                let total = content.lines().count();
                let start = args.start_line.unwrap_or(1).saturating_add(args.offset);
                if start == 0 {
                    return Err(Failure::new(
                        "rejected_argument",
                        "start_line must be positive",
                    ));
                }
                let end = args
                    .end_line
                    .unwrap_or(total)
                    .min(
                        start
                            .saturating_add(args.limit(self.limits.max_lines))
                            .saturating_sub(1),
                    )
                    .min(total);
                if start > end {
                    return Err(Failure::new(
                        "rejected_argument",
                        "line range is empty or outside the file",
                    ));
                }
                let range = format!("{start},{end}");
                let listing = repo
                    .text(&[
                        "blame",
                        "--no-ext-diff",
                        "--line-porcelain",
                        "--no-textconv",
                        "-L",
                        &range,
                        &commit,
                        "--",
                        path,
                    ])
                    .await?;
                let mut entries = Vec::new();
                let mut entry = json!({});
                for line in listing.lines() {
                    if let Some(text) = line.strip_prefix('\t') {
                        entry["line"] = json!(text);
                        entries.push(std::mem::take(&mut entry));
                        entry = json!({});
                    } else if let Some(author) = line.strip_prefix("author ") {
                        entry["author"] = json!(author);
                    } else if let Some(time) = line.strip_prefix("author-time ") {
                        let seconds = time
                            .parse::<i64>()
                            .map_err(|_| Failure::new("git_error", "invalid blame date"))?;
                        entry["date"] = json!(chrono::DateTime::from_timestamp(seconds, 0)
                            .map(|date| date.to_rfc3339()));
                    } else {
                        let fields: Vec<_> = line.split_whitespace().collect();
                        if fields.len() >= 3
                            && fields[0].len() >= 40
                            && fields[0].bytes().all(|c| c.is_ascii_hexdigit())
                        {
                            entry["sha"] = json!(fields[0]);
                            entry["line_number"] = json!(fields[2].parse::<usize>().map_err(
                                |_| Failure::new("git_error", "invalid blame line number")
                            )?);
                        }
                    }
                }
                let more = end < args.end_line.unwrap_or(total).min(total);
                Ok(
                    json!({"commits":[commit],"path":path,"lines":entries,"truncated":more,"next_offset":more.then_some(args.offset.saturating_add(end-start+1))}),
                )
            }
            _ => Err(Failure::new("rejected_argument", "unknown operation")),
        }
    }
}

pub fn schema() -> Value {
    json!({"type":"object", "additionalProperties":false, "required":["command"], "properties":{
        "command":{"type":"string","enum":OPERATIONS}, "repo":{"type":"string"}, "rev":{"type":"string"},
        "path":{"type":"string"}, "paths":{"type":"array","items":{"type":"string"}}, "pattern":{"type":"string"},
        "a":{"type":"string"}, "b":{"type":"string"}, "mode":{"type":"string","enum":["stat","patch"]},
        "offset":{"type":"integer","minimum":0}, "limit":{"type":"integer","minimum":1},
        "recursive":{"type":"boolean"}, "follow":{"type":"boolean"}, "patch":{"type":"boolean"},
        "start_line":{"type":"integer","minimum":1}, "end_line":{"type":"integer","minimum":1}
    }})
}

fn page(entries: Vec<Value>, commit: &str, offset: usize, limit: usize, field: &str) -> Value {
    let end = offset.saturating_add(limit).min(entries.len());
    let start = offset.min(entries.len());
    let mut output = json!({"commits":[commit],"truncated":end<entries.len(),"next_offset":(end<entries.len()).then_some(end)});
    output[field] = json!(entries[start..end]);
    output
}

async fn pinned_range(repo: &ReadRepo, revision: &str) -> Result<(String, Vec<String>), Failure> {
    if let Some((a, b)) = revision.split_once("..") {
        if a.is_empty() || b.is_empty() || b.starts_with('.') || b.contains("..") {
            return Err(Failure::new(
                "rejected_argument",
                "range requires two revisions: a..b",
            ));
        }
        let a = repo.commit(a).await?;
        let b = repo.commit(b).await?;
        Ok((format!("{a}..{b}"), vec![a, b]))
    } else {
        let commit = repo.commit(revision).await?;
        Ok((commit.clone(), vec![commit]))
    }
}

async fn history(
    repo: &ReadRepo,
    range: &str,
    args: &Arguments,
    max_commits: usize,
) -> Result<Vec<Value>, Failure> {
    let limit = format!("--max-count={}", args.limit(max_commits).saturating_add(1));
    let skip = format!("--skip={}", args.offset);
    let mut argv = vec![
        "log",
        "--no-ext-diff",
        "--no-textconv",
        "--no-show-signature",
        "--no-use-mailmap",
        "--encoding=UTF-8",
        "--format=%H%x00%an%x00%aI%x00%cI%x00%s",
        "-z",
        &limit,
        &skip,
    ];
    let paths = args.paths();
    if args.follow {
        if paths.len() != 1 {
            return Err(Failure::new(
                "rejected_argument",
                "follow requires exactly one path",
            ));
        }
        argv.push("--follow");
    }
    argv.extend([range, "--"]);
    argv.extend(paths.iter().map(String::as_str));
    let out = repo.text(&argv).await?;
    let fields: Vec<_> = out.split_terminator('\0').collect();
    let mut entries = Vec::new();
    for record in fields.chunks(5) {
        if record.len() != 5 {
            return Err(Failure::new("git_error", "invalid log record"));
        }
        entries.push(json!({"sha":record[0],"author":record[1],"author_date":record[2],"committer_date":record[3],"subject":record[4]}));
    }
    Ok(entries)
}

fn numstat(text: &str) -> Result<Vec<Value>, Failure> {
    text.split_terminator('\0').filter(|s| !s.is_empty()).map(|record| {
        let mut fields = record.splitn(3,'\t');
        let added = fields.next().ok_or_else(|| Failure::new("git_error","invalid numstat"))?;
        let deleted = fields.next().ok_or_else(|| Failure::new("git_error","invalid numstat"))?;
        let path = fields.next().ok_or_else(|| Failure::new("git_error","invalid numstat path"))?;
        Ok(json!({"path":path,"added":added.parse::<u64>().ok(),"deleted":deleted.parse::<u64>().ok()}))
    }).collect()
}

async fn patch_preview(
    repo: &ReadRepo,
    argv: &[&str],
    max_bytes: usize,
) -> Result<(String, bool), Failure> {
    let output = repo.output(argv).await?;
    repo.check(&output)?;
    // Managed capture uses a head/tail preview on overflow. Never label that as a complete patch.
    let cut = super::floor_char_boundary(&output.stdout, max_bytes);
    Ok((
        output.stdout[..cut].to_string(),
        output.stdout_truncated || cut < output.stdout.len(),
    ))
}
