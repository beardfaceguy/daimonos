use super::*;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime};

type Snapshot = BTreeMap<PathBuf, (u64, SystemTime, Vec<u8>)>;

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("fixture git");
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("fixture UTF-8")
        .trim()
        .into()
}

struct Fixture {
    workspace: tempfile::TempDir,
    repo: PathBuf,
    first: String,
    last: String,
}
impl Fixture {
    fn new() -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let repo = workspace.path().join("source");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.name", "Test Author"]);
        git(&repo, &["config", "user.email", "author@example.test"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        fs::create_dir(repo.join("nested")).unwrap();
        fs::write(repo.join("old.txt"), "one\ntwo\nthree\n").unwrap();
        fs::write(repo.join("nested/child.txt"), "needle in nested\n").unwrap();
        fs::write(repo.join("binary.bin"), [0, 255, 42]).unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "Initial"]);
        let first = git(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["tag", "-a", "pinned", "-m", "Review pin"]);
        git(&repo, &["mv", "old.txt", "renamed.txt"]);
        git(&repo, &["commit", "-m", "Rename"]);
        fs::write(repo.join("renamed.txt"), "one\nchanged\nthree\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "Change line"]);
        let last = git(&repo, &["rev-parse", "HEAD"]);
        Self {
            workspace,
            repo,
            first,
            last,
        }
    }
    async fn call(&self, args: Value) -> ToolResult {
        self.with_limits(args, GitReadConfig::default()).await
    }
    async fn with_limits(&self, args: Value, limits: GitReadConfig) -> ToolResult {
        GitReadPlugin::new(limits)
            .run_command_with_config(
                args["command"].as_str().unwrap(),
                self.workspace.path(),
                &HashMap::new(),
                None,
                Some(&args),
                &ProcessConfig::default(),
            )
            .await
            .unwrap()
    }
    fn cases(&self) -> Vec<Value> {
        vec![
            json!({"command":"resolve","repo":"source","rev":"pinned"}),
            json!({"command":"show_file","repo":"source","path":"old.txt","rev":self.first}),
            json!({"command":"list_tree","repo":"source","recursive":true}),
            json!({"command":"grep","repo":"source","pattern":"needle"}),
            json!({"command":"log","repo":"source","follow":true,"path":"renamed.txt"}),
            json!({"command":"show_commit","repo":"source","patch":true}),
            json!({"command":"diff","repo":"source","a":self.first,"b":self.last,"mode":"patch"}),
            json!({"command":"blame","repo":"source","path":"renamed.txt","start_line":1,"end_line":3}),
            json!({"command":"refs","repo":"source"}),
            json!({"command":"ancestry","repo":"source","a":self.first,"b":self.last}),
            json!({"command":"worktree_status","repo":"source"}),
        ]
    }
}

fn snapshot(root: &Path) -> Snapshot {
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(|entry| {
            let entry = entry.unwrap();
            let metadata = fs::symlink_metadata(entry.path()).unwrap();
            let bytes = if metadata.is_file() {
                fs::read(entry.path()).unwrap()
            } else {
                Vec::new()
            };
            (
                entry.path().strip_prefix(root).unwrap().to_path_buf(),
                (
                    metadata.len(),
                    metadata.modified().unwrap(),
                    Sha256::digest(bytes).to_vec(),
                ),
            )
        })
        .collect()
}

