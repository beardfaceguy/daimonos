# ADR-031: One cooperative budget covers delegated and external tool work

- **Status:** Accepted
- **Date:** 2026-10-28
- **Deciders:** Repository maintainer, by explicit user approval on 2026-10-29 UTC.
- **Consulted:** None recorded.
- **Informed:** None recorded.
- **Spec:** Local dbunk requests and PM amendments, sealed in the [package plan](../handoffs/2026-10-28-dbunk-remaining-work.md).
- **Packages:** WP18, WP19
- **Dependencies:** Accepted ADR-017. Accepted ADR-022 and ADR-023 supply access and delegation policy.
- **Supersedes:** None
- **Superseded by:** None

## Context

Output caps limit displayed bytes, not the file or git content scanned to produce them.
Cached indexes and external git processes can keep working after a naive async timeout stops awaiting them.
Optional full-file digests also need complete reads even when returned text is paginated.
How can per-call limits remain meaningful across native, cached, delegated, and subprocess execution?

## Decision drivers

- Count logical input, not physical I/O or output size.
- Stop external work, not only the awaiting future.
- A composite must not reset limits for every child.
- Never report a partial digest or truncated scan as complete success.

## Decision

We will pass an operation-scoped cooperative budget through dispatch and actual data producers.
It carries an absolute deadline and independent optional logical-byte and directory-entry counters.
An inner call inherits the remaining parent allowance and cannot extend the deadline.
Concurrent children share atomic admission/accounting so delegation cannot multiply the allowance.

Charge logical file/git object content read or scanned, including index-file content and repeated work needed to serve the call.
Do not charge physical device I/O or displayed output bytes as a substitute.
Walk entries have their own optional counter. Limits are caller-set and disabled by default.

Native readers and index workers check cancellation/deadline/allowance at bounded work boundaries.
On failure, stop owned work and return a named limit error. Do not return successful partial results.
A blocking worker cannot be abandoned through spawn_blocking cancellation alone.
External calls use the remaining deadline and the managed process-group termination/reaping path.
A timeout wrapper that leaves descendants running does not implement the contract.
Refuse tools whose requested input/entry budget cannot be enforced, only when that budget is enabled.
Deadline enforcement remains required for external work even where exact input metering is unsupported.

For optional digests, hash the complete stored bytes of the opened file or pinned blob.
The digest and returned slice must correspond to the same read identity. Observed inconsistent mutation causes failure, not a mixed-version claim.
If limits prevent complete hashing, return the limit error and no purported full-file digest.
Use the existing SHA-256 dependency. Do not hash decoded, normalized, or merely displayed text.
Output truncation and its continuation remain a separate mechanism.

## Alternatives

- **Use result size as the budget:** a tiny result can require a huge scan.
- **Read filesystem I/O counters:** misses cached content and measures the wrong quantity.
- **Wrap every future in timeout:** can abandon native/background/process work.
- **Hash only the returned slice:** produces misleading evidence identifiers.

## Consequences

Counters and cancellation must reach low-level consumers and private git helpers.
Some tools are unavailable only for enabled budgets they cannot enforce.
Cleanup has bounded scheduling overhead that tests must state honestly.
Full digests can cost a complete read even for a small displayed slice.

## Confirmation

Run future tool-budget and file-digest suites with native read/search and git tests.
Exercise cached indexes, repeated scans, walks, parallel composites, slow readers, and child/grandchild processes.
Observe stopped work and reaped children, not only a returned timeout error.
Compare full stored-byte hashes with independent sha256sum on paged, truncated, binary, and pinned inputs.
Limit-interrupted hashing must fail explicitly. Disabled budgets preserve current behavior.

## Anchors

_Anchors:_ src/ops/mod.rs::dispatch_with_progress, src/managed_process.rs::run, src/managed_process.rs::ManagedChild::terminate, src/plugins/git_read/runner.rs::ReadRepo::output, src/tool_output.rs::ToolOutputConfig::resolved_directory

## Revisit when

A new producer cannot meter logical input or a measured cleanup/scan cost requires a different bounded execution mechanism.

## Open questions

- The user accepted this decision on 2026-10-29 UTC. Remaining interface questions do not imply implementation approval.
- Public limit names, error fields, and cleanup tolerances need interface review before implementation.
