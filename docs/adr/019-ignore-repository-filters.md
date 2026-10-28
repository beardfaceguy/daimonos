# ADR-019: Ignore repository-configured filters during git reads

- **Status:** Proposed
- **Date:** 2026-10-28
- **Deciders:** The human approved option 2 of Q-FR-06-2.1 on 2026-10-27. This record awaits architectural review.
- **Consulted:** dbunk PM, through the written requirements.
- **Informed:** None recorded.
- **Spec:** FR-06, PM round 1, and `docs/reviews/2026-10-27-dbunk-follow-up.md`.
- **Tracking:** Vikunja #1576, under #1575.
- **Supersedes:** None. This clarifies the security guarantee in ADR-017.
- **Superseded by:** None.

## Context

Git status can execute a configured clean or process filter while checking dirty files.
The expanded hostile-configuration test reproduced that execution through git_read.
Disabling filters also changes status results for legitimate filtered repositories.
The human explicitly approved that compatibility exception instead of retaining the execution risk.
Should filter suppression apply in both policies, despite the changed status semantics?

## Decision drivers

- A read-only tool must not execute repository-configured programs.
- Strict pinning alone does not protect existing callers of worktree_status.
- The compatibility change must remain explicit and tested.

## Decision

We will suppress configured clean, smudge, and process filters in both pinning policies.
The runner retains bounded per-process overrides as defense for committed-object reads.
Working-tree status does not depend on that scan. It uses a private, frozen Git metadata directory outside the workspace.
The directory contains a pinned HEAD, copied index metadata, and only data-only configuration needed for built-in status behavior.
It contains no filter definitions or include directives. Mutable repository configuration cannot add a driver to that process.
The process reads the original worktree and object store without changing them.
Split-index and local attribute/exclude metadata are copied into the private view.
Copied metadata retains its original mtime. Git needs it for racy-index content checks.
The detached HEAD and porcelain v1 output do not need upstream refs or branch configuration.
Git stores a linked worktree's shared index beside its index. A combined regression verifies Git's reported path.
The metadata budget includes top-level entry names to bound that scan. The scan does not recurse into refs or objects.
The private view is removed after success, failure, or cancellation. No repository configuration file changes.
The worktree result excludes external filter transformations. Built-in Git conversions can still apply.
The user-facing documentation will warn that a filtered file can appear modified where ordinary Git reports clean.

## Alternatives

- Retain filter execution in default mode. Rejected by the human because the read-only tool would remain an execution surface.
- Suppress filters only under strict pinning. This protects the dbunk policy but not ordinary worktree-status callers.
- Execute only familiar filters. Rejected because repository configuration selects executable commands, not trustworthy implementations.

## Consequences

The fix changes default status semantics for repositories such as Git LFS repositories.
This is the narrow compatibility exception approved as option 2, not permission for unrelated default changes.
The existing git tool remains unchanged.
The metadata budgets reject excessive configuration or index snapshots instead of applying incomplete protection.
`git_read.max_status_metadata_bytes` bounds the private view. `git_read.status_directory` selects an existing directory outside the workspace.
Without an override, the view uses the system temporary directory. FR-08 must route this internal state under an explicit state root.

## Confirmation

- Run `cargo test default_status_reports_raw_bytes_without_running_normalizing_filters`.
- In that fixture, ordinary Git reports clean after a real filter transformation.
- Verify that git_read instead reports the tracked file as modified and leaves the full repository snapshot unchanged.
- Run `cargo test every_operation_ignores_malicious_config` under both policies.
- Run `python3 -m pytest tests/test_git_read.py::test_git_read_filter_added_after_inspection_cannot_execute -v`.
- Prove that a filter added through an include immediately before status cannot execute.
- Test mixed-case keys, malformed configuration, split indexes, local excludes, built-in normalization, linked worktrees, reftable, and SHA-256 defaults.
- Verify metadata caps and private-directory cleanup after failure.
- Run `cargo test plugins::git_read::tests` and `python3 -m pytest tests/test_git_read.py -v`.
- Run the full Rust and Python suites before completing this slice.

## Anchors

_Anchors:_ src/plugins/git_read/status.rs::StatusView, src/plugins/git_read/runner.rs::ReadRepo, src/plugins/git_read/tests.rs::default_status_reports_raw_bytes_without_running_normalizing_filters, src/plugins/git_read/tests.rs::every_operation_ignores_malicious_config

## Revisit when

Git provides a way to inspect filter-normalized status without executing repository-controlled programs.
Also revisit this decision if the caller needs explicitly trusted filter execution in a different tool.

## Open questions

The human resolved the product choice. Architectural review of this record remains pending.