#[tokio::test]
async fn every_operation_preserves_git_and_worktree_even_when_stat_stale() {
    let f = Fixture::new();
    fs::write(f.repo.join("renamed.txt"), "one\nDIRTY!!\nthree\n").unwrap();
    fs::File::open(f.repo.join("renamed.txt"))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(5)))
        .unwrap();
    for (args, limits) in f.policy_cases() {
        let before = snapshot(&f.repo);
        let response = f.with_limits(args.clone(), limits).await;
        assert_eq!(response.exit_code, 0, "{args}: {}", response.output);
        assert_eq!(snapshot(&f.repo), before, "mutation in {args}");
        assert!(response.output["repo"].as_str().is_some());
        assert!(!response.output["commits"].as_array().unwrap().is_empty());
        if args["command"] == "worktree_status" {
            assert_eq!(response.output["clean"], false);
        }
    }
}
#[tokio::test]
async fn every_operation_ignores_malicious_config() {
    let f = Fixture::new();
    let sentinel = f.workspace.path().join("executed");
    let script = f.workspace.path().join("trap.sh");
    fs::write(
        &script,
        format!("#!/bin/sh\nprintf bad > '{}'\n", sentinel.display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    git(
        &f.repo,
        &["config", "core.fsmonitor", script.to_str().unwrap()],
    );
    git(
        &f.repo,
        &["config", "diff.external", script.to_str().unwrap()],
    );
    git(
        &f.repo,
        &["config", "diff.trap.textconv", script.to_str().unwrap()],
    );
    git(
        &f.repo,
        &["config", "filter.trap.clean", script.to_str().unwrap()],
    );
    git(
        &f.repo,
        &["config", "filter.trap.smudge", script.to_str().unwrap()],
    );
    git(
        &f.repo,
        &["config", "filter.trap.process", script.to_str().unwrap()],
    );
    git(&f.repo, &["config", "filter.trap.required", "true"]);
    git(
        &f.repo,
        &["config", "alias.show", &format!("!{}", script.display())],
    );
    let hooks = f.workspace.path().join("hooks");
    fs::create_dir(&hooks).unwrap();
    for hook in ["post-index-change", "post-checkout", "pre-auto-gc"] {
        fs::copy(&script, hooks.join(hook)).unwrap();
    }
    git(
        &f.repo,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    );
    fs::write(
        f.repo.join(".gitattributes"),
        "*.txt diff=trap filter=trap\n",
    )
    .unwrap();
    fs::write(f.repo.join("renamed.txt"), "dirty working-tree content\n").unwrap();
    for (args, limits) in f.policy_cases() {
        let response = f.with_limits(args.clone(), limits).await;
        assert_eq!(response.exit_code, 0, "{args}: {}", response.output);
        assert!(!sentinel.exists(), "repo code ran in {args}");
    }
}

#[tokio::test]
async fn caller_flags_and_pathspec_magic_do_not_escape_templates() {
    let f = Fixture::new();
    let target = f.workspace.path().join("should-not-exist");
    for field in ["rev", "path", "pattern", "a", "b"] {
        for value in [
            format!("--output={}", target.display()),
            "-p".into(),
            "--exec=touch sentinel".into(),
        ] {
            let mut args = json!({"command":"resolve","repo":"source"});
            args[field] = json!(value);
            assert_eq!(
                f.call(args).await.output["error"]["kind"],
                "rejected_argument"
            );
        }
    }
    for paths in [json!(["-p"]), json!(["../escape"])] {
        let args = json!({"command":"grep","repo":"source","pattern":"one","paths":paths});
        assert_eq!(
            f.call(args).await.output["error"]["kind"],
            "rejected_argument"
        );
    }
    let literal = f
        .call(json!({"command":"grep","repo":"source","pattern":"needle","paths":[":(glob)**"]}))
        .await;
    assert_eq!(literal.exit_code, 0);
    assert!(literal.output["matches"].as_array().unwrap().is_empty());
    assert!(!target.exists());
}

#[tokio::test]
async fn binary_and_missing_revisions_have_distinct_errors() {
    let f = Fixture::new();
    for (args, kind) in [
        (
            json!({"command":"resolve","repo":"source","rev":"missing-ref"}),
            "unknown_revision",
        ),
        (
            json!({"command":"show_file","repo":"source","path":"absent.txt"}),
            "path_not_found",
        ),
        (
            json!({"command":"show_file","repo":"source","path":"binary.bin"}),
            "binary_file",
        ),
    ] {
        let response = f.call(args).await;
        assert_ne!(response.exit_code, 0);
        assert_eq!(
            response.output["error"]["kind"], kind,
            "{}",
            response.output
        );
        assert!(response.output.get("content").is_none());
    }
}

#[tokio::test]
async fn workspace_escape_and_external_alternates_are_rejected() {
    let f = Fixture::new();
    let outside = Fixture::new();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside.repo, f.workspace.path().join("escape")).unwrap();
        let response = f.call(json!({"command":"resolve","repo":"escape"})).await;
        assert_eq!(response.output["error"]["kind"], "repo_outside_workspace");
    }
    let parent_escape = format!(
        "../{}/source",
        outside
            .workspace
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
    );
    assert_eq!(
        f.call(json!({"command":"resolve","repo":parent_escape}))
            .await
            .output["error"]["kind"],
        "repo_outside_workspace"
    );
    fs::write(
        f.repo.join(".git/objects/info/alternates"),
        outside
            .repo
            .join(".git/objects")
            .to_string_lossy()
            .as_bytes(),
    )
    .unwrap();
    assert_eq!(
        f.call(json!({"command":"resolve","repo":"source"}))
            .await
            .output["error"]["kind"],
        "repo_outside_workspace"
    );
}

