# ADR-017: Read-only git access is a separate `git_read` tool

- **Status:** Accepted
- **Date:** 2026-10-24
- **Deciders:** beardface (repo owner). Accepted on 2026-10-24.
- **Consulted:** None recorded.
- **Informed:** None recorded.
- **Spec:** Vikunja #1573, "Add a read-only git tool (`git_read`) to Daimonos" (#225 in project 183)
- **Tracking:** Vikunja #1573
- **Supersedes:** None
- **Superseded by:** None
- **Anchors:** `src/safety.rs::DESTRUCTIVE_TOOLS`, `src/safety.rs::SafetyPolicy::gate`,
  `src/plugins/mod.rs::builtin_cli_plugins`, `src/plugins/git.rs::GitPlugin`
  (full list in [Anchors](#anchors))

## Context

A separate project, dbunk, will run a reviewer agent. The reviewer checks
claims in research packets against source code at pinned commits. It must not
change a repository, contact a network, or run code that a reviewed repository
controls. It connects to `daimonos --mcp`. In agent mode it runs with
`DAIMONOS_AGENT_ALLOWED_COMMANDS=read_file,search,workspace_info,git_read`.

These facts shape the decision:

- Daimonos gates tools by name only. `SafetyPolicy::gate(name)` applies the
  denylist, then the allowlist, then the approval mode. `is_destructive_tool(name)`
  decides whether `interactive` mode prompts. Neither function sees the
  `command` argument.
- The `git` tool (`GitPlugin`) mixes reads (`status`, `log`, `diff`, `branch`)
  with writes (`add`, `commit`, `push`, `pull`, `checkout`) and a `raw`
  passthrough. `git` is in `DESTRUCTIVE_TOOLS`. So an allowlist entry for `git`
  grants every git operation, and `interactive` mode prompts for every git read.
- `GitRun::output` adds only `NONINTERACTIVE_ENV`. The `git` ops do not set
  `GIT_OPTIONAL_LOCKS=0`, `--no-ext-diff`, `--no-textconv`, or
  `core.fsmonitor=false`. So a git read can write the index (`status` refreshes
  it) or run a program that the repository config names.
- A repository's `.git/config` is untrusted input for a reviewer. Keys such as
  `core.fsmonitor`, `diff.external`, and textconv drivers make git run programs.
- A partial clone can fetch missing objects from its remote on demand. A
  shallow clone has no ancestors past its boundary.
- `Session::resolve_path` joins a relative path to the session directory and
  accepts an absolute path as it is. It does not keep a path inside the
  workspace.
- `managed_process::apply_environment` clears the environment. Then it passes
  only the parent variables in `process.inherit_env` and
  `process.inherit_env_prefixes`. The defaults pass no `GIT_*` variable. The
  session and per-call overlays come last, with no filter.
- `src/mcp.rs` does not call `SafetyPolicy`. In MCP mode, the client decides
  which listed tools it calls.
- `AGENTS.md` reserves opcodes for core primitives with no external binary. A
  tool that runs the `git` binary is a plugin.
- The installed git is 2.56.0. Its manual documents `GIT_OPTIONAL_LOCKS` and
  `GIT_NO_LAZY_FETCH`.

Decision question: how does Daimonos give a reviewer read access to git history
and file contents at pinned commits, so that the allowlist and the approval gate
can grant this access without also granting git writes?

## Decision drivers

1. The read-only property must hold inside Daimonos. It must not depend on the
   caller.
2. The permission and approval model works per tool name. The decision must
   work with that model as it is.
3. The `git` tool keeps its current behavior (spec).
4. Every response needs full commit SHAs and a repository path, so citations
   can be pinned.
5. One review can span several repositories inside the workspace.
6. A new crate dependency needs confirmation first (spec).
7. Limits come from config (`.cursor/rules/configurable-limits.mdc`).
8. Model-facing text lives in `prompts/` (spec).

## Decision

We will add a new built-in plugin tool named `git_read`. It is separate from
`git`.

1. **Tool boundary.** `git_read` is a `ToolPlugin` in
   `src/plugins/git_read.rs`. It registers in `plugins::builtin_cli_plugins`
   with `git_read::is_available` as its capability probe. So MCP, agent, and ACP mode all list
   it.
2. **Operations.** `git_read` has only the 11 read ops in the spec: `resolve`,
   `show_file`, `list_tree`, `grep`, `log`, `show_commit`, `diff`, `blame`,
   `refs`, `ancestry`, and `worktree_status`. It has no `raw` op. It accepts no
   flags from the caller.
3. **Classification.** `git_read` is not in `DESTRUCTIVE_TOOLS`, so
   `interactive` mode does not prompt for it. `git` stays in
   `DESTRUCTIVE_TOOLS`.
4. **Backend.** `git_read` runs the system `git` binary through
   `managed_process::run`. Each op builds its argv from a fixed template.
   Caller values go only into the argument slots that the template defines.
5. **Hardening.** Every invocation does all of these steps:
   - It sets `GIT_OPTIONAL_LOCKS=0`, `GIT_NO_LAZY_FETCH=1`, and `GIT_PAGER=cat`.
   - It passes `--no-pager` and `-c core.fsmonitor=false`.
   - It passes `--no-ext-diff` and `--no-textconv` on every op that shows a
     diff or file content.
   - It reuses `git::NONINTERACTIVE_ENV`.
   - It removes from the overlay map each `GIT_*` variable that selects another
     repository, index, or object store. Examples: `GIT_DIR`, `GIT_WORK_TREE`,
     `GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY`, and
     `GIT_ALTERNATE_OBJECT_DIRECTORIES`. The full list is proposed.
6. **Argument checks.** `git_read` rejects each revision, path, or pattern that
   starts with `-`. It uses `--end-of-options` where git supports it. It puts
   `--` before pathspecs and uses literal pathspecs.
7. **Confinement.** `git_read` resolves `repo` after symlinks. It rejects a
   `repo` that is outside the canonical workspace root or that is not a git
   repository. It never accepts `-C`, `--git-dir`, or `--work-tree` from the
   caller.
8. **No network.** No op contacts a remote. A missing object in a shallow or
   partial clone gives an "object missing" error. Git does not fetch the object.
9. **Output.** Each successful response is structured JSON. It contains the
   full commit SHA or SHAs that the op read and the repository path. All caps
   come from `GitReadConfig` and `[git_read]` in `daimonos.default.toml`. A truncated result says `"truncated": true`
   and gives a continuation offset where one makes sense.
   Responses also respect `tool_output.max_bytes`. Reject oversized responses
   before the shared output handler can persist them.
10. **Errors.** Each failure has its own error kind: unknown revision, path not
    found at revision, object missing, binary file, repository outside the
    workspace, and rejected argument. Proposed names: `unknown_revision`,
    `path_not_found`, `object_missing`, `binary_file`,
    `repo_outside_workspace`, and `rejected_argument`.
11. **Description.** The model-facing description goes in
    `prompts/tool_descriptions.toml` under `[git_read]`. It tells the model
    when to use `git_read` and when to use `git`.

12. **Submodules.** `worktree_status` ignores submodules with
    `--ignore-submodules=all`. The reviewer does not run git inside submodules.
13. **Alternates.** Reject repositories with alternate object stores outside the
    workspace. Check chained stores after symlink resolution.
14. **Old git.** Register `git_read` only when git accepts `--no-lazy-fetch`.
    Every request also passes that option. Never use a fallback that can fetch.

## Open questions
The implementation resolves the proposed names. No product question remains open.
Consulted and Informed remain empty because no other participants were recorded.


## Alternatives

- **Gate the existing git tool by operation.** This requires a new permission contract across all callers.
  It also changes the existing git tool, which the spec excludes.
- **Add a read-only mode to git.** A configuration mistake could restore writes under an already allowed tool name.
  Interactive approval would still classify the whole tool as destructive.
- **Permit selected git commands through exec.** Caller-controlled flags remain an injection risk.
  For example, `git diff --output` can write a file.
- **Use gix or git2.** A library avoids subprocess configuration hazards.
  It adds a crate dependency and different behavior for grep, shallow clones, and partial clones.
  This change does not add that dependency.
- **Rely only on client filters or read-only mounts.** These remain useful additional controls.
  A mount does not prevent network access or repository-controlled programs.
  Client filters do not replace a safe implementation of the granted tool.

## Consequences

The existing permission model can grant git reads without granting git writes.
Reviewer citations use full commit SHAs rather than moving refs.

The tool adds a schema and a second set of git argument templates.
Maintainers must review git behavior changes that affect configuration or object fetching.
Older git installations do not register the tool.
Some operations need multiple subprocesses to resolve and read pinned objects.
Configured output caps can reject large reads instead of returning incomplete structured records.

This decision does not add an MCP allowlist or repair delegated policy enforcement.
The report in `docs/security-reports/2026-10-27-delegated-tool-policy.md` records those separate findings.

## Confirmation

The implementation uses these executable checks:

```bash
cargo test plugins::git_read::tests
cargo test git_read_runs_in_agent_mode_while_git_is_blocked
cargo test git_read_allowlist_does_not_grant_git_or_request_approval
python3 -m pytest tests/test_git_read.py -v
cargo test
python3 -m pytest tests/ -v
cargo clippy --all-targets -- -D warnings
```

The focused suite checks all operations against full repository mutation snapshots.
It also checks malicious configuration, option injection, local object failures, and workspace confinement.
The agent test verifies that reads run while the write-capable git tool remains blocked.
The policy audit reproductions document unsafe behavior. They do not assert that the policy is secure.

## Anchors

_Anchors:_

- `src/plugins/git_read.rs::GitReadPlugin`
- `src/plugins/git_read.rs::is_available`
- `src/plugins/git_read/runner.rs::ReadRepo`
- `src/plugins/git_read/tests.rs::every_operation_preserves_git_and_worktree_even_when_stat_stale`
- `src/plugins/git_read/tests.rs::every_operation_ignores_malicious_config`
- `src/config.rs::GitReadConfig`
- `src/plugins/mod.rs::builtin_cli_plugins`
- `src/plugins/git.rs::GitPlugin`
- `src/safety.rs::DESTRUCTIVE_TOOLS`
- `src/safety.rs::SafetyPolicy::gate`
- `src/agent.rs::git_read_runs_in_agent_mode_while_git_is_blocked`
- `src/managed_process.rs::run`

## Revisit when

- Another tool family needs a separate read-only identity.
- A git release adds a configuration path that can execute code during these operations.
- A library backend meets the operation requirements and a new dependency is acceptable.
- Daimonos adopts an MCP policy or inner-call policy enforcement.

Implementation notes updated: 2026-10-27. The accepted boundary remains unchanged.
