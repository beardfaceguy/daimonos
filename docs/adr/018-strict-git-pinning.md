# ADR-018: Strict git pinning is an opt-in policy

- **Status:** Proposed
- **Date:** 2026-10-27
- **Deciders:** Pending architectural review. The human approved the product requirements.
- **Consulted:** dbunk PM, through the round-1 written answers
- **Informed:** None recorded
- **Spec:** `daimonos-feature-requests-for-dbunk.md`, FR-06, and `daimonos-pm-answers-round-1.md`
- **Tracking:** Vikunja #1576, parent #1575
- **Supersedes:** None
- **Superseded by:** None

## Context

ADR-017 permits local revision resolution and returns full commit identifiers.
The dbunk reviewer instead needs caller-pinned input identifiers before any git read.
The PM accepted an additive policy and rejected a global revision-contract change.
How can dbunk require pinned inputs without breaking existing callers?

## Decision drivers

- Existing calls, defaults, and result shapes must remain unchanged.
- Every strict read must identify its input commits explicitly.
- Output pinning cannot replace input validation.
- The existing process hardening must remain active under both policies.

## Decision

We will add `git_read.strict_pinning`, default false.
When true, operations require explicit full 40-character hexadecimal commit identifiers.
`diff` and `ancestry` require both `a` and `b` in that form.
Other pinned operations require `rev`. Implicit HEAD, ranges, relative revisions, and short hashes are rejected.
`refs` and `worktree_status` are refused because they cannot supply a pinned view.
`resolve` remains useful only for verifying an explicit full commit identifier.
The existing runner still resolves and checks the object type locally.
This policy does not weaken confinement, no-mutation, or no-network controls.

We will add `include_body`, default false, for `log` and `show_commit`.
The field `body` appears in commit metadata only when the caller requests it.
Existing names and result fields remain unchanged by default.

## Alternatives

- Globally reject revision names. This breaks ADR-017 and existing callers.
- Accept names and only pin outputs. A moving ref can change before the request starts.
- Create a second git tool. This duplicates the operation surface and process hardening.

## Consequences

The caller must explicitly select strict pinning. It is not a complete reviewer sandbox.
Forty-character hashes restrict the strict profile to SHA-1 repositories.
The default policy still supports other locally resolvable git object formats.
Commit bodies can increase output size, so existing response caps still apply.

## Confirmation

- Test every strict operation through the public plugin interface.
- Reject missing hashes, names, tags, short hashes, ranges, and unpinned operations.
- Prove names still work with the default policy.
- Compare repository snapshots before and after strict calls and hostile-config tests.
- Exercise strict reads on a real read-only mount. Record unavailable platform facilities explicitly.
- Verify `include_body` through Rust and MCP without changing default metadata fields.
- Run `cargo test` and `python3 -m pytest tests/ -v`.

## Anchors

_Anchors:_ src/plugins/git_read.rs::GitReadPlugin, src/plugins/git_read.rs::Arguments, src/plugins/git_read.rs::history, src/plugins/git_read/runner.rs::ReadRepo, src/config.rs::GitReadConfig

## Revisit when

The caller needs strict SHA-256 repositories or a versioned pinned-only tool contract.

## Open questions

No product acceptance blocker remains for this slice. Architectural review is pending.
