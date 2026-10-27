# dbunk reviewer hibernation checkpoint

Date: 2026-10-27
Status: Paused for the human's commute. The overall feature is incomplete.
Repository: `/home/beardface/work/daimonos`
Branch: `feat/1575-dbunk-reviewer`
Tracker: Vikunja project #183, parent task #1575.

## Tracking decision

The human says: "document this in vikunja. We'll do a Linear port of our notes once we're done."
Vikunja remains authoritative. Do not migrate notes or post to Linear now.

## Contract inputs

- `daimonos-feature-requests-for-dbunk.md`
- `daimonos-pm-answers-round-1.md`
- Initial review: `docs/reviews/2026-10-27-dbunk-initial-review.md`
- Follow-up and approved exception: `docs/reviews/2026-10-27-dbunk-follow-up.md`

The two user-provided input files remain unchanged and untracked. Do not delete them.
The PM answers authorize work in priority order, with default-off controls.
The human approved the future JSON Schema 2020-12 validator dependency.
FR-16 publishing is withdrawn. No registry or publishing CI work is allowed.
No live credentialed model test is authorized.
The final-answer call stays on the model side. Never forward it to a worker.

## Implemented locally: FR-06 only

- `[git_read] strict_pinning = true` requires explicit full SHA-1 commit inputs.
- The default remains false. Names and ranges remain available with the policy off.
- `diff` and `ancestry` require explicit `a` and `b` hashes in strict mode.
- Other permitted operations require explicit `rev`.
- Strict mode refuses `refs`, `worktree_status`, implicit HEAD, ranges, short hashes, names, and tag objects.
- `include_body=true` adds commit bodies to `log` and `show_commit` only when requested.
- Operation names remain unchanged.
- The runner suppresses configured clean, smudge, and process filters in both modes.
- Unsupported filter keys and oversized metadata fail closed.
- No crate dependency was added in this slice.

## Approved compatibility exception

The hostile-config regression reproduced clean/process filter execution from default worktree status.
Suppressing filters changes status semantics for legitimate filtered repositories, including Git LFS repositories.
The human selected option 2: "option 2 sounds good".
Keep the suppression in both modes. This question is resolved. Do not ask it again.
Record the exception in the architecture and user documentation before final review.
ADR-018 remains Proposed. Product approval does not imply architectural acceptance.

## Latest actual validation

These results cover the current production patch. Hibernation preparation changed documentation only.

- Formatting passed.
- `cargo clippy --all-targets -- -D warnings` passed.
- Full Rust suite: 2003 passed, 0 failed, 10 ignored.
- Full Python suite: 384 passed, 0 failed, 1 skipped.
- Focused Rust tests: 22 passed.
- Focused MCP tests: 23 passed.
- The Linux user/mount namespace test actually ran. It verified EROFS on a read-only bind mount.
- That test exercised nine pinned operations and two refused operations, with unchanged evidence snapshots.
- Full logs: `target/dbunk-fr06-rust.log` and `target/dbunk-fr06-pytest.log`.
- Those logs and temporary test programs are not committed.
- No cargo, pytest, clippy, review-generation, or index-update job ran at the pause check.

## Resume

1. Inspect this branch and the latest local checkpoint commit. Do not overwrite unrelated work.
2. Read the contract inputs, PM answers, ADR-017, Proposed ADR-018, and the latest Vikunja notes.
3. Add the focused filtered-status compatibility test for the approved option 2.
4. Document that default status ignores filter transformations and can report different statuses.
5. Run focused tests, both full suites, formatting, and clippy after any code change.
6. Submit the complete FR-06 diff to Rubber Ducky. No review ran for this slice yet.
7. Resolve review findings. Do not claim approval before the durable reviewer approves.
8. Continue with FR-08 state relocation, then FR-07 workspace write protection and their dependencies.
9. Continue P1, P2, and P3 in the accepted order. Exclude FR-16.
10. Record actual names, validator dependency/version, and exit codes as those slices land.

Validation commands:

```bash
export PATH="$PWD/target/test-tools/bin:$PWD/.venv/bin:$PATH"
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
python3 -m pytest tests/ -v
```

## Pending task map

- #1576: FR-06. Local implementation and tests exist. Compatibility test, documentation, and review remain pending.
- #1577: FR-08. Not implemented.
- #1578: FR-07. Not implemented.
- #1579: P1 tool policy, confinement, and workers. Not implemented.
- #1580: P1 structured output, transport, probe, and audit. Not implemented.
- #1581: P2 configuration, schemas, link/output policies, and local image. Not implemented.
- #1582: P3 digests, offline profile, and budgets. Not implemented.

No task is complete. No push, PR, or merge occurred for this new feature.
The local checkpoint preserves the code and notes. Its SHA is recorded in Vikunja.

_Anchors:_ src/config.rs::GitReadConfig, src/plugins/git_read.rs::GitReadPlugin, src/plugins/git_read/runner.rs::ReadRepo
