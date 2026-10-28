# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="dbunk-fr06-1576" -->

<!-- event id="request" artifact path="1576-dbunk-git-pinning/artifacts/round-1-review-request.diff" sha256="932fc1eec0b566eae60bde23139eba37f9b9285817a37f4fa5be19cf431ce95e" -->
## Review Request — Round 1
**Task:** 1576 — dbunk FR-06: strict commit pinning and authorized filter suppression
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Review only the FR-06 slice for Vikunja #1576, under parent #1575. The full supplied diff is relative to fabb08c and includes the checkpoint plus current changes. Existing git_read names and defaults remain, except the human explicitly approved suppressing repository-configured external filters in both modes. This closes reproduced clean/process execution in default worktree_status. It intentionally changes filtered-repository status semantics and is documented. New git_read.strict_pinning defaults false; when true every active commit input is an explicit full 40-character SHA-1 commit hash. Names, tags, short hashes, relative refs, ranges, implicit HEAD, annotated-tag object IDs, refs, and worktree_status are refused. Diff and ancestry require a/b, others explicit rev. include_body defaults false and adds body metadata only on log and show_commit. No raw flags or new crate. Preserve no mutation, no network/lazy fetch, and workspace/object-store confinement. Tests cover both policies, malicious hooks/fsmonitor/diff/textconv/filters/aliases, full snapshots, and actual EROFS read-only bind mount for all nine pinned operations. Fresh full results: Rust 2004 passed, 10 ignored; Python 384 passed, 1 skipped; fmt and clippy passed. New normalizing-filter regression proves ordinary Git reports clean while git_read reports modified with unchanged snapshots. ADR-018 and ADR-019 remain Proposed; product approval is not claimed as architectural review. Do not treat this slice as completed cross-runtime FR-02/FR-03: those integration tasks remain pending and will receive separate review. The existing git tool is unchanged. No publication or merge is authorized.

### Relevant Code / Diff
diff --git c/CONTEXT-MAP.md w/CONTEXT-MAP.md
index e2585b1..c0431e8 100644
--- c/CONTEXT-MAP.md
+++ w/CONTEXT-MAP.md
@@ -1,10 +1,10 @@
 # Context map
 
-Date: 2026-10-26
+Date: 2026-10-28
 
 | Context | Glossary | Decisions |
 |---------|----------|-----------|
-| Pinned git reads | [Git reads](docs/contexts/git-read/CONTEXT.md) | [ADR-017](docs/adr/017-git-read-tool.md) |
+| Pinned git reads | [Git reads](docs/contexts/git-read/CONTEXT.md) | [ADR-017](docs/adr/017-git-read-tool.md), [Proposed ADR-018](docs/adr/018-strict-git-pinning.md), [Proposed ADR-019](docs/adr/019-ignore-repository-filters.md) |
 
 This map covers the git-read context. Other contexts are not mapped here yet.
 
diff --git c/daimonos.default.toml w/daimonos.default.toml
index 7de95d9..76f9f6d 100644
--- c/daimonos.default.toml
+++ w/daimonos.default.toml
@@ -607,6 +607,8 @@ poll_interval_ms = 1500
 
 # Pinned, local git reads. See ADR-017 and GitReadConfig in src/config.rs.
 [git_read]
+# Require explicit full SHA-1 commit identifiers. Reject names, ranges, and unpinned operations.
+strict_pinning = false
 # Maximum captured bytes per stream and maximum serialized response bytes.
 max_output_bytes = 1048576
 # Reject larger blobs before reading their content.
diff --git c/docs/adr/018-strict-git-pinning.md w/docs/adr/018-strict-git-pinning.md
new file mode 100644
index 0000000..4550b43
--- /dev/null
+++ w/docs/adr/018-strict-git-pinning.md
@@ -0,0 +1,75 @@
+# ADR-018: Strict git pinning is an opt-in policy
+
+- **Status:** Proposed
+- **Date:** 2026-10-27
+- **Deciders:** Pending architectural review. The human approved the product requirements.
+- **Consulted:** dbunk PM, through the round-1 written answers
+- **Informed:** None recorded
+- **Spec:** `daimonos-feature-requests-for-dbunk.md`, FR-06, and `daimonos-pm-answers-round-1.md`
+- **Tracking:** Vikunja #1576, parent #1575
+- **Supersedes:** None
+- **Superseded by:** None
+
+## Context
+
+ADR-017 permits local revision resolution and returns full commit identifiers.
+The dbunk reviewer instead needs caller-pinned input identifiers before any git read.
+The PM accepted an additive policy and rejected a global revision-contract change.
+How can dbunk require pinned inputs without breaking existing callers?
+
+## Decision drivers
+
+- Existing calls, defaults, and result shapes remain unchanged except for the approved filter security fix in ADR-019.
+- Every strict read must identify its input commits explicitly.
+- Output pinning cannot replace input validation.
+- The existing process hardening must remain active under both policies.
+
+## Decision
+
+We will add `git_read.strict_pinning`, default false.
+When true, operations require explicit full 40-character hexadecimal commit identifiers.
+`diff` and `ancestry` require both `a` and `b` in that form.
+Other pinned operations require `rev`. Implicit HEAD, ranges, relative revisions, and short hashes are rejected.
+`refs` and `worktree_status` are refused because they cannot supply a pinned view.
+`resolve` remains useful only for verifying an explicit full commit identifier.
+The existing runner still resolves and checks the object type locally.
+This policy does not weaken confinement, no-mutation, or no-network controls.
+
+We will add `include_body`, default false, for `log` and `show_commit`.
+The field `body` appears in commit metadata only when the caller requests it.
+Existing names and result fields remain unchanged by default.
+
+## Alternatives
+
+- Globally reject revision names. This breaks ADR-017 and existing callers.
+- Accept names and only pin outputs. A moving ref can change before the request starts.
+- Create a second git tool. This duplicates the operation surface and process hardening.
+
+## Consequences
+
+The caller must explicitly select strict pinning. It is not a complete reviewer sandbox.
+Forty-character hashes restrict the strict profile to SHA-1 repositories.
+The default policy still supports other locally resolvable git object formats.
+Commit bodies can increase output size, so existing response caps still apply.
+
+## Confirmation
+
+- Test every strict operation through the public plugin interface.
+- Reject missing hashes, names, tags, short hashes, ranges, and unpinned operations.
+- Prove names still work with the default policy.
+- Compare repository snapshots before and after strict calls and hostile-config tests.
+- Exercise strict reads on a real read-only mount. Record unavailable platform facilities explicitly.
+- Verify `include_body` through Rust and MCP without changing default metadata fields.
+- Run `cargo test` and `python3 -m pytest tests/ -v`.
+
+## Anchors
+
+_Anchors:_ src/plugins/git_read.rs::GitReadPlugin, src/plugins/git_read.rs::Arguments, src/plugins/git_read.rs::history, src/plugins/git_read/runner.rs::ReadRepo, src/config.rs::GitReadConfig
+
+## Revisit when
+
+The caller needs strict SHA-256 repositories or a versioned pinned-only tool contract.
+
+## Open questions
+
+No product acceptance blocker remains for this slice. Architectural review is pending.
diff --git c/docs/adr/019-ignore-repository-filters.md w/docs/adr/019-ignore-repository-filters.md
new file mode 100644
index 0000000..009a92f
--- /dev/null
+++ w/docs/adr/019-ignore-repository-filters.md
@@ -0,0 +1,69 @@
+# ADR-019: Ignore repository-configured filters during git reads
+
+- **Status:** Proposed
+- **Date:** 2026-10-28
+- **Deciders:** The human approved option 2 of Q-FR-06-2.1 on 2026-10-27. This record awaits architectural review.
+- **Consulted:** dbunk PM, through the written requirements.
+- **Informed:** None recorded.
+- **Spec:** FR-06, PM round 1, and `docs/reviews/2026-10-27-dbunk-follow-up.md`.
+- **Tracking:** Vikunja #1576, under #1575.
+- **Supersedes:** None. This clarifies the security guarantee in ADR-017.
+- **Superseded by:** None.
+
+## Context
+
+Git status can execute a configured clean or process filter while checking dirty files.
+The expanded hostile-configuration test reproduced that execution through git_read.
+Disabling filters also changes status results for legitimate filtered repositories.
+The human explicitly approved that compatibility exception instead of retaining the execution risk.
+Should filter suppression apply in both policies, despite the changed status semantics?
+
+## Decision drivers
+
+- A read-only tool must not execute repository-configured programs.
+- Strict pinning alone does not protect existing callers of worktree_status.
+- The compatibility change must remain explicit and tested.
+
+## Decision
+
+We will suppress configured clean, smudge, and process filters in both pinning policies.
+The runner reads driver keys without executing filters, bounds that metadata, and overrides the keys per process.
+Required filters become optional. Unsupported keys or incomplete metadata cause a structured failure before the read.
+No repository configuration file changes.
+The worktree result excludes external filter transformations. Built-in Git conversions can still apply.
+The user-facing documentation will warn that a filtered file can appear modified where ordinary Git reports clean.
+
+## Alternatives
+
+- Retain filter execution in default mode. Rejected by the human because the read-only tool would remain an execution surface.
+- Suppress filters only under strict pinning. This protects the dbunk policy but not ordinary worktree-status callers.
+- Execute only familiar filters. Rejected because repository configuration selects executable commands, not trustworthy implementations.
+
+## Consequences
+
+The fix changes default status semantics for repositories such as Git LFS repositories.
+This is the narrow compatibility exception approved as option 2, not permission for unrelated default changes.
+The existing git tool remains unchanged.
+The metadata budget can reject repositories with excessive filter configuration rather than apply incomplete suppression.
+
+## Confirmation
+
+- Run `cargo test default_status_reports_raw_bytes_without_running_normalizing_filters`.
+- In that fixture, ordinary Git reports clean after a real filter transformation.
+- Verify that git_read instead reports the tracked file as modified and leaves the full repository snapshot unchanged.
+- Run `cargo test every_operation_ignores_malicious_config` under both policies.
+- Run `cargo test plugins::git_read::tests` and `python3 -m pytest tests/test_git_read.py -v`.
+- Run the full Rust and Python suites before completing this slice.
+
+## Anchors
+
+_Anchors:_ src/plugins/git_read/runner.rs::ReadRepo, src/plugins/git_read/tests.rs::default_status_reports_raw_bytes_without_running_normalizing_filters, src/plugins/git_read/tests.rs::every_operation_ignores_malicious_config
+
+## Revisit when
+
+Git provides a way to inspect filter-normalized status without executing repository-controlled programs.
+Also revisit this decision if the caller needs explicitly trusted filter execution in a different tool.
+
+## Open questions
+
+The human resolved the product choice. Architectural review of this record remains pending.
diff --git c/docs/configuration.md w/docs/configuration.md
index 8072776..8ecaa70 100644
--- c/docs/configuration.md
+++ w/docs/configuration.md
@@ -1051,6 +1051,25 @@ raw model responses; the UUID is local, not a provider request ID.
 
 ## Read-only git (`git_read`)
 
+`git_read.strict_pinning` is false by default. Enable it to require explicit
+40-character SHA-1 commit identifiers. `diff` and `ancestry` then require `a` and
+`b`. Other permitted operations require `rev`. Names, ranges, implicit HEAD,
+short hashes, and annotated-tag object identifiers are rejected. `refs` and
+`worktree_status` are refused under this policy. Default revision resolution remains unchanged.
+
+`log` and `show_commit` accept `include_body=true`. Only that opt-in adds the
+`body` field to commit metadata. The existing response cap still applies.
+
+The runner disables configured clean, smudge, and process filters in both modes.
+This is an approved security-fix exception to the unchanged-default rule.
+`worktree_status` does not apply those external transformations. It can report a
+modified file where ordinary Git reports a clean filtered file, including Git LFS cases.
+Do not interpret this result as filter-aware status. Built-in Git conversions can still apply.
+[ADR-019](adr/019-ignore-repository-filters.md) records the reason and the human approval.
+Unsupported filter keys or oversized configuration fail closed before reads.
+Strict pinning is not required for process hardening. It adds immutable input selection.
+
+
 The `[git_read]` section bounds local, pinned git reads. `GitReadConfig` in
 `src/config.rs` defines the defaults. `daimonos.default.toml` lists every key.
 ADR-017 records the safety boundary.
diff --git c/docs/contexts/git-read/CONTEXT.md w/docs/contexts/git-read/CONTEXT.md
index b0080cd..11ef21c 100644
--- c/docs/contexts/git-read/CONTEXT.md
+++ w/docs/contexts/git-read/CONTEXT.md
@@ -1,14 +1,16 @@
 # Pinned git reads
 
-Date: 2026-10-27
+Date: 2026-10-28
 
 - **Workspace:** The fixed root that contains repositories. The session cwd does not change this boundary.
 - **Pinned commit:** A full commit SHA returned with a read result. It makes a citation independent of moving refs.
+- **Strict pinning:** The optional policy that also requires explicit SHA-1 commit identifiers as inputs. It rejects mutable names and tag objects.
 - **Promisor:** A partial-clone remote that holds objects absent locally. `git_read` never asks it for objects.
 - **Alternate store:** Another git object directory. Chained stores must stay inside the workspace.
 - **Patch preview:** Bounded patch text. A truncated preview is not a complete patch.
 - **Pointer file:** Git metadata that selects a git directory or common directory. It must stay inside the workspace.
 - **Bare repository:** A git database without a working tree. History reads are valid, but worktree status is not.