#[tokio::test]
async fn shallow_and_partial_missing_objects_never_contact_remote() {
    let f = Fixture::new();
    git(
        f.workspace.path(),
        &[
            "clone",
            "--depth=1",
            &format!("file://{}", f.repo.display()),
            "shallow",
        ],
    );
    let shallow = f.workspace.path().join("shallow");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/forbidden", listener.local_addr().unwrap());
    git(&shallow, &["remote", "set-url", "origin", &url]);
    let before = snapshot(&shallow);
    let response = f
        .call(json!({"command":"resolve","repo":"shallow","rev":"HEAD~1"}))
        .await;
    assert_eq!(
        response.output["error"]["kind"], "object_missing",
        "{}",
        response.output
    );
    assert_eq!(snapshot(&shallow), before);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );

    // A promised loose blob is absent. cat-file must not ask the promisor remote for it.
    let blob = git(&f.repo, &["rev-parse", "HEAD:renamed.txt"]);
    fs::remove_file(
        f.repo
            .join(format!(".git/objects/{}/{}", &blob[..2], &blob[2..])),
    )
    .unwrap();
    git(&f.repo, &["config", "remote.origin.promisor", "true"]);
    git(
        &f.repo,
        &["config", "remote.origin.partialclonefilter", "blob:none"],
    );
    git(&f.repo, &["config", "remote.origin.url", &url]);
    let before = snapshot(&f.repo);
    let response = f
        .call(json!({"command":"show_file","repo":"source","path":"renamed.txt"}))
        .await;
    assert_eq!(
        response.output["error"]["kind"], "object_missing",
        "{}",
        response.output
    );
    assert_eq!(snapshot(&f.repo), before);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn configured_caps_and_continuation_are_observable() {
    let f = Fixture::new();
    let limits = GitReadConfig {
        max_lines: 1,
        max_entries: 1,
        max_results: 1,
        max_commits: 1,
        max_patch_bytes: 8,
        ..Default::default()
    };
    for command in ["list_tree", "log", "worktree_status"] {
        if command == "worktree_status" {
            fs::write(f.repo.join("untracked-a"), "a").unwrap();
            fs::write(f.repo.join("untracked-b"), "b").unwrap();
        }
        let first = f
            .with_limits(json!({"command":command,"repo":"source"}), limits.clone())
            .await;
        assert_eq!(first.exit_code, 0, "{}", first.output);
        assert_eq!(first.output["truncated"], true);
        assert_eq!(first.output["next_offset"], 1);
        let second = f
            .with_limits(
                json!({"command":command,"repo":"source","offset":1}),
                limits.clone(),
            )
            .await;
        assert_ne!(first.output, second.output);
    }
    let patch = f
        .with_limits(
            json!({"command":"diff","repo":"source","a":f.first,"b":f.last,"mode":"patch"}),
            limits.clone(),
        )
        .await;
    assert_eq!(patch.exit_code, 0, "{}", patch.output);
    assert_eq!(patch.output["truncated"], true);
    assert!(patch.output["patch"].as_str().unwrap().len() <= 8);
    let first = f
        .with_limits(
            json!({"command":"show_file","repo":"source","path":"renamed.txt"}),
            limits.clone(),
        )
        .await;
    assert_eq!(first.output["content"], "one\n");
    assert_eq!(first.output["next_offset"], 1);
    let second = f
        .with_limits(
            json!({"command":"show_file","repo":"source","path":"renamed.txt","offset":1}),
            limits,
        )
        .await;
    assert_eq!(second.output["content"], "changed\n");
}

#[tokio::test]
async fn custom_cli_config_cannot_replace_the_read_only_tool() {
    let f = Fixture::new();
    let mut cfg = crate::config::Config::default();
    cfg.tools.insert(
        "git_read".into(),
        crate::config::ToolConfig {
            bin: "git".into(),
            source_pattern: None,
            manifest: None,
        },
    );
    let registry = crate::tool_runner::ToolRegistry::with_process_config(cfg.process.clone());
    crate::config::register_tools(&cfg, &registry, true).await;
    assert!(
        registry.get("git_read").await.is_none(),
        "the reserved identity must not register as a generic CLI tool"
    );
    crate::plugins::register_builtin_plugins(&cfg, &registry, true).await;
    let output = registry
        .run(
            "git_read",
            "resolve",
            f.workspace.path(),
            &HashMap::new(),
            None,
            Some(&json!({"repo":"source","rev":"HEAD"})),
        )
        .await
        .unwrap();
    assert_eq!(output.exit_code, 0, "{}", output.output);
    assert_eq!(output.output["commits"][0], f.last);
}

#[tokio::test]
async fn serialized_errors_respect_the_response_byte_cap() {
    let f = Fixture::new();
    let limits = GitReadConfig {
        max_output_bytes: 256,
        max_file_bytes: 128,
        max_patch_bytes: 128,
        ..Default::default()
    };
    let result = f
        .with_limits(
            json!({"command":"resolve","repo":"source","rev":"x".repeat(4000)}),
            limits,
        )
        .await;
    assert_ne!(result.exit_code, 0);
    assert!(
        result.output.to_string().len() <= 256,
        "{} bytes",
        result.output.to_string().len()
    );
    assert_eq!(result.output["error"]["kind"], "output_limit");
    assert_eq!(result.output["truncated"], true);
}

#[tokio::test]
async fn a_false_promisor_setting_does_not_report_a_partial_clone() {
    let f = Fixture::new();
    git(&f.repo, &["config", "remote.origin.promisor", "false"]);
    let result = f.call(json!({"command":"resolve","repo":"source"})).await;
    assert_eq!(result.exit_code, 0, "{}", result.output);
    assert_eq!(result.output["partial"], false);
}

#[cfg(unix)]
#[tokio::test]
async fn gitdir_pointer_file_cannot_escape_the_workspace() {
    let f = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    let store = f.workspace.path().join("git-storage");
    fs::rename(f.repo.join(".git"), &store).unwrap();
    let pointer = outside.path().join("pointer");
    fs::write(&pointer, format!("gitdir: {}\n", store.display())).unwrap();
    std::os::unix::fs::symlink(&pointer, f.repo.join(".git")).unwrap();
    let result = f.call(json!({"command":"resolve","repo":"source"})).await;
    assert_eq!(result.output["error"]["kind"], "repo_outside_workspace");
}

