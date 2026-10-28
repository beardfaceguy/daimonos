# FR-06 local validation

Date: 2026-10-28
Task: Vikunja #1576, under #1575.
Branch: `feat/1575-dbunk-reviewer`.
Status: Rubber Ducky approved round 3. No blocking concerns remain.
Review: `rubber_ducky/1576-dbunk-git-pinning.md`, thread `dbunk-fr06-1576`.
The task remains open until merge. This is a local implementation checkpoint.

## Scope

This evidence covers strict git pinning, additive bodies, and the approved filter security exception.
It does not establish completion of FR-02, FR-03, FR-07, or FR-08 integration.
No new crate, live credentialed endpoint test, publication, or merge occurred.

## Actual checks

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --all-targets -- -D warnings`: passed.
- `cargo test`: 2016 passed, 0 failed, 10 ignored.
- `python3 -m pytest tests/ -v`: 386 passed, 0 failed, 1 skipped.
- The full-suite commands used `target/test-tools/bin` and `.venv/bin` as PATH prefixes.
- Logs: `target/dbunk-fr06-rust-final.log` and `target/dbunk-fr06-pytest-final.log`.
- Those logs and temporary test tools are local artifacts, not committed dependencies.
- Generated review logs and diff artifacts retain their original whitespace. Authored-file whitespace checks exclude those verbatim artifacts.

## Read-only mount evidence

The Linux namespace test ran and passed on this host. It checked EROFS on a real bind mount.
It exercised all nine pinned operations and both refused operations, with unchanged evidence snapshots.
The test skips on non-Linux hosts or when user/mount namespaces are unavailable.
No macOS mount test ran here. A skip does not count as a successful read-only-mount check.

## Review regressions

- A process-boundary fixture introduced a filter through an include after the original inspection.
  The old implementation executed it. The frozen-metadata implementation does not.
- Mixed-case filter keys are canonicalized by Git and suppressed under both policies.
- Both sides of diff and ancestry reject full annotated-tag object IDs under strict pinning.
- Malformed configuration returns a structured failure without mutating the repository.
- The private metadata budget fails closed, and the temporary directory is removed after failure.
- Split indexes, local excludes, built-in text normalization, linked worktrees, reftable, and SHA-256 defaults pass.
- Uppercase SHA-1 inputs return canonical lowercase commit identifiers.
- Global/system filter definitions execute in the native Git baseline but not through the frozen status view.
- A linked worktree with a split index and configured upstream returns the correct porcelain entries without a connection attempt.
- Copied index timestamps retain Git's racy-index content checks. The deterministic regression failed before that fix.
- Status accepts an alternate store inside the workspace and refuses one outside it.
- Quoted, backslash, newline, tab, and carriage-return path settings preserve native Git behavior.
- FIFO metadata is refused without waiting for a writer. The private directory is removed after the failure.

## Internal state

Status uses private metadata outside the workspace. It contains no filter definitions or include directives.
`git_read.status_directory` selects an existing directory. Its default is the system temporary directory.
`git_read.max_status_metadata_bytes` defaults to 64 MiB.
FR-08 must route this internal state under the explicit state root. That integration remains pending.