+- **Filter-free status:** Status without repository-configured external filter transformations. A filtered file can appear modified even when ordinary Git reports clean.
 
 [ADR-017](../../adr/017-git-read-tool.md) records why the tool is separate from `git`.
 [Configuration](../../configuration.md#read-only-git-git_read) names its limits.
diff --git c/prompts/tool_descriptions.toml w/prompts/tool_descriptions.toml
index 6e44a35..6bf6e5b 100644
--- c/prompts/tool_descriptions.toml
+++ w/prompts/tool_descriptions.toml
@@ -198,12 +198,12 @@ limit = "Max reservations to return (most-recent first)."
 full = """Python-style function signatures for all tool bindings available in execute_script. Already included in server instructions."""
 
 [git_read]
-full = """Read local git history at pinned commits without writes or network access. Use git_read for reviewer reads. Use git for writes. Returns JSON with the repo and full commit SHAs. Use workspace-relative repo and path values. Revisions, paths, repo names, and patterns must not start with a hyphen. Requires git with --no-lazy-fetch support. Caps come from [git_read] config. Oversized metadata returns output_limit. Binary files return metadata only."""
+full = """Read local git history at pinned commits without writes or network access. Use git_read for reviewer reads. Use git for writes. Returns JSON with the repo and full commit SHAs. Use workspace-relative repo and path values. Revisions, paths, repo names, and patterns must not start with a hyphen. Requires git with --no-lazy-fetch support. Caps come from [git_read] config. Oversized metadata returns output_limit. Binary files return metadata only. When strict pinning is enabled, supply full SHA-1 commit hashes. Names, ranges, and unpinned operations are refused."""
 terse = "Read-only git at pinned commits. No writes or network."
 [git_read.parameters]
 command = "Operation to run. No raw commands or user flags."
 repo = "Repository root relative to the workspace. Default: workspace root."
-rev = "Commit, ref, tag, or log range. Default: HEAD."
+rev = "Commit, ref, tag, or log range. Default: HEAD. Strict pinning requires an explicit full commit hash."
 path = "Literal repository-relative path."
 paths = "Literal repository-relative paths."
 pattern = "Extended regular expression for grep. Must not start with a hyphen."
@@ -215,6 +215,7 @@ limit = "Maximum returned items, bounded by configuration."
 recursive = "list_tree: include descendants. Default: false."
 follow = "log: follow renames for one path. Default: false."
 patch = "show_commit: include a bounded patch preview. Default: false."
+include_body = "log and show_commit: include the commit body. Default: false."
 start_line = "blame: first line, one-based. Default: 1."
 end_line = "blame: final line, inclusive. Bounded by max_lines."
 
diff --git c/src/config.rs w/src/config.rs
index d0c0720..bb7b020 100644
--- c/src/config.rs
+++ w/src/config.rs
@@ -11,6 +11,7 @@ use std::sync::Arc;
 #[derive(Debug, Deserialize, Clone)]
 #[serde(default)]
 pub struct GitReadConfig {
+    pub strict_pinning: bool,
     pub max_output_bytes: usize,
     pub max_file_bytes: usize,
     pub max_lines: usize,
@@ -25,6 +26,7 @@ pub struct GitReadConfig {
 impl Default for GitReadConfig {
     fn default() -> Self {
         Self {
+            strict_pinning: false,
             max_output_bytes: 1024 * 1024,
             max_file_bytes: 512 * 1024,
             max_lines: 1000,
diff --git c/src/plugins/git_read.rs w/src/plugins/git_read.rs
index 3bf1341..23e9ad7 100644
--- c/src/plugins/git_read.rs
+++ w/src/plugins/git_read.rs
@@ -99,6 +99,7 @@ struct Arguments {
     recursive: bool,
     follow: bool,
     patch: bool,
+    include_body: bool,
     start_line: Option<usize>,
     end_line: Option<usize>,
 }
@@ -130,6 +131,9 @@ impl ToolPlugin for GitReadPlugin {
             let args: Arguments = serde_json::from_value(input)
                 .map_err(|e| Failure::new("rejected_argument", e.to_string()))?;
             args.validate(command)?;
+            if self.limits.strict_pinning {
+                args.validate_pinning(command)?;
+            }
             let repo = ReadRepo::open(
                 workspace,
                 args.repo.as_deref().unwrap_or("."),
@@ -211,6 +215,39 @@ impl Arguments {
         }
         Ok(())
     }
+    fn validate_pinning(&self, command: &str) -> Result<(), Failure> {
+        // ADR-018: strict inputs are commit identifiers, not names resolved after the call starts.
+        if matches!(command, "refs" | "worktree_status") {
+            return Err(Failure::new(
+                "rejected_argument",
+                "strict pinning refuses unpinned operations",
+            ));
+        }
+        let full_commit =
+            |value: &str| value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
+        for value in [&self.rev, &self.a, &self.b].into_iter().flatten() {
+            if !full_commit(value) {
+                return Err(Failure::new(
+                    "rejected_argument",
+                    "strict pinning requires full 40-character commit hashes",
+                ));
+            }
+        }
+        if matches!(command, "diff" | "ancestry") {
+            if self.a.is_none() || self.b.is_none() || self.rev.is_some() {
+                return Err(Failure::new(
+                    "rejected_argument",
+                    "strict pinning requires explicit a and b hashes, without rev",
+                ));
+            }
+        } else if self.rev.is_none() {
+            return Err(Failure::new(
+                "rejected_argument",
+                "strict pinning requires an explicit rev hash",
+            ));
+        }
+        Ok(())
+    }
     fn paths(&self) -> Vec<String> {
         let mut paths = self.paths.clone().unwrap_or_default();
         if let Some(path) = &self.path {
@@ -448,6 +485,7 @@ impl GitReadPlugin {
                     &commit,
                     &Arguments {
                         limit: Some(1),
+                        include_body: args.include_body,
                         ..Default::default()
                     },
                     1,
@@ -721,6 +759,7 @@ pub fn schema() -> Value {
         "a":{"type":"string"}, "b":{"type":"string"}, "mode":{"type":"string","enum":["stat","patch"]},
         "offset":{"type":"integer","minimum":0}, "limit":{"type":"integer","minimum":1},
         "recursive":{"type":"boolean"}, "follow":{"type":"boolean"}, "patch":{"type":"boolean"},
+        "include_body":{"type":"boolean"},
         "start_line":{"type":"integer","minimum":1}, "end_line":{"type":"integer","minimum":1}
     }})
 }
@@ -758,6 +797,11 @@ async fn history(
 ) -> Result<Vec<Value>, Failure> {
     let limit = format!("--max-count={}", args.limit(max_commits).saturating_add(1));
     let skip = format!("--skip={}", args.offset);
+    let format = if args.include_body {
+        "--format=%H%x00%an%x00%aI%x00%cI%x00%s%x00%b"
+    } else {
+        "--format=%H%x00%an%x00%aI%x00%cI%x00%s"
+    };
     let mut argv = vec![
         "log",
         "--no-ext-diff",
@@ -765,7 +809,7 @@ async fn history(
         "--no-show-signature",
         "--no-use-mailmap",
         "--encoding=UTF-8",
-        "--format=%H%x00%an%x00%aI%x00%cI%x00%s",
+        format,
         "-z",
         &limit,
         &skip,
@@ -785,11 +829,16 @@ async fn history(
     let out = repo.text(&argv).await?;
     let fields: Vec<_> = out.split_terminator('\0').collect();
     let mut entries = Vec::new();
-    for record in fields.chunks(5) {
-        if record.len() != 5 {
+    let width = if args.include_body { 6 } else { 5 };
+    for record in fields.chunks(width) {
+        if record.len() != width {
             return Err(Failure::new("git_error", "invalid log record"));
         }
-        entries.push(json!({"sha":record[0],"author":record[1],"author_date":record[2],"committer_date":record[3],"subject":record[4]}));
+        let mut entry = json!({"sha":record[0],"author":record[1],"author_date":record[2],"committer_date":record[3],"subject":record[4]});
+        if args.include_body {
+            entry["body"] = json!(record[5]);
+        }
+        entries.push(entry);
     }
     Ok(entries)
 }
diff --git c/src/plugins/git_read/runner.rs w/src/plugins/git_read/runner.rs
index 91bfcb2..e750ded 100644
--- c/src/plugins/git_read/runner.rs
+++ w/src/plugins/git_read/runner.rs
@@ -44,6 +44,8 @@ pub(super) struct ReadRepo {
     pub shallow: bool,
     pub partial: bool,
     pub bare: bool,
+    strict_pinning: bool,
+    filter_overrides: Vec<String>,
     env: HashMap<String, String>,
     process: ProcessConfig,
 }
@@ -197,9 +199,54 @@ impl ReadRepo {
             shallow: false,
             partial: false,
             bare,
+            strict_pinning: limits.strict_pinning,
+            filter_overrides: Vec::new(),
             env,
             process,
         };
+        // ADR-019: status can run clean/process filters. Disable every
+        // configured driver before any operation that can inspect worktree content.
+        let filters = repo
+            .output(&[
+                "config",
+                "--null",
+                "--name-only",
+                "--get-regexp",
+                "^filter\\..*\\.(clean|smudge|process|required)$",
+            ])
+            .await?;
+        if filters.status.code() != Some(1) {
+            repo.check(&filters)?;
+        }
+        if filters.stdout_truncated
+            || filters.stderr_truncated
+            || filters.stdout.len() > limits.max_input_bytes
+        {
+            return Err(Failure::new(
+                "output_limit",
+                "filter configuration exceeds metadata budget",
+            ));
+        }
+        if !filters.stderr.trim().is_empty() {
+            return Err(Failure::new(
+                "invalid_repository",
+                "cannot safely inspect filter configuration",
+            ));
+        }
+        for key in filters.stdout.split_terminator('\0') {
+            if key.contains('=') || key.chars().any(char::is_control) {
+                return Err(Failure::new(
+                    "invalid_repository",
+                    "unsupported filter configuration key",
+                ));
+            }
+            let value = if key.ends_with(".required") {
+                "false"
+            } else {
+                ""
+            };
+            repo.filter_overrides.push(format!("{key}={value}"));
+        }
         repo.shallow = repo
             .text(&["rev-parse", "--is-shallow-repository"])
             .await?
@@ -259,6 +306,9 @@ impl ReadRepo {
         .iter()
         .map(|s| (*s).into())
         .collect();
+        for setting in &self.filter_overrides {
+            argv.extend(["-c".to_string(), setting.clone()]);
+        }
         argv.extend(arguments.iter().map(|s| (*s).into()));
         managed_process::run("git", &argv, &self.path, &self.env, &self.process, None)
             .await
@@ -338,7 +388,15 @@ impl ReadRepo {
         self.resolve(revision, "object").await
     }
     pub async fn commit(&self, revision: &str) -> Result<String, Failure> {
-        self.resolve(revision, "commit").await
+        let commit = self.resolve(revision, "commit").await?;
+        if self.strict_pinning && commit != revision.to_ascii_lowercase() {
+            return Err(Failure::new(
+                "rejected_argument",
+                "strict pinning requires a commit object, not a peeled tag",
+            ));
+        }
+
+        Ok(commit)
     }
     async fn resolve(&self, revision: &str, kind: &str) -> Result<String, Failure> {
         if revision.starts_with('-') || revision.contains('\0') {
diff --git c/src/plugins/git_read/tests.rs w/src/plugins/git_read/tests.rs
index 547eba9..07bf958 100644
--- c/src/plugins/git_read/tests.rs
+++ w/src/plugins/git_read/tests.rs
@@ -127,9 +127,9 @@ async fn every_operation_preserves_git_and_worktree_even_when_stat_stale() {
         .unwrap()
         .set_times(fs::FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(5)))
         .unwrap();
-    for args in f.cases() {
+    for (args, limits) in f.policy_cases() {
         let before = snapshot(&f.repo);
-        let response = f.call(args.clone()).await;
+        let response = f.with_limits(args.clone(), limits).await;
         assert_eq!(response.exit_code, 0, "{args}: {}", response.output);
         assert_eq!(snapshot(&f.repo), before, "mutation in {args}");
         assert!(response.output["repo"].as_str().is_some());
@@ -166,9 +166,40 @@ async fn every_operation_ignores_malicious_config() {
         &f.repo,
         &["config", "diff.trap.textconv", script.to_str().unwrap()],
     );
-    fs::write(f.repo.join(".gitattributes"), "*.txt diff=trap\n").unwrap();
-    for args in f.cases() {
-        let response = f.call(args.clone()).await;
+    git(
+        &f.repo,
+        &["config", "filter.trap.clean", script.to_str().unwrap()],
+    );
+    git(
+        &f.repo,
+        &["config", "filter.trap.smudge", script.to_str().unwrap()],
+    );
+    git(
+        &f.repo,
+        &["config", "filter.trap.process", script.to_str().unwrap()],
+    );
+    git(&f.repo, &["config", "filter.trap.required", "true"]);
+    git(
+        &f.repo,
+        &["config", "alias.show", &format!("!{}", script.display())],
+    );
+    let hooks = f.workspace.path().join("hooks");
+    fs::create_dir(&hooks).unwrap();
+    for hook in ["post-index-change", "post-checkout", "pre-auto-gc"] {
+        fs::copy(&script, hooks.join(hook)).unwrap();
+    }
+    git(
+        &f.repo,
+        &["config", "core.hooksPath", hooks.to_str().unwrap()],
+    );
+    fs::write(
+        f.repo.join(".gitattributes"),
+        "*.txt diff=trap filter=trap\n",
+    )
+    .unwrap();
+    fs::write(f.repo.join("renamed.txt"), "dirty working-tree content\n").unwrap();
+    for (args, limits) in f.policy_cases() {
+        let response = f.with_limits(args.clone(), limits).await;
         assert_eq!(response.exit_code, 0, "{args}: {}", response.output);
         assert!(!sentinel.exists(), "repo code ran in {args}");
     }
@@ -648,3 +679,221 @@ async fn registered_plugin_uses_the_smaller_git_and_shared_response_caps() {
         assert_eq!(snapshot(&f.repo), before);
     }
 }
+
+#[tokio::test]
+async fn strict_pinning_refuses_a_branch_while_the_default_keeps_it() {
+    let f = Fixture::new();
+    let args = json!({"command":"resolve","repo":"source","rev":"main"});
+    assert_eq!(f.call(args.clone()).await.exit_code, 0);
+    let strict = GitReadConfig {
+        strict_pinning: true,
+        ..Default::default()
+    };
+    let response = f.with_limits(args, strict).await;
+    assert_eq!(
+        response.output["error"]["kind"], "rejected_argument",
+        "{}",
+        response.output
+    );
+}
+
+#[tokio::test]
+async fn strict_pinning_requires_commit_objects_not_annotated_tag_objects() {
+    let f = Fixture::new();
+    git(
+        &f.repo,
+        &[
+            "tag",
+            "-a",
+            "policy-tag",
+            "-m",
+            "Tag object for strict policy",
+        ],
+    );
+    let tag = git(&f.repo, &["rev-parse", "refs/tags/policy-tag"]);
+    assert_eq!(
+        f.call(json!({"command":"resolve","repo":"source","rev":tag}))
+            .await
+            .exit_code,
+        0
+    );
+    let strict = GitReadConfig {
+        strict_pinning: true,
+        ..Default::default()
+    };
+    let response = f
+        .with_limits(
+            json!({"command":"resolve","repo":"source","rev":tag}),
+            strict,
+        )
+        .await;
+    assert_eq!(
+        response.output["error"]["kind"], "rejected_argument",
+        "{}",
+        response.output
+    );
+}
+
+impl Fixture {
+    fn policy_cases(&self) -> Vec<(Value, GitReadConfig)> {
+        let mut cases: Vec<_> = self
+            .cases()
+            .into_iter()
+            .map(|args| (args, GitReadConfig::default()))
+            .collect();
+        for mut args in self.cases() {
+            let command = args["command"].as_str().unwrap();
+            if matches!(command, "refs" | "worktree_status") {
+                continue;
+            }
+            if !matches!(command, "diff" | "ancestry")
+                && args
+                    .get("rev")
+                    .and_then(Value::as_str)
+                    .is_none_or(|rev| rev.len() != 40)
+            {
+                args["rev"] = json!(self.last);
+            }
+            cases.push((
+                args,
+                GitReadConfig {
+                    strict_pinning: true,
+                    ..Default::default()
+                },
+            ));
+        }
+        cases
+    }
+}
+
+#[tokio::test]
+async fn strict_pinning_rejects_all_unpinned_revision_forms_and_operations() {
+    let f = Fixture::new();
+    let strict = GitReadConfig {
+        strict_pinning: true,
+        ..Default::default()
+    };
+    for command in OPERATIONS {
+        if matches!(*command, "refs" | "worktree_status") {
+            let result = f
+                .with_limits(
+                    json!({"command":command,"repo":"source","rev":f.last}),
+                    strict.clone(),
+                )
+                .await;
+            assert_eq!(result.output["error"]["kind"], "rejected_argument");
+            continue;
+        }
+        for revision in [
+            None,
+            Some("HEAD"),
+            Some("HEAD~1"),
+            Some("main"),
+            Some("review"),
+            Some("abcd123"),
+            Some("HEAD..main"),
+        ] {
+            let mut args = json!({"command":command,"repo":"source","path":"renamed.txt"});
+            if matches!(*command, "diff" | "ancestry") {
+                args["b"] = json!(f.last);
+                if let Some(revision) = revision {
+                    args["a"] = json!(revision);
+                }
+            } else if let Some(revision) = revision {
+                args["rev"] = json!(revision);
+            }
+            let result = f.with_limits(args.clone(), strict.clone()).await;
+            assert_eq!(
+                result.output["error"]["kind"], "rejected_argument",
+                "{args}: {}",
+                result.output
+            );
+        }
+    }
+}
+
+#[tokio::test]
+async fn commit_bodies_are_additive_and_do_not_change_default_log_fields() {
+    let f = Fixture::new();
+    git(
+        &f.repo,
+        &[
+            "commit",
+            "--allow-empty",
+            "-m",
+            "Body fixture",
+            "-m",
+            "First body line.\nSecond body line.",
+        ],
+    );
+    let ordinary = f
+        .call(json!({"command":"log","repo":"source","limit":1}))
+        .await;
+    assert!(ordinary.output["entries"][0].get("body").is_none());
+    for command in ["log", "show_commit"] {
+        let result = f
+            .call(json!({"command":command,"repo":"source","limit":1,"include_body":true}))
+            .await;
+        assert_eq!(result.exit_code, 0, "{}", result.output);
+        let commit = if command == "log" {
+            &result.output["entries"][0]
+        } else {
+            &result.output["commit"]
+        };
+        assert_eq!(commit["body"], "First body line.\nSecond body line.\n");
+        assert_eq!(commit["subject"], "Body fixture");
+    }
+}
+
+#[tokio::test]
+async fn default_status_reports_raw_bytes_without_running_normalizing_filters() {
+    let f = Fixture::new();
+    let filtered = f.repo.join("filtered.txt");
+    fs::write(&filtered, "worktree content\n").unwrap();
+    fs::write(
+        f.repo.join(".gitattributes"),
+        "filtered.txt filter=normalize\n",
+    )
+    .unwrap();
+    git(
+        &f.repo,
+        &[
+            "config",
+            "filter.normalize.clean",
+            "sed s/worktree/canonical/g",
+        ],
+    );
+    git(&f.repo, &["config", "filter.normalize.required", "true"]);
+    git(&f.repo, &["add", "filtered.txt", ".gitattributes"]);
+    git(&f.repo, &["commit", "-m", "Normalizing filter fixture"]);
+    assert_eq!(
+        git(&f.repo, &["show", "HEAD:filtered.txt"]),
+        "canonical content"
+    );
+    assert_eq!(fs::read_to_string(&filtered).unwrap(), "worktree content\n");
+    // Force a content check without allowing ordinary Git to refresh the index.
+    fs::File::open(&filtered)
+        .unwrap()
+        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(5)))
+        .unwrap();
+    let before = snapshot(&f.repo);
+    assert_eq!(
+        git(
+            &f.repo,
+            &["--no-optional-locks", "status", "--porcelain=v1"]
+        ),
+        ""
+    );
+    assert_eq!(snapshot(&f.repo), before);
+    let result = f
+        .call(json!({"command":"worktree_status","repo":"source"}))
+        .await;
+    assert_eq!(result.exit_code, 0, "{}", result.output);
+    assert_eq!(result.output["clean"], false);
+    assert!(result.output["entries"]
+        .as_array()
+        .unwrap()
+        .iter()
+        .any(|entry| entry["path"] == "filtered.txt" && entry["status"] == " M"));
+    assert_eq!(snapshot(&f.repo), before);
+}
diff --git c/src/tool_descriptions.rs w/src/tool_descriptions.rs
index 088518c..1e62cbb 100644
--- c/src/tool_descriptions.rs
+++ w/src/tool_descriptions.rs
@@ -11,7 +11,7 @@ pub const DEFAULT_TEXT: &str = include_str!("../prompts/tool_descriptions.toml")
 /// deletion fails parity tests. Future intentional additions/removals update
 /// this single value.
 #[cfg(test)]
-pub(crate) const DEFAULT_PARAMETER_DESCRIPTION_COUNT: usize = 156;
+pub(crate) const DEFAULT_PARAMETER_DESCRIPTION_COUNT: usize = 157;
 #[cfg(test)]
 pub(crate) const AGENT_ONLY_PARAMETER_DESCRIPTION_COUNT: usize = 2;
 #[cfg(test)]
diff --git c/tests/test_git_read.py w/tests/test_git_read.py
index 88b11ea..f3b241e 100644
--- c/tests/test_git_read.py
+++ w/tests/test_git_read.py
@@ -292,3 +292,90 @@ def test_git_read_short_inheritance_prefixes_do_not_leak_git_controls(daimonos_f
         response=client.call_tool("git_read",{"command":"resolve","repo":"source"})
         assert not response.get("isError"),response
         assert not trace.exists(),f"git controls leaked through prefix {prefix!r}"
+
+
+def test_git_read_strict_policy_keeps_default_resolution_and_adds_bodies(daimonos_factory,tmp_path):
+    repo,first=repository(tmp_path)
+    git(repo,"commit","--allow-empty","-m","Body fixture","-m","Stored body.")
+    commit=git(repo,"rev-parse","HEAD")
+    ordinary=daimonos_factory(tmp_path)
+    assert not ordinary.call_tool("git_read",{"command":"resolve","repo":"source","rev":"HEAD"}).get("isError")
+    (tmp_path/"daimonos.toml").write_text("[git_read]\nstrict_pinning=true\n")
+    strict=daimonos_factory(tmp_path)
+    for revision in (None,"HEAD","main",commit[:10],"HEAD~1",f"{first}..{commit}"):
+        args={"command":"resolve","repo":"source"}
+        if revision is not None:
+            args["rev"]=revision
+        response=strict.call_tool("git_read",args)
+        assert response.get("isError"),response
+        assert json.loads(response["content"][0]["text"])["error"]["kind"]=="rejected_argument"
+    resolved=strict.call_tool("git_read",{"command":"resolve","repo":"source","rev":commit})
+    assert not resolved.get("isError"),resolved
+    assert json.loads(resolved["content"][0]["text"])["commits"]==[commit]
+    history=strict.call_tool("git_read",{"command":"log","repo":"source","rev":commit,"include_body":True,"limit":1})
+    assert not history.get("isError"),history
+    assert json.loads(history["content"][0]["text"])["entries"][0]["body"]=="Stored body.\n"
+    assert not ordinary.call_tool("git_read",{"command":"resolve","repo":"source","rev":"HEAD"}).get("isError")
+
+
+def test_git_read_strict_operations_on_a_real_read_only_bind_mount(daimonos_binary,tmp_path):
+    import errno
+    import os
+    import shutil
+    import sys
+    import pytest
+
+    if sys.platform!="linux" or not shutil.which("unshare"):
+        pytest.skip("requires Linux user and mount namespaces")
+    supported=subprocess.run(["unshare","-Urnm","true"],capture_output=True,text=True)
+    if supported.returncode:
+        pytest.skip("user or mount namespaces are unavailable")
+    workspace=tmp_path/"evidence"
+    workspace.mkdir()
+    _,commit=repository(workspace)
+    (workspace/"daimonos.toml").write_text("[git_read]\nstrict_pinning=true\n[logging]\nenabled=false\n[analytics]\nenabled=false\n")
+    source_tests=str(Path(__file__).resolve().parent)
+    program=r'''
+import errno,hashlib,json,os,subprocess,sys
+from pathlib import Path
+sys.path.insert(0,sys.argv[1])
+from conftest import DaimonosClient
+binary,workspace,commit=sys.argv[2:]
+workspace=Path(workspace)
+def snapshot():
+    return [(str(p.relative_to(workspace)),p.lstat().st_size,p.lstat().st_mtime_ns,hashlib.sha256(p.read_bytes()).hexdigest() if p.is_file() else None) for p in sorted(workspace.rglob("*"))]
+subprocess.run(["mount","--bind",str(workspace),str(workspace)],check=True)
+subprocess.run(["mount","-o","remount,bind,ro",str(workspace)],check=True)
+try:
+    (workspace/"must-not-write").write_text("forbidden")
+    raise AssertionError("the mount is not read-only")
+except OSError as error:
+    assert error.errno==errno.EROFS,error
+before=snapshot()
+process=subprocess.Popen([binary,"--mcp","-w",str(workspace)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
+try:
+    client=DaimonosClient(process,str(workspace))
+    client.send_raw({"jsonrpc":"2.0","id":client._next_id(),"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"readonly-probe","version":"1"}}})
+    client.send_raw({"jsonrpc":"2.0","method":"notifications/initialized"})
+    calls=[{"command":"resolve"},{"command":"show_file","path":"file.txt"},{"command":"list_tree"},{"command":"grep","pattern":"first"},{"command":"log","include_body":True},{"command":"show_commit","include_body":True},{"command":"diff","a":commit,"b":commit},{"command":"blame","path":"file.txt"},{"command":"ancestry","a":commit,"b":commit}]
+    for args in calls:
+        if args["command"] not in ("diff","ancestry"):
+            args["rev"]=commit
+        response=client.call_tool("git_read",{"repo":"source",**args})
+        assert not response.get("isError"),(args,response)
+        assert commit in json.loads(response["content"][0]["text"])["commits"]
+    for command in ("refs","worktree_status"):
+        response=client.call_tool("git_read",{"repo":"source","command":command})
+        assert response.get("isError"),response
+        assert json.loads(response["content"][0]["text"])["error"]["kind"]=="rejected_argument"
+finally:
+    process.terminate()
+    process.wait(timeout=5)
+assert snapshot()==before,"the evidence changed"
+print(json.dumps({"read_only_mount":True,"pinned_operations":9,"refused_operations":2,"unchanged":True}))
+'''
+    env=os.environ.copy()
+    env["DAIMONOS_KGL_AUTOINDEX"]="0"
+    result=subprocess.run(["unshare","-Urnm",sys.executable,"-c",program,source_tests,str(daimonos_binary),str(workspace),commit],env=env,capture_output=True,text=True,timeout=60)
+    assert result.returncode==0,result.stdout+result.stderr
+    assert json.loads(result.stdout)=={"read_only_mount":True,"pinned_operations":9,"refused_operations":2,"unchanged":True}

### Known Concerns
1. The default filtered-status change is explicitly authorized by the human as option 2 of Q-FR-06-2.1. Do not require restoring filter execution for compatibility.
2. Cross-runtime allowlists, general filesystem confinement, state relocation, and workspace-write policy remain separate pending slices. This review cannot establish the complete dbunk integration.
3. Full test logs use the disclosed local test prerequisite PATH prefix target/test-tools/bin and .venv/bin. No live model endpoint was used.

### Specific Questions for Reviewer
1. Does strict pinning reject every unpinned or non-commit input without regressing the default revision surface?
2. Can repository-configured filters still execute despite suppression, or bypass the bounded key inspection?
3. Do history bodies, caps, tests, and documentation preserve the agreed observable contract?

<!-- event id="review-1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. Filter suppression has a time-of-check/time-of-use gap: ReadRepo::open reads the filter driver keys once and stores per-process -c overrides, but repository config can change between open and the actual git invocation (e.g., include.path indirection, a concurrently rewritten .git/config, or a conditional include that evaluates differently in the child process). More concretely, the regexp ^filter\..*\.(clean|smudge|process|required)$ only overrides drivers that are configured at open time; a driver configured via a key the regexp misses cannot occur, but a driver added by an includeIf/include.path target whose content differs when the subsequent git subprocess re-reads config will execute. The safer design is to pass unconditional suppression to every invocation (e.g., -c filter.<driver> overrides derived per call, or better, disable the filter mechanism wholesale via core.attributesFile/attribute overrides or GIT_CONFIG_* environment forcing) rather than a one-shot snapshot. At minimum, add a test where the filter driver is introduced via include.path after ReadRepo::open snapshot, or re-derive overrides per invocation, and document why the remaining window is closed.
B2. The bounded key inspection can be bypassed by case variation: git config section and variable names are case-insensitive for the section and key parts (filter.X.CLEAN is normalized by git, but `git config --get-regexp` matches against the canonical lowercased form while the subsection — the driver name — is case-sensitive). Verify and add a test that a driver configured with mixed-case keys (e.g., [FILTER "trap"] Clean=...) is still captured by the --get-regexp pattern and suppressed; if git canonicalizes before matching this is fine, but the suite currently contains no case-variation fixture, so the claimed 'cannot bypass bounded key inspection' guarantee is untested.
B3. strict commit validation in ReadRepo::commit compares the resolved commit to revision.to_ascii_lowercase(), relying on validate_pinning having already enforced the 40-hex form. However validate_pinning only runs when self.limits.strict_pinning is true at the plugin layer, while ReadRepo carries its own strict_pinning copy; if any future caller constructs ReadRepo with strict_pinning=true without the plugin-layer validation, a 40-hex abbreviation of a tree/blob or an ambiguous object would be handled only by resolve's type check. More importantly today: a full 40-hex SHA of an annotated tag passes validate_pinning, then resolve(revision, "commit") peels it and commit() rejects the mismatch — good — but a 40-hex hash that abbreviates to a different commit cannot occur with full SHAs, yet the equality check is the only guard and it is not covered for the diff/ancestry path: confirm (with a test) that diff and ancestry route a/b through commit() and not resolve_object(), otherwise an annotated-tag SHA could slip through a and b under strict pinning. The current test suite only exercises the tag-object rejection via the resolve command.
B4. In ReadRepo::open, the filter-config probe runs via repo.output(...) before filter_overrides is populated, which is correct for `git config` (it does not execute filters), but the shallow/partial probes and any other open-time git invocations also run with the overrides now appended — however `repo.check(&filters)` is skipped whenever status code is exactly 1 (no matches), yet `git config --get-regexp` can also exit 1 with stderr noise or exit 2 on malformed config. Exit 2 falls into the check path, fine; but when exit is 1 the code also skips the truncation/stderr validation ordering issue: stdout/stderr truncation and the non-empty-stderr rejection are evaluated after the code()==1 branch, so a truncated or noisy exit-1 result is still vetted — verify this ordering is intentional and add a malformed-config fixture (e.g., a config file with an invalid line) asserting the structured failure path, since 'incomplete metadata cause a structured failure before the read' is asserted in ADR-019 but no malformed-config test exists in the diff.

### Non-Blocking Suggestions
S1. validate_pinning accepts a mixed-case 40-hex rev and commit() then compares against revision.to_ascii_lowercase(); the semantics work, but consider normalizing (or rejecting non-lowercase) at the validation boundary so the pinned input recorded in logs/citations is canonical, and add a test with an uppercase full SHA under strict pinning to pin down the intended behavior.
S2. The body field parsing in history() uses fixed-width NUL chunks; a commit body containing a NUL cannot occur in practice, but %b for the last record is followed by the -z terminator while bodies themselves end with a newline git appends — the test asserts a trailing \n in body. Document that trailing-newline contract in configuration.md or the tool description so callers do not strip/compare bodies inconsistently.
S3. ADR-018 scopes strict pinning to SHA-1 (40 hex). Consider making the length check derive from the repository's object format (rev-parse --show-object-format) now, or add an explicit structured error message mentioning SHA-256 unsupported, so SHA-256 repositories fail with an actionable message rather than a generic rejected_argument.
S4. The read-only bind-mount test skips on non-Linux and when namespaces are unavailable; record that skip in the slice's test evidence summary (e.g., CI matrix note) so 'actual EROFS ... for all nine pinned operations' is not read as universally executed. ADR-018 already says to record unavailable platform facilities — wire that into the test report artifact.

**Verdict:** REVISE

<!-- event id="rebuttal-1" artifact path="1576-dbunk-git-pinning/artifacts/round-1-rebuttal.diff" sha256="6eb653e6eb8c883d2cb043497482061d8c49c03af3ee4759acaed00256669a6d" -->
## Rebuttal — Round 1
**Position:** PARTIAL

### Responses to Blocking Concerns
Re B1: ACCEPT — A controlled real-MCP/git-boundary regression reproduced the race. It introduces a process filter through include.path immediately before the status invocation, after the original key inspection. Status now runs against a private frozen metadata view outside the workspace, not the mutable repository configuration. The view has a pinned HEAD, copied index/shared-index and local attribute/exclude metadata, and a closed whitelist of data-only status configuration. It contains no filter definitions or include directives. GIT_DIR, GIT_COMMON_DIR, and GIT_INDEX_FILE target that private view. Only the original worktree and object store remain read sources. Mutating an include or adding a driver cannot configure that child process. The race regression passes. The view is private, bounded by a configurable metadata budget, and removed through a drop guard. Directory acquisition has no cancellation point before ownership. Existing per-process overrides remain defense for committed-object reads, but status no longer relies on the scan.
Re B2: CLARIFY — Git canonicalizes section/variable names before regexp matching. The new mixed_case_filter_configuration_is_suppressed_under_both_policies fixture writes uppercase section/key spellings and a case-sensitive mixed-case subsection. It asserts real Git returns filter.MiXeD.clean and filter.MiXeD.process, then exercises both policies with unchanged snapshots and no marker. This is coverage of existing correct behavior, not a reproduced bypass. The frozen status config additionally contains no driver sections at all.
Re B3: CLARIFY — Both active a/b inputs already pass through ReadRepo::commit in diff and ancestry. The new public regression tests an annotated-tag SHA on each side of each operation: default mode succeeds and strict mode returns rejected_argument. The runner now also enforces the full SHA-1 syntax itself, using the same helper as the argument boundary. This preserves the invariant if internal callers change later. No future caller or current bypass was found.
Re B4: CLARIFY — The original ordering already checked truncation and stderr after the exit-code-1 branch. Noisy or truncated code-1 responses were not accepted. The new malformed_filter_configuration_fails_structurally_without_mutation fixture tests actual malformed repository config through worktree_status and a pinned file read. Both return structured git_error without repository changes. The new frozen-status config query independently checks capture truncation and stderr before preparing its view.

### Revised Code / Diff
diff --git c/CONTEXT-MAP.md w/CONTEXT-MAP.md
index e2585b1..c0431e8 100644
--- c/CONTEXT-MAP.md
+++ w/CONTEXT-MAP.md
@@ -1,10 +1,10 @@
 # Context map
 
-Date: 2026-10-26
+Date: 2026-10-28
 
 | Context | Glossary | Decisions |
 |---------|----------|-----------|
-| Pinned git reads | [Git reads](docs/contexts/git-read/CONTEXT.md) | [ADR-017](docs/adr/017-git-read-tool.md) |
+| Pinned git reads | [Git reads](docs/contexts/git-read/CONTEXT.md) | [ADR-017](docs/adr/017-git-read-tool.md), [Proposed ADR-018](docs/adr/018-strict-git-pinning.md), [Proposed ADR-019](docs/adr/019-ignore-repository-filters.md) |
 
 This map covers the git-read context. Other contexts are not mapped here yet.
 
diff --git c/daimonos.default.toml w/daimonos.default.toml
index 7de95d9..7c9a5ee 100644
--- c/daimonos.default.toml
+++ w/daimonos.default.toml
@@ -607,6 +607,12 @@ poll_interval_ms = 1500
 
 # Pinned, local git reads. See ADR-017 and GitReadConfig in src/config.rs.
 [git_read]
+# Require explicit full SHA-1 commit identifiers. Reject names, ranges, and unpinned operations.
+strict_pinning = false
+# Default: system temporary directory. An override must already exist outside the workspace.
+# status_directory = "/private/state"
+# Bound the complete private index/configuration snapshot used by worktree_status.
+max_status_metadata_bytes = 67108864
 # Maximum captured bytes per stream and maximum serialized response bytes.
 max_output_bytes = 1048576
 # Reject larger blobs before reading their content.
diff --git c/docs/adr/018-strict-git-pinning.md w/docs/adr/018-strict-git-pinning.md
new file mode 100644
index 0000000..4550b43
--- /dev/null
+++ w/docs/adr/018-strict-git-pinning.md
@@ -0,0 +1,75 @@
+# ADR-018: Strict git pinning is an opt-in policy
+
+- **Status:** Proposed
+- **Date:** 2026-10-27
+- **Deciders:** Pending architectural review. The human approved the product requirements.
+- **Consulted:** dbunk PM, through the round-1 written answers
+- **Informed:** None recorded
+- **Spec:** `daimonos-feature-requests-for-dbunk.md`, FR-06, and `daimonos-pm-answers-round-1.md`
+- **Tracking:** Vikunja #1576, parent #1575
+- **Supersedes:** None
+- **Superseded by:** None
+
+## Context
+
+ADR-017 permits local revision resolution and returns full commit identifiers.
+The dbunk reviewer instead needs caller-pinned input identifiers before any git read.
+The PM accepted an additive policy and rejected a global revision-contract change.
+How can dbunk require pinned inputs without breaking existing callers?
+
+## Decision drivers
+
+- Existing calls, defaults, and result shapes remain unchanged except for the approved filter security fix in ADR-019.
+- Every strict read must identify its input commits explicitly.
+- Output pinning cannot replace input validation.
+- The existing process hardening must remain active under both policies.
+
+## Decision
+
+We will add `git_read.strict_pinning`, default false.
+When true, operations require explicit full 40-character hexadecimal commit identifiers.
+`diff` and `ancestry` require both `a` and `b` in that form.
+Other pinned operations require `rev`. Implicit HEAD, ranges, relative revisions, and short hashes are rejected.
+`refs` and `worktree_status` are refused because they cannot supply a pinned view.
+`resolve` remains useful only for verifying an explicit full commit identifier.
+The existing runner still resolves and checks the object type locally.
+This policy does not weaken confinement, no-mutation, or no-network controls.
+
+We will add `include_body`, default false, for `log` and `show_commit`.
+The field `body` appears in commit metadata only when the caller requests it.
+Existing names and result fields remain unchanged by default.
+
+## Alternatives
+
+- Globally reject revision names. This breaks ADR-017 and existing callers.
+- Accept names and only pin outputs. A moving ref can change before the request starts.
+- Create a second git tool. This duplicates the operation surface and process hardening.
+
+## Consequences
+
+The caller must explicitly select strict pinning. It is not a complete reviewer sandbox.
+Forty-character hashes restrict the strict profile to SHA-1 repositories.
+The default policy still supports other locally resolvable git object formats.
+Commit bodies can increase output size, so existing response caps still apply.
+
+## Confirmation
+
+- Test every strict operation through the public plugin interface.
+- Reject missing hashes, names, tags, short hashes, ranges, and unpinned operations.
+- Prove names still work with the default policy.
+- Compare repository snapshots before and after strict calls and hostile-config tests.
+- Exercise strict reads on a real read-only mount. Record unavailable platform facilities explicitly.
+- Verify `include_body` through Rust and MCP without changing default metadata fields.
+- Run `cargo test` and `python3 -m pytest tests/ -v`.
+
+## Anchors
+
+_Anchors:_ src/plugins/git_read.rs::GitReadPlugin, src/plugins/git_read.rs::Arguments, src/plugins/git_read.rs::history, src/plugins/git_read/runner.rs::ReadRepo, src/config.rs::GitReadConfig
+
+## Revisit when
+
+The caller needs strict SHA-256 repositories or a versioned pinned-only tool contract.
+
+## Open questions
+
+No product acceptance blocker remains for this slice. Architectural review is pending.
diff --git c/docs/adr/019-ignore-repository-filters.md w/docs/adr/019-ignore-repository-filters.md
new file mode 100644
index 0000000..feb774d
--- /dev/null
+++ w/docs/adr/019-ignore-repository-filters.md
@@ -0,0 +1,79 @@
+# ADR-019: Ignore repository-configured filters during git reads
+
+- **Status:** Proposed
+- **Date:** 2026-10-28
+- **Deciders:** The human approved option 2 of Q-FR-06-2.1 on 2026-10-27. This record awaits architectural review.
+- **Consulted:** dbunk PM, through the written requirements.
+- **Informed:** None recorded.
+- **Spec:** FR-06, PM round 1, and `docs/reviews/2026-10-27-dbunk-follow-up.md`.
+- **Tracking:** Vikunja #1576, under #1575.
+- **Supersedes:** None. This clarifies the security guarantee in ADR-017.
+- **Superseded by:** None.
+
+## Context
+
+Git status can execute a configured clean or process filter while checking dirty files.
+The expanded hostile-configuration test reproduced that execution through git_read.
+Disabling filters also changes status results for legitimate filtered repositories.
+The human explicitly approved that compatibility exception instead of retaining the execution risk.
+Should filter suppression apply in both policies, despite the changed status semantics?
+
+## Decision drivers
+
+- A read-only tool must not execute repository-configured programs.
+- Strict pinning alone does not protect existing callers of worktree_status.
+- The compatibility change must remain explicit and tested.
+
+## Decision
+
+We will suppress configured clean, smudge, and process filters in both pinning policies.
+The runner retains bounded per-process overrides as defense for committed-object reads.
+Working-tree status does not depend on that scan. It uses a private, frozen Git metadata directory outside the workspace.
+The directory contains a pinned HEAD, copied index metadata, and only data-only configuration needed for built-in status behavior.
+It contains no filter definitions or include directives. Mutable repository configuration cannot add a driver to that process.
+The process reads the original worktree and object store without changing them.
+Split-index and local attribute/exclude metadata are copied into the private view.
+The private view is removed after success, failure, or cancellation. No repository configuration file changes.
+The worktree result excludes external filter transformations. Built-in Git conversions can still apply.
+The user-facing documentation will warn that a filtered file can appear modified where ordinary Git reports clean.
+
+## Alternatives
+
+- Retain filter execution in default mode. Rejected by the human because the read-only tool would remain an execution surface.
+- Suppress filters only under strict pinning. This protects the dbunk policy but not ordinary worktree-status callers.
+- Execute only familiar filters. Rejected because repository configuration selects executable commands, not trustworthy implementations.
+
+## Consequences
+
+The fix changes default status semantics for repositories such as Git LFS repositories.
+This is the narrow compatibility exception approved as option 2, not permission for unrelated default changes.
+The existing git tool remains unchanged.
+The metadata budgets reject excessive configuration or index snapshots instead of applying incomplete protection.
+`git_read.max_status_metadata_bytes` bounds the private view. `git_read.status_directory` selects an existing directory outside the workspace.
+Without an override, the view uses the system temporary directory. FR-08 must route this internal state under an explicit state root.
+
+## Confirmation
+
+- Run `cargo test default_status_reports_raw_bytes_without_running_normalizing_filters`.
+- In that fixture, ordinary Git reports clean after a real filter transformation.
+- Verify that git_read instead reports the tracked file as modified and leaves the full repository snapshot unchanged.
+- Run `cargo test every_operation_ignores_malicious_config` under both policies.
+- Run `python3 -m pytest tests/test_git_read.py::test_git_read_filter_added_after_inspection_cannot_execute -v`.
+- Prove that a filter added through an include immediately before status cannot execute.
+- Test mixed-case keys, malformed configuration, split indexes, local excludes, built-in normalization, linked worktrees, reftable, and SHA-256 defaults.
+- Verify metadata caps and private-directory cleanup after failure.
+- Run `cargo test plugins::git_read::tests` and `python3 -m pytest tests/test_git_read.py -v`.
+- Run the full Rust and Python suites before completing this slice.
+
+## Anchors
+
+_Anchors:_ src/plugins/git_read/status.rs::StatusView, src/plugins/git_read/runner.rs::ReadRepo, src/plugins/git_read/tests.rs::default_status_reports_raw_bytes_without_running_normalizing_filters, src/plugins/git_read/tests.rs::every_operation_ignores_malicious_config
+
+## Revisit when
+
+Git provides a way to inspect filter-normalized status without executing repository-controlled programs.
+Also revisit this decision if the caller needs explicitly trusted filter execution in a different tool.
+
+## Open questions
+
+The human resolved the product choice. Architectural review of this record remains pending.
diff --git c/docs/configuration.md w/docs/configuration.md
index 8072776..74e08c0 100644
--- c/docs/configuration.md
+++ w/docs/configuration.md
@@ -1051,6 +1051,31 @@ raw model responses; the UUID is local, not a provider request ID.
 
 ## Read-only git (`git_read`)
 
+`git_read.strict_pinning` is false by default. Enable it to require explicit
+40-character SHA-1 commit identifiers. `diff` and `ancestry` then require `a` and
+`b`. Other permitted operations require `rev`. Names, ranges, implicit HEAD,
+short hashes, and annotated-tag object identifiers are rejected. `refs` and
+`worktree_status` are refused under this policy. Default revision resolution remains unchanged.
+
+`log` and `show_commit` accept `include_body=true`. Only that opt-in adds the
+`body` field to commit metadata. The existing response cap still applies.
+Bodies preserve Git's body text, including any trailing newline. Do not trim them before comparisons.
+
+The runner disables configured clean, smudge, and process filters in both modes.
+This is an approved security-fix exception to the unchanged-default rule.
+`worktree_status` does not apply those external transformations. It can report a
+modified file where ordinary Git reports a clean filtered file, including Git LFS cases.
+Do not interpret this result as filter-aware status. Built-in Git conversions can still apply.
+[ADR-019](adr/019-ignore-repository-filters.md) records the reason and the human approval.
+Status runs against private, frozen metadata with no filter definitions or include directives.
+Later changes to repository configuration cannot add executable filters to that process.
+`git_read.status_directory` can select an existing metadata directory outside the workspace.
+The default is the system temporary directory. The private directory is removed after the call.
+`git_read.max_status_metadata_bytes` defaults to 64 MiB and bounds the copied metadata.
+Unsupported filter keys or oversized configuration fail closed before reads.
+Strict pinning is not required for process hardening. It adds immutable input selection.
+
+
 The `[git_read]` section bounds local, pinned git reads. `GitReadConfig` in
 `src/config.rs` defines the defaults. `daimonos.default.toml` lists every key.
 ADR-017 records the safety boundary.
diff --git c/docs/contexts/git-read/CONTEXT.md w/docs/contexts/git-read/CONTEXT.md
index b0080cd..11ef21c 100644
--- c/docs/contexts/git-read/CONTEXT.md
+++ w/docs/contexts/git-read/CONTEXT.md
@@ -1,14 +1,16 @@
 # Pinned git reads
 
-Date: 2026-10-27
+Date: 2026-10-28
 
 - **Workspace:** The fixed root that contains repositories. The session cwd does not change this boundary.
 - **Pinned commit:** A full commit SHA returned with a read result. It makes a citation independent of moving refs.
+- **Strict pinning:** The optional policy that also requires explicit SHA-1 commit identifiers as inputs. It rejects mutable names and tag objects.
 - **Promisor:** A partial-clone remote that holds objects absent locally. `git_read` never asks it for objects.
 - **Alternate store:** Another git object directory. Chained stores must stay inside the workspace.
 - **Patch preview:** Bounded patch text. A truncated preview is not a complete patch.
 - **Pointer file:** Git metadata that selects a git directory or common directory. It must stay inside the workspace.
 - **Bare repository:** A git database without a working tree. History reads are valid, but worktree status is not.
+- **Filter-free status:** Status without repository-configured external filter transformations. A filtered file can appear modified even when ordinary Git reports clean.
 
 [ADR-017](../../adr/017-git-read-tool.md) records why the tool is separate from `git`.
 [Configuration](../../configuration.md#read-only-git-git_read) names its limits.
diff --git c/docs/reviews/2026-10-28-fr06-validation.md w/docs/reviews/2026-10-28-fr06-validation.md
new file mode 100644
index 0000000..40d3463
--- /dev/null
+++ w/docs/reviews/2026-10-28-fr06-validation.md
@@ -0,0 +1,47 @@
+# FR-06 local validation
+
+Date: 2026-10-28
+Task: Vikunja #1576, under #1575.
+Branch: `feat/1575-dbunk-reviewer`.
+Status: Local validation passed. Rubber Ducky round 2 is pending.
+
+## Scope
+
+This evidence covers strict git pinning, additive bodies, and the approved filter security exception.
+It does not establish completion of FR-02, FR-03, FR-07, or FR-08 integration.
+No new crate, live credentialed endpoint test, publication, or merge occurred.
+
+## Actual checks
+
+- `cargo fmt --all -- --check`: passed.
+- `cargo clippy --all-targets -- -D warnings`: passed.
+- `cargo test`: 2011 passed, 0 failed, 10 ignored.
+- `python3 -m pytest tests/ -v`: 385 passed, 0 failed, 1 skipped.
+- The full-suite commands used `target/test-tools/bin` and `.venv/bin` as PATH prefixes.
+- Logs: `target/dbunk-fr06-rust-review1.log` and `target/dbunk-fr06-pytest-review1.log`.
+- Those logs and temporary test tools are local artifacts, not committed dependencies.
+
+## Read-only mount evidence
+
+The Linux namespace test ran and passed on this host. It checked EROFS on a real bind mount.
+It exercised all nine pinned operations and both refused operations, with unchanged evidence snapshots.
+The test skips on non-Linux hosts or when user/mount namespaces are unavailable.
+No macOS mount test ran here. A skip does not count as a successful read-only-mount check.
+
+## Review regressions
+
+- A process-boundary fixture introduced a filter through an include after the original inspection.
+  The old implementation executed it. The frozen-metadata implementation does not.
+- Mixed-case filter keys are canonicalized by Git and suppressed under both policies.
+- Both sides of diff and ancestry reject full annotated-tag object IDs under strict pinning.
+- Malformed configuration returns a structured failure without mutating the repository.
+- The private metadata budget fails closed, and the temporary directory is removed after failure.
+- Split indexes, local excludes, built-in text normalization, linked worktrees, reftable, and SHA-256 defaults pass.
+- Uppercase SHA-1 inputs return canonical lowercase commit identifiers.
+
+## Internal state
+
+Status uses private metadata outside the workspace. It contains no filter definitions or include directives.
+`git_read.status_directory` selects an existing directory. Its default is the system temporary directory.
+`git_read.max_status_metadata_bytes` defaults to 64 MiB.
+FR-08 must route this internal state under the explicit state root. That integration remains pending.
diff --git c/prompts/tool_descriptions.toml w/prompts/tool_descriptions.toml
index 6e44a35..da684f5 100644
--- c/prompts/tool_descriptions.toml
+++ w/prompts/tool_descriptions.toml
@@ -198,12 +198,12 @@ limit = "Max reservations to return (most-recent first)."
 full = """Python-style function signatures for all tool bindings available in execute_script. Already included in server instructions."""
 
 [git_read]
-full = """Read local git history at pinned commits without writes or network access. Use git_read for reviewer reads. Use git for writes. Returns JSON with the repo and full commit SHAs. Use workspace-relative repo and path values. Revisions, paths, repo names, and patterns must not start with a hyphen. Requires git with --no-lazy-fetch support. Caps come from [git_read] config. Oversized metadata returns output_limit. Binary files return metadata only."""
-terse = "Read-only git at pinned commits. No writes or network."
+full = """Read local git history at pinned commits without repository writes or network access. Use git_read for reviewer reads. Use git for writes. Returns JSON with the repo and full commit SHAs. Use workspace-relative repo and path values. Revisions, paths, repo names, and patterns must not start with a hyphen. Requires git with --no-lazy-fetch support. Caps come from [git_read] config. Oversized metadata returns output_limit. Binary files return metadata only. When strict pinning is enabled, supply full SHA-1 commit hashes. Names, ranges, and unpinned operations are refused."""
+terse = "Read-only git at pinned commits. No repository writes or network."
 [git_read.parameters]
 command = "Operation to run. No raw commands or user flags."
 repo = "Repository root relative to the workspace. Default: workspace root."
-rev = "Commit, ref, tag, or log range. Default: HEAD."
+rev = "Commit, ref, tag, or log range. Default: HEAD. Strict pinning requires an explicit full commit hash."
 path = "Literal repository-relative path."
 paths = "Literal repository-relative paths."
 pattern = "Extended regular expression for grep. Must not start with a hyphen."
@@ -215,6 +215,7 @@ limit = "Maximum returned items, bounded by configuration."
 recursive = "list_tree: include descendants. Default: false."
 follow = "log: follow renames for one path. Default: false."
 patch = "show_commit: include a bounded patch preview. Default: false."
+include_body = "log and show_commit: include the commit body. Default: false."
 start_line = "blame: first line, one-based. Default: 1."
 end_line = "blame: final line, inclusive. Bounded by max_lines."
 
diff --git c/src/config.rs w/src/config.rs
index d0c0720..3db0bee 100644
--- c/src/config.rs
+++ w/src/config.rs
@@ -11,6 +11,9 @@ use std::sync::Arc;
 #[derive(Debug, Deserialize, Clone)]
 #[serde(default)]
 pub struct GitReadConfig {
+    pub strict_pinning: bool,
+    pub status_directory: Option<String>,
+    pub max_status_metadata_bytes: usize,
     pub max_output_bytes: usize,
     pub max_file_bytes: usize,
     pub max_lines: usize,
@@ -25,6 +28,9 @@ pub struct GitReadConfig {
 impl Default for GitReadConfig {
     fn default() -> Self {
         Self {
+            strict_pinning: false,
+            status_directory: None,
+            max_status_metadata_bytes: 64 * 1024 * 1024,
             max_output_bytes: 1024 * 1024,
             max_file_bytes: 512 * 1024,
             max_lines: 1000,
@@ -50,6 +56,7 @@ impl GitReadConfig {
             ("max_patch_bytes", self.max_patch_bytes),
             ("max_input_bytes", self.max_input_bytes),
             ("max_alternate_dirs", self.max_alternate_dirs),
+            ("max_status_metadata_bytes", self.max_status_metadata_bytes),
         ] {
             if value == 0 {
                 return Err(format!("git_read.{name} must be greater than zero"));
diff --git c/src/plugins/git_read.rs w/src/plugins/git_read.rs
index 3bf1341..c7a182b 100644
--- c/src/plugins/git_read.rs
+++ w/src/plugins/git_read.rs
@@ -1,8 +1,9 @@
-//! Pinned git reads with no writes, network, or repository-controlled programs.
+//! Pinned git reads with no repository writes, network, or repository-controlled programs.
 //! Context: docs/adr/017-git-read-tool.md. The caller supplies the workspace,
 //! not the mutable session cwd. All errors stay structured across frontends.
 
 mod runner;
+mod status;
 #[cfg(test)]
 mod tests;
 use crate::config::{GitReadConfig, ProcessConfig};
@@ -13,6 +14,10 @@ use serde_json::{json, Value};
 use std::collections::HashMap;
 use std::path::Path;
 
+fn is_full_sha1(value: &str) -> bool {
+    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
+}
+
 const OPERATIONS: &[&str] = &[
     "resolve",
     "show_file",
@@ -99,6 +104,7 @@ struct Arguments {
     recursive: bool,
     follow: bool,
     patch: bool,
+    include_body: bool,
     start_line: Option<usize>,
     end_line: Option<usize>,
 }
@@ -130,6 +136,9 @@ impl ToolPlugin for GitReadPlugin {
             let args: Arguments = serde_json::from_value(input)
                 .map_err(|e| Failure::new("rejected_argument", e.to_string()))?;
             args.validate(command)?;
+            if self.limits.strict_pinning {
+                args.validate_pinning(command)?;
+            }
             let repo = ReadRepo::open(
                 workspace,
                 args.repo.as_deref().unwrap_or("."),
@@ -211,6 +220,38 @@ impl Arguments {
         }
         Ok(())
     }
+    fn validate_pinning(&self, command: &str) -> Result<(), Failure> {
+        // ADR-018: strict inputs are commit identifiers, not names resolved after the call starts.
+        if matches!(command, "refs" | "worktree_status") {
+            return Err(Failure::new(
+                "rejected_argument",
+                "strict pinning refuses unpinned operations",
+            ));
+        }
+
+        for value in [&self.rev, &self.a, &self.b].into_iter().flatten() {
+            if !is_full_sha1(value) {
+                return Err(Failure::new(
+                    "rejected_argument",
+                    "strict pinning requires full 40-character SHA-1 commit hashes; SHA-256 is unsupported",
+                ));
+            }
+        }
+        if matches!(command, "diff" | "ancestry") {
+            if self.a.is_none() || self.b.is_none() || self.rev.is_some() {
+                return Err(Failure::new(
+                    "rejected_argument",
+                    "strict pinning requires explicit a and b hashes, without rev",
+                ));
+            }
+        } else if self.rev.is_none() {
+            return Err(Failure::new(
+                "rejected_argument",
+                "strict pinning requires an explicit rev hash",
+            ));
+        }
+        Ok(())
+    }
     fn paths(&self) -> Vec<String> {
         let mut paths = self.paths.clone().unwrap_or_default();
         if let Some(path) = &self.path {
@@ -448,6 +489,7 @@ impl GitReadPlugin {
                     &commit,
                     &Arguments {
                         limit: Some(1),
+                        include_body: args.include_body,
                         ..Default::default()
                     },
                     1,
@@ -577,14 +619,17 @@ impl GitReadPlugin {
                 }
                 let commit = repo.commit("HEAD").await?;
                 let listing = repo
-                    .text(&[
-                        "status",
-                        "--porcelain=v1",
-                        "-z",
-                        "--no-renames",
-                        "--untracked-files=all",
-                        "--ignore-submodules=all",
-                    ])
+                    .worktree_status(
+                        &commit,
+                        &[
+                            "status",
+                            "--porcelain=v1",
+                            "-z",
+                            "--no-renames",
+                            "--untracked-files=all",
+                            "--ignore-submodules=all",
+                        ],
+                    )
                     .await?;
                 let mut entries = Vec::new();
                 for record in listing.split_terminator('\0') {
@@ -721,6 +766,7 @@ pub fn schema() -> Value {
         "a":{"type":"string"}, "b":{"type":"string"}, "mode":{"type":"string","enum":["stat","patch"]},
         "offset":{"type":"integer","minimum":0}, "limit":{"type":"integer","minimum":1},
         "recursive":{"type":"boolean"}, "follow":{"type":"boolean"}, "patch":{"type":"boolean"},
+        "include_body":{"type":"boolean"},
         "start_line":{"type":"integer","minimum":1}, "end_line":{"type":"integer","minimum":1}
     }})
 }
@@ -758,6 +804,11 @@ async fn history(
 ) -> Result<Vec<Value>, Failure> {
     let limit = format!("--max-count={}", args.limit(max_commits).saturating_add(1));
     let skip = format!("--skip={}", args.offset);
+    let format = if args.include_body {
+        "--format=%H%x00%an%x00%aI%x00%cI%x00%s%x00%b"
+    } else {
+        "--format=%H%x00%an%x00%aI%x00%cI%x00%s"
+    };
     let mut argv = vec![
         "log",
         "--no-ext-diff",
@@ -765,7 +816,7 @@ async fn history(
         "--no-show-signature",
         "--no-use-mailmap",
         "--encoding=UTF-8",
-        "--format=%H%x00%an%x00%aI%x00%cI%x00%s",
+        format,
         "-z",
         &limit,
         &skip,
@@ -785,11 +836,16 @@ async fn history(
     let out = repo.text(&argv).await?;
     let fields: Vec<_> = out.split_terminator('\0').collect();
     let mut entries = Vec::new();
-    for record in fields.chunks(5) {
-        if record.len() != 5 {
+    let width = if args.include_body { 6 } else { 5 };
+    for record in fields.chunks(width) {
+        if record.len() != width {
             return Err(Failure::new("git_error", "invalid log record"));
         }
-        entries.push(json!({"sha":record[0],"author":record[1],"author_date":record[2],"committer_date":record[3],"subject":record[4]}));
+        let mut entry = json!({"sha":record[0],"author":record[1],"author_date":record[2],"committer_date":record[3],"subject":record[4]});
+        if args.include_body {
+            entry["body"] = json!(record[5]);
+        }
+        entries.push(entry);
     }
     Ok(entries)
 }
diff --git c/src/plugins/git_read/runner.rs w/src/plugins/git_read/runner.rs
index 91bfcb2..fe0e95c 100644
--- c/src/plugins/git_read/runner.rs
+++ w/src/plugins/git_read/runner.rs
@@ -44,6 +44,11 @@ pub(super) struct ReadRepo {
     pub shallow: bool,
     pub partial: bool,
     pub bare: bool,
+    workspace: PathBuf,
+    gitdir: PathBuf,
+    common: PathBuf,
+    limits: GitReadConfig,
+    filter_overrides: Vec<String>,
     env: HashMap<String, String>,
     process: ProcessConfig,
 }
@@ -197,9 +202,57 @@ impl ReadRepo {
             shallow: false,
             partial: false,
             bare,
+            workspace,
+            gitdir,
+            common,
+            limits: limits.clone(),
+            filter_overrides: Vec::new(),
             env,
             process,
         };
+        // ADR-019: status can run clean/process filters. Disable every
+        // configured driver before any operation that can inspect worktree content.
+        let filters = repo
+            .output(&[
+                "config",
+                "--null",
+                "--name-only",
+                "--get-regexp",
+                "^filter\\..*\\.(clean|smudge|process|required)$",
+            ])
+            .await?;
+        if filters.status.code() != Some(1) {
+            repo.check(&filters)?;
+        }
+        if filters.stdout_truncated
+            || filters.stderr_truncated
+            || filters.stdout.len() > limits.max_input_bytes
+        {
+            return Err(Failure::new(
+                "output_limit",
+                "filter configuration exceeds metadata budget",
+            ));
+        }
+        if !filters.stderr.trim().is_empty() {
+            return Err(Failure::new(
+                "invalid_repository",
+                "cannot safely inspect filter configuration",
+            ));
+        }
+        for key in filters.stdout.split_terminator('\0') {
+            if key.contains('=') || key.chars().any(char::is_control) {
+                return Err(Failure::new(
+                    "invalid_repository",
+                    "unsupported filter configuration key",
+                ));
+            }
+            let value = if key.ends_with(".required") {
+                "false"
+            } else {
+                ""
+            };
+            repo.filter_overrides.push(format!("{key}={value}"));
+        }
         repo.shallow = repo
             .text(&["rev-parse", "--is-shallow-repository"])
             .await?
@@ -237,6 +290,13 @@ impl ReadRepo {
     }
 
     pub async fn output(&self, arguments: &[&str]) -> Result<ManagedOutput, Failure> {
+        self.output_with_env(arguments, &self.env).await
+    }
+    async fn output_with_env(
+        &self,
+        arguments: &[&str],
+        env: &HashMap<String, String>,
+    ) -> Result<ManagedOutput, Failure> {
         let mut argv: Vec<String> = [
             "--no-pager",
             "--no-lazy-fetch",
@@ -259,13 +319,23 @@ impl ReadRepo {
         .iter()
         .map(|s| (*s).into())
         .collect();
+        for setting in &self.filter_overrides {
+            argv.extend(["-c".to_string(), setting.clone()]);
+        }
         argv.extend(arguments.iter().map(|s| (*s).into()));
-        managed_process::run("git", &argv, &self.path, &self.env, &self.process, None)
+        managed_process::run("git", &argv, &self.path, env, &self.process, None)
             .await
             .map_err(Into::into)
     }
     pub async fn text(&self, arguments: &[&str]) -> Result<String, Failure> {
-        let output = self.output(arguments).await?;
+        self.text_with_env(arguments, &self.env).await
+    }
+    async fn text_with_env(
+        &self,
+        arguments: &[&str],
+        env: &HashMap<String, String>,
+    ) -> Result<String, Failure> {
+        let output = self.output_with_env(arguments, env).await?;
         self.check(&output)?;
         if output.stdout_truncated || output.stderr_truncated {
             return Err(Failure::new(
@@ -276,6 +346,59 @@ impl ReadRepo {
         }
         Ok(output.stdout)
     }
+    pub async fn worktree_status(
+        &self,
+        commit: &str,
+        arguments: &[&str],
+    ) -> Result<String, Failure> {
+        let config = self
+            .output(&[
+                "config",
+                "--null",
+                "--get-regexp",
+                super::status::STATUS_CONFIG_PATTERN,
+            ])
+            .await?;
+        if config.status.code() != Some(1) {
+            self.check(&config)?;
+        }
+        if config.stdout_truncated || config.stderr_truncated {
+            return Err(Failure::new(
+                "output_limit",
+                "status configuration exceeds capture limit",
+            ));
+        }
+        if !config.stderr.trim().is_empty() {
+            return Err(Failure::new(
+                "invalid_repository",
+                "cannot freeze status configuration",
+            ));
+        }
+        let view = super::status::StatusView::create(
+            &self.workspace,
+            &self.gitdir,
+            &self.common,
+            commit,
+            &config.stdout,
+            &self.limits,
+        )
+        .await?;
+        let mut env = self.env.clone();
+        env.insert("GIT_DIR".into(), view.path.to_string_lossy().into_owned());
+        env.insert(
+            "GIT_COMMON_DIR".into(),
+            view.path.to_string_lossy().into_owned(),
+        );
+        env.insert(
+            "GIT_INDEX_FILE".into(),
+            view.path.join("index").to_string_lossy().into_owned(),
+        );
+        env.insert(
+            "GIT_OBJECT_DIRECTORY".into(),
+            self.common.join("objects").to_string_lossy().into_owned(),
+        );
+        self.text_with_env(arguments, &env).await
+    }
     pub fn check(&self, output: &ManagedOutput) -> Result<(), Failure> {
         if output.status.success() {
             return Ok(());
@@ -338,7 +461,21 @@ impl ReadRepo {
         self.resolve(revision, "object").await
     }
     pub async fn commit(&self, revision: &str) -> Result<String, Failure> {
-        self.resolve(revision, "commit").await
+        if self.limits.strict_pinning && !super::is_full_sha1(revision) {
+            return Err(Failure::new(
+                "rejected_argument",
+                "strict pinning requires a full SHA-1 commit hash",
+            ));
+        }
+        let commit = self.resolve(revision, "commit").await?;
+        if self.limits.strict_pinning && commit != revision.to_ascii_lowercase() {
+            return Err(Failure::new(
+                "rejected_argument",
+                "strict pinning requires an unpeeled SHA-1 commit object",
+            ));
+        }
+
+        Ok(commit)
     }
     async fn resolve(&self, revision: &str, kind: &str) -> Result<String, Failure> {
         if revision.starts_with('-') || revision.contains('\0') {
diff --git c/src/plugins/git_read/status.rs w/src/plugins/git_read/status.rs
new file mode 100644
index 0000000..b54ccd4
--- /dev/null
+++ w/src/plugins/git_read/status.rs
@@ -0,0 +1,214 @@
+//! Frozen metadata prevents repository-config races from executing status filters.
+//! Context: docs/contexts/git-read/CONTEXT.md and ADR-019.
+
+use super::runner::Failure;
+use crate::config::GitReadConfig;
+use std::path::{Path, PathBuf};
+use tokio::io::AsyncReadExt;
+
+// These data-only settings preserve built-in status normalization. Executable settings,
+// includes, and filter sections never enter the private repository.
+pub(super) const STATUS_CONFIG_PATTERN: &str = r"^(core\.(filemode|ignorecase|symlinks|precomposeunicode|trustctime|checkstat|ignorestat|autocrlf|eol|safecrlf|attributesfile|excludesfile|checkroundtripencoding|sparsecheckout|sparsecheckoutcone|protectntfs|protecthfs)|index\.(sparse|version))$";
+
+pub(super) struct StatusView {
+    pub path: PathBuf,
+    remaining: u64,
+}
+
+impl Drop for StatusView {
+    fn drop(&mut self) {
+        if let Err(error) = std::fs::remove_dir_all(&self.path) {
+            tracing::warn!(target:"daimonos::git_read",event="status_metadata_cleanup_failed",%error);
+        }
+    }
+}
+
+impl StatusView {
+    pub async fn create(
+        workspace: &Path,
+        gitdir: &Path,
+        common: &Path,
+        commit: &str,
+        config: &str,
+        limits: &GitReadConfig,
+    ) -> Result<Self, Failure> {
+        let directory = limits
+            .status_directory
+            .as_deref()
+            .map(crate::paths::expand_tilde)
+            .unwrap_or_else(std::env::temp_dir);
+        let directory = if directory.is_absolute() {
+            directory
+        } else {
+            workspace.join(directory)
+        };
+        let directory = tokio::fs::canonicalize(&directory).await.map_err(|error| {
+            Failure::new(
+                "io_error",
+                format!("git_read.status_directory {}: {error}", directory.display()),
+            )
+        })?;
+        if directory.starts_with(workspace) {
+            return Err(Failure::new(
+                "rejected_argument",
+                format!(
+                    "git_read.status_directory must be outside the workspace: {}",
+                    directory.display()
+                ),
+            ));
+        }
+        let path = directory.join(format!("daimonos-git-status-{}", uuid::Uuid::new_v4()));
+        // Acquire directory ownership without an await between creation and the drop guard.
+        let mut builder = std::fs::DirBuilder::new();
+        #[cfg(unix)]
+        {
+            use std::os::unix::fs::DirBuilderExt;
+            builder.mode(0o700);
+        }
+        builder.create(&path).map_err(|error| {
+            Failure::new(
+                "io_error",
+                format!("git_read.status_directory {}: {error}", directory.display()),
+            )
+        })?;
+        let mut view = Self {
+            path,
+            remaining: limits.max_status_metadata_bytes as u64,
+        };
+        tokio::fs::create_dir(view.path.join("refs")).await?;
+        tokio::fs::create_dir(view.path.join("info")).await?;
+        let format = match commit.len() {
+            40 => "[core]\nrepositoryformatversion=0\nbare=false\n",
+            64 => {
+                "[core]\nrepositoryformatversion=1\nbare=false\n[extensions]\nobjectformat=sha256\n"
+            }
+            _ => {
+                return Err(Failure::new(
+                    "git_error",
+                    "unsupported commit identifier format",
+                ))
+            }
+        };
+        let mut settings = format.to_string();
+        for record in config.split_terminator('\0') {
+            let (key, value) = record.split_once('\n').unwrap_or((record, "true"));
+            let (section, name) = key
+                .split_once('.')
+                .ok_or_else(|| Failure::new("git_error", "invalid status configuration"))?;
+            let allowed = match section {
+                "core" => matches!(
+                    name,
+                    "filemode"
+                        | "ignorecase"
+                        | "symlinks"
+                        | "precomposeunicode"
+                        | "trustctime"
+                        | "checkstat"
+                        | "ignorestat"
+                        | "autocrlf"
+                        | "eol"
+                        | "safecrlf"
+                        | "attributesfile"
+                        | "excludesfile"
+                        | "checkroundtripencoding"
+                        | "sparsecheckout"
+                        | "sparsecheckoutcone"
+                        | "protectntfs"
+                        | "protecthfs"
+                ),
+                "index" => matches!(name, "sparse" | "version"),
+                _ => false,
+            };
+            if !allowed {
+                return Err(Failure::new(
+                    "git_error",
+                    "unexpected status configuration key",
+                ));
+            }
+            let value = value
+                .replace('\\', "\\\\")
+                .replace('"', "\\\"")
+                .replace('\n', "\\n")
+                .replace('\t', "\\t");
+            settings.push_str(&format!("[{section}]\n{name}=\"{value}\"\n"));
+        }
+        view.write("config", settings.as_bytes()).await?;
+        view.write("HEAD", format!("{commit}\n").as_bytes()).await?;
+        view.copy(&gitdir.join("index"), "index", workspace).await?;
+        for name in ["attributes", "exclude"] {
+            view.copy(
+                &common.join("info").join(name),
+                &format!("info/{name}"),
+                workspace,
+            )
+            .await?;
+        }
+        view.copy(
+            &gitdir.join("info/sparse-checkout"),
+            "info/sparse-checkout",
+            workspace,
+        )
+        .await?;
+        let mut entries = tokio::fs::read_dir(gitdir).await?;
+        while let Some(entry) = entries.next_entry().await? {
+            let name = entry.file_name();
+            view.charge(name.len() as u64)?;
+            if name.to_string_lossy().starts_with("sharedindex.") {
+                let name = name
+                    .to_str()
+                    .ok_or_else(|| Failure::new("git_error", "invalid shared-index name"))?;
+                view.copy(&entry.path(), name, workspace).await?;
+            }
+        }
+        Ok(view)
+    }
+
+    fn charge(&mut self, bytes: u64) -> Result<(), Failure> {
+        self.remaining = self.remaining.checked_sub(bytes).ok_or_else(|| {
+            Failure::new(
+                "output_limit",
+                "status metadata exceeds git_read.max_status_metadata_bytes",
+            )
+        })?;
+        Ok(())
+    }
+    async fn write(&mut self, name: &str, bytes: &[u8]) -> Result<(), Failure> {
+        self.charge(bytes.len() as u64)?;
+        let mut options = tokio::fs::OpenOptions::new();
+        options.write(true).create_new(true);
+        #[cfg(unix)]
+        options.mode(0o600);
+        let mut file = options.open(self.path.join(name)).await?;
+        tokio::io::AsyncWriteExt::write_all(&mut file, bytes).await?;
+        Ok(())
+    }
+    async fn copy(&mut self, source: &Path, name: &str, workspace: &Path) -> Result<(), Failure> {
+        match tokio::fs::symlink_metadata(source).await {
+            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
+            Err(error) => return Err(error.into()),
+            Ok(_) => {}
+        }
+        let source = tokio::fs::canonicalize(source).await?;
+        if !source.starts_with(workspace) {
+            return Err(Failure::new(
+                "repo_outside_workspace",
+                "status metadata points outside workspace",
+            ));
+        }
+        let file = tokio::fs::File::open(&source).await?;
+        if !file.metadata().await?.is_file() {
+            return Err(Failure::new(
+                "invalid_repository",
+                "status metadata is not a regular file",
+            ));
+        }
+        let mut input = file.take(self.remaining.saturating_add(1));
+        let mut options = tokio::fs::OpenOptions::new();
+        options.write(true).create_new(true);
+        #[cfg(unix)]
+        options.mode(0o600);
+        let mut output = options.open(self.path.join(name)).await?;
+        let bytes = tokio::io::copy(&mut input, &mut output).await?;
+        self.charge(bytes)
+    }
+}
diff --git c/src/plugins/git_read/tests.rs w/src/plugins/git_read/tests.rs
index 547eba9..5c54039 100644
--- c/src/plugins/git_read/tests.rs
+++ w/src/plugins/git_read/tests.rs
@@ -127,9 +127,9 @@ async fn every_operation_preserves_git_and_worktree_even_when_stat_stale() {
         .unwrap()
         .set_times(fs::FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(5)))
         .unwrap();
-    for args in f.cases() {
+    for (args, limits) in f.policy_cases() {
         let before = snapshot(&f.repo);
-        let response = f.call(args.clone()).await;
+        let response = f.with_limits(args.clone(), limits).await;
         assert_eq!(response.exit_code, 0, "{args}: {}", response.output);
         assert_eq!(snapshot(&f.repo), before, "mutation in {args}");
         assert!(response.output["repo"].as_str().is_some());
@@ -166,9 +166,40 @@ async fn every_operation_ignores_malicious_config() {
         &f.repo,
         &["config", "diff.trap.textconv", script.to_str().unwrap()],
     );
-    fs::write(f.repo.join(".gitattributes"), "*.txt diff=trap\n").unwrap();
-    for args in f.cases() {
-        let response = f.call(args.clone()).await;
+    git(
+        &f.repo,
+        &["config", "filter.trap.clean", script.to_str().unwrap()],
+    );
+    git(
+        &f.repo,
+        &["config", "filter.trap.smudge", script.to_str().unwrap()],
+    );
+    git(
+        &f.repo,
+        &["config", "filter.trap.process", script.to_str().unwrap()],
+    );
+    git(&f.repo, &["config", "filter.trap.required", "true"]);
+    git(
+        &f.repo,
+        &["config", "alias.show", &format!("!{}", script.display())],
+    );
+    let hooks = f.workspace.path().join("hooks");
+    fs::create_dir(&hooks).unwrap();
+    for hook in ["post-index-change", "post-checkout", "pre-auto-gc"] {
+        fs::copy(&script, hooks.join(hook)).unwrap();
+    }
+    git(
+        &f.repo,
+        &["config", "core.hooksPath", hooks.to_str().unwrap()],
+    );
+    fs::write(
+        f.repo.join(".gitattributes"),
+        "*.txt diff=trap filter=trap\n",
+    )
+    .unwrap();
+    fs::write(f.repo.join("renamed.txt"), "dirty working-tree content\n").unwrap();
+    for (args, limits) in f.policy_cases() {
+        let response = f.with_limits(args.clone(), limits).await;
         assert_eq!(response.exit_code, 0, "{args}: {}", response.output);
         assert!(!sentinel.exists(), "repo code ran in {args}");
     }
@@ -648,3 +679,431 @@ async fn registered_plugin_uses_the_smaller_git_and_shared_response_caps() {
         assert_eq!(snapshot(&f.repo), before);
     }
 }
+
+#[tokio::test]
+async fn strict_pinning_refuses_a_branch_while_the_default_keeps_it() {
+    let f = Fixture::new();
+    let args = json!({"command":"resolve","repo":"source","rev":"main"});
+    assert_eq!(f.call(args.clone()).await.exit_code, 0);
+    let strict = GitReadConfig {
+        strict_pinning: true,
+        ..Default::default()
+    };
+    let response = f.with_limits(args, strict).await;
+    assert_eq!(
+        response.output["error"]["kind"], "rejected_argument",
+        "{}",
+        response.output
+    );
+}
+
+#[tokio::test]
+async fn strict_pinning_requires_commit_objects_not_annotated_tag_objects() {
+    let f = Fixture::new();
+    git(
+        &f.repo,
+        &[
+            "tag",
+            "-a",
+            "policy-tag",
+            "-m",
+            "Tag object for strict policy",
+        ],
+    );
+    let tag = git(&f.repo, &["rev-parse", "refs/tags/policy-tag"]);
+    assert_eq!(
+        f.call(json!({"command":"resolve","repo":"source","rev":tag}))
+            .await
+            .exit_code,
+        0
+    );
+    let strict = GitReadConfig {
+        strict_pinning: true,
+        ..Default::default()
+    };
+    let response = f
+        .with_limits(
+            json!({"command":"resolve","repo":"source","rev":tag}),
+            strict,
+        )
+        .await;
+    assert_eq!(
+        response.output["error"]["kind"], "rejected_argument",
+        "{}",
+        response.output
+    );
+}
+
+impl Fixture {
+    fn policy_cases(&self) -> Vec<(Value, GitReadConfig)> {
+        let mut cases: Vec<_> = self
+            .cases()
+            .into_iter()
+            .map(|args| (args, GitReadConfig::default()))
+            .collect();
+        for mut args in self.cases() {
+            let command = args["command"].as_str().unwrap();
+            if matches!(command, "refs" | "worktree_status") {
+                continue;
+            }
+            if !matches!(command, "diff" | "ancestry")
+                && args
+                    .get("rev")
+                    .and_then(Value::as_str)
+                    .is_none_or(|rev| rev.len() != 40)
+            {
+                args["rev"] = json!(self.last);
+            }
+            cases.push((
+                args,
+                GitReadConfig {
+                    strict_pinning: true,
+                    ..Default::default()
+                },
+            ));
+        }
+        cases
+    }
+}
+
+#[tokio::test]
+async fn strict_pinning_rejects_all_unpinned_revision_forms_and_operations() {
+    let f = Fixture::new();
+    let strict = GitReadConfig {
+        strict_pinning: true,
+        ..Default::default()
+    };
+    for command in OPERATIONS {
+        if matches!(*command, "refs" | "worktree_status") {
+            let result = f
+                .with_limits(
+                    json!({"command":command,"repo":"source","rev":f.last}),
+                    strict.clone(),
+                )
+                .await;
+            assert_eq!(result.output["error"]["kind"], "rejected_argument");
+            continue;
+        }
+        for revision in [
+            None,
+            Some("HEAD"),
+            Some("HEAD~1"),
+            Some("main"),
+            Some("review"),
+            Some("abcd123"),
+            Some("HEAD..main"),
+        ] {
+            let mut args = json!({"command":command,"repo":"source","path":"renamed.txt"});
+            if matches!(*command, "diff" | "ancestry") {
+                args["b"] = json!(f.last);
+                if let Some(revision) = revision {
+                    args["a"] = json!(revision);
+                }
+            } else if let Some(revision) = revision {
+                args["rev"] = json!(revision);
+            }
+            let result = f.with_limits(args.clone(), strict.clone()).await;
+            assert_eq!(
+                result.output["error"]["kind"], "rejected_argument",
+                "{args}: {}",
+                result.output
+            );
+        }
+    }
+}
+
+#[tokio::test]
+async fn commit_bodies_are_additive_and_do_not_change_default_log_fields() {
+    let f = Fixture::new();
+    git(
+        &f.repo,
+        &[
+            "commit",
+            "--allow-empty",
+            "-m",
+            "Body fixture",
+            "-m",
+            "First body line.\nSecond body line.",
+        ],
+    );
+    let ordinary = f
+        .call(json!({"command":"log","repo":"source","limit":1}))
+        .await;
+    assert!(ordinary.output["entries"][0].get("body").is_none());
+    for command in ["log", "show_commit"] {
+        let result = f
+            .call(json!({"command":command,"repo":"source","limit":1,"include_body":true}))
+            .await;
+        assert_eq!(result.exit_code, 0, "{}", result.output);
+        let commit = if command == "log" {
+            &result.output["entries"][0]
+        } else {
+            &result.output["commit"]
+        };
+        assert_eq!(commit["body"], "First body line.\nSecond body line.\n");
+        assert_eq!(commit["subject"], "Body fixture");
+    }
+}
+
+#[tokio::test]
+async fn default_status_reports_raw_bytes_without_running_normalizing_filters() {
+    let f = Fixture::new();
+    let filtered = f.repo.join("filtered.txt");
+    fs::write(&filtered, "worktree content\n").unwrap();
+    fs::write(
+        f.repo.join(".gitattributes"),
+        "filtered.txt filter=normalize\n",
+    )
+    .unwrap();
+    git(
+        &f.repo,
+        &[
+            "config",
+            "filter.normalize.clean",
+            "sed s/worktree/canonical/g",
+        ],
+    );
+    git(&f.repo, &["config", "filter.normalize.required", "true"]);
+    git(&f.repo, &["add", "filtered.txt", ".gitattributes"]);
+    git(&f.repo, &["commit", "-m", "Normalizing filter fixture"]);
+    assert_eq!(
+        git(&f.repo, &["show", "HEAD:filtered.txt"]),
+        "canonical content"
+    );
+    assert_eq!(fs::read_to_string(&filtered).unwrap(), "worktree content\n");
+    // Force a content check without allowing ordinary Git to refresh the index.
+    fs::File::open(&filtered)
+        .unwrap()
+        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(5)))
+        .unwrap();
+    let before = snapshot(&f.repo);
+    assert_eq!(
+        git(
+            &f.repo,
+            &["--no-optional-locks", "status", "--porcelain=v1"]
+        ),
+        ""
+    );
+    assert_eq!(snapshot(&f.repo), before);
+    let result = f
+        .call(json!({"command":"worktree_status","repo":"source"}))
+        .await;
+    assert_eq!(result.exit_code, 0, "{}", result.output);
+    assert_eq!(result.output["clean"], false);
+    assert!(result.output["entries"]
+        .as_array()
+        .unwrap()
+        .iter()
+        .any(|entry| entry["path"] == "filtered.txt" && entry["status"] == " M"));
+    assert_eq!(snapshot(&f.repo), before);
+}
+
+#[tokio::test]
+async fn mixed_case_filter_configuration_is_suppressed_under_both_policies() {
+    use std::io::Write;
+    let f = Fixture::new();
+    let marker = f.workspace.path().join("mixed-filter-executed");
+    let command = format!("printf executed > '{}'", marker.display());
+    let mut config = fs::OpenOptions::new()
+        .append(true)
+        .open(f.repo.join(".git/config"))
+        .unwrap();
+    writeln!(
+        config,
+        "[FILTER \"MiXeD\"]\nClEaN = {command}\nPrOcEsS = {command}\nReQuIrEd = true"
+    )
+    .unwrap();
+    drop(config);
+    fs::write(f.repo.join(".gitattributes"), "*.txt filter=MiXeD\n").unwrap();
+    let content = fs::read(f.repo.join("renamed.txt")).unwrap();
+    fs::write(f.repo.join("renamed.txt"), vec![b'X'; content.len()]).unwrap();
+    let keys = git(
+        &f.repo,
+        &["config", "--name-only", "--get-regexp", "^filter\\."],
+    );
+    assert!(keys.contains("filter.MiXeD.clean"), "{keys}");
+    assert!(keys.contains("filter.MiXeD.process"), "{keys}");
+    let before = snapshot(&f.repo);
+    for (args, limits) in f.policy_cases() {
+        let result = f.with_limits(args.clone(), limits).await;
+        assert_eq!(result.exit_code, 0, "{args}: {}", result.output);
+        assert!(!marker.exists(), "a mixed-case filter executed");
+    }
+    assert_eq!(snapshot(&f.repo), before);
+}
+
+#[tokio::test]
+async fn strict_pair_operations_reject_annotated_tags_on_both_sides() {
+    let f = Fixture::new();
+    git(&f.repo, &["tag", "-a", "pair-tag", "-m", "Pair input tag"]);
+    let tag = git(&f.repo, &["rev-parse", "refs/tags/pair-tag"]);
+    let strict = GitReadConfig {
+        strict_pinning: true,
+        ..Default::default()
+    };
+    let before = snapshot(&f.repo);
+    for command in ["diff", "ancestry"] {
+        for (a, b) in [(&tag, &f.last), (&f.first, &tag)] {
+            let args = json!({"command":command,"repo":"source","a":a,"b":b});
+            let ordinary = f.call(args.clone()).await;
+            assert_eq!(ordinary.exit_code, 0, "{}", ordinary.output);
+            let pinned = f.with_limits(args.clone(), strict.clone()).await;
+            assert_eq!(
+                pinned.output["error"]["kind"], "rejected_argument",
+                "{args}: {}",
+                pinned.output
+            );
+        }
+    }
+    assert_eq!(snapshot(&f.repo), before);
+}
+
+#[tokio::test]
+async fn malformed_filter_configuration_fails_structurally_without_mutation() {
+    use std::io::Write;
+    let f = Fixture::new();
+    let mut config = fs::OpenOptions::new()
+        .append(true)
+        .open(f.repo.join(".git/config"))
+        .unwrap();
+    writeln!(config, "[unterminated").unwrap();
+    drop(config);
+    let before = snapshot(&f.repo);
+    for args in [
+        json!({"command":"worktree_status","repo":"source"}),
+        json!({"command":"show_file","repo":"source","rev":f.first,"path":"old.txt"}),
+    ] {
+        let result = f.call(args).await;
+        assert_ne!(result.exit_code, 0);
+        assert_eq!(
+            result.output["error"]["kind"], "git_error",
+            "{}",
+            result.output
+        );
+        assert!(result.output["error"]["message"]
+            .as_str()
+            .unwrap()
+            .contains("config"));
+    }
+    assert_eq!(snapshot(&f.repo), before);
+}
+
+#[tokio::test]
+async fn private_status_metadata_is_bounded_and_removed_after_failure() {
+    let f = Fixture::new();
+    let state = tempfile::tempdir().unwrap();
+    let limits = GitReadConfig {
+        status_directory: Some(state.path().to_string_lossy().into_owned()),
+        max_status_metadata_bytes: 1,
+        ..Default::default()
+    };
+    let before = snapshot(&f.repo);
+    let result = f
+        .with_limits(json!({"command":"worktree_status","repo":"source"}), limits)
+        .await;
+    assert_eq!(
+        result.output["error"]["kind"], "output_limit",
+        "{}",
+        result.output
+    );
+    assert_eq!(fs::read_dir(state.path()).unwrap().count(), 0);
+    assert_eq!(snapshot(&f.repo), before);
+    let unsafe_directory = GitReadConfig {
+        status_directory: Some(f.repo.to_string_lossy().into_owned()),
+        ..Default::default()
+    };
+    let refused = f
+        .with_limits(
+            json!({"command":"worktree_status","repo":"source"}),
+            unsafe_directory,
+        )
+        .await;
+    assert_eq!(refused.output["error"]["kind"], "rejected_argument");
+    assert_eq!(snapshot(&f.repo), before);
+}
+
+#[tokio::test]
+async fn frozen_status_preserves_builtin_normalization_split_index_and_local_excludes() {
+    let f = Fixture::new();
+    git(&f.repo, &["config", "core.autocrlf", "true"]);
+    fs::write(f.repo.join(".git/info/attributes"), "eol.txt text\n").unwrap();
+    fs::write(f.repo.join("eol.txt"), "first\r\nsecond\r\n").unwrap();
+    git(&f.repo, &["add", "eol.txt"]);
+    git(&f.repo, &["commit", "-m", "Built-in normalization fixture"]);
+    fs::File::open(f.repo.join("eol.txt"))
+        .unwrap()
+        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(5)))
+        .unwrap();
+    fs::write(f.repo.join(".git/info/exclude"), "ignored.local\n").unwrap();
+    fs::write(f.repo.join("ignored.local"), "ignored\n").unwrap();
+    git(&f.repo, &["update-index", "--split-index"]);
+    assert_eq!(
+        git(
+            &f.repo,
+            &["--no-optional-locks", "status", "--porcelain=v1"]
+        ),
+        ""
+    );
+    let before = snapshot(&f.repo);
+    let result = f
+        .call(json!({"command":"worktree_status","repo":"source"}))
+        .await;
+    assert_eq!(result.exit_code, 0, "{}", result.output);
+    assert_eq!(result.output["clean"], true, "{}", result.output);
+    assert_eq!(snapshot(&f.repo), before);
+}
+
+#[tokio::test]
+async fn frozen_status_handles_linked_worktrees_reftable_and_sha256() {
+    let f = Fixture::new();
+    git(
+        &f.repo,
+        &["worktree", "add", "--detach", "../linked", &f.last],
+    );
+    git(f.workspace.path(), &["clone", "source", "reftable"]);
+    git(
+        &f.workspace.path().join("reftable"),
+        &["refs", "migrate", "--ref-format=reftable"],
+    );
+    git(
+        f.workspace.path(),
+        &["init", "--object-format=sha256", "sha256"],
+    );
+    let sha256 = f.workspace.path().join("sha256");
+    git(&sha256, &["config", "user.name", "Format Fixture"]);
+    git(&sha256, &["config", "user.email", "format@example.test"]);
+    git(&sha256, &["config", "commit.gpgsign", "false"]);
+    fs::write(sha256.join("file.txt"), "format fixture\n").unwrap();
+    git(&sha256, &["add", "."]);
+    git(&sha256, &["commit", "-m", "Format fixture"]);
+    let before = snapshot(f.workspace.path());
+    for repo in ["linked", "reftable", "sha256"] {
+        let result = f
+            .call(json!({"command":"worktree_status","repo":repo}))
+            .await;
+        assert_eq!(result.exit_code, 0, "{repo}: {}", result.output);
+        assert_eq!(result.output["clean"], true, "{repo}: {}", result.output);
+        assert_eq!(
+            result.output["commits"][0].as_str().unwrap().len(),
+            if repo == "sha256" { 64 } else { 40 }
+        );
+    }
+    assert_eq!(snapshot(f.workspace.path()), before);
+}
+
+#[tokio::test]
+async fn strict_uppercase_hash_inputs_return_canonical_lowercase_commits() {
+    let f = Fixture::new();
+    let strict = GitReadConfig {
+        strict_pinning: true,
+        ..Default::default()
+    };
+    let result = f
+        .with_limits(
+            json!({"command":"resolve","repo":"source","rev":f.last.to_ascii_uppercase()}),
+            strict,
+        )
+        .await;
+    assert_eq!(result.exit_code, 0, "{}", result.output);
+    assert_eq!(result.output["commits"], json!([f.last]));
+}
diff --git c/src/tool_descriptions.rs w/src/tool_descriptions.rs
index 088518c..1e62cbb 100644
--- c/src/tool_descriptions.rs
+++ w/src/tool_descriptions.rs
@@ -11,7 +11,7 @@ pub const DEFAULT_TEXT: &str = include_str!("../prompts/tool_descriptions.toml")
 /// deletion fails parity tests. Future intentional additions/removals update
 /// this single value.
 #[cfg(test)]
-pub(crate) const DEFAULT_PARAMETER_DESCRIPTION_COUNT: usize = 156;
+pub(crate) const DEFAULT_PARAMETER_DESCRIPTION_COUNT: usize = 157;
 #[cfg(test)]
 pub(crate) const AGENT_ONLY_PARAMETER_DESCRIPTION_COUNT: usize = 2;
 #[cfg(test)]
diff --git c/tests/test_git_read.py w/tests/test_git_read.py
index 88b11ea..743107f 100644
--- c/tests/test_git_read.py
+++ w/tests/test_git_read.py
@@ -292,3 +292,135 @@ def test_git_read_short_inheritance_prefixes_do_not_leak_git_controls(daimonos_f
         response=client.call_tool("git_read",{"command":"resolve","repo":"source"})
         assert not response.get("isError"),response
         assert not trace.exists(),f"git controls leaked through prefix {prefix!r}"
+
+
+def test_git_read_strict_policy_keeps_default_resolution_and_adds_bodies(daimonos_factory,tmp_path):
+    repo,first=repository(tmp_path)
+    git(repo,"commit","--allow-empty","-m","Body fixture","-m","Stored body.")
+    commit=git(repo,"rev-parse","HEAD")
+    ordinary=daimonos_factory(tmp_path)
+    assert not ordinary.call_tool("git_read",{"command":"resolve","repo":"source","rev":"HEAD"}).get("isError")
+    (tmp_path/"daimonos.toml").write_text("[git_read]\nstrict_pinning=true\n")
+    strict=daimonos_factory(tmp_path)
+    for revision in (None,"HEAD","main",commit[:10],"HEAD~1",f"{first}..{commit}"):
+        args={"command":"resolve","repo":"source"}
+        if revision is not None:
+            args["rev"]=revision
+        response=strict.call_tool("git_read",args)
+        assert response.get("isError"),response
+        assert json.loads(response["content"][0]["text"])["error"]["kind"]=="rejected_argument"
+    resolved=strict.call_tool("git_read",{"command":"resolve","repo":"source","rev":commit})
+    assert not resolved.get("isError"),resolved
+    assert json.loads(resolved["content"][0]["text"])["commits"]==[commit]
+    history=strict.call_tool("git_read",{"command":"log","repo":"source","rev":commit,"include_body":True,"limit":1})
+    assert not history.get("isError"),history
+    assert json.loads(history["content"][0]["text"])["entries"][0]["body"]=="Stored body.\n"
+    assert not ordinary.call_tool("git_read",{"command":"resolve","repo":"source","rev":"HEAD"}).get("isError")
+
+
+def test_git_read_strict_operations_on_a_real_read_only_bind_mount(daimonos_binary,tmp_path):
+    import errno
+    import os
+    import shutil
+    import sys
+    import pytest
+
+    if sys.platform!="linux" or not shutil.which("unshare"):
+        pytest.skip("requires Linux user and mount namespaces")
+    supported=subprocess.run(["unshare","-Urnm","true"],capture_output=True,text=True)
+    if supported.returncode:
+        pytest.skip("user or mount namespaces are unavailable")
+    workspace=tmp_path/"evidence"
+    workspace.mkdir()
+    _,commit=repository(workspace)
+    (workspace/"daimonos.toml").write_text("[git_read]\nstrict_pinning=true\n[logging]\nenabled=false\n[analytics]\nenabled=false\n")
+    source_tests=str(Path(__file__).resolve().parent)
+    program=r'''
+import errno,hashlib,json,os,subprocess,sys
+from pathlib import Path
+sys.path.insert(0,sys.argv[1])
+from conftest import DaimonosClient
+binary,workspace,commit=sys.argv[2:]
+workspace=Path(workspace)
+def snapshot():
+    return [(str(p.relative_to(workspace)),p.lstat().st_size,p.lstat().st_mtime_ns,hashlib.sha256(p.read_bytes()).hexdigest() if p.is_file() else None) for p in sorted(workspace.rglob("*"))]
+subprocess.run(["mount","--bind",str(workspace),str(workspace)],check=True)
+subprocess.run(["mount","-o","remount,bind,ro",str(workspace)],check=True)
+try:
+    (workspace/"must-not-write").write_text("forbidden")
+    raise AssertionError("the mount is not read-only")
+except OSError as error:
+    assert error.errno==errno.EROFS,error
+before=snapshot()
+process=subprocess.Popen([binary,"--mcp","-w",str(workspace)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
+try:
+    client=DaimonosClient(process,str(workspace))
+    client.send_raw({"jsonrpc":"2.0","id":client._next_id(),"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"readonly-probe","version":"1"}}})
+    client.send_raw({"jsonrpc":"2.0","method":"notifications/initialized"})
+    calls=[{"command":"resolve"},{"command":"show_file","path":"file.txt"},{"command":"list_tree"},{"command":"grep","pattern":"first"},{"command":"log","include_body":True},{"command":"show_commit","include_body":True},{"command":"diff","a":commit,"b":commit},{"command":"blame","path":"file.txt"},{"command":"ancestry","a":commit,"b":commit}]
+    for args in calls:
+        if args["command"] not in ("diff","ancestry"):
+            args["rev"]=commit
+        response=client.call_tool("git_read",{"repo":"source",**args})
+        assert not response.get("isError"),(args,response)
+        assert commit in json.loads(response["content"][0]["text"])["commits"]
+    for command in ("refs","worktree_status"):
+        response=client.call_tool("git_read",{"repo":"source","command":command})
+        assert response.get("isError"),response
+        assert json.loads(response["content"][0]["text"])["error"]["kind"]=="rejected_argument"
+finally:
+    process.terminate()
+    process.wait(timeout=5)
+assert snapshot()==before,"the evidence changed"
+print(json.dumps({"read_only_mount":True,"pinned_operations":9,"refused_operations":2,"unchanged":True}))
+'''
+    env=os.environ.copy()
+    env["DAIMONOS_KGL_AUTOINDEX"]="0"
+    result=subprocess.run(["unshare","-Urnm",sys.executable,"-c",program,source_tests,str(daimonos_binary),str(workspace),commit],env=env,capture_output=True,text=True,timeout=60)
+    assert result.returncode==0,result.stdout+result.stderr
+    assert json.loads(result.stdout)=={"read_only_mount":True,"pinned_operations":9,"refused_operations":2,"unchanged":True}
+
+
+def test_git_read_filter_added_after_inspection_cannot_execute(daimonos_factory,tmp_path):
+    import os
+    import shutil
+    import sys
+
+    repo,_=repository(tmp_path)
+    included=tmp_path/"late-filter.cfg"
+    included.write_text("")
+    marker=tmp_path/"late-filter-executed"
+    malicious=tmp_path/"late-filter.sh"
+    malicious.write_text(f"#!/bin/sh\nprintf executed > '{marker}'\n")
+    malicious.chmod(0o755)
+    git(repo,"config","include.path",str(included))
+    (repo/".gitattributes").write_text("file.txt filter=late\n")
+    git(repo,"add",".gitattributes")
+    git(repo,"commit","-m","Late filter attribute")
+    original=(repo/"file.txt").read_bytes()
+    (repo/"file.txt").write_bytes(b"X"*len(original))
+    real_git=shutil.which("git")
+    assert real_git
+    wrapper_dir=tmp_path/"git-wrapper"
+    wrapper_dir.mkdir()
+    wrapper=wrapper_dir/"git"
+    payload=f'[filter "late"]\nprocess = {malicious}\nrequired = true\n'
+    trigger=tmp_path/"status-boundary-reached"
+    wrapper.write_text(
+        f"#!{sys.executable}\nimport pathlib,subprocess,sys\n"
+        f"if 'status' in sys.argv[1:]:\n    pathlib.Path({str(included)!r}).write_text({payload!r})\n    pathlib.Path({str(trigger)!r}).write_text('triggered')\n"
+        f"sys.exit(subprocess.run([{real_git!r},*sys.argv[1:]]).returncode)\n"
+    )
+    wrapper.chmod(0o755)
+    client=daimonos_factory(tmp_path,env={"PATH":str(wrapper_dir)+os.pathsep+os.environ["PATH"]})
+    trigger.unlink(missing_ok=True)
+    included.write_text(payload)
+    baseline=subprocess.run([real_git,"--no-optional-locks","status","--porcelain=v1"],cwd=repo,capture_output=True,text=True)
+    assert marker.exists(),baseline.stdout+baseline.stderr
+    marker.unlink()
+    included.write_text("")
+    response=client.call_tool("git_read",{"command":"worktree_status","repo":"source"})
+    assert trigger.exists(),"the process-boundary race did not run"
+    assert included.read_text()==payload
+    assert not marker.exists(),"a filter introduced after the inspection executed"
+    assert not response.get("isError"),response

### New Points
R1. The revised artifact is the complete FR-06 slice relative to fabb08c, including the newly added status.rs module, new metadata controls, architecture records, all tests, and validation evidence. No unrelated feature implementation is claimed.
R2. Fresh checks passed after all production changes: fmt; clippy with -D warnings; cargo test with 2011 passed and 10 ignored; pytest with 385 passed and 1 skipped. docs/reviews/2026-10-28-fr06-validation.md records the exact scope, commands, logs, and platform limitations. The Linux EROFS mount test ran. No macOS result is claimed.
R3. Suggestions: S1 is covered by an uppercase-input/canonical-output regression. S2 is documented as preserving Git body text including any trailing newline, rather than promising every body ends in one. S3 cannot change to repository-dependent lengths because the accepted PM contract explicitly requires strict SHA-1; the error now names SHA-1 and unsupported SHA-256, while default SHA-256 status is covered. S4 is covered by explicit Linux-only execution/skip evidence in the validation artifact.
R4. New state controls are git_read.status_directory, an optional existing directory outside the workspace, and git_read.max_status_metadata_bytes, default 64 MiB. The default temporary directory is only this slice's legacy path. FR-08 will integrate it with the explicit state root. Metadata budget failure and cleanup have a public regression. No crate dependency was added.

**Requesting:** RE-REVIEW

<!-- event id="review-2" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 2
**Position:** PARTIAL

### Blocking Concerns
B5. B1 follow-up — the frozen StatusView closes the config race for the drivers themselves, and the include.path regression (test_git_read_filter_added_after_inspection_cannot_execute) demonstrates it, so the core of B1 is addressed. However the frozen view does not freeze worktree-level attribute sources: `git status` in the child still reads .gitattributes files from the live working tree (the view only copies info/attributes and info/exclude). That is safe only because the frozen config contains no [filter] sections, so a `filter=X` attribute resolves to an undefined driver. Git's behavior for an undefined-but-required driver differs from a defined-optional one; the frozen config also deliberately omits filter.*.required overrides. Confirm with a test (or cite the existing one) that a worktree .gitattributes declaring `filter=X required` semantics cannot cause the child status to fail open or to fall back to any inherited config scope: specifically, the child process still reads the user/global/system config scopes unless GIT_CONFIG_GLOBAL/GIT_CONFIG_SYSTEM (or GIT_CONFIG_NOSYSTEM) are forced. The diff shows GIT_DIR/GIT_COMMON_DIR/GIT_INDEX_FILE/GIT_OBJECT_DIRECTORY being set, but I cannot see where global/system config is suppressed for the status child. If self.env already strips HOME/forces GIT_CONFIG_NOSYSTEM from the pre-existing hardening, state that explicitly and add an assertion-level test that a filter driver defined in a global config scope does not execute during frozen status; otherwise a user-scope [filter] section re-arms exactly the execution path ADR-019 forbids.
B6. StatusView::create copies gitdir.join("index") and scans gitdir for sharedindex.* files, but in a linked worktree the sharedindex files live in the commondir (gitdir of a linked worktree is .git/worktrees/<name>, while shared index files are written next to the index — verify which applies for split-index in linked worktrees), and the frozen_status_handles_linked_worktrees_reftable_and_sha256 test exercises a linked worktree but not a linked worktree WITH split-index enabled. Additionally, GIT_COMMON_DIR is pointed at the private view, which means the child cannot see the real commondir's refs/packed-refs/reftable — status mostly needs HEAD and the index, and HEAD is pinned, but `status` also computes ahead/behind against the upstream branch config and reads refs for that; with the whitelisted config containing no branch.* keys this is presumably inert, yet the reftable test passes only because refs are never consulted. Please either (a) add a test combining linked worktree + split-index + a configured upstream to prove no failure or wrong result, or (b) document in ADR-019/status.rs why refs and shared-index resolution outside the private view are provably unnecessary for the porcelain v1 output produced. Also: view.charge(name.len()) charges for every directory entry name in gitdir before filtering to sharedindex.*, which makes the budget sensitive to unrelated gitdir contents (e.g., many refs files) — likely benign but should be intentional and documented.

### Non-Blocking Suggestions
S5. The include.path race test writes the malicious payload via a PATH-shim git wrapper triggered on any argv containing 'status'. This also fires on the tool's internal probes if any use the word; the trigger file assertion guards against the race not running, which is good. Consider additionally asserting that the daimonos child process itself (not the wrapper) was the process that read the frozen config, e.g., by having the payload driver write argv/GIT_DIR into the marker, to make the evidence self-describing in CI logs.
S6. StatusView writes config values with a hand-rolled escape (backslash, quote, \n, \t). Git config values can also contain other control characters and the semantics of escaped values inside double quotes differ subtly from git's own serializer. Since the whitelist is small and mostly boolean/path-valued, consider rejecting values containing any control character outright instead of escaping, mirroring the filter-key rejection posture — fail closed rather than re-encode.
S7. GIT_OBJECT_DIRECTORY is set to common/objects, but alternates (objects/info/alternates) resolution under GIT_OBJECT_DIRECTORY differs from normal gitdir resolution (GIT_ALTERNATE_OBJECT_DIRECTORIES is not set). The existing ADR-017 alternate-store confinement tests should be re-run against worktree_status specifically; add one status-path alternate-store case so the confinement guarantee is demonstrated through the new code path, not only the old one.

### Responses to Prior Points
Re R1: Acknowledged. The revised diff is reviewed as the complete FR-06 slice relative to fabb08c, including the new status.rs module and configuration keys. The scope statement (no FR-02/FR-03/FR-07/FR-08 completion claim, no new crate... though note uuid is used in status.rs; I assume it was already a dependency since no Cargo.toml change appears in the diff) is accepted.
Re R2: Accepted. The validation artifact docs/reviews/2026-10-28-fr06-validation.md with explicit commands, counts (2011 Rust passed, 385 Python passed), PATH-prefix disclosure, and the explicit statement that a skip does not count as a successful read-only-mount check addresses S4 and the evidence-transparency expectations.
Re R3: S1 is resolved by strict_uppercase_hash_inputs_return_canonical_lowercase_commits. S2 is resolved by the configuration.md wording that bodies preserve Git's text including any trailing newline. S3 is adequately resolved: the PM contract pins SHA-1, the error message now names SHA-1 and unsupported SHA-256, and the SHA-256 default-mode coverage exists; no further change requested. S4 is resolved by the validation artifact. All four suggestions are closed.
Re R4: The new controls are reasonable but introduce the surface my new concerns B5 and B6 target: the StatusView copies live repository metadata (index, sharedindex.*, info files) and injects GIT_INDEX_FILE/GIT_OBJECT_DIRECTORY into a child process whose config is a hand-built whitelist. The budget-failure and cleanup regression and the workspace-containment rejection are good, but see B5 regarding linked-worktree index location and B6 regarding the include.path race test asserting on the old failure mode rather than through the frozen path end-to-end for the bare/linked gitdir/common split. Also note the default system temp directory is acceptable only as an interim pending FR-08, as the ADR states.
B2: resolved
B3: resolved
B4: resolved

**Verdict:** REVISE

<!-- event id="rebuttal-2" artifact path="1576-dbunk-git-pinning/artifacts/round-2-rebuttal.diff" sha256="8aaded5b8f66be9a9fc5b5a0f7e319b04f92deb407878c03df3b51de03498b16" -->
## Rebuttal — Round 2
**Position:** PARTIAL

### Responses to Blocking Concerns
Re B1: ACCEPT — The private metadata view closes the reproduced include/config race. The child has no executable driver definitions or include directives. It uses a pinned detached HEAD, copied index/shared indexes and local attribute/exclude data, a closed data-only config whitelist, and forced process-level Git config controls. The include-path process-boundary regression passes. B5 below establishes the global/system controls, so the remaining B1 question is also covered. Please explicitly resolve B1 if this evidence satisfies it.
Re B5: CLARIFY — ReadRepo::open already sets GIT_CONFIG_NOSYSTEM=1, GIT_CONFIG_SYSTEM=/dev/null and GIT_CONFIG_GLOBAL=/dev/null in self.env. It also strips inherited GIT_* controls from ProcessConfig, including broad prefixes. worktree_status clones this exact self.env before changing the four metadata-directory variables, and output_with_env passes it unchanged to managed_process::run. HOME is not fabricated or stripped. The revised full diff uses 60 context lines so these pre-existing guards are visible. New test_frozen_status_cannot_inherit_global_or_system_filters defines required process drivers in BOTH real global and system config files and enables both scopes in the caller environment. Native Git executes each marker in the positive baseline. The Daimonos status call executes neither marker and reports the changed files correctly. Required belongs to filter configuration; a separate required attribute cannot define a filter driver in the private config.
Re B6: CLARIFY — The new frozen_status_handles_a_linked_split_index_with_an_upstream fixture combines all requested conditions. It verifies Git's own rev-parse --shared-index-path lies beside the linked worktree's index, configures a real local upstream ref and remote URL, and compares correct porcelain-v1 entries. The full workspace snapshot remains unchanged and a TCP listener sees no connection. Status uses a detached HEAD, no --branch output, and no branch/upstream config, so branch refs and reftable metadata are unnecessary to this result. The existing reftable fixture also passes. The top-level name charge is intentional: it bounds the one-directory scan without traversing refs or objects. ADR-019 and status.rs explain this. An additional deterministic regression found and fixed lost index timestamps: copied metadata now retains original mtimes, preserving racy-index checks.

### Revised Code / Diff
diff --git c/CONTEXT-MAP.md w/CONTEXT-MAP.md
index e2585b1..c0431e8 100644
--- c/CONTEXT-MAP.md
+++ w/CONTEXT-MAP.md
@@ -1,11 +1,11 @@
 # Context map
 
-Date: 2026-10-26
+Date: 2026-10-28
 
 | Context | Glossary | Decisions |
 |---------|----------|-----------|
-| Pinned git reads | [Git reads](docs/contexts/git-read/CONTEXT.md) | [ADR-017](docs/adr/017-git-read-tool.md) |
+| Pinned git reads | [Git reads](docs/contexts/git-read/CONTEXT.md) | [ADR-017](docs/adr/017-git-read-tool.md), [Proposed ADR-018](docs/adr/018-strict-git-pinning.md), [Proposed ADR-019](docs/adr/019-ignore-repository-filters.md) |
 
 This map covers the git-read context. Other contexts are not mapped here yet.
 
 _Anchors:_ src/plugins/git_read.rs::GitReadPlugin
diff --git c/Cargo.toml w/Cargo.toml
index 70f72c8..5e83546 100644
--- c/Cargo.toml
+++ w/Cargo.toml
@@ -27,73 +27,73 @@ async-trait = "0.1"
 blocking = "1"
 starlark = "0.13"
 # Upper bound is deliberate. allocative 0.3.6 changed the trait bound on
 # `Visitor::visit_field` in a way that starlark_map 0.13 does not satisfy, so a
 # plain caret requirement lets dependency resolution pick a combination that
 # cannot compile. Cargo.lock hides this locally, but `cargo install --path .`
 # re-resolves from scratch and ignores the lockfile, so an install would fail
 # with a trait-bound error inside starlark_map while a normal `cargo build`
 # succeeded. Semver says 0.3.6 should be compatible with 0.3.4; upstream broke
 # that, so the bound stays until we move to starlark 0.14, which builds against
 # current allocative.
 #
 # On the range: 0.3.4 is what Cargo.lock pins and what everything here is built
 # and tested against. 0.3.5 is allowed but untested — kept in range so a genuine
 # patch fix can be picked up without a Cargo.toml edit. 0.3.6 is the known-broken
 # one. Narrow this to an exact 0.3.4 if an untested patch bump is unwelcome.
 allocative = ">=0.3.4, <0.3.6"
 # Pinned to our fork (branch fix/stateless-sse-stream-close): the #215 stderr
 # busy-loop fix plus two streamable-HTTP client fixes needed for stateless,
 # POST-only servers like Linear MCP — (1) a graceful POST-stream close is
 # end-of-stream, not a fatal error, and (2) the SSE parser no longer discards
 # the trailing partial line, so large multi-chunk responses assemble. Return
 # to crates.io once these land upstream (rust-mcp-stack/rust-mcp-sdk#215 +
 # follow-ups).
 rust-mcp-sdk = { git = "https://github.com/beardfaceguy/rust-mcp-sdk", rev = "0d9ecb4f9adb275d25a29e84c79635431f6fbd8d", default-features = false, features = ["server", "client", "stdio", "streamable-http", "sse", "auth"] }
 similar = "3"
 uuid = { version = "1", features = ["v4"] }
 walkdir = "2"
 rusqlite = { version = "0.31", features = ["bundled"] }
 chrono = { version = "0.4", default-features = false, features = ["clock", "std"] }
 reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls", "stream"] }
 eventsource-stream = "0.2"
 futures-util = { version = "0.3", default-features = false, features = ["std"] }
 keyring = { version = "3", optional = true }
 reedline = "0.49"
 crossterm = { version = "0.29", features = ["event-stream"] }
 agent-client-protocol = "1.2.0"
 tracing = "0.1.44"
 tracing-subscriber = { version = "0.3.23", features = ["env-filter", "json"] }
 tracing-appender = "0.2.5"
 opentelemetry = { version = "0.32.0", default-features = false, features = ["trace"] }
 opentelemetry_sdk = { version = "0.32.1", default-features = false, features = ["trace"] }
 opentelemetry-otlp = { version = "0.32.0", default-features = false, features = ["trace", "http-proto", "reqwest-blocking-client", "reqwest-rustls-webpki-roots"] }
 tracing-opentelemetry = { version = "0.33.0", default-features = false }
 base64 = "0.22.1"
 # `line_count` gives Unicode-aware post-wrap row counts for exact transcript
 # scrolling; Ratatui 0.29 exposes it behind this narrow unstable feature.
 # `crossterm_0_29` pins ratatui's backend to the same crossterm major as
 # reedline 0.49, so the tree carries one crossterm rather than 0.28 + 0.29
 # (vikunja #1221). 0.30 also moves the layout cache onto lru 0.18, which
 # clears RUSTSEC-2026-0002 and RUSTSEC-2026-0253 at the root (#1226).
 ratatui = { version = "0.30", features = [
     "unstable-rendered-line-info",
     "crossterm_0_29",
 ] }
 axum = { version = "0.8.9", features = ["ws"] }
 ed25519-dalek = "3.0.0"
 unicode-width = "=0.2.0"
 unicode-segmentation = "1.13.3"
 
-# Only for fcntl(F_GETFL/F_SETFL) on the ACP stdin descriptor, to clear an
-# O_NONBLOCK flag a child process left behind on the shared file description.
+# ACP stdin flag repair and nonblocking, no-follow reads of git status metadata.
+# Both uses require Unix descriptor flags without a new dependency.
 [target.'cfg(unix)'.dependencies]
 libc = "0.2"
 
 [features]
 keyring = ["dep:keyring"]
 
 [dev-dependencies]
 futures-util = { version = "0.3", features = ["sink"] }
 opentelemetry_sdk = { version = "0.32.1", default-features = false, features = ["trace", "testing"] }
 tempfile = "3"
 tokio-tungstenite = "0.29.0"
diff --git c/daimonos.default.toml w/daimonos.default.toml
index 7de95d9..7c9a5ee 100644
--- c/daimonos.default.toml
+++ w/daimonos.default.toml
@@ -550,78 +550,84 @@ poll_interval_ms = 1500
 # do NOT put comments inside a prompt file. See prompts/README.md for details.
 #
 # WARNING: these steer agent behavior, tool-use strategy, and output token cost.
 # Removing the execute_script guidance or the terse directive typically INCREASES
 # token usage. Override deliberately; prefer editing copies over the committed
 # defaults in prompts/ so you can compare against the baseline.
 #
 # agent_system     = "~/.config/daimonos/prompts/agent_system.md"
 # mcp_instructions = "~/.config/daimonos/prompts/mcp_instructions.md"
 # kgl_hint         = "~/.config/daimonos/prompts/kgl_hint.md"
 # summary          = "~/.config/daimonos/prompts/summary.md"
 # loop_steer       = "~/.config/daimonos/prompts/loop_steer.md"
 # cancelled_turn   = "~/.config/daimonos/prompts/cancelled_turn.md"
 # skill_catalog    = "~/.config/daimonos/prompts/skill_catalog.md"
 # tool_descriptions = "~/.config/daimonos/prompts/tool_descriptions.toml"
 # Note: `summary` is also overridable via the agent-env DAIMONOS_AGENT_SUMMARY_PROMPT,
 # which takes precedence over the path above.
 # `tool_descriptions` is a partial TOML overlay: omitted tools/variants keep
 # their embedded defaults; unknown tools and empty values are ignored.
 #
 # Agent/chat/ACP append optional user rules from
 # ~/.config/daimonos/agent-instructions.md (or $XDG_CONFIG_HOME/daimonos/...)
 # after the resolved agent_system prompt. Override that location per run with
 # `--agent-instructions /path/to/rules.md`. This does not apply to --mcp mode.
 
 # Registered tools for the tool runner.
 # Each tool gets a [tools.<id>] section.
 # Tools with id "x07" use the built-in X07 plugin; all others use the generic CLI plugin.
 
 # [tools.x07]
 # bin = "/path/to/x07"
 # source_pattern = "**/*.x07.json"
 # manifest = "x07.json"
 
 # [tools.cargo]
 # bin = "cargo"
 # source_pattern = "**/*.rs"
 # manifest = "Cargo.toml"
 
 # --- agent (the `daimonos agent` CLI) ---
 # The agentic CLI's connection config is NOT in this TOML file. It is loaded
 # from a dotenv-style *agent env file* (vikunja #949), resolved as:
 #   --agent-env <path>  >  $DAIMONOS_AGENT_ENV  >  ~/.config/daimonos/agent.env
 # ALL values are required — `daimonos agent` errors out if the file or any of
 # them is missing (no built-in defaults). Example agent.env:
 #
 #   DAIMONOS_AGENT_PROVIDER=openrouter          # openrouter | anthropic
 #   # model slug style differs by provider:
 #   #   openrouter -> "vendor/model": anthropic/claude-sonnet-4.6 | openai/gpt-4o-mini
 #   #   anthropic  -> bare id:        claude-opus-4-8 | claude-haiku-4-5
 #   DAIMONOS_AGENT_MODEL=anthropic/claude-sonnet-4.6
 #   DAIMONOS_AGENT_BASE_URL=https://openrouter.ai/api/v1
 #   DAIMONOS_AGENT_APPROVAL_MODE=interactive    # auto | interactive | paranoid
 #   DAIMONOS_AGENT_API_KEY=sk-...
 #   DAIMONOS_AGENT_PROMPT_CACHE=off             # optional; Anthropic models, default off
 #   DAIMONOS_AGENT_ALLOWED_COMMANDS=            # optional, comma-separated
 #   DAIMONOS_AGENT_DENIED_COMMANDS=exec         # optional, comma-separated
 
 # Pinned, local git reads. See ADR-017 and GitReadConfig in src/config.rs.
 [git_read]
+# Require explicit full SHA-1 commit identifiers. Reject names, ranges, and unpinned operations.
+strict_pinning = false
+# Default: system temporary directory. An override must already exist outside the workspace.
+# status_directory = "/private/state"
+# Bound the complete private index/configuration snapshot used by worktree_status.
+max_status_metadata_bytes = 67108864
 # Maximum captured bytes per stream and maximum serialized response bytes.
 max_output_bytes = 1048576
 # Reject larger blobs before reading their content.
 max_file_bytes = 524288
 # Maximum lines for show_file and blame.
 max_lines = 1000
 # Maximum entries for list_tree, refs, worktree_status, and changed-file lists.
 max_entries = 1000
 # Maximum grep matches per response.
 max_results = 200
 # Maximum log entries per response.
 max_commits = 100
 # Maximum UTF-8 bytes in a patch preview. Truncated previews are not complete patches.
 max_patch_bytes = 65536
 # Maximum request bytes and repository metadata bytes.
 max_input_bytes = 16384
 # Maximum reachable alternate object stores during confinement checks.
 max_alternate_dirs = 32
diff --git c/docs/adr/018-strict-git-pinning.md w/docs/adr/018-strict-git-pinning.md
new file mode 100644
index 0000000..4550b43
--- /dev/null
+++ w/docs/adr/018-strict-git-pinning.md
@@ -0,0 +1,75 @@
+# ADR-018: Strict git pinning is an opt-in policy
+
+- **Status:** Proposed
+- **Date:** 2026-10-27
+- **Deciders:** Pending architectural review. The human approved the product requirements.
+- **Consulted:** dbunk PM, through the round-1 written answers
+- **Informed:** None recorded
+- **Spec:** `daimonos-feature-requests-for-dbunk.md`, FR-06, and `daimonos-pm-answers-round-1.md`
+- **Tracking:** Vikunja #1576, parent #1575
+- **Supersedes:** None
+- **Superseded by:** None
+
+## Context
+
+ADR-017 permits local revision resolution and returns full commit identifiers.
+The dbunk reviewer instead needs caller-pinned input identifiers before any git read.
+The PM accepted an additive policy and rejected a global revision-contract change.
+How can dbunk require pinned inputs without breaking existing callers?
+
+## Decision drivers
+
+- Existing calls, defaults, and result shapes remain unchanged except for the approved filter security fix in ADR-019.
+- Every strict read must identify its input commits explicitly.
+- Output pinning cannot replace input validation.
+- The existing process hardening must remain active under both policies.
+
+## Decision
+
+We will add `git_read.strict_pinning`, default false.
+When true, operations require explicit full 40-character hexadecimal commit identifiers.
+`diff` and `ancestry` require both `a` and `b` in that form.
+Other pinned operations require `rev`. Implicit HEAD, ranges, relative revisions, and short hashes are rejected.
+`refs` and `worktree_status` are refused because they cannot supply a pinned view.
+`resolve` remains useful only for verifying an explicit full commit identifier.
+The existing runner still resolves and checks the object type locally.
+This policy does not weaken confinement, no-mutation, or no-network controls.
+
+We will add `include_body`, default false, for `log` and `show_commit`.
+The field `body` appears in commit metadata only when the caller requests it.
+Existing names and result fields remain unchanged by default.
+
+## Alternatives
+
+- Globally reject revision names. This breaks ADR-017 and existing callers.
+- Accept names and only pin outputs. A moving ref can change before the request starts.
+- Create a second git tool. This duplicates the operation surface and process hardening.
+
+## Consequences
+
+The caller must explicitly select strict pinning. It is not a complete reviewer sandbox.
+Forty-character hashes restrict the strict profile to SHA-1 repositories.
+The default policy still supports other locally resolvable git object formats.
+Commit bodies can increase output size, so existing response caps still apply.
+
+## Confirmation
+
+- Test every strict operation through the public plugin interface.
+- Reject missing hashes, names, tags, short hashes, ranges, and unpinned operations.
+- Prove names still work with the default policy.
+- Compare repository snapshots before and after strict calls and hostile-config tests.
+- Exercise strict reads on a real read-only mount. Record unavailable platform facilities explicitly.
+- Verify `include_body` through Rust and MCP without changing default metadata fields.
+- Run `cargo test` and `python3 -m pytest tests/ -v`.
+
+## Anchors
+
+_Anchors:_ src/plugins/git_read.rs::GitReadPlugin, src/plugins/git_read.rs::Arguments, src/plugins/git_read.rs::history, src/plugins/git_read/runner.rs::ReadRepo, src/config.rs::GitReadConfig
+
+## Revisit when
+
+The caller needs strict SHA-256 repositories or a versioned pinned-only tool contract.
+
+## Open questions
+
+No product acceptance blocker remains for this slice. Architectural review is pending.
diff --git c/docs/adr/019-ignore-repository-filters.md w/docs/adr/019-ignore-repository-filters.md
new file mode 100644
index 0000000..1784a5e
--- /dev/null
+++ w/docs/adr/019-ignore-repository-filters.md
@@ -0,0 +1,83 @@
+# ADR-019: Ignore repository-configured filters during git reads
+
+- **Status:** Proposed
+- **Date:** 2026-10-28
+- **Deciders:** The human approved option 2 of Q-FR-06-2.1 on 2026-10-27. This record awaits architectural review.
+- **Consulted:** dbunk PM, through the written requirements.
+- **Informed:** None recorded.
+- **Spec:** FR-06, PM round 1, and `docs/reviews/2026-10-27-dbunk-follow-up.md`.
+- **Tracking:** Vikunja #1576, under #1575.
+- **Supersedes:** None. This clarifies the security guarantee in ADR-017.
+- **Superseded by:** None.
+
+## Context
+
+Git status can execute a configured clean or process filter while checking dirty files.
+The expanded hostile-configuration test reproduced that execution through git_read.
+Disabling filters also changes status results for legitimate filtered repositories.
+The human explicitly approved that compatibility exception instead of retaining the execution risk.
+Should filter suppression apply in both policies, despite the changed status semantics?
+
+## Decision drivers
+
+- A read-only tool must not execute repository-configured programs.
+- Strict pinning alone does not protect existing callers of worktree_status.
+- The compatibility change must remain explicit and tested.
+
+## Decision
+
+We will suppress configured clean, smudge, and process filters in both pinning policies.
+The runner retains bounded per-process overrides as defense for committed-object reads.
+Working-tree status does not depend on that scan. It uses a private, frozen Git metadata directory outside the workspace.
+The directory contains a pinned HEAD, copied index metadata, and only data-only configuration needed for built-in status behavior.
+It contains no filter definitions or include directives. Mutable repository configuration cannot add a driver to that process.
+The process reads the original worktree and object store without changing them.
+Split-index and local attribute/exclude metadata are copied into the private view.
+Copied metadata retains its original mtime. Git needs it for racy-index content checks.
+The detached HEAD and porcelain v1 output do not need upstream refs or branch configuration.
+Git stores a linked worktree's shared index beside its index. A combined regression verifies Git's reported path.
+The metadata budget includes top-level entry names to bound that scan. The scan does not recurse into refs or objects.
+The private view is removed after success, failure, or cancellation. No repository configuration file changes.
+The worktree result excludes external filter transformations. Built-in Git conversions can still apply.
+The user-facing documentation will warn that a filtered file can appear modified where ordinary Git reports clean.
+
+## Alternatives
+
+- Retain filter execution in default mode. Rejected by the human because the read-only tool would remain an execution surface.
+- Suppress filters only under strict pinning. This protects the dbunk policy but not ordinary worktree-status callers.
+- Execute only familiar filters. Rejected because repository configuration selects executable commands, not trustworthy implementations.
+
+## Consequences
+
+The fix changes default status semantics for repositories such as Git LFS repositories.
+This is the narrow compatibility exception approved as option 2, not permission for unrelated default changes.
+The existing git tool remains unchanged.
+The metadata budgets reject excessive configuration or index snapshots instead of applying incomplete protection.
+`git_read.max_status_metadata_bytes` bounds the private view. `git_read.status_directory` selects an existing directory outside the workspace.
+Without an override, the view uses the system temporary directory. FR-08 must route this internal state under an explicit state root.
+
+## Confirmation
+
+- Run `cargo test default_status_reports_raw_bytes_without_running_normalizing_filters`.
+- In that fixture, ordinary Git reports clean after a real filter transformation.
+- Verify that git_read instead reports the tracked file as modified and leaves the full repository snapshot unchanged.
+- Run `cargo test every_operation_ignores_malicious_config` under both policies.
+- Run `python3 -m pytest tests/test_git_read.py::test_git_read_filter_added_after_inspection_cannot_execute -v`.
+- Prove that a filter added through an include immediately before status cannot execute.
+- Test mixed-case keys, malformed configuration, split indexes, local excludes, built-in normalization, linked worktrees, reftable, and SHA-256 defaults.
+- Verify metadata caps and private-directory cleanup after failure.
+- Run `cargo test plugins::git_read::tests` and `python3 -m pytest tests/test_git_read.py -v`.
+- Run the full Rust and Python suites before completing this slice.
+
+## Anchors
+
+_Anchors:_ src/plugins/git_read/status.rs::StatusView, src/plugins/git_read/runner.rs::ReadRepo, src/plugins/git_read/tests.rs::default_status_reports_raw_bytes_without_running_normalizing_filters, src/plugins/git_read/tests.rs::every_operation_ignores_malicious_config
+
+## Revisit when
+
+Git provides a way to inspect filter-normalized status without executing repository-controlled programs.
+Also revisit this decision if the caller needs explicitly trusted filter execution in a different tool.
+
+## Open questions
+
+The human resolved the product choice. Architectural review of this record remains pending.
diff --git c/docs/configuration.md w/docs/configuration.md
index 8072776..74e08c0 100644
--- c/docs/configuration.md
+++ w/docs/configuration.md
@@ -994,119 +994,144 @@ manifest = "x07.json"
 These are not part of the config file but affect daimonos behavior:
 
 | Variable | Description |
 |----------|-------------|
 | `DAIMONOS_IDLE_TIMEOUT_SECS` | Overrides `[mcp] idle_timeout_secs` when set to a parseable integer. `0` disables the idle watchdog. Used by tests. |
 | `DAIMONOS_LOG_STARTUP` | When non-empty and not `0` / `false` / `no`, enables MCP stderr startup diagnostics before config load (same family as `--verbose` / `[mcp] startup_logs`). |
 | `DAIMONOS_AGENT_SESSION_ID` | Optional agent-runtime session id (e.g. a UUID matching `claude --session-id`). When set, every analytics row recorded by this daimonos process is tagged with this id, and `daimonos --stats --session-id <id>` / `session_stats {external_session_id: <id>}` will filter to it. The `set_external_session_id` MCP tool can override this at runtime. |
 | `DAIMONOS_AGENT_SUMMARY_PROMPT` | Overrides the context-compaction summarizer prompt. Takes precedence over `[prompts].summary` and the embedded default. |
 | `DISCORD_BOT_TOKEN` | Default Discord bot token variable when `[discord].bot_token_env_var` is unchanged. |
 | `PATH` | Daimonos inherits the launching process's `PATH` for exec/bg commands |
 
 ## Performance Tuning Tips
 
 **Large monorepos**: increase `max_depth`, `max_files`, and
 `max_walk_entries` together if important paths fall outside reported
 coverage. Add project-specific generated/binary extensions to
 `skip_extensions` to spend the path budget on useful files.
 
 **Slow first file search**: use `mode = "hybrid"` or `"eager"` to warm
 long-lived sessions, or narrow `path` and `glob` filters. Later searches reuse
 the warm path index until its watcher reports a filesystem change.
 
 **Truncated exec output**: if you're losing important build output, increase
 `exec_output_max_chars`. The default of 100 KB covers most cases.
 
 ### OpenRouter structural request tracing (opt-in)
 
 To retain the tool schemas sent on future OpenRouter requests, set
 `DAIMONOS_OPENROUTER_TRACE_DIR` in the **Daimonos process environment** (not
 `agent.env`) and restart the agent. For example:
 
 ```sh
 export DAIMONOS_OPENROUTER_TRACE_DIR="$HOME/.local/state/daimonos/openrouter-traces"
 ```
 
 The parent directory must exist. The trace directory is created with mode 0700;
 existing directories must be private and owned by the current user. Trace files
 use mode 0600. On Unix the final directory component and files cannot be
 symlinks, files cannot be hard-linked, and opens/rotation are relative to a
 validated directory handle. Parent symlinks are resolved at directory open;
 this does not isolate tracing from other processes running as the same user.
 Use a trusted local filesystem (filesystem IO itself is not time-bounded).
 Each JSONL record has a local UUID/timestamp, model, streaming flag,
 message count, and the exact outgoing `tools` array. It excludes messages,
 tool-call arguments/results, URLs, HTTP headers, and API keys. **Schemas may still
 contain private descriptions, defaults or examples: treat these files as sensitive.**
 
 `requests.jsonl` rotates into `requests.jsonl.1` at 16 MiB (at most 32 MiB of
 retained payload); oversized records are skipped, not truncated. Lock contention
 or trace IO failures skip the record and emit a generic warning without failing
 the model request. Unset the variable to disable tracing and delete both files
 to remove retained data. Rotation replaces old history; archive needed evidence
 securely before it is rotated out.
 
 This captures Daimonos's outgoing schema, not OpenRouter's downstream transformed
 request. It cannot recover older requests, prove downstream behavior, or correlate
 raw model responses; the UUID is local, not a provider request ID.
 
 ## Read-only git (`git_read`)
 
+`git_read.strict_pinning` is false by default. Enable it to require explicit
+40-character SHA-1 commit identifiers. `diff` and `ancestry` then require `a` and
+`b`. Other permitted operations require `rev`. Names, ranges, implicit HEAD,
+short hashes, and annotated-tag object identifiers are rejected. `refs` and
+`worktree_status` are refused under this policy. Default revision resolution remains unchanged.
+
+`log` and `show_commit` accept `include_body=true`. Only that opt-in adds the
+`body` field to commit metadata. The existing response cap still applies.
+Bodies preserve Git's body text, including any trailing newline. Do not trim them before comparisons.
+
+The runner disables configured clean, smudge, and process filters in both modes.
+This is an approved security-fix exception to the unchanged-default rule.
+`worktree_status` does not apply those external transformations. It can report a
+modified file where ordinary Git reports a clean filtered file, including Git LFS cases.
+Do not interpret this result as filter-aware status. Built-in Git conversions can still apply.
+[ADR-019](adr/019-ignore-repository-filters.md) records the reason and the human approval.
+Status runs against private, frozen metadata with no filter definitions or include directives.
+Later changes to repository configuration cannot add executable filters to that process.
+`git_read.status_directory` can select an existing metadata directory outside the workspace.
+The default is the system temporary directory. The private directory is removed after the call.
+`git_read.max_status_metadata_bytes` defaults to 64 MiB and bounds the copied metadata.
+Unsupported filter keys or oversized configuration fail closed before reads.
+Strict pinning is not required for process hardening. It adds immutable input selection.
+
+
 The `[git_read]` section bounds local, pinned git reads. `GitReadConfig` in
 `src/config.rs` defines the defaults. `daimonos.default.toml` lists every key.
 ADR-017 records the safety boundary.
 
 | Key | Controls |
 |-----|----------|
 | `max_output_bytes` | Captured bytes per stream and serialized response bytes |
 | `max_file_bytes` | Blob size before content reads |
 | `max_lines` | Lines in `show_file` and `blame` |
 | `max_entries` | Tree entries, refs, status entries, and changed files |
 | `max_results` | Grep matches |
 | `max_commits` | Log entries |
 | `max_patch_bytes` | UTF-8 bytes in a patch preview |
 | `max_input_bytes` | Request size and repository metadata size |
 | `max_alternate_dirs` | Reachable alternate object stores |
 
 Each value must be positive. File and patch caps must not exceed the output cap.
 The output cap must fit the minimal structured error response.
 The effective response cap is the smaller of `git_read.max_output_bytes` and `tool_output.max_bytes`.
 Oversized responses fail before the shared output handler can spill them to disk.
 Caller limits can reduce a cap, but cannot increase it.
 `max_alternate_dirs` counts the primary store and its reachable alternate stores.
 The tool name `git_read` is reserved. A generic CLI configuration cannot replace it.
 
 `repo` names a repository root relative to the workspace, not the session's
 current directory. Git directories and alternate stores must also stay inside
 the workspace after symlink resolution. `worktree_status` ignores submodules.
 The tool requires git with `--no-lazy-fetch` support. Older git does not register it.
 History reads support bare repositories. `worktree_status` rejects them with `bare_repository`.
 Grep rejects stderr diagnostics instead of reporting a potentially incomplete search as successful.
 Malformed nested ranges return `rejected_argument`.
 
 Every successful result includes `repo` and `commits`. Annotated tags retain
 their object SHA and report their peeled commit SHA separately. Tags that name
 non-commit objects report a null commit SHA. `log` accepts one revision or an
 explicit `a..b` range. `diff` requires `a` and `b`, or `rev=a..b`.
 `show_commit` compares a merge commit with its first parent.
 
 `offset` starts at zero. `next_offset` continues a truncated page with the same
 arguments. Blame uses one-based `start_line` and `end_line`. Its continuation
 keeps those arguments and changes only `offset`. Patch previews do not support
 continuation. A truncated preview is not a complete patch.
 
 Failures return `error.kind` and `error.message`. Required kinds are
 `unknown_revision`, `path_not_found`, `object_missing`, `binary_file`,
 `repo_outside_workspace`, and `rejected_argument`. Binary errors include metadata,
 not content. Additional kinds describe invalid repositories, I/O failures,
 unavailable tools, and git errors. Oversized captures or responses return
 `output_limit` with `truncated: true`. They never return partially parsed records.
 
 Agent mode can grant `git_read` without granting `git`:
 
 ```dotenv
 DAIMONOS_AGENT_ALLOWED_COMMANDS=read_file,search,workspace_info,git_read
 ```
 
 MCP mode does not enforce this agent allowlist. MCP clients must expose only the
 required read tools. Do not expose `exec`, `batch`, or `execute_script` to a
 strictly read-only reviewer. Their internal calls do not apply the agent policy.
diff --git c/docs/contexts/git-read/CONTEXT.md w/docs/contexts/git-read/CONTEXT.md
index b0080cd..11ef21c 100644
--- c/docs/contexts/git-read/CONTEXT.md
+++ w/docs/contexts/git-read/CONTEXT.md
@@ -1,16 +1,18 @@
 # Pinned git reads
 
-Date: 2026-10-27
+Date: 2026-10-28
 
 - **Workspace:** The fixed root that contains repositories. The session cwd does not change this boundary.
 - **Pinned commit:** A full commit SHA returned with a read result. It makes a citation independent of moving refs.
+- **Strict pinning:** The optional policy that also requires explicit SHA-1 commit identifiers as inputs. It rejects mutable names and tag objects.
 - **Promisor:** A partial-clone remote that holds objects absent locally. `git_read` never asks it for objects.
 - **Alternate store:** Another git object directory. Chained stores must stay inside the workspace.
 - **Patch preview:** Bounded patch text. A truncated preview is not a complete patch.
 - **Pointer file:** Git metadata that selects a git directory or common directory. It must stay inside the workspace.
 - **Bare repository:** A git database without a working tree. History reads are valid, but worktree status is not.
+- **Filter-free status:** Status without repository-configured external filter transformations. A filtered file can appear modified even when ordinary Git reports clean.
 
 [ADR-017](../../adr/017-git-read-tool.md) records why the tool is separate from `git`.
 [Configuration](../../configuration.md#read-only-git-git_read) names its limits.
 
 _Anchors:_ src/plugins/git_read.rs::GitReadPlugin, src/plugins/git_read/runner.rs::ReadRepo, src/config.rs::GitReadConfig
diff --git c/docs/reviews/2026-10-28-fr06-validation.md w/docs/reviews/2026-10-28-fr06-validation.md
new file mode 100644
index 0000000..eb3fcaf
--- /dev/null
+++ w/docs/reviews/2026-10-28-fr06-validation.md
@@ -0,0 +1,53 @@
+# FR-06 local validation
+
+Date: 2026-10-28
+Task: Vikunja #1576, under #1575.
+Branch: `feat/1575-dbunk-reviewer`.
+Status: Final local validation passed. Rubber Ducky round 3 is pending.
+
+## Scope
+
+This evidence covers strict git pinning, additive bodies, and the approved filter security exception.
+It does not establish completion of FR-02, FR-03, FR-07, or FR-08 integration.
+No new crate, live credentialed endpoint test, publication, or merge occurred.
+
+## Actual checks
+
+- `cargo fmt --all -- --check`: passed.
+- `cargo clippy --all-targets -- -D warnings`: passed.
+- `cargo test`: 2016 passed, 0 failed, 10 ignored.
+- `python3 -m pytest tests/ -v`: 386 passed, 0 failed, 1 skipped.
+- The full-suite commands used `target/test-tools/bin` and `.venv/bin` as PATH prefixes.
+- Logs: `target/dbunk-fr06-rust-final.log` and `target/dbunk-fr06-pytest-final.log`.
+- Those logs and temporary test tools are local artifacts, not committed dependencies.
+
+## Read-only mount evidence
+
+The Linux namespace test ran and passed on this host. It checked EROFS on a real bind mount.
+It exercised all nine pinned operations and both refused operations, with unchanged evidence snapshots.
+The test skips on non-Linux hosts or when user/mount namespaces are unavailable.
+No macOS mount test ran here. A skip does not count as a successful read-only-mount check.
+
+## Review regressions
+
+- A process-boundary fixture introduced a filter through an include after the original inspection.
+  The old implementation executed it. The frozen-metadata implementation does not.
+- Mixed-case filter keys are canonicalized by Git and suppressed under both policies.
+- Both sides of diff and ancestry reject full annotated-tag object IDs under strict pinning.
+- Malformed configuration returns a structured failure without mutating the repository.
+- The private metadata budget fails closed, and the temporary directory is removed after failure.
+- Split indexes, local excludes, built-in text normalization, linked worktrees, reftable, and SHA-256 defaults pass.
+- Uppercase SHA-1 inputs return canonical lowercase commit identifiers.
+- Global/system filter definitions execute in the native Git baseline but not through the frozen status view.
+- A linked worktree with a split index and configured upstream returns the correct porcelain entries without a connection attempt.
+- Copied index timestamps retain Git's racy-index content checks. The deterministic regression failed before that fix.
+- Status accepts an alternate store inside the workspace and refuses one outside it.
+- Quoted, backslash, newline, tab, and carriage-return path settings preserve native Git behavior.
+- FIFO metadata is refused without waiting for a writer. The private directory is removed after the failure.
+
+## Internal state
+
+Status uses private metadata outside the workspace. It contains no filter definitions or include directives.
+`git_read.status_directory` selects an existing directory. Its default is the system temporary directory.
+`git_read.max_status_metadata_bytes` defaults to 64 MiB.
+FR-08 must route this internal state under the explicit state root. That integration remains pending.
diff --git c/prompts/tool_descriptions.toml w/prompts/tool_descriptions.toml
index 6e44a35..da684f5 100644
--- c/prompts/tool_descriptions.toml
+++ w/prompts/tool_descriptions.toml
@@ -141,137 +141,138 @@ full = """Mark one message read for you (agent mail, ADR-009). Idempotent: a sec
 terse = """Mark one message read (per-recipient; idempotent)."""
 
 [mark_read.parameters]
 agent = "Your registered agent name."
 message_id = "Id of the message to mark read."
 [acknowledge]
 full = """Acknowledge one message addressed to you (also marks it read) — a lightweight non-textual reply for ack_required messages (agent mail, ADR-009). Idempotent: repeat calls keep the original ack time."""
 terse = """Acknowledge one message (also marks read; idempotent)."""
 
 [acknowledge.parameters]
 agent = "Your registered agent name."
 message_id = "Id of the message to acknowledge."
 [fetch_thread]
 full = """Fetch every message in a thread, oldest-first, bounded by a hard cap (agent mail, ADR-009). Pass the `thread_id` from a message or a send/reply receipt. Reconstruction is a flat, bounded read — never a recursive walk."""
 terse = """Fetch a thread's messages (oldest-first, bounded)."""
 
 [fetch_thread.parameters]
 thread_id = "Id of the thread to fetch (from a message or a send/reply receipt)."
 [reserve_paths]
 full = """Post ADVISORY reservations on file glob patterns for this workspace (agent mail, ADR-009) — a soft \"I'm working here\" signal to other agents, NOT a lock (it never blocks a write). Returns the granted reservations plus any conflicts with other agents' active exclusive reservations. Patterns are opaque globs (e.g. src/api/*.rs), never resolved against the filesystem."""
 terse = """Advisory-reserve file globs (soft signal, not a lock); reports conflicts."""
 
 [reserve_paths.parameters]
 agent = "Your registered agent name (the reservation holder)."
 paths = "Glob pattern(s) to reserve (array), e.g. [\"src/api/*.rs\"]."
 exclusive = "Whether THIS reservation is exclusive (default true). Recorded for future callers: another agent's check overlapping YOUR exclusive reservation is reported a conflict; a shared (exclusive=false) reservation never causes a conflict for others. It does not change whether conflicts against others are reported to you on this call — those are always reported."
 reason = "Optional short note on why you're reserving these paths."
 ttl_secs = "Lifetime in seconds; clamped to the configured [60, max] range. Defaults to the configured default TTL."
 [renew_reservations]
 full = """Extend the expiry of all your own active reservations in this workspace (agent mail, ADR-009). Only your unreleased, unexpired reservations are touched."""
 terse = """Extend expiry of your own active reservations."""
 
 [renew_reservations.parameters]
 agent = "Your registered agent name."
 ttl_secs = "New lifetime in seconds from now; clamped to the configured [60, max] range."
 [release_reservations]
 full = """Release your own reservations in this workspace (agent mail, ADR-009). Omit `paths` to release ALL your active reservations; otherwise release only those whose pattern exactly matches one you give. Idempotent."""
 terse = """Release your own reservations (all, or by exact pattern)."""
 
 [release_reservations.parameters]
 agent = "Your registered agent name."
 paths = "Optional exact pattern(s) to release (array). Omit to release all your active reservations."
 [check_conflicts]
 full = """Read-only pre-edit check: do the given glob patterns overlap another agent's active exclusive reservation? (agent mail, ADR-009.) Ignores your own reservations and mutates nothing. Use before editing to see if someone else signaled they're working there."""
 terse = """Read-only: check paths against others' active reservations."""
 
 [check_conflicts.parameters]
 agent = "Your registered agent name (your own reservations are ignored)."
 paths = "Glob pattern(s) to check (array)."
 [list_reservations]
 full = """List your own active (unreleased, unexpired) reservations in this workspace (agent mail, ADR-009)."""
 terse = """List your own active reservations."""
 
 [list_reservations.parameters]
 agent = "Your registered agent name."
 limit = "Max reservations to return (most-recent first)."
 [list_tool_signatures]
 full = """Python-style function signatures for all tool bindings available in execute_script. Already included in server instructions."""
 
 [git_read]
-full = """Read local git history at pinned commits without writes or network access. Use git_read for reviewer reads. Use git for writes. Returns JSON with the repo and full commit SHAs. Use workspace-relative repo and path values. Revisions, paths, repo names, and patterns must not start with a hyphen. Requires git with --no-lazy-fetch support. Caps come from [git_read] config. Oversized metadata returns output_limit. Binary files return metadata only."""
-terse = "Read-only git at pinned commits. No writes or network."
+full = """Read local git history at pinned commits without repository writes or network access. Use git_read for reviewer reads. Use git for writes. Returns JSON with the repo and full commit SHAs. Use workspace-relative repo and path values. Revisions, paths, repo names, and patterns must not start with a hyphen. Requires git with --no-lazy-fetch support. Caps come from [git_read] config. Oversized metadata returns output_limit. Binary files return metadata only. When strict pinning is enabled, supply full SHA-1 commit hashes. Names, ranges, and unpinned operations are refused."""
+terse = "Read-only git at pinned commits. No repository writes or network."
 [git_read.parameters]
 command = "Operation to run. No raw commands or user flags."
 repo = "Repository root relative to the workspace. Default: workspace root."
-rev = "Commit, ref, tag, or log range. Default: HEAD."
+rev = "Commit, ref, tag, or log range. Default: HEAD. Strict pinning requires an explicit full commit hash."
 path = "Literal repository-relative path."
 paths = "Literal repository-relative paths."
 pattern = "Extended regular expression for grep. Must not start with a hyphen."
 a = "First revision for diff or ancestry."
 b = "Second revision for diff or ancestry."
 mode = "diff: stat or patch. Default: stat."
 offset = "Zero-based line or entry offset. Default: 0."
 limit = "Maximum returned items, bounded by configuration."
 recursive = "list_tree: include descendants. Default: false."
 follow = "log: follow renames for one path. Default: false."
 patch = "show_commit: include a bounded patch preview. Default: false."
+include_body = "log and show_commit: include the commit body. Default: false."
 start_line = "blame: first line, one-based. Default: 1."
 end_line = "blame: final line, inclusive. Bounded by max_lines."
 
 [git]
 full = """Git operations. Structured commands: status, log, diff, branch, add, commit, push, pull, checkout. Any other git subcommand (merge, rebase, stash, fetch, tag, reset, cherry-pick, show, ...): command=raw with args=[subcommand, ...]; returns exit_code/stdout/stderr."""
 terse = """Git: status, log, diff, branch, add, commit, push, pull, checkout; raw for any other subcommand (args=[subcommand, ...])."""
 
 [git.parameters]
 args = "raw: git argv after 'git', e.g. [\"merge\", \"--no-ff\", \"topic\"]"
 message = "commit: message"
 all = "commit: auto-stage (-a)"
 limit = "log: max commits (default 10)"
 oneline = "log: compact format (hash + subject per line)"
 path = "log: filter by path"
 mode = "diff: scope"
 paths = "add: files (default [\".\"])"
 branch = "checkout/push/pull: branch"
 create = "checkout: create new branch (-b)"
 files = "checkout: restore files"
 remote = "push/pull: remote (default origin)"
 set_upstream = "push: -u flag"
 rebase = "pull: --rebase"
 [cargo]
 full = """Cargo operations. Commands: test, build, check, clippy, fmt, add."""
 terse = """Cargo: test, build, check, clippy, fmt, add."""
 
 [cargo.parameters]
 package = "Target package (--package)"
 filter = "test: name filter"
 lib = "test: --lib flag"
 release = "build/check: --release flag"
 dev = "add: --dev flag"
 [pytest]
 full = """Python test runner. Commands: run (passed/failed/skipped + failure ids), collect (--collect-only test ids)."""
 terse = """Python tests: run, collect."""
 
 [pytest.parameters]
 path = "Test file or directory (default: pytest's auto-discovery)"
 filter = "run: -k expression (test name selector)"
 markers = "run: -m expression (marker selector)"
 verbose = "run: -v flag"
 failfast = "run: -x flag (stop on first failure)"
 [gh]
 full = """GitHub CLI (structured). Commands: pr_view, pr_list, pr_create, pr_diff, pr_checks, pr_merge, pr_checkout, run_list, run_view, issue_list, issue_view, issue_create, issue_comment, api, raw (arbitrary gh args)."""
 terse = """GitHub: pr_view/list/create/diff/checks/merge/checkout, run_list/view, issue_list/view/create/comment, api, raw."""
 
 [gh.parameters]
 number = "pr_view/pr_diff/pr_checks/pr_merge/pr_checkout/issue_view/issue_comment: PR or issue number (PR ops default to current branch)"
 state = "pr_list/issue_list: filter (default open; 'merged' is PR-only)"
 limit = "pr_list/run_list/issue_list: max results"
 author = "pr_list/issue_list: filter by author"
 title = "pr_create/issue_create: title (required)"
 body = "pr_create/pr_merge/issue_create/issue_comment: body text"
 base = "pr_create: base branch"
 draft = "pr_create: create as draft"
 merge_method = "pr_merge: merge strategy (default merge)"
 delete_branch = "pr_merge: delete the head branch after merge"
 subject = "pr_merge: commit subject line"
 branch = "pr_checkout: branch to check out; run_list: filter by branch"
 workflow = "run_list: filter by workflow name or file"
diff --git c/src/config.rs w/src/config.rs
index d0c0720..3db0bee 100644
--- c/src/config.rs
+++ w/src/config.rs
@@ -1,112 +1,119 @@
 use crate::plugins::generic_cli::GenericCliPlugin;
 use crate::plugins::x07::X07Plugin;
 use crate::tool_runner::ToolRegistry;
 use crate::verbosity::Verbosity;
 use serde::Deserialize;
 use std::collections::{HashMap, HashSet};
 use std::path::Path;
 use std::sync::Arc;
 
 /// Read-only git bounds. Context: docs/adr/017-git-read-tool.md.
 #[derive(Debug, Deserialize, Clone)]
 #[serde(default)]
 pub struct GitReadConfig {
+    pub strict_pinning: bool,
+    pub status_directory: Option<String>,
+    pub max_status_metadata_bytes: usize,
     pub max_output_bytes: usize,
     pub max_file_bytes: usize,
     pub max_lines: usize,
     pub max_entries: usize,
     pub max_results: usize,
     pub max_commits: usize,
     pub max_patch_bytes: usize,
     pub max_input_bytes: usize,
     pub max_alternate_dirs: usize,
 }
 
 impl Default for GitReadConfig {
     fn default() -> Self {
         Self {
+            strict_pinning: false,
+            status_directory: None,
+            max_status_metadata_bytes: 64 * 1024 * 1024,
             max_output_bytes: 1024 * 1024,
             max_file_bytes: 512 * 1024,
             max_lines: 1000,
             max_entries: 1000,
             max_results: 200,
             max_commits: 100,
             max_patch_bytes: 64 * 1024,
             max_input_bytes: 16 * 1024,
             max_alternate_dirs: 32,
         }
     }
 }
 
 impl GitReadConfig {
     fn validate(&self) -> Result<(), String> {
         for (name, value) in [
             ("max_output_bytes", self.max_output_bytes),
             ("max_file_bytes", self.max_file_bytes),
             ("max_lines", self.max_lines),
             ("max_entries", self.max_entries),
             ("max_results", self.max_results),
             ("max_commits", self.max_commits),
             ("max_patch_bytes", self.max_patch_bytes),
             ("max_input_bytes", self.max_input_bytes),
             ("max_alternate_dirs", self.max_alternate_dirs),
+            ("max_status_metadata_bytes", self.max_status_metadata_bytes),
         ] {
             if value == 0 {
                 return Err(format!("git_read.{name} must be greater than zero"));
             }
         }
         if self.max_output_bytes
             < crate::plugins::git_read::output_limit_response()
                 .to_string()
                 .len()
         {
             return Err("git_read.max_output_bytes must fit the minimal structured error".into());
         }
         if self.max_file_bytes > self.max_output_bytes
             || self.max_patch_bytes > self.max_output_bytes
         {
             return Err("git_read file and patch caps must not exceed max_output_bytes".into());
         }
         Ok(())
     }
 }
 
 #[derive(Debug, Default, Deserialize, Clone)]
 #[serde(default)]
 pub struct Config {
     pub index: IndexConfig,
     pub search: SearchConfig,
     pub git_read: GitReadConfig,
     pub process: ProcessConfig,
     pub tool_output: ToolOutputConfig,
     pub loop_detector: LoopDetectorConfig,
     /// Automatic per-turn workspace checkpoints (#1239). Off by default.
     pub checkpoint: CheckpointConfig,
     /// Bounded provider HTTP/SSE deadlines (#1107).
     pub provider_timeouts: ProviderTimeoutConfig,
     pub logging: LoggingConfig,
     pub observability: ObservabilityConfig,
     pub analytics: AnalyticsConfig,
     pub pipeline_cache: PipelineCacheConfig,
     pub mcp: McpConfig,
     pub acp: AcpConfig,
     /// `[agent.*]` — daimonos's own agent frontends (TUI, one-shot, chat
     /// REPL, session daemon), as opposed to `[acp.*]` which configures the
     /// ACP server this binary exposes to Zed.
     pub agent: AgentModeConfig,
     pub session: SessionRuntimeConfig,
     pub tui: TuiConfig,
     pub discord: DiscordConfig,
     pub kgl: KglConfig,
     pub coordination: CoordinationConfig,
     pub prompts: PromptsConfig,
     #[serde(default)]
     pub tools: HashMap<String, ToolConfig>,
 }
 
 /// `[agent]` — settings for daimonos's own agent frontends.
 #[derive(Debug, Default, Deserialize, Clone)]
 #[serde(default)]
 pub struct AgentModeConfig {
     pub mcp: AgentMcpConfig,
     pub skills: AgentSkillsConfig,
diff --git c/src/plugins/git_read.rs w/src/plugins/git_read.rs
index 3bf1341..c7a182b 100644
--- c/src/plugins/git_read.rs
+++ w/src/plugins/git_read.rs
@@ -1,273 +1,314 @@
-//! Pinned git reads with no writes, network, or repository-controlled programs.
+//! Pinned git reads with no repository writes, network, or repository-controlled programs.
 //! Context: docs/adr/017-git-read-tool.md. The caller supplies the workspace,
 //! not the mutable session cwd. All errors stay structured across frontends.
 
 mod runner;
+mod status;
 #[cfg(test)]
 mod tests;
 use crate::config::{GitReadConfig, ProcessConfig};
 use crate::tool_runner::{ToolCommand, ToolDescriptor, ToolPlugin, ToolResult};
 use runner::{Failure, ReadRepo};
 use serde::Deserialize;
 use serde_json::{json, Value};
 use std::collections::HashMap;
 use std::path::Path;
 
+fn is_full_sha1(value: &str) -> bool {
+    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
+}
+
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
+    include_body: bool,
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
+            if self.limits.strict_pinning {
+                args.validate_pinning(command)?;
+            }
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
+    fn validate_pinning(&self, command: &str) -> Result<(), Failure> {
+        // ADR-018: strict inputs are commit identifiers, not names resolved after the call starts.
+        if matches!(command, "refs" | "worktree_status") {
+            return Err(Failure::new(
+                "rejected_argument",
+                "strict pinning refuses unpinned operations",
+            ));
+        }
+
+        for value in [&self.rev, &self.a, &self.b].into_iter().flatten() {
+            if !is_full_sha1(value) {
+                return Err(Failure::new(
+                    "rejected_argument",
+                    "strict pinning requires full 40-character SHA-1 commit hashes; SHA-256 is unsupported",
+                ));
+            }
+        }
+        if matches!(command, "diff" | "ancestry") {
+            if self.a.is_none() || self.b.is_none() || self.rev.is_some() {
+                return Err(Failure::new(
+                    "rejected_argument",
+                    "strict pinning requires explicit a and b hashes, without rev",
+                ));
+            }
+        } else if self.rev.is_none() {
+            return Err(Failure::new(
+                "rejected_argument",
+                "strict pinning requires an explicit rev hash",
+            ));
+        }
+        Ok(())
+    }
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
@@ -391,120 +432,121 @@ impl GitReadPlugin {
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
+                        include_body: args.include_body,
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
@@ -520,128 +562,131 @@ impl GitReadPlugin {
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
-                    .text(&[
-                        "status",
-                        "--porcelain=v1",
-                        "-z",
-                        "--no-renames",
-                        "--untracked-files=all",
-                        "--ignore-submodules=all",
-                    ])
+                    .worktree_status(
+                        &commit,
+                        &[
+                            "status",
+                            "--porcelain=v1",
+                            "-z",
+                            "--no-renames",
+                            "--untracked-files=all",
+                            "--ignore-submodules=all",
+                        ],
+                    )
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
@@ -664,157 +709,168 @@ impl GitReadPlugin {
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
+        "include_body":{"type":"boolean"},
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
+    let format = if args.include_body {
+        "--format=%H%x00%an%x00%aI%x00%cI%x00%s%x00%b"
+    } else {
+        "--format=%H%x00%an%x00%aI%x00%cI%x00%s"
+    };
     let mut argv = vec![
         "log",
         "--no-ext-diff",
         "--no-textconv",
         "--no-show-signature",
         "--no-use-mailmap",
         "--encoding=UTF-8",
-        "--format=%H%x00%an%x00%aI%x00%cI%x00%s",
+        format,
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
-    for record in fields.chunks(5) {
-        if record.len() != 5 {
+    let width = if args.include_body { 6 } else { 5 };
+    for record in fields.chunks(width) {
+        if record.len() != width {
             return Err(Failure::new("git_error", "invalid log record"));
         }
-        entries.push(json!({"sha":record[0],"author":record[1],"author_date":record[2],"committer_date":record[3],"subject":record[4]}));
+        let mut entry = json!({"sha":record[0],"author":record[1],"author_date":record[2],"committer_date":record[3],"subject":record[4]});
+        if args.include_body {
+            entry["body"] = json!(record[5]);
+        }
+        entries.push(entry);
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
diff --git c/src/plugins/git_read/runner.rs w/src/plugins/git_read/runner.rs
index 91bfcb2..fe0e95c 100644
--- c/src/plugins/git_read/runner.rs
+++ w/src/plugins/git_read/runner.rs
@@ -1,106 +1,111 @@
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
+    workspace: PathBuf,
+    gitdir: PathBuf,
+    common: PathBuf,
+    limits: GitReadConfig,
+    filter_overrides: Vec<String>,
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
@@ -140,237 +145,369 @@ impl ReadRepo {
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
+            workspace,
+            gitdir,
+            common,
+            limits: limits.clone(),
+            filter_overrides: Vec::new(),
             env,
             process,
         };
+        // ADR-019: status can run clean/process filters. Disable every
+        // configured driver before any operation that can inspect worktree content.
+        let filters = repo
+            .output(&[
+                "config",
+                "--null",
+                "--name-only",
+                "--get-regexp",
+                "^filter\\..*\\.(clean|smudge|process|required)$",
+            ])
+            .await?;
+        if filters.status.code() != Some(1) {
+            repo.check(&filters)?;
+        }
+        if filters.stdout_truncated
+            || filters.stderr_truncated
+            || filters.stdout.len() > limits.max_input_bytes
+        {
+            return Err(Failure::new(
+                "output_limit",
+                "filter configuration exceeds metadata budget",
+            ));
+        }
+        if !filters.stderr.trim().is_empty() {
+            return Err(Failure::new(
+                "invalid_repository",
+                "cannot safely inspect filter configuration",
+            ));
+        }
+        for key in filters.stdout.split_terminator('\0') {
+            if key.contains('=') || key.chars().any(char::is_control) {
+                return Err(Failure::new(
+                    "invalid_repository",
+                    "unsupported filter configuration key",
+                ));
+            }
+            let value = if key.ends_with(".required") {
+                "false"
+            } else {
+                ""
+            };
+            repo.filter_overrides.push(format!("{key}={value}"));
+        }
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
+        self.output_with_env(arguments, &self.env).await
+    }
+    async fn output_with_env(
+        &self,
+        arguments: &[&str],
+        env: &HashMap<String, String>,
+    ) -> Result<ManagedOutput, Failure> {
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
+        for setting in &self.filter_overrides {
+            argv.extend(["-c".to_string(), setting.clone()]);
+        }
         argv.extend(arguments.iter().map(|s| (*s).into()));
-        managed_process::run("git", &argv, &self.path, &self.env, &self.process, None)
+        managed_process::run("git", &argv, &self.path, env, &self.process, None)
             .await
             .map_err(Into::into)
     }
     pub async fn text(&self, arguments: &[&str]) -> Result<String, Failure> {
-        let output = self.output(arguments).await?;
+        self.text_with_env(arguments, &self.env).await
+    }
+    async fn text_with_env(
+        &self,
+        arguments: &[&str],
+        env: &HashMap<String, String>,
+    ) -> Result<String, Failure> {
+        let output = self.output_with_env(arguments, env).await?;
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
+    pub async fn worktree_status(
+        &self,
+        commit: &str,
+        arguments: &[&str],
+    ) -> Result<String, Failure> {
+        let config = self
+            .output(&[
+                "config",
+                "--null",
+                "--get-regexp",
+                super::status::STATUS_CONFIG_PATTERN,
+            ])
+            .await?;
+        if config.status.code() != Some(1) {
+            self.check(&config)?;
+        }
+        if config.stdout_truncated || config.stderr_truncated {
+            return Err(Failure::new(
+                "output_limit",
+                "status configuration exceeds capture limit",
+            ));
+        }
+        if !config.stderr.trim().is_empty() {
+            return Err(Failure::new(
+                "invalid_repository",
+                "cannot freeze status configuration",
+            ));
+        }
+        let view = super::status::StatusView::create(
+            &self.workspace,
+            &self.gitdir,
+            &self.common,
+            commit,
+            &config.stdout,
+            &self.limits,
+        )
+        .await?;
+        let mut env = self.env.clone();
+        env.insert("GIT_DIR".into(), view.path.to_string_lossy().into_owned());
+        env.insert(
+            "GIT_COMMON_DIR".into(),
+            view.path.to_string_lossy().into_owned(),
+        );
+        env.insert(
+            "GIT_INDEX_FILE".into(),
+            view.path.join("index").to_string_lossy().into_owned(),
+        );
+        env.insert(
+            "GIT_OBJECT_DIRECTORY".into(),
+            self.common.join("objects").to_string_lossy().into_owned(),
+        );
+        self.text_with_env(arguments, &env).await
+    }
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
-        self.resolve(revision, "commit").await
+        if self.limits.strict_pinning && !super::is_full_sha1(revision) {
+            return Err(Failure::new(
+                "rejected_argument",
+                "strict pinning requires a full SHA-1 commit hash",
+            ));
+        }
+        let commit = self.resolve(revision, "commit").await?;
+        if self.limits.strict_pinning && commit != revision.to_ascii_lowercase() {
+            return Err(Failure::new(
+                "rejected_argument",
+                "strict pinning requires an unpeeled SHA-1 commit object",
+            ));
+        }
+
+        Ok(commit)
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
diff --git c/src/plugins/git_read/status.rs w/src/plugins/git_read/status.rs
new file mode 100644
index 0000000..a61482f
--- /dev/null
+++ w/src/plugins/git_read/status.rs
@@ -0,0 +1,229 @@
+//! Frozen metadata prevents repository-config races from executing status filters.
+//! Context: docs/contexts/git-read/CONTEXT.md and ADR-019.
+
+use super::runner::Failure;
+use crate::config::GitReadConfig;
+use std::path::{Path, PathBuf};
+use tokio::io::AsyncReadExt;
+
+// These data-only settings preserve built-in status normalization. Executable settings,
+// includes, and filter sections never enter the private repository.
+pub(super) const STATUS_CONFIG_PATTERN: &str = r"^(core\.(filemode|ignorecase|symlinks|precomposeunicode|trustctime|checkstat|ignorestat|autocrlf|eol|safecrlf|attributesfile|excludesfile|checkroundtripencoding|sparsecheckout|sparsecheckoutcone|protectntfs|protecthfs)|index\.(sparse|version))$";
+
+pub(super) struct StatusView {
+    pub path: PathBuf,
+    remaining: u64,
+}
+
+impl Drop for StatusView {
+    fn drop(&mut self) {
+        if let Err(error) = std::fs::remove_dir_all(&self.path) {
+            tracing::warn!(target:"daimonos::git_read",event="status_metadata_cleanup_failed",%error);
+        }
+    }
+}
+
+impl StatusView {
+    pub async fn create(
+        workspace: &Path,
+        gitdir: &Path,
+        common: &Path,
+        commit: &str,
+        config: &str,
+        limits: &GitReadConfig,
+    ) -> Result<Self, Failure> {
+        let directory = limits
+            .status_directory
+            .as_deref()
+            .map(crate::paths::expand_tilde)
+            .unwrap_or_else(std::env::temp_dir);
+        let directory = if directory.is_absolute() {
+            directory
+        } else {
+            workspace.join(directory)
+        };
+        let directory = tokio::fs::canonicalize(&directory).await.map_err(|error| {
+            Failure::new(
+                "io_error",
+                format!("git_read.status_directory {}: {error}", directory.display()),
+            )
+        })?;
+        if directory.starts_with(workspace) {
+            return Err(Failure::new(
+                "rejected_argument",
+                format!(
+                    "git_read.status_directory must be outside the workspace: {}",
+                    directory.display()
+                ),
+            ));
+        }
+        let path = directory.join(format!("daimonos-git-status-{}", uuid::Uuid::new_v4()));
+        // Acquire directory ownership without an await between creation and the drop guard.
+        let mut builder = std::fs::DirBuilder::new();
+        #[cfg(unix)]
+        {
+            use std::os::unix::fs::DirBuilderExt;
+            builder.mode(0o700);
+        }
+        builder.create(&path).map_err(|error| {
+            Failure::new(
+                "io_error",
+                format!("git_read.status_directory {}: {error}", directory.display()),
+            )
+        })?;
+        let mut view = Self {
+            path,
+            remaining: limits.max_status_metadata_bytes as u64,
+        };
+        tokio::fs::create_dir(view.path.join("refs")).await?;
+        tokio::fs::create_dir(view.path.join("info")).await?;
+        let format = match commit.len() {
+            40 => "[core]\nrepositoryformatversion=0\nbare=false\n",
+            64 => {
+                "[core]\nrepositoryformatversion=1\nbare=false\n[extensions]\nobjectformat=sha256\n"
+            }
+            _ => {
+                return Err(Failure::new(
+                    "git_error",
+                    "unsupported commit identifier format",
+                ))
+            }
+        };
+        let mut settings = format.to_string();
+        for record in config.split_terminator('\0') {
+            let (key, value) = record.split_once('\n').unwrap_or((record, "true"));
+            let (section, name) = key
+                .split_once('.')
+                .ok_or_else(|| Failure::new("git_error", "invalid status configuration"))?;
+            let allowed = match section {
+                "core" => matches!(
+                    name,
+                    "filemode"
+                        | "ignorecase"
+                        | "symlinks"
+                        | "precomposeunicode"
+                        | "trustctime"
+                        | "checkstat"
+                        | "ignorestat"
+                        | "autocrlf"
+                        | "eol"
+                        | "safecrlf"
+                        | "attributesfile"
+                        | "excludesfile"
+                        | "checkroundtripencoding"
+                        | "sparsecheckout"
+                        | "sparsecheckoutcone"
+                        | "protectntfs"
+                        | "protecthfs"
+                ),
+                "index" => matches!(name, "sparse" | "version"),
+                _ => false,
+            };
+            if !allowed {
+                return Err(Failure::new(
+                    "git_error",
+                    "unexpected status configuration key",
+                ));
+            }
+            let value = value
+                .replace('\\', "\\\\")
+                .replace('"', "\\\"")
+                .replace('\n', "\\n")
+                .replace('\t', "\\t");
+            settings.push_str(&format!("[{section}]\n{name}=\"{value}\"\n"));
+        }
+        view.write("config", settings.as_bytes()).await?;
+        // Detached HEAD and porcelain v1 without --branch do not need upstream refs.
+        view.write("HEAD", format!("{commit}\n").as_bytes()).await?;
+        view.copy(&gitdir.join("index"), "index", workspace).await?;
+        for name in ["attributes", "exclude"] {
+            view.copy(
+                &common.join("info").join(name),
+                &format!("info/{name}"),
+                workspace,
+            )
+            .await?;
+        }
+        view.copy(
+            &gitdir.join("info/sparse-checkout"),
+            "info/sparse-checkout",
+            workspace,
+        )
+        .await?;
+        // Git keeps a linked worktree's shared indexes next to its index. The combined
+        // worktree/split-index regression verifies rev-parse --shared-index-path.
+        // Charge top-level names to bound the scan. Do not recurse into refs or objects.
+        let mut entries = tokio::fs::read_dir(gitdir).await?;
+        while let Some(entry) = entries.next_entry().await? {
+            let name = entry.file_name();
+            view.charge(name.len() as u64)?;
+            if name.to_string_lossy().starts_with("sharedindex.") {
+                let name = name
+                    .to_str()
+                    .ok_or_else(|| Failure::new("git_error", "invalid shared-index name"))?;
+                view.copy(&entry.path(), name, workspace).await?;
+            }
+        }
+        Ok(view)
+    }
+
+    fn charge(&mut self, bytes: u64) -> Result<(), Failure> {
+        self.remaining = self.remaining.checked_sub(bytes).ok_or_else(|| {
+            Failure::new(
+                "output_limit",
+                "status metadata exceeds git_read.max_status_metadata_bytes",
+            )
+        })?;
+        Ok(())
+    }
+    async fn write(&mut self, name: &str, bytes: &[u8]) -> Result<(), Failure> {
+        self.charge(bytes.len() as u64)?;
+        let mut options = tokio::fs::OpenOptions::new();
+        options.write(true).create_new(true);
+        #[cfg(unix)]
+        options.mode(0o600);
+        let mut file = options.open(self.path.join(name)).await?;
+        tokio::io::AsyncWriteExt::write_all(&mut file, bytes).await?;
+        // Finish queued writes before a separate Git process opens the metadata.
+        tokio::io::AsyncWriteExt::flush(&mut file).await?;
+        Ok(())
+    }
+    async fn copy(&mut self, source: &Path, name: &str, workspace: &Path) -> Result<(), Failure> {
+        match tokio::fs::symlink_metadata(source).await {
+            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
+            Err(error) => return Err(error.into()),
+            Ok(_) => {}
+        }
+        let source = tokio::fs::canonicalize(source).await?;
+        if !source.starts_with(workspace) {
+            return Err(Failure::new(
+                "repo_outside_workspace",
+                "status metadata points outside workspace",
+            ));
+        }
+        let mut source_options = tokio::fs::OpenOptions::new();
+        source_options.read(true);
+        #[cfg(unix)]
+        source_options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
+        let file = source_options.open(&source).await?;
+        let metadata = file.metadata().await?;
+        if !metadata.is_file() {
+            return Err(Failure::new(
+                "invalid_repository",
+                "status metadata is not a regular file",
+            ));
+        }
+        let mut input = file.take(self.remaining.saturating_add(1));
+        let mut options = tokio::fs::OpenOptions::new();
+        options.write(true).create_new(true);
+        #[cfg(unix)]
+        options.mode(0o600);
+        let mut output = options.open(self.path.join(name)).await?;
+        let bytes = tokio::io::copy(&mut input, &mut output).await?;
+        self.charge(bytes)?;
+        // Git's racy-index check depends on the original index timestamp.
+        let output = output.into_std().await;
+        output.set_times(std::fs::FileTimes::new().set_modified(metadata.modified()?))?;
+        Ok(())
+    }
+}
diff --git c/src/plugins/git_read/tests.rs w/src/plugins/git_read/tests.rs
index 547eba9..e40e0d0 100644
--- c/src/plugins/git_read/tests.rs
+++ w/src/plugins/git_read/tests.rs
@@ -70,162 +70,193 @@ impl Fixture {
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
-    for args in f.cases() {
+    for (args, limits) in f.policy_cases() {
         let before = snapshot(&f.repo);
-        let response = f.call(args.clone()).await;
+        let response = f.with_limits(args.clone(), limits).await;
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
-    fs::write(f.repo.join(".gitattributes"), "*.txt diff=trap\n").unwrap();
-    for args in f.cases() {
-        let response = f.call(args.clone()).await;
+    git(
+        &f.repo,
+        &["config", "filter.trap.clean", script.to_str().unwrap()],
+    );
+    git(
+        &f.repo,
+        &["config", "filter.trap.smudge", script.to_str().unwrap()],
+    );
+    git(
+        &f.repo,
+        &["config", "filter.trap.process", script.to_str().unwrap()],
+    );
+    git(&f.repo, &["config", "filter.trap.required", "true"]);
+    git(
+        &f.repo,
+        &["config", "alias.show", &format!("!{}", script.display())],
+    );
+    let hooks = f.workspace.path().join("hooks");
+    fs::create_dir(&hooks).unwrap();
+    for hook in ["post-index-change", "post-checkout", "pre-auto-gc"] {
+        fs::copy(&script, hooks.join(hook)).unwrap();
+    }
+    git(
+        &f.repo,
+        &["config", "core.hooksPath", hooks.to_str().unwrap()],
+    );
+    fs::write(
+        f.repo.join(".gitattributes"),
+        "*.txt diff=trap filter=trap\n",
+    )
+    .unwrap();
+    fs::write(f.repo.join("renamed.txt"), "dirty working-tree content\n").unwrap();
+    for (args, limits) in f.policy_cases() {
+        let response = f.with_limits(args.clone(), limits).await;
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
@@ -591,60 +622,697 @@ async fn upstream_resolution_is_local_and_search_syntax_has_a_structured_result(
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
+
+#[tokio::test]
+async fn strict_pinning_refuses_a_branch_while_the_default_keeps_it() {
+    let f = Fixture::new();
+    let args = json!({"command":"resolve","repo":"source","rev":"main"});
+    assert_eq!(f.call(args.clone()).await.exit_code, 0);
+    let strict = GitReadConfig {
+        strict_pinning: true,
+        ..Default::default()
+    };
+    let response = f.with_limits(args, strict).await;
+    assert_eq!(
+        response.output["error"]["kind"], "rejected_argument",
+        "{}",
+        response.output
+    );
+}
+
+#[tokio::test]
+async fn strict_pinning_requires_commit_objects_not_annotated_tag_objects() {
+    let f = Fixture::new();
+    git(
+        &f.repo,
+        &[
+            "tag",
+            "-a",
+            "policy-tag",
+            "-m",
+            "Tag object for strict policy",
+        ],
+    );
+    let tag = git(&f.repo, &["rev-parse", "refs/tags/policy-tag"]);
+    assert_eq!(
+        f.call(json!({"command":"resolve","repo":"source","rev":tag}))
+            .await
+            .exit_code,
+        0
+    );
+    let strict = GitReadConfig {
+        strict_pinning: true,
+        ..Default::default()
+    };
+    let response = f
+        .with_limits(
+            json!({"command":"resolve","repo":"source","rev":tag}),
+            strict,
+        )
+        .await;
+    assert_eq!(
+        response.output["error"]["kind"], "rejected_argument",
+        "{}",
+        response.output
+    );
+}
+
+impl Fixture {
+    fn policy_cases(&self) -> Vec<(Value, GitReadConfig)> {
+        let mut cases: Vec<_> = self
+            .cases()
+            .into_iter()
+            .map(|args| (args, GitReadConfig::default()))
+            .collect();
+        for mut args in self.cases() {
+            let command = args["command"].as_str().unwrap();
+            if matches!(command, "refs" | "worktree_status") {
+                continue;
+            }
+            if !matches!(command, "diff" | "ancestry")
+                && args
+                    .get("rev")
+                    .and_then(Value::as_str)
+                    .is_none_or(|rev| rev.len() != 40)
+            {
+                args["rev"] = json!(self.last);
+            }
+            cases.push((
+                args,
+                GitReadConfig {
+                    strict_pinning: true,
+                    ..Default::default()
+                },
+            ));
+        }
+        cases
+    }
+}
+
+#[tokio::test]
+async fn strict_pinning_rejects_all_unpinned_revision_forms_and_operations() {
+    let f = Fixture::new();
+    let strict = GitReadConfig {
+        strict_pinning: true,
+        ..Default::default()
+    };
+    for command in OPERATIONS {
+        if matches!(*command, "refs" | "worktree_status") {
+            let result = f
+                .with_limits(
+                    json!({"command":command,"repo":"source","rev":f.last}),
+                    strict.clone(),
+                )
+                .await;
+            assert_eq!(result.output["error"]["kind"], "rejected_argument");
+            continue;
+        }
+        for revision in [
+            None,
+            Some("HEAD"),
+            Some("HEAD~1"),
+            Some("main"),
+            Some("review"),
+            Some("abcd123"),
+            Some("HEAD..main"),
+        ] {
+            let mut args = json!({"command":command,"repo":"source","path":"renamed.txt"});
+            if matches!(*command, "diff" | "ancestry") {
+                args["b"] = json!(f.last);
+                if let Some(revision) = revision {
+                    args["a"] = json!(revision);
+                }
+            } else if let Some(revision) = revision {
+                args["rev"] = json!(revision);
+            }
+            let result = f.with_limits(args.clone(), strict.clone()).await;
+            assert_eq!(
+                result.output["error"]["kind"], "rejected_argument",
+                "{args}: {}",
+                result.output
+            );
+        }
+    }
+}
+
+#[tokio::test]
+async fn commit_bodies_are_additive_and_do_not_change_default_log_fields() {
+    let f = Fixture::new();
+    git(
+        &f.repo,
+        &[
+            "commit",
+            "--allow-empty",
+            "-m",
+            "Body fixture",
+            "-m",
+            "First body line.\nSecond body line.",
+        ],
+    );
+    let ordinary = f
+        .call(json!({"command":"log","repo":"source","limit":1}))
+        .await;
+    assert!(ordinary.output["entries"][0].get("body").is_none());
+    for command in ["log", "show_commit"] {
+        let result = f
+            .call(json!({"command":command,"repo":"source","limit":1,"include_body":true}))
+            .await;
+        assert_eq!(result.exit_code, 0, "{}", result.output);
+        let commit = if command == "log" {
+            &result.output["entries"][0]
+        } else {
+            &result.output["commit"]
+        };
+        assert_eq!(commit["body"], "First body line.\nSecond body line.\n");
+        assert_eq!(commit["subject"], "Body fixture");
+    }
+}
+
+#[tokio::test]
+async fn default_status_reports_raw_bytes_without_running_normalizing_filters() {
+    let f = Fixture::new();
+    let filtered = f.repo.join("filtered.txt");
+    fs::write(&filtered, "worktree content\n").unwrap();
+    fs::write(
+        f.repo.join(".gitattributes"),
+        "filtered.txt filter=normalize\n",
+    )
+    .unwrap();
+    git(
+        &f.repo,
+        &[
+            "config",
+            "filter.normalize.clean",
+            "sed s/worktree/canonical/g",
+        ],
+    );
+    git(&f.repo, &["config", "filter.normalize.required", "true"]);
+    git(&f.repo, &["add", "filtered.txt", ".gitattributes"]);
+    git(&f.repo, &["commit", "-m", "Normalizing filter fixture"]);
+    assert_eq!(
+        git(&f.repo, &["show", "HEAD:filtered.txt"]),
+        "canonical content"
+    );
+    assert_eq!(fs::read_to_string(&filtered).unwrap(), "worktree content\n");
+    // Force a content check without allowing ordinary Git to refresh the index.
+    fs::File::open(&filtered)
+        .unwrap()
+        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(5)))
+        .unwrap();
+    let before = snapshot(&f.repo);
+    assert_eq!(
+        git(
+            &f.repo,
+            &["--no-optional-locks", "status", "--porcelain=v1"]
+        ),
+        ""
+    );
+    assert_eq!(snapshot(&f.repo), before);
+    let result = f
+        .call(json!({"command":"worktree_status","repo":"source"}))
+        .await;
+    assert_eq!(result.exit_code, 0, "{}", result.output);
+    assert_eq!(result.output["clean"], false);
+    assert!(result.output["entries"]
+        .as_array()
+        .unwrap()
+        .iter()
+        .any(|entry| entry["path"] == "filtered.txt" && entry["status"] == " M"));
+    assert_eq!(snapshot(&f.repo), before);
+}
+
+#[tokio::test]
+async fn mixed_case_filter_configuration_is_suppressed_under_both_policies() {
+    use std::io::Write;
+    let f = Fixture::new();
+    let marker = f.workspace.path().join("mixed-filter-executed");
+    let command = format!("printf executed > '{}'", marker.display());
+    let mut config = fs::OpenOptions::new()
+        .append(true)
+        .open(f.repo.join(".git/config"))
+        .unwrap();
+    writeln!(
+        config,
+        "[FILTER \"MiXeD\"]\nClEaN = {command}\nPrOcEsS = {command}\nReQuIrEd = true"
+    )
+    .unwrap();
+    drop(config);
+    fs::write(f.repo.join(".gitattributes"), "*.txt filter=MiXeD\n").unwrap();
+    let content = fs::read(f.repo.join("renamed.txt")).unwrap();
+    fs::write(f.repo.join("renamed.txt"), vec![b'X'; content.len()]).unwrap();
+    let keys = git(
+        &f.repo,
+        &["config", "--name-only", "--get-regexp", "^filter\\."],
+    );
+    assert!(keys.contains("filter.MiXeD.clean"), "{keys}");
+    assert!(keys.contains("filter.MiXeD.process"), "{keys}");
+    let before = snapshot(&f.repo);
+    for (args, limits) in f.policy_cases() {
+        let result = f.with_limits(args.clone(), limits).await;
+        assert_eq!(result.exit_code, 0, "{args}: {}", result.output);
+        assert!(!marker.exists(), "a mixed-case filter executed");
+    }
+    assert_eq!(snapshot(&f.repo), before);
+}
+
+#[tokio::test]
+async fn strict_pair_operations_reject_annotated_tags_on_both_sides() {
+    let f = Fixture::new();
+    git(&f.repo, &["tag", "-a", "pair-tag", "-m", "Pair input tag"]);
+    let tag = git(&f.repo, &["rev-parse", "refs/tags/pair-tag"]);
+    let strict = GitReadConfig {
+        strict_pinning: true,
+        ..Default::default()
+    };
+    let before = snapshot(&f.repo);
+    for command in ["diff", "ancestry"] {
+        for (a, b) in [(&tag, &f.last), (&f.first, &tag)] {
+            let args = json!({"command":command,"repo":"source","a":a,"b":b});
+            let ordinary = f.call(args.clone()).await;
+            assert_eq!(ordinary.exit_code, 0, "{}", ordinary.output);
+            let pinned = f.with_limits(args.clone(), strict.clone()).await;
+            assert_eq!(
+                pinned.output["error"]["kind"], "rejected_argument",
+                "{args}: {}",
+                pinned.output
+            );
+        }
+    }
+    assert_eq!(snapshot(&f.repo), before);
+}
+
+#[tokio::test]
+async fn malformed_filter_configuration_fails_structurally_without_mutation() {
+    use std::io::Write;
+    let f = Fixture::new();
+    let mut config = fs::OpenOptions::new()
+        .append(true)
+        .open(f.repo.join(".git/config"))
+        .unwrap();
+    writeln!(config, "[unterminated").unwrap();
+    drop(config);
+    let before = snapshot(&f.repo);
+    for args in [
+        json!({"command":"worktree_status","repo":"source"}),
+        json!({"command":"show_file","repo":"source","rev":f.first,"path":"old.txt"}),
+    ] {
+        let result = f.call(args).await;
+        assert_ne!(result.exit_code, 0);
+        assert_eq!(
+            result.output["error"]["kind"], "git_error",
+            "{}",
+            result.output
+        );
+        assert!(result.output["error"]["message"]
+            .as_str()
+            .unwrap()
+            .contains("config"));
+    }
+    assert_eq!(snapshot(&f.repo), before);
+}
+
+#[tokio::test]
+async fn private_status_metadata_is_bounded_and_removed_after_failure() {
+    let f = Fixture::new();
+    let state = tempfile::tempdir().unwrap();
+    let limits = GitReadConfig {
+        status_directory: Some(state.path().to_string_lossy().into_owned()),
+        max_status_metadata_bytes: 1,
+        ..Default::default()
+    };
+    let before = snapshot(&f.repo);
+    let result = f
+        .with_limits(json!({"command":"worktree_status","repo":"source"}), limits)
+        .await;
+    assert_eq!(
+        result.output["error"]["kind"], "output_limit",
+        "{}",
+        result.output
+    );
+    assert_eq!(fs::read_dir(state.path()).unwrap().count(), 0);
+    assert_eq!(snapshot(&f.repo), before);
+    let unsafe_directory = GitReadConfig {
+        status_directory: Some(f.repo.to_string_lossy().into_owned()),
+        ..Default::default()
+    };
+    let refused = f
+        .with_limits(
+            json!({"command":"worktree_status","repo":"source"}),
+            unsafe_directory,
+        )
+        .await;
+    assert_eq!(refused.output["error"]["kind"], "rejected_argument");
+    assert_eq!(snapshot(&f.repo), before);
+}
+
+#[tokio::test]
+async fn frozen_status_preserves_builtin_normalization_split_index_and_local_excludes() {
+    let f = Fixture::new();
+    git(&f.repo, &["config", "core.autocrlf", "true"]);
+    fs::write(f.repo.join(".git/info/attributes"), "eol.txt text\n").unwrap();
+    fs::write(f.repo.join("eol.txt"), "first\r\nsecond\r\n").unwrap();
+    git(&f.repo, &["add", "eol.txt"]);
+    git(&f.repo, &["commit", "-m", "Built-in normalization fixture"]);
+    fs::File::open(f.repo.join("eol.txt"))
+        .unwrap()
+        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(5)))
+        .unwrap();
+    fs::write(f.repo.join(".git/info/exclude"), "ignored.local\n").unwrap();
+    fs::write(f.repo.join("ignored.local"), "ignored\n").unwrap();
+    git(&f.repo, &["update-index", "--split-index"]);
+    assert_eq!(
+        git(
+            &f.repo,
+            &["--no-optional-locks", "status", "--porcelain=v1"]
+        ),
+        ""
+    );
+    let before = snapshot(&f.repo);
+    let result = f
+        .call(json!({"command":"worktree_status","repo":"source"}))
+        .await;
+    assert_eq!(result.exit_code, 0, "{}", result.output);
+    assert_eq!(result.output["clean"], true, "{}", result.output);
+    assert_eq!(snapshot(&f.repo), before);
+}
+
+#[tokio::test]
+async fn frozen_status_handles_linked_worktrees_reftable_and_sha256() {
+    let f = Fixture::new();
+    git(
+        &f.repo,
+        &["worktree", "add", "--detach", "../linked", &f.last],
+    );
+    git(f.workspace.path(), &["clone", "source", "reftable"]);
+    git(
+        &f.workspace.path().join("reftable"),
+        &["refs", "migrate", "--ref-format=reftable"],
+    );
+    git(
+        f.workspace.path(),
+        &["init", "--object-format=sha256", "sha256"],
+    );
+    let sha256 = f.workspace.path().join("sha256");
+    git(&sha256, &["config", "user.name", "Format Fixture"]);
+    git(&sha256, &["config", "user.email", "format@example.test"]);
+    git(&sha256, &["config", "commit.gpgsign", "false"]);
+    fs::write(sha256.join("file.txt"), "format fixture\n").unwrap();
+    git(&sha256, &["add", "."]);
+    git(&sha256, &["commit", "-m", "Format fixture"]);
+    let before = snapshot(f.workspace.path());
+    for repo in ["linked", "reftable", "sha256"] {
+        let result = f
+            .call(json!({"command":"worktree_status","repo":repo}))
+            .await;
+        assert_eq!(result.exit_code, 0, "{repo}: {}", result.output);
+        assert_eq!(result.output["clean"], true, "{repo}: {}", result.output);
+        assert_eq!(
+            result.output["commits"][0].as_str().unwrap().len(),
+            if repo == "sha256" { 64 } else { 40 }
+        );
+    }
+    assert_eq!(snapshot(f.workspace.path()), before);
+}
+
+#[tokio::test]
+async fn strict_uppercase_hash_inputs_return_canonical_lowercase_commits() {
+    let f = Fixture::new();
+    let strict = GitReadConfig {
+        strict_pinning: true,
+        ..Default::default()
+    };
+    let result = f
+        .with_limits(
+            json!({"command":"resolve","repo":"source","rev":f.last.to_ascii_uppercase()}),
+            strict,
+        )
+        .await;
+    assert_eq!(result.exit_code, 0, "{}", result.output);
+    assert_eq!(result.output["commits"], json!([f.last]));
+}
+
+#[tokio::test]
+async fn frozen_status_preserves_racy_index_content_checks() {
+    let f = Fixture::new();
+    let file = f.repo.join("renamed.txt");
+    let index = f.repo.join(".git/index");
+    let stamp = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
+    git(&f.repo, &["config", "core.trustctime", "false"]);
+    fs::File::open(&file)
+        .unwrap()
+        .set_times(fs::FileTimes::new().set_modified(stamp))
+        .unwrap();
+    git(
+        &f.repo,
+        &["update-index", "--really-refresh", "--", "renamed.txt"],
+    );
+    let previous = fs::read(&file).unwrap();
+    fs::write(&file, vec![b'X'; previous.len()]).unwrap();
+    fs::File::open(&file)
+        .unwrap()
+        .set_times(fs::FileTimes::new().set_modified(stamp))
+        .unwrap();
+    fs::File::open(&index)
+        .unwrap()
+        .set_times(fs::FileTimes::new().set_modified(stamp))
+        .unwrap();
+    assert_eq!(
+        git(
+            &f.repo,
+            &["--no-optional-locks", "status", "--porcelain=v1"]
+        ),
+        "M renamed.txt"
+    );
+    let before = snapshot(&f.repo);
+    let result = f
+        .call(json!({"command":"worktree_status","repo":"source"}))
+        .await;
+    assert_eq!(result.exit_code, 0, "{}", result.output);
+    assert_eq!(result.output["clean"], false, "{}", result.output);
+    assert!(result.output["entries"]
+        .as_array()
+        .unwrap()
+        .iter()
+        .any(|entry| entry["path"] == "renamed.txt"));
+    assert_eq!(snapshot(&f.repo), before);
+}
+
+#[tokio::test]
+async fn frozen_status_handles_a_linked_split_index_with_an_upstream() {
+    let f = Fixture::new();
+    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
+    listener.set_nonblocking(true).unwrap();
+    git(
+        &f.repo,
+        &["update-ref", "refs/remotes/origin/main", &f.first],
+    );
+    git(
+        &f.repo,
+        &[
+            "config",
+            "remote.origin.fetch",
+            "+refs/heads/*:refs/remotes/origin/*",
+        ],
+    );
+    git(
+        &f.repo,
+        &[
+            "config",
+            "remote.origin.url",
+            &format!("http://{}/forbidden", listener.local_addr().unwrap()),
+        ],
+    );
+    git(
+        &f.repo,
+        &[
+            "worktree",
+            "add",
+            "-b",
+            "linked-split",
+            "../linked-split",
+            &f.last,
+        ],
+    );
+    let linked = f.workspace.path().join("linked-split");
+    git(&linked, &["branch", "--set-upstream-to=origin/main"]);
+    git(&linked, &["update-index", "--split-index"]);
+    assert_eq!(git(&linked, &["rev-parse", "@{upstream}"]), f.first);
+    let gitdir = PathBuf::from(git(&linked, &["rev-parse", "--absolute-git-dir"]))
+        .canonicalize()
+        .unwrap();
+    let shared = linked
+        .join(git(&linked, &["rev-parse", "--shared-index-path"]))
+        .canonicalize()
+        .unwrap();
+    assert_eq!(shared.parent(), Some(gitdir.as_path()));
+    fs::write(linked.join("note.txt"), "untracked note\n").unwrap();
+    assert_eq!(
+        git(
+            &linked,
+            &["--no-optional-locks", "status", "--porcelain=v1"]
+        ),
+        "?? note.txt"
+    );
+    let before = snapshot(f.workspace.path());
+    let result = f
+        .call(json!({"command":"worktree_status","repo":"linked-split"}))
+        .await;
+    assert_eq!(result.exit_code, 0, "{}", result.output);
+    assert_eq!(
+        result.output["entries"],
+        json!([{"status":"??","path":"note.txt"}])
+    );
+    assert_eq!(snapshot(f.workspace.path()), before);
+    assert_eq!(
+        listener.accept().unwrap_err().kind(),
+        std::io::ErrorKind::WouldBlock
+    );
+}
+
+#[tokio::test]
+async fn frozen_status_respects_alternate_object_store_confinement() {
+    let f = Fixture::new();
+    git(
+        f.workspace.path(),
+        &["clone", "--shared", "source", "shared"],
+    );
+    let before = snapshot(f.workspace.path());
+    let allowed = f
+        .call(json!({"command":"worktree_status","repo":"shared"}))
+        .await;
+    assert_eq!(allowed.exit_code, 0, "{}", allowed.output);
+    assert_eq!(allowed.output["clean"], true);
+    assert_eq!(snapshot(f.workspace.path()), before);
+    let outside = tempfile::tempdir().unwrap();
+    let external = outside.path().join("objects");
+    fs::create_dir(&external).unwrap();
+    fs::write(
+        f.workspace
+            .path()
+            .join("shared/.git/objects/info/alternates"),
+        format!("{}\n", external.display()),
+    )
+    .unwrap();
+    let changed = snapshot(f.workspace.path());
+    let refused = f
+        .call(json!({"command":"worktree_status","repo":"shared"}))
+        .await;
+    assert_eq!(refused.output["error"]["kind"], "repo_outside_workspace");
+    assert_eq!(snapshot(f.workspace.path()), changed);
+}
+
+#[tokio::test]
+async fn frozen_status_preserves_quoted_path_configuration_values() {
+    let f = Fixture::new();
+    let exclude = f
+        .workspace
+        .path()
+        .join("exclude-\"-backslash\\-line\n-tab\t-return\r");
+    fs::write(&exclude, "ignored.local\n").unwrap();
+    git(
+        &f.repo,
+        &["config", "core.excludesFile", exclude.to_str().unwrap()],
+    );
+    fs::write(f.repo.join("ignored.local"), "ignored\n").unwrap();
+    assert_eq!(
+        git(
+            &f.repo,
+            &["--no-optional-locks", "status", "--porcelain=v1"]
+        ),
+        ""
+    );
+    let before = snapshot(f.workspace.path());
+    let result = f
+        .call(json!({"command":"worktree_status","repo":"source"}))
+        .await;
+    assert_eq!(result.exit_code, 0, "{}", result.output);
+    assert_eq!(result.output["clean"], true, "{}", result.output);
+    assert_eq!(snapshot(f.workspace.path()), before);
+}
+
+#[cfg(unix)]
+#[tokio::test]
+async fn frozen_status_refuses_fifo_metadata_without_waiting_for_a_writer() {
+    let f = Fixture::new();
+    let state = tempfile::tempdir().unwrap();
+    let index = f.repo.join(".git/index");
+    fs::remove_file(&index).unwrap();
+    assert!(Command::new("mkfifo")
+        .arg(&index)
+        .status()
+        .unwrap()
+        .success());
+    let limits = GitReadConfig {
+        status_directory: Some(state.path().to_string_lossy().into_owned()),
+        ..Default::default()
+    };
+    let result = tokio::time::timeout(
+        Duration::from_secs(2),
+        f.with_limits(json!({"command":"worktree_status","repo":"source"}), limits),
+    )
+    .await
+    .expect("FIFO metadata must not block");
+    assert_eq!(
+        result.output["error"]["kind"], "invalid_repository",
+        "{}",
+        result.output
+    );
+    assert_eq!(fs::read_dir(state.path()).unwrap().count(), 0);
+}
diff --git c/src/tool_descriptions.rs w/src/tool_descriptions.rs
index 088518c..1e62cbb 100644
--- c/src/tool_descriptions.rs
+++ w/src/tool_descriptions.rs
@@ -1,74 +1,74 @@
 //! Runtime-configurable tool and parameter descriptions.
 
 use serde::Deserialize;
 use std::collections::HashMap;
 
 pub const DEFAULT_TEXT: &str = include_str!("../prompts/tool_descriptions.toml");
 
 /// Migration sentinel: the number of model-facing parameter strings moved
 /// from structural Rust schemas into the embedded catalog by #1007. Kept as
 /// an independent literal (rather than derived from the catalog) so accidental
 /// deletion fails parity tests. Future intentional additions/removals update
 /// this single value.
 #[cfg(test)]
-pub(crate) const DEFAULT_PARAMETER_DESCRIPTION_COUNT: usize = 156;
+pub(crate) const DEFAULT_PARAMETER_DESCRIPTION_COUNT: usize = 157;
 #[cfg(test)]
 pub(crate) const AGENT_ONLY_PARAMETER_DESCRIPTION_COUNT: usize = 2;
 #[cfg(test)]
 pub(crate) const MCP_PARAMETER_DESCRIPTION_COUNT: usize =
     DEFAULT_PARAMETER_DESCRIPTION_COUNT - AGENT_ONLY_PARAMETER_DESCRIPTION_COUNT;
 
 #[derive(Debug, Clone, Default, Deserialize)]
 #[serde(default)]
 struct DescriptionEntry {
     full: Option<String>,
     terse: Option<String>,
     parameters: HashMap<String, String>,
     #[serde(flatten)]
     extra: HashMap<String, toml::Value>,
 }
 
 #[derive(Debug, Clone)]
 pub struct ToolDescriptions {
     entries: HashMap<String, DescriptionEntry>,
 }
 
 impl Default for ToolDescriptions {
     fn default() -> Self {
         Self {
             entries: parse(DEFAULT_TEXT)
                 .expect("embedded prompts/tool_descriptions.toml must be valid TOML"),
         }
     }
 }
 
 impl ToolDescriptions {
     pub fn full(&self, name: &str) -> Option<&str> {
         self.entries.get(name)?.full.as_deref()
     }
 
     pub fn terse(&self, name: &str) -> Option<&str> {
         self.entries.get(name)?.terse.as_deref()
     }
 
     #[cfg(test)]
     fn parameter(&self, tool: &str, parameter: &str) -> Option<&str> {
         self.entries
             .get(tool)?
             .parameters
             .get(parameter)
             .map(String::as_str)
     }
 
     /// Clone a tool's structural schema and inject the resolved parameter
     /// descriptions into its top-level `properties` entries. Schema shape,
     /// validation keywords, and lazy exposure policy remain owned by
     /// `tools.rs`; only model-facing text comes from this catalog (#1007).
     pub fn schema_with_parameters(
         &self,
         tool: &str,
         schema: &serde_json::Value,
     ) -> serde_json::Value {
         let mut schema = schema.clone();
         let Some(parameters) = self.entries.get(tool).map(|entry| &entry.parameters) else {
             return schema;
diff --git c/tests/test_git_read.py w/tests/test_git_read.py
index 88b11ea..94b505a 100644
--- c/tests/test_git_read.py
+++ w/tests/test_git_read.py
@@ -235,60 +235,232 @@ def test_git_read_grep_frames_matches_without_final_newlines(daimonos):
     git(repo,"commit","-m","Grep framing fixture")
     response=daimonos.call_tool("git_read",{"command":"grep","repo":"source","pattern":"needle"})
     assert not response.get("isError"),response
     assert json.loads(response["content"][0]["text"])["matches"]==[
         {"path":"a.txt","line_number":1,"line":"needle without newline"},
         {"path":"b.txt","line_number":1,"line":"needle with newline"},
         {"path":"c.txt","line_number":2,"line":"needle two"},
         {"path":"c.txt","line_number":3,"line":"needle three"},
     ]
 
 
 def test_git_read_blame_continuation_keeps_the_original_start_line(daimonos):
     repository(daimonos.workspace)
     params={"command":"blame","repo":"source","path":"file.txt","start_line":1,"end_line":2,"limit":1}
     first=daimonos.call_tool("git_read",params)
     assert not first.get("isError"),first
     first_page=json.loads(first["content"][0]["text"])
     second=daimonos.call_tool("git_read",{**params,"offset":first_page["next_offset"]})
     assert not second.get("isError"),second
     second_page=json.loads(second["content"][0]["text"])
     assert [first_page["lines"][0]["line_number"],second_page["lines"][0]["line_number"]]==[1,2]
     assert second_page["truncated"] is False
 
 
 def test_git_read_grep_reports_successful_stderr(daimonos_factory,tmp_path):
     import os
     import shutil
     import sys
 
     repo,_=repository(tmp_path)
     real_git=shutil.which("git")
     assert real_git
     wrapper_dir=tmp_path/"wrapper"
     wrapper_dir.mkdir()
     wrapper=wrapper_dir/"git"
     wrapper.write_text(
         f"#!{sys.executable}\nimport subprocess,sys\n"
         f"code=subprocess.run([{real_git!r},*sys.argv[1:]]).returncode\n"
         "if 'grep' in sys.argv[1:]: print('warning: incomplete search',file=sys.stderr)\n"
         "sys.exit(code)\n"
     )
     wrapper.chmod(0o755)
     client=daimonos_factory(tmp_path,env={"PATH":str(wrapper_dir)+os.pathsep+os.environ["PATH"]})
     response=client.call_tool("git_read",{"command":"grep","repo":"source","pattern":"first"})
     assert response.get("isError"),response
     failure=json.loads(response["content"][0]["text"])
     assert failure["error"]["kind"]=="git_error"
     assert "incomplete search" in failure["error"]["message"]
 
 
 def test_git_read_short_inheritance_prefixes_do_not_leak_git_controls(daimonos_factory,tmp_path):
     repository(tmp_path)
     for index,prefix in enumerate(("G","")):
         trace=tmp_path/f"git-trace-{index}"
         (tmp_path/"daimonos.toml").write_text(f'[process]\ninherit_env_prefixes=["{prefix}"]\n')
         client=daimonos_factory(tmp_path,env={"GIT_TRACE":str(trace)})
         trace.unlink(missing_ok=True)
         response=client.call_tool("git_read",{"command":"resolve","repo":"source"})
         assert not response.get("isError"),response
         assert not trace.exists(),f"git controls leaked through prefix {prefix!r}"
+
+
+def test_git_read_strict_policy_keeps_default_resolution_and_adds_bodies(daimonos_factory,tmp_path):
+    repo,first=repository(tmp_path)
+    git(repo,"commit","--allow-empty","-m","Body fixture","-m","Stored body.")
+    commit=git(repo,"rev-parse","HEAD")
+    ordinary=daimonos_factory(tmp_path)
+    assert not ordinary.call_tool("git_read",{"command":"resolve","repo":"source","rev":"HEAD"}).get("isError")
+    (tmp_path/"daimonos.toml").write_text("[git_read]\nstrict_pinning=true\n")
+    strict=daimonos_factory(tmp_path)
+    for revision in (None,"HEAD","main",commit[:10],"HEAD~1",f"{first}..{commit}"):
+        args={"command":"resolve","repo":"source"}
+        if revision is not None:
+            args["rev"]=revision
+        response=strict.call_tool("git_read",args)
+        assert response.get("isError"),response
+        assert json.loads(response["content"][0]["text"])["error"]["kind"]=="rejected_argument"
+    resolved=strict.call_tool("git_read",{"command":"resolve","repo":"source","rev":commit})
+    assert not resolved.get("isError"),resolved
+    assert json.loads(resolved["content"][0]["text"])["commits"]==[commit]
+    history=strict.call_tool("git_read",{"command":"log","repo":"source","rev":commit,"include_body":True,"limit":1})
+    assert not history.get("isError"),history
+    assert json.loads(history["content"][0]["text"])["entries"][0]["body"]=="Stored body.\n"
+    assert not ordinary.call_tool("git_read",{"command":"resolve","repo":"source","rev":"HEAD"}).get("isError")
+
+
+def test_git_read_strict_operations_on_a_real_read_only_bind_mount(daimonos_binary,tmp_path):
+    import errno
+    import os
+    import shutil
+    import sys
+    import pytest
+
+    if sys.platform!="linux" or not shutil.which("unshare"):
+        pytest.skip("requires Linux user and mount namespaces")
+    supported=subprocess.run(["unshare","-Urnm","true"],capture_output=True,text=True)
+    if supported.returncode:
+        pytest.skip("user or mount namespaces are unavailable")
+    workspace=tmp_path/"evidence"
+    workspace.mkdir()
+    _,commit=repository(workspace)
+    (workspace/"daimonos.toml").write_text("[git_read]\nstrict_pinning=true\n[logging]\nenabled=false\n[analytics]\nenabled=false\n")
+    source_tests=str(Path(__file__).resolve().parent)
+    program=r'''
+import errno,hashlib,json,os,subprocess,sys
+from pathlib import Path
+sys.path.insert(0,sys.argv[1])
+from conftest import DaimonosClient
+binary,workspace,commit=sys.argv[2:]
+workspace=Path(workspace)
+def snapshot():
+    return [(str(p.relative_to(workspace)),p.lstat().st_size,p.lstat().st_mtime_ns,hashlib.sha256(p.read_bytes()).hexdigest() if p.is_file() else None) for p in sorted(workspace.rglob("*"))]
+subprocess.run(["mount","--bind",str(workspace),str(workspace)],check=True)
+subprocess.run(["mount","-o","remount,bind,ro",str(workspace)],check=True)
+try:
+    (workspace/"must-not-write").write_text("forbidden")
+    raise AssertionError("the mount is not read-only")
+except OSError as error:
+    assert error.errno==errno.EROFS,error
+before=snapshot()
+process=subprocess.Popen([binary,"--mcp","-w",str(workspace)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
+try:
+    client=DaimonosClient(process,str(workspace))
+    client.send_raw({"jsonrpc":"2.0","id":client._next_id(),"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"readonly-probe","version":"1"}}})
+    client.send_raw({"jsonrpc":"2.0","method":"notifications/initialized"})
+    calls=[{"command":"resolve"},{"command":"show_file","path":"file.txt"},{"command":"list_tree"},{"command":"grep","pattern":"first"},{"command":"log","include_body":True},{"command":"show_commit","include_body":True},{"command":"diff","a":commit,"b":commit},{"command":"blame","path":"file.txt"},{"command":"ancestry","a":commit,"b":commit}]
+    for args in calls:
+        if args["command"] not in ("diff","ancestry"):
+            args["rev"]=commit
+        response=client.call_tool("git_read",{"repo":"source",**args})
+        assert not response.get("isError"),(args,response)
+        assert commit in json.loads(response["content"][0]["text"])["commits"]
+    for command in ("refs","worktree_status"):
+        response=client.call_tool("git_read",{"repo":"source","command":command})
+        assert response.get("isError"),response
+        assert json.loads(response["content"][0]["text"])["error"]["kind"]=="rejected_argument"
+finally:
+    process.terminate()
+    process.wait(timeout=5)
+assert snapshot()==before,"the evidence changed"
+print(json.dumps({"read_only_mount":True,"pinned_operations":9,"refused_operations":2,"unchanged":True}))
+'''
+    env=os.environ.copy()
+    env["DAIMONOS_KGL_AUTOINDEX"]="0"
+    result=subprocess.run(["unshare","-Urnm",sys.executable,"-c",program,source_tests,str(daimonos_binary),str(workspace),commit],env=env,capture_output=True,text=True,timeout=60)
+    assert result.returncode==0,result.stdout+result.stderr
+    assert json.loads(result.stdout)=={"read_only_mount":True,"pinned_operations":9,"refused_operations":2,"unchanged":True}
+
+
+def test_git_read_filter_added_after_inspection_cannot_execute(daimonos_factory,tmp_path):
+    import os
+    import shutil
+    import sys
+
+    repo,_=repository(tmp_path)
+    included=tmp_path/"late-filter.cfg"
+    included.write_text("")
+    marker=tmp_path/"late-filter-executed"
+    malicious=tmp_path/"late-filter.sh"
+    malicious.write_text(f"#!/bin/sh\nprintf executed > '{marker}'\n")
+    malicious.chmod(0o755)
+    git(repo,"config","include.path",str(included))
+    (repo/".gitattributes").write_text("file.txt filter=late\n")
+    git(repo,"add",".gitattributes")
+    git(repo,"commit","-m","Late filter attribute")
+    original=(repo/"file.txt").read_bytes()
+    (repo/"file.txt").write_bytes(b"X"*len(original))
+    real_git=shutil.which("git")
+    assert real_git
+    wrapper_dir=tmp_path/"git-wrapper"
+    wrapper_dir.mkdir()
+    wrapper=wrapper_dir/"git"
+    payload=f'[filter "late"]\nprocess = {malicious}\nrequired = true\n'
+    trigger=tmp_path/"status-boundary-reached"
+    wrapper.write_text(
+        f"#!{sys.executable}\nimport pathlib,subprocess,sys\n"
+        f"if 'status' in sys.argv[1:]:\n    pathlib.Path({str(included)!r}).write_text({payload!r})\n    pathlib.Path({str(trigger)!r}).write_text('triggered')\n"
+        f"sys.exit(subprocess.run([{real_git!r},*sys.argv[1:]]).returncode)\n"
+    )
+    wrapper.chmod(0o755)
+    client=daimonos_factory(tmp_path,env={"PATH":str(wrapper_dir)+os.pathsep+os.environ["PATH"]})
+    trigger.unlink(missing_ok=True)
+    included.write_text(payload)
+    baseline=subprocess.run([real_git,"--no-optional-locks","status","--porcelain=v1"],cwd=repo,capture_output=True,text=True)
+    assert marker.exists(),baseline.stdout+baseline.stderr
+    marker.unlink()
+    included.write_text("")
+    response=client.call_tool("git_read",{"command":"worktree_status","repo":"source"})
+    assert trigger.exists(),"the process-boundary race did not run"
+    assert included.read_text()==payload
+    assert not marker.exists(),"a filter introduced after the inspection executed"
+    assert not response.get("isError"),response
+
+
+def test_frozen_status_cannot_inherit_global_or_system_filters(daimonos_factory,tmp_path):
+    import os
+    import shutil
+
+    workspace=tmp_path/"evidence"
+    workspace.mkdir()
+    repo,_=repository(workspace)
+    original=(repo/"file.txt").read_bytes()
+    (repo/"system.txt").write_bytes(original)
+    (repo/".gitattributes").write_text("file.txt filter=fromglobal required\nsystem.txt filter=fromsystem required\n")
+    git(repo,"add",".")
+    git(repo,"commit","-m","Inherited filter attributes")
+    for name in ("file.txt","system.txt"):
+        (repo/name).write_bytes(b"X"*len(original))
+    global_config=tmp_path/"global.gitconfig"
+    system_config=tmp_path/"system.gitconfig"
+    markers=[]
+    for driver,config in (("fromglobal",global_config),("fromsystem",system_config)):
+        marker=tmp_path/f"{driver}-executed"
+        script=tmp_path/f"{driver}.sh"
+        script.write_text(f"#!/bin/sh\nprintf executed > '{marker}'\n")
+        script.chmod(0o755)
+        config.write_text(f'[FILTER "{driver}"]\nPrOcEsS = {script}\nReQuIrEd = true\n')
+        markers.append(marker)
+    inherited={"GIT_CONFIG_GLOBAL":str(global_config),"GIT_CONFIG_SYSTEM":str(system_config),"GIT_CONFIG_NOSYSTEM":"0"}
+    real_git=shutil.which("git")
+    assert real_git
+    for name,marker in zip(("file.txt","system.txt"),markers):
+        baseline=subprocess.run([real_git,"--no-optional-locks","status","--porcelain=v1","--",name],cwd=repo,env={**os.environ,**inherited},capture_output=True,text=True)
+        assert marker.exists(),baseline.stdout+baseline.stderr
+        marker.unlink()
+    client=daimonos_factory(workspace,env=inherited)
+    for marker in markers:
+        marker.unlink(missing_ok=True)
+    response=client.call_tool("git_read",{"command":"worktree_status","repo":"source"})
+    assert not response.get("isError"),response
+    assert not any(marker.exists() for marker in markers)
+    assert json.loads(response["content"][0]["text"])["clean"] is False

### New Points
R5. The complete revised FR-06 diff includes context for all unchanged Git environment guards, every source/test/doc change, and the new status.rs module. No other request is implemented here. Fresh final validation after all production changes passed: fmt, clippy -D warnings, cargo test 2016 passed/10 ignored, and pytest 386 passed/1 skipped. Linux EROFS mount coverage actually ran. Final evidence/log paths appear in docs/reviews/2026-10-28-fr06-validation.md.
R6. Additional correctness/resource fixes: preserve copied metadata mtimes so a fresh copy cannot hide racy-index changes; flush queued HEAD/config writes before the child opens them; use nonblocking/no-follow Unix metadata opens and reject non-regular files before copying. The FIFO/cleanup regression passes. Cargo.toml changes only the existing libc dependency comment. No crate was added.
R7. S5: the race regression already includes a native Git positive baseline, an explicit boundary-trigger assertion, and unchanged malicious payload assertions. I kept production instrumentation out. S6: retained Git-compatible quoting rather than reject legitimate path values; a real-Git comparison now covers quotes, backslashes, newline, tab and carriage return in core.excludesFile. Key names remain a closed whitelist. S7: added frozen_status_respects_alternate_object_store_confinement, which passes for an internal shared clone and refuses an external alternate without mutation.
R8. The user narrowed this session to completing FR-06 and saving a local checkpoint. No publication/merge, other feature implementation, or multi-agent coordination is authorized yet. FR-08 must still route the new internal metadata directory under its explicit state root. Architecture records remain Proposed. Please resolve the remaining concern IDs explicitly if satisfied. This is review round 3, the protocol's final review round.

**Requesting:** RE-REVIEW

<!-- event id="review-3" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 3
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S8. Non-blocking, for the FR-08 slice: when status_directory moves under the explicit state root, add a regression that two concurrent worktree_status calls on the same repository produce independent private views and independent cleanup (the UUID-named directories make collisions implausible, but a concurrency test would pin the drop-guard behavior under parallel load, which no current test exercises).
S9. Non-blocking documentation note: configuration.md describes the frozen-status view but does not mention that copied index metadata preserves original mtimes for racy-index correctness. A one-line note in ADR-019's Decision section exists; mirroring it in the status.rs module doc would help the next maintainer not 'simplify' the set_times call away.

### Responses to Prior Points
Re R5: Accepted. The round-3 diff carries the full context for the pre-existing environment hardening (GIT_CONFIG_NOSYSTEM=1, GIT_CONFIG_SYSTEM=/dev/null, GIT_CONFIG_GLOBAL=/dev/null, GIT_ATTR_NOSYSTEM=1, and the GIT_*/PAGER inheritance stripping), which was the missing visibility behind B5. Fresh final validation (fmt, clippy -D warnings, 2016 Rust passed / 10 ignored, 386 Python passed / 1 skipped) with the updated evidence artifact is sufficient closing evidence for this slice.
Re R6: Accepted. The mtime-preservation fix for copied index metadata is a genuine correctness catch — a fresh-mtime copy would defeat Git's racy-index content check, and frozen_status_preserves_racy_index_content_checks pins it deterministically. The O_NONBLOCK|O_NOFOLLOW no-follow open, the non-regular-file rejection before copy, the explicit flush before the child opens the metadata, and the FIFO timeout regression with verified cleanup all close the remaining filesystem-shaped failure modes I would otherwise have raised. The Cargo.toml change being comment-only on the existing libc dependency is confirmed in the diff; no new crate was added.
Re R7: S5: acceptable as-is — the native-Git positive baseline plus the trigger-file and payload-intact assertions make the race evidence sufficient, and keeping instrumentation out of production is the right trade. S5 is closed without further change. S6: the real-Git comparison test covering quote, backslash, newline, tab, and carriage-return in core.excludesFile (frozen_status_preserves_quoted_path_configuration_values) demonstrates serializer parity on exactly the value shapes that matter for the whitelisted keys; combined with the closed key whitelist, I accept quoting over rejection. S6 closed. S7: frozen_status_respects_alternate_object_store_confinement exercises the status path specifically for both the permitted internal shared clone and the refused external alternate, which is what I asked for. S7 closed. All suggestions S1–S7 are now resolved or explicitly accepted as non-changes.
Re R8: Scope acknowledged. This approval covers only the FR-06 slice as presented: strict pinning, additive bodies, and the human-approved filter-suppression exception, with no claim over FR-02/FR-03/FR-07/FR-08 integration, no publication or merge authorization, and ADR-018/ADR-019 remaining Proposed pending architectural review. Resolving B1, B5, and B6 explicitly per your request. Residual items that must be tracked outside this review: (1) FR-08 must move the status_directory default off the system temp directory onto the explicit state root; (2) the Linux-only EROFS coverage means macOS mount behavior remains unverified, as the validation artifact correctly states; (3) the Deciders fields in both ADRs still await architectural sign-off, which this product-level review does not supply.
B1: resolved
B5: resolved
B6: resolved

**Verdict:** APPROVE
