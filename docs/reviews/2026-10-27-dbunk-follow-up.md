# dbunk follow-up: Q-FR-06-2.1

Date: 2026-10-27
Parent: Vikunja #1575
Slice: Vikunja #1576
Branch: feat/1575-dbunk-reviewer
Status: The human approved option 2 on 2026-10-27. Implementation is paused for hibernation. Not published.

## Q-FR-06-2.1

The expanded hostile-configuration test found a default-mode security gap.
`git_read worktree_status` can execute a configured clean/process filter while examining dirty tracked files.
The subprocess ran in the reproduction. The old fsmonitor, external-diff, and textconv checks do not prevent it.

Disabling those filters prevents execution, but changes status semantics for legitimate filtered repositories.
For example, raw working-tree bytes can differ from stored filter output in a Git LFS repository.
The no-regression rule prohibits silently selecting that new behavior for existing callers.

Options presented before the human decision:

1. **Preserve default behavior.** Apply the additional filter suppression only under strict pinning.
   Keep default status behavior and report that the full guarantee does not hold with the policy off.
   dbunk must enable strict pinning. Record the default-mode security issue separately.
2. **Authorize a default security-fix exception.** Suppress configured filters in both modes.
   Document that default `worktree_status` ignores those transformations and can report different status results.

Strict pinning already refuses worktree_status, refs, names, ranges, short hashes, and implicit HEAD.
Other strict operations use explicit full SHA-1 commit identifiers.
The pending decision does not permit weakening strict mode.

## Local implementation and evidence

- New policy: `[git_read] strict_pinning = true`, default false.
- New option: `include_body=true` on `log` and `show_commit`, default false.
- No operation names changed.
- The patch suppresses filters in both modes. The human approved this default security-fix exception.
- Full validation passed: 2003 Rust tests, 384 Python tests, formatting, and clippy.
- Ten Rust tests were ignored. One Python test was skipped.
- Focused tests passed: 22 Rust tests and 23 MCP tests.
- The Linux bind-mount test ran successfully. It verified EROFS and exercised nine pinned operations.
- Both policies passed mutation snapshots and the expanded hostile-config test with the provisional patch.
- No validator crate was added yet. Dependency selection belongs to the P1 slice.
- FR-07, FR-08, and P1–P3 implementation remain pending. FR-16 is excluded.
- The hibernation handoff records the local checkpoint. No push, merge, or Rubber Ducky approval occurred.

## Human decision

The human selected option 2: "option 2 sounds good".
Keep filter suppression in both modes. Document the changed status semantics for filtered repositories.
The dedicated compatibility test and Rubber Ducky review remain pending.
The later Linear port is deferred until the work is complete. Vikunja remains the current tracker.