#[tokio::test]
async fn missing_tree_is_not_reported_as_an_absent_path() {
    let f = Fixture::new();
    let tree = git(&f.repo, &["rev-parse", "HEAD:nested"]);
    fs::remove_file(
        f.repo
            .join(format!(".git/objects/{}/{}", &tree[..2], &tree[2..])),
    )
    .unwrap();
    git(&f.repo, &["config", "remote.origin.promisor", "true"]);
    for args in [
        json!({"command":"list_tree","repo":"source","path":"nested"}),
        json!({"command":"show_file","repo":"source","path":"nested/child.txt"}),
    ] {
        let result = f.call(args).await;
        assert_eq!(
            result.output["error"]["kind"], "object_missing",
            "{}",
            result.output
        );
    }
}

#[tokio::test]
async fn bare_repository_rejects_worktree_status_without_mutation() {
    let f = Fixture::new();
    git(f.workspace.path(), &["clone", "--bare", "source", "bare"]);
    let bare = f.workspace.path().join("bare");
    let before = snapshot(&bare);
    let response = f
        .call(json!({"command":"worktree_status","repo":"bare"}))
        .await;
    assert_ne!(response.exit_code, 0, "{}", response.output);
    assert_eq!(response.output["error"]["kind"], "bare_repository");
    assert_eq!(snapshot(&bare), before);
}

#[tokio::test]
async fn bare_repository_supports_all_history_reads_without_mutation() {
    let f = Fixture::new();
    git(f.workspace.path(), &["clone", "--bare", "source", "bare"]);
    let bare = f.workspace.path().join("bare");
    for mut args in f.cases() {
        if args["command"] == "worktree_status" {
            continue;
        }
        args["repo"] = json!("bare");
        let before = snapshot(&bare);
        let response = f.call(args.clone()).await;
        assert_eq!(response.exit_code, 0, "{args}: {}", response.output);
        assert_eq!(snapshot(&bare), before, "mutation in {args}");
    }
}

#[tokio::test]
async fn malformed_nested_ranges_are_rejected_before_resolution() {
    let f = Fixture::new();
    for command in ["log", "diff"] {
        for rev in [
            "HEAD..HEAD..HEAD",
            "HEAD...HEAD",
            "..HEAD",
            "HEAD..",
            "HEAD..-p",
        ] {
            let before = snapshot(&f.repo);
            let result = f
                .call(json!({"command":command,"repo":"source","rev":rev}))
                .await;
            assert_eq!(
                result.output["error"]["kind"], "rejected_argument",
                "{command} {rev}: {}",
                result.output
            );
            assert_eq!(snapshot(&f.repo), before);
        }
    }
}

