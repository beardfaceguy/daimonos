# dbunk reviewer checkpoint: FR-06 reviewed

Date: 2026-10-28
Status: FR-06 implementation and local review are complete. The remaining requests are not implemented.
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
- Status uses a private frozen metadata view. Later repository-config changes cannot add executable filters to that process.
- Original metadata timestamps preserve Git's racy-index checks. FIFO metadata is refused without waiting.
- `git_read.status_directory` selects an existing private-state directory outside the workspace.
- `git_read.max_status_metadata_bytes` bounds that metadata. FR-08 must route it under the explicit state root.
- Unsupported filter keys and oversized metadata fail closed.
- No crate dependency was added in this slice.

## Approved compatibility exception

The hostile-config regression reproduced clean/process filter execution from default worktree status.
Suppressing filters changes status semantics for legitimate filtered repositories, including Git LFS repositories.
The human selected option 2: "option 2 sounds good".
Keep the suppression in both modes. This question is resolved. Do not ask it again.
The configuration guide and Proposed ADR-019 record the exception.
ADR-018 and ADR-019 remain Proposed. Code-review approval does not imply architectural acceptance.

## Latest actual validation

These results cover the reviewed production patch. Only completion notes changed after approval.

- Formatting passed.
- `cargo clippy --all-targets -- -D warnings` passed.
- Full Rust suite: 2016 passed, 0 failed, 10 ignored.
- Full Python suite: 386 passed, 0 failed, 1 skipped.
- The full suites include the FR-06 pinning, mutation, configuration-race, filter-scope, timestamp, and metadata regressions.
- The Linux user/mount namespace test actually ran. It verified EROFS on a read-only bind mount.
- That test exercised nine pinned operations and two refused operations, with unchanged evidence snapshots.
- Full logs: `target/dbunk-fr06-rust-final.log` and `target/dbunk-fr06-pytest-final.log`.
- Those logs and temporary test programs are not committed.
- Rubber Ducky thread `dbunk-fr06-1576` approved in round 3. All blocking concern IDs are resolved.
- Review log: `rubber_ducky/1576-dbunk-git-pinning.md`.
- Validation evidence: `docs/reviews/2026-10-28-fr06-validation.md`.
- The macOS read-only mount check remains unverified. No macOS result is claimed.

## Next session

1. Inspect this branch and the latest local checkpoint commit. Do not overwrite unrelated work.
2. Read the PM answers, ADR-017, Proposed ADR-018/019, and the latest Vikunja notes.
3. Discuss smaller, independently verifiable work packages with the human before starting another feature.
4. Plan multi-agent ownership, dependencies, branch boundaries, and integration checks. No agents are assigned yet.
5. Keep FR-16 withdrawn and the Linear port deferred.
6. In FR-08, route the private status metadata under the explicit state root.
7. Include reviewer suggestion S8: test two concurrent status calls with independent private views and cleanup.
8. Keep #1576 open until merge. No publication or merge is authorized yet.

Validation commands:

```bash
export PATH="$PWD/target/test-tools/bin:$PWD/.venv/bin:$PATH"
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
python3 -m pytest tests/ -v
```

## Pending task map

- #1576: FR-06. Implementation and review are complete locally. Publication and merge remain pending.
- #1577: FR-08. Not implemented.
- #1578: FR-07. Not implemented.
- #1579: P1 tool policy, confinement, and workers. Not implemented.
- #1580: P1 structured output, transport, probe, and audit. Not implemented.
- #1581: P2 configuration, schemas, link/output policies, and local image. Not implemented.
- #1582: P3 digests, offline profile, and budgets. Not implemented.

FR-06 local work is complete. The tracking task stays open until merge. No push, PR, or merge occurred.
The local checkpoint preserves the code and notes. Its SHA is recorded in Vikunja.

_Anchors:_ src/config.rs::GitReadConfig, src/plugins/git_read.rs::GitReadPlugin, src/plugins/git_read/runner.rs::ReadRepo