#[tokio::test]
async fn upstream_resolution_is_local_and_search_syntax_has_a_structured_result() {
    let f = Fixture::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    git(
        &f.repo,
        &["update-ref", "refs/remotes/origin/main", &f.first],
    );
    git(&f.repo, &["config", "branch.main.remote", "origin"]);
    git(
        &f.repo,
        &[
            "config",
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/origin/*",
        ],
    );
    git(&f.repo, &["config", "branch.main.merge", "refs/heads/main"]);
    git(
        &f.repo,
        &[
            "config",
            "remote.origin.url",
            &format!("http://{}/forbidden", listener.local_addr().unwrap()),
        ],
    );
    let before = snapshot(&f.repo);
    let upstream = f
        .call(json!({"command":"resolve","repo":"source","rev":"@{u}"}))
        .await;
    assert_eq!(upstream.exit_code, 0, "{}", upstream.output);
    assert_eq!(upstream.output["commits"], json!([f.first]));
    let unsupported = f
        .call(json!({"command":"resolve","repo":"source","rev":":/Initial"}))
        .await;
    assert_eq!(unsupported.output["error"]["kind"], "unknown_revision");
    assert_eq!(snapshot(&f.repo), before);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn grep_distinguishes_no_matches_from_an_invalid_expression() {
    let f = Fixture::new();
    let absent = f
        .call(json!({"command":"grep","repo":"source","pattern":"no matching text"}))
        .await;
    assert_eq!(absent.exit_code, 0, "{}", absent.output);
    assert_eq!(absent.output["matches"], json!([]));
    let invalid = f
        .call(json!({"command":"grep","repo":"source","pattern":"["}))
        .await;
    assert_ne!(invalid.exit_code, 0);
    assert_eq!(invalid.output["error"]["kind"], "git_error");
}

#[tokio::test]
async fn registered_plugin_uses_the_smaller_git_and_shared_response_caps() {
    let f = Fixture::new();
    // Raw capture fits both git caps. JSON escaping makes the successful response larger.
    fs::write(f.repo.join("quoted.txt"), "\"".repeat(4096)).unwrap();
    git(&f.repo, &["add", "."]);
    git(&f.repo, &["commit", "-m", "Escaped response fixture"]);
    for (git_cap, shared_cap) in [(16384, 1024), (5000, 16384)] {
        let mut cfg = crate::config::Config::default();
        cfg.git_read.max_output_bytes = git_cap;
        cfg.git_read.max_file_bytes = git_cap;
        cfg.git_read.max_patch_bytes = git_cap;
        cfg.tool_output.max_bytes = shared_cap;
        let registry = crate::tool_runner::ToolRegistry::with_process_config(cfg.process.clone());
        crate::plugins::register_builtin_plugins(&cfg, &registry, true).await;
        let before = snapshot(&f.repo);
        let response = registry
            .run(
                "git_read",
                "show_file",
                f.workspace.path(),
                &HashMap::new(),
                None,
                Some(&json!({"repo":"source","path":"quoted.txt"})),
            )
            .await
            .unwrap();
        assert_eq!(response.exit_code, 1, "{}", response.output);
        assert_eq!(response.output["error"]["kind"], "output_limit");
        assert!(response.output.to_string().len() <= git_cap.min(shared_cap));
        assert_eq!(snapshot(&f.repo), before);
    }
}

#[tokio::test]
async fn strict_pinning_refuses_a_branch_while_the_default_keeps_it() {
    let f = Fixture::new();
    let args = json!({"command":"resolve","repo":"source","rev":"main"});
    assert_eq!(f.call(args.clone()).await.exit_code, 0);
    let strict = GitReadConfig {
        strict_pinning: true,
        ..Default::default()
    };
    let response = f.with_limits(args, strict).await;
    assert_eq!(
        response.output["error"]["kind"], "rejected_argument",
        "{}",
        response.output
    );
}

#[tokio::test]
async fn strict_pinning_requires_commit_objects_not_annotated_tag_objects() {
    let f = Fixture::new();
    git(
        &f.repo,
        &[
            "tag",
            "-a",
            "policy-tag",
            "-m",
            "Tag object for strict policy",
        ],
    );
    let tag = git(&f.repo, &["rev-parse", "refs/tags/policy-tag"]);
    assert_eq!(
        f.call(json!({"command":"resolve","repo":"source","rev":tag}))
            .await
            .exit_code,
        0
    );
    let strict = GitReadConfig {
        strict_pinning: true,
        ..Default::default()
    };
    let response = f
        .with_limits(
            json!({"command":"resolve","repo":"source","rev":tag}),
            strict,
        )
        .await;
    assert_eq!(
        response.output["error"]["kind"], "rejected_argument",
        "{}",
        response.output
    );
}

impl Fixture {
    fn policy_cases(&self) -> Vec<(Value, GitReadConfig)> {
        let mut cases: Vec<_> = self
            .cases()
            .into_iter()
            .map(|args| (args, GitReadConfig::default()))
            .collect();
        for mut args in self.cases() {
            let command = args["command"].as_str().unwrap();
            if matches!(command, "refs" | "worktree_status") {
                continue;
            }
            if !matches!(command, "diff" | "ancestry")
                && args
                    .get("rev")
                    .and_then(Value::as_str)
                    .is_none_or(|rev| rev.len() != 40)
            {
                args["rev"] = json!(self.last);
            }
            cases.push((
                args,
                GitReadConfig {
                    strict_pinning: true,
                    ..Default::default()
                },
            ));
        }
        cases
    }
}

#[tokio::test]
async fn strict_pinning_rejects_all_unpinned_revision_forms_and_operations() {
    let f = Fixture::new();
    let strict = GitReadConfig {
        strict_pinning: true,
        ..Default::default()
    };
    for command in OPERATIONS {
        if matches!(*command, "refs" | "worktree_status") {
            let result = f
                .with_limits(
                    json!({"command":command,"repo":"source","rev":f.last}),
                    strict.clone(),
                )
                .await;
            assert_eq!(result.output["error"]["kind"], "rejected_argument");
            continue;
        }
        for revision in [
            None,
            Some("HEAD"),
            Some("HEAD~1"),
            Some("main"),
            Some("review"),
            Some("abcd123"),
            Some("HEAD..main"),
        ] {
            let mut args = json!({"command":command,"repo":"source","path":"renamed.txt"});
            if matches!(*command, "diff" | "ancestry") {
                args["b"] = json!(f.last);
                if let Some(revision) = revision {
                    args["a"] = json!(revision);
                }
            } else if let Some(revision) = revision {
                args["rev"] = json!(revision);
            }
            let result = f.with_limits(args.clone(), strict.clone()).await;
            assert_eq!(
                result.output["error"]["kind"], "rejected_argument",
                "{args}: {}",
                result.output
            );
        }
    }
}

#[tokio::test]
async fn commit_bodies_are_additive_and_do_not_change_default_log_fields() {
    let f = Fixture::new();
    git(
        &f.repo,
        &[
            "commit",
            "--allow-empty",
            "-m",
            "Body fixture",
            "-m",
            "First body line.\nSecond body line.",
        ],
    );
    let ordinary = f
        .call(json!({"command":"log","repo":"source","limit":1}))
        .await;
    assert!(ordinary.output["entries"][0].get("body").is_none());
    for command in ["log", "show_commit"] {
        let result = f
            .call(json!({"command":command,"repo":"source","limit":1,"include_body":true}))
            .await;
        assert_eq!(result.exit_code, 0, "{}", result.output);
        let commit = if command == "log" {
            &result.output["entries"][0]
        } else {
            &result.output["commit"]
        };
        assert_eq!(commit["body"], "First body line.\nSecond body line.\n");
        assert_eq!(commit["subject"], "Body fixture");
    }
}

#[tokio::test]
async fn default_status_reports_raw_bytes_without_running_normalizing_filters() {
    let f = Fixture::new();
    let filtered = f.repo.join("filtered.txt");
    fs::write(&filtered, "worktree content\n").unwrap();
    fs::write(
        f.repo.join(".gitattributes"),
        "filtered.txt filter=normalize\n",
    )
    .unwrap();
    git(
        &f.repo,
        &[
            "config",
            "filter.normalize.clean",
            "sed s/worktree/canonical/g",
        ],
    );
    git(&f.repo, &["config", "filter.normalize.required", "true"]);
    git(&f.repo, &["add", "filtered.txt", ".gitattributes"]);
    git(&f.repo, &["commit", "-m", "Normalizing filter fixture"]);
    assert_eq!(
        git(&f.repo, &["show", "HEAD:filtered.txt"]),
        "canonical content"
    );
    assert_eq!(fs::read_to_string(&filtered).unwrap(), "worktree content\n");
    // Force a content check without allowing ordinary Git to refresh the index.
    fs::File::open(&filtered)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(5)))
        .unwrap();
    let before = snapshot(&f.repo);
    assert_eq!(
        git(
            &f.repo,
            &["--no-optional-locks", "status", "--porcelain=v1"]
        ),
        ""
    );
    assert_eq!(snapshot(&f.repo), before);
    let result = f
        .call(json!({"command":"worktree_status","repo":"source"}))
        .await;
    assert_eq!(result.exit_code, 0, "{}", result.output);
    assert_eq!(result.output["clean"], false);
    assert!(result.output["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["path"] == "filtered.txt" && entry["status"] == " M"));
    assert_eq!(snapshot(&f.repo), before);
}

#[tokio::test]
async fn mixed_case_filter_configuration_is_suppressed_under_both_policies() {
    use std::io::Write;
    let f = Fixture::new();
    let marker = f.workspace.path().join("mixed-filter-executed");
    let command = format!("printf executed > '{}'", marker.display());
    let mut config = fs::OpenOptions::new()
        .append(true)
        .open(f.repo.join(".git/config"))
        .unwrap();
    writeln!(
        config,
        "[FILTER \"MiXeD\"]\nClEaN = {command}\nPrOcEsS = {command}\nReQuIrEd = true"
    )
    .unwrap();
    drop(config);
    fs::write(f.repo.join(".gitattributes"), "*.txt filter=MiXeD\n").unwrap();
    let content = fs::read(f.repo.join("renamed.txt")).unwrap();
    fs::write(f.repo.join("renamed.txt"), vec![b'X'; content.len()]).unwrap();
    let keys = git(
        &f.repo,
        &["config", "--name-only", "--get-regexp", "^filter\\."],
    );
    assert!(keys.contains("filter.MiXeD.clean"), "{keys}");
    assert!(keys.contains("filter.MiXeD.process"), "{keys}");
    let before = snapshot(&f.repo);
    for (args, limits) in f.policy_cases() {
        let result = f.with_limits(args.clone(), limits).await;
        assert_eq!(result.exit_code, 0, "{args}: {}", result.output);
        assert!(!marker.exists(), "a mixed-case filter executed");
    }
    assert_eq!(snapshot(&f.repo), before);
}

#[tokio::test]
async fn strict_pair_operations_reject_annotated_tags_on_both_sides() {
    let f = Fixture::new();
    git(&f.repo, &["tag", "-a", "pair-tag", "-m", "Pair input tag"]);
    let tag = git(&f.repo, &["rev-parse", "refs/tags/pair-tag"]);
    let strict = GitReadConfig {
        strict_pinning: true,
        ..Default::default()
    };
    let before = snapshot(&f.repo);
    for command in ["diff", "ancestry"] {
        for (a, b) in [(&tag, &f.last), (&f.first, &tag)] {
            let args = json!({"command":command,"repo":"source","a":a,"b":b});
            let ordinary = f.call(args.clone()).await;
            assert_eq!(ordinary.exit_code, 0, "{}", ordinary.output);
            let pinned = f.with_limits(args.clone(), strict.clone()).await;
            assert_eq!(
                pinned.output["error"]["kind"], "rejected_argument",
                "{args}: {}",
                pinned.output
            );
        }
    }
    assert_eq!(snapshot(&f.repo), before);
}

#[tokio::test]
async fn malformed_filter_configuration_fails_structurally_without_mutation() {
    use std::io::Write;
    let f = Fixture::new();
    let mut config = fs::OpenOptions::new()
        .append(true)
        .open(f.repo.join(".git/config"))
        .unwrap();
    writeln!(config, "[unterminated").unwrap();
    drop(config);
    let before = snapshot(&f.repo);
    for args in [
        json!({"command":"worktree_status","repo":"source"}),
        json!({"command":"show_file","repo":"source","rev":f.first,"path":"old.txt"}),
    ] {
        let result = f.call(args).await;
        assert_ne!(result.exit_code, 0);
        assert_eq!(
            result.output["error"]["kind"], "git_error",
            "{}",
            result.output
        );
        assert!(result.output["error"]["message"]
            .as_str()
            .unwrap()
            .contains("config"));
    }
    assert_eq!(snapshot(&f.repo), before);
}

#[tokio::test]
async fn private_status_metadata_is_bounded_and_removed_after_failure() {
    let f = Fixture::new();
    let state = tempfile::tempdir().unwrap();
    let limits = GitReadConfig {
        status_directory: Some(state.path().to_string_lossy().into_owned()),
        max_status_metadata_bytes: 1,
        ..Default::default()
    };
    let before = snapshot(&f.repo);
    let result = f
        .with_limits(json!({"command":"worktree_status","repo":"source"}), limits)
        .await;
    assert_eq!(
        result.output["error"]["kind"], "output_limit",
        "{}",
        result.output
    );
    assert_eq!(fs::read_dir(state.path()).unwrap().count(), 0);
    assert_eq!(snapshot(&f.repo), before);
    let unsafe_directory = GitReadConfig {
        status_directory: Some(f.repo.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let refused = f
        .with_limits(
            json!({"command":"worktree_status","repo":"source"}),
            unsafe_directory,
        )
        .await;
    assert_eq!(refused.output["error"]["kind"], "rejected_argument");
    assert_eq!(snapshot(&f.repo), before);
}

#[tokio::test]
async fn frozen_status_preserves_builtin_normalization_split_index_and_local_excludes() {
    let f = Fixture::new();
    git(&f.repo, &["config", "core.autocrlf", "true"]);
    fs::write(f.repo.join(".git/info/attributes"), "eol.txt text\n").unwrap();
    fs::write(f.repo.join("eol.txt"), "first\r\nsecond\r\n").unwrap();
    git(&f.repo, &["add", "eol.txt"]);
    git(&f.repo, &["commit", "-m", "Built-in normalization fixture"]);
    fs::File::open(f.repo.join("eol.txt"))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(5)))
        .unwrap();
    fs::write(f.repo.join(".git/info/exclude"), "ignored.local\n").unwrap();
    fs::write(f.repo.join("ignored.local"), "ignored\n").unwrap();
    git(&f.repo, &["update-index", "--split-index"]);
    assert_eq!(
        git(
            &f.repo,
            &["--no-optional-locks", "status", "--porcelain=v1"]
        ),
        ""
    );
    let before = snapshot(&f.repo);
    let result = f
        .call(json!({"command":"worktree_status","repo":"source"}))
        .await;
    assert_eq!(result.exit_code, 0, "{}", result.output);
    assert_eq!(result.output["clean"], true, "{}", result.output);
    assert_eq!(snapshot(&f.repo), before);
}

#[tokio::test]
async fn frozen_status_handles_linked_worktrees_reftable_and_sha256() {
    let f = Fixture::new();
    git(
        &f.repo,
        &["worktree", "add", "--detach", "../linked", &f.last],
    );
    git(f.workspace.path(), &["clone", "source", "reftable"]);
    git(
        &f.workspace.path().join("reftable"),
        &["refs", "migrate", "--ref-format=reftable"],
    );
    git(
        f.workspace.path(),
        &["init", "--object-format=sha256", "sha256"],
    );
    let sha256 = f.workspace.path().join("sha256");
    git(&sha256, &["config", "user.name", "Format Fixture"]);
    git(&sha256, &["config", "user.email", "format@example.test"]);
    git(&sha256, &["config", "commit.gpgsign", "false"]);
    fs::write(sha256.join("file.txt"), "format fixture\n").unwrap();
    git(&sha256, &["add", "."]);
    git(&sha256, &["commit", "-m", "Format fixture"]);
    let before = snapshot(f.workspace.path());
    for repo in ["linked", "reftable", "sha256"] {
        let result = f
            .call(json!({"command":"worktree_status","repo":repo}))
            .await;
        assert_eq!(result.exit_code, 0, "{repo}: {}", result.output);
        assert_eq!(result.output["clean"], true, "{repo}: {}", result.output);
        assert_eq!(
            result.output["commits"][0].as_str().unwrap().len(),
            if repo == "sha256" { 64 } else { 40 }
        );
    }
    assert_eq!(snapshot(f.workspace.path()), before);
}

#[tokio::test]
async fn strict_uppercase_hash_inputs_return_canonical_lowercase_commits() {
    let f = Fixture::new();
    let strict = GitReadConfig {
        strict_pinning: true,
        ..Default::default()
    };
    let result = f
        .with_limits(
            json!({"command":"resolve","repo":"source","rev":f.last.to_ascii_uppercase()}),
            strict,
        )
        .await;
    assert_eq!(result.exit_code, 0, "{}", result.output);
    assert_eq!(result.output["commits"], json!([f.last]));
}

#[tokio::test]
async fn frozen_status_preserves_racy_index_content_checks() {
    let f = Fixture::new();
    let file = f.repo.join("renamed.txt");
    let index = f.repo.join(".git/index");
    let stamp = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
    git(&f.repo, &["config", "core.trustctime", "false"]);
    fs::File::open(&file)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(stamp))
        .unwrap();
    git(
        &f.repo,
        &["update-index", "--really-refresh", "--", "renamed.txt"],
    );
    let previous = fs::read(&file).unwrap();
    fs::write(&file, vec![b'X'; previous.len()]).unwrap();
    fs::File::open(&file)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(stamp))
        .unwrap();
    fs::File::open(&index)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(stamp))
        .unwrap();
    assert_eq!(
        git(
            &f.repo,
            &["--no-optional-locks", "status", "--porcelain=v1"]
        ),
        "M renamed.txt"
    );
    let before = snapshot(&f.repo);
    let result = f
        .call(json!({"command":"worktree_status","repo":"source"}))
        .await;
    assert_eq!(result.exit_code, 0, "{}", result.output);
    assert_eq!(result.output["clean"], false, "{}", result.output);
    assert!(result.output["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["path"] == "renamed.txt"));
    assert_eq!(snapshot(&f.repo), before);
}

#[tokio::test]
async fn frozen_status_handles_a_linked_split_index_with_an_upstream() {
    let f = Fixture::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    git(
        &f.repo,
        &["update-ref", "refs/remotes/origin/main", &f.first],
    );
    git(
        &f.repo,
        &[
            "config",
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/origin/*",
        ],
    );
    git(
        &f.repo,
        &[
            "config",
            "remote.origin.url",
            &format!("http://{}/forbidden", listener.local_addr().unwrap()),
        ],
    );
    git(
        &f.repo,
        &[
            "worktree",
            "add",
            "-b",
            "linked-split",
            "../linked-split",
            &f.last,
        ],
    );
    let linked = f.workspace.path().join("linked-split");
    git(&linked, &["branch", "--set-upstream-to=origin/main"]);
    git(&linked, &["update-index", "--split-index"]);
    assert_eq!(git(&linked, &["rev-parse", "@{upstream}"]), f.first);
    let gitdir = PathBuf::from(git(&linked, &["rev-parse", "--absolute-git-dir"]))
        .canonicalize()
        .unwrap();
    let shared = linked
        .join(git(&linked, &["rev-parse", "--shared-index-path"]))
        .canonicalize()
        .unwrap();
    assert_eq!(shared.parent(), Some(gitdir.as_path()));
    fs::write(linked.join("note.txt"), "untracked note\n").unwrap();
    assert_eq!(
        git(
            &linked,
            &["--no-optional-locks", "status", "--porcelain=v1"]
        ),
        "?? note.txt"
    );
    let before = snapshot(f.workspace.path());
    let result = f
        .call(json!({"command":"worktree_status","repo":"linked-split"}))
        .await;
    assert_eq!(result.exit_code, 0, "{}", result.output);
    assert_eq!(
        result.output["entries"],
        json!([{"status":"??","path":"note.txt"}])
    );
    assert_eq!(snapshot(f.workspace.path()), before);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn frozen_status_respects_alternate_object_store_confinement() {
    let f = Fixture::new();
    git(
        f.workspace.path(),
        &["clone", "--shared", "source", "shared"],
    );
    let before = snapshot(f.workspace.path());
    let allowed = f
        .call(json!({"command":"worktree_status","repo":"shared"}))
        .await;
    assert_eq!(allowed.exit_code, 0, "{}", allowed.output);
    assert_eq!(allowed.output["clean"], true);
    assert_eq!(snapshot(f.workspace.path()), before);
    let outside = tempfile::tempdir().unwrap();
    let external = outside.path().join("objects");
    fs::create_dir(&external).unwrap();
    fs::write(
        f.workspace
            .path()
            .join("shared/.git/objects/info/alternates"),
        format!("{}\n", external.display()),
    )
    .unwrap();
    let changed = snapshot(f.workspace.path());
    let refused = f
        .call(json!({"command":"worktree_status","repo":"shared"}))
        .await;
    assert_eq!(refused.output["error"]["kind"], "repo_outside_workspace");
    assert_eq!(snapshot(f.workspace.path()), changed);
}

#[tokio::test]
async fn frozen_status_preserves_quoted_path_configuration_values() {
    let f = Fixture::new();
    let exclude = f
        .workspace
        .path()
        .join("exclude-\"-backslash\\-line\n-tab\t-return\r");
    fs::write(&exclude, "ignored.local\n").unwrap();
    git(
        &f.repo,
        &["config", "core.excludesFile", exclude.to_str().unwrap()],
    );
    fs::write(f.repo.join("ignored.local"), "ignored\n").unwrap();
    assert_eq!(
        git(
            &f.repo,
            &["--no-optional-locks", "status", "--porcelain=v1"]
        ),
        ""
    );
    let before = snapshot(f.workspace.path());
    let result = f
        .call(json!({"command":"worktree_status","repo":"source"}))
        .await;
    assert_eq!(result.exit_code, 0, "{}", result.output);
    assert_eq!(result.output["clean"], true, "{}", result.output);
    assert_eq!(snapshot(f.workspace.path()), before);
}

#[cfg(unix)]
#[tokio::test]
async fn frozen_status_refuses_fifo_metadata_without_waiting_for_a_writer() {
    let f = Fixture::new();
    let state = tempfile::tempdir().unwrap();
    let index = f.repo.join(".git/index");
    fs::remove_file(&index).unwrap();
    assert!(Command::new("mkfifo")
        .arg(&index)
        .status()
        .unwrap()
        .success());
    let limits = GitReadConfig {
        status_directory: Some(state.path().to_string_lossy().into_owned()),
        ..Default::default()
    };
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        f.with_limits(json!({"command":"worktree_status","repo":"source"}), limits),
    )
    .await
    .expect("FIFO metadata must not block");
    assert_eq!(
        result.output["error"]["kind"], "invalid_repository",
        "{}",
        result.output
    );
    assert_eq!(fs::read_dir(state.path()).unwrap().count(), 0);
}
