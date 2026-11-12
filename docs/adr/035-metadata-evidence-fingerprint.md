# ADR-035: The evidence fingerprint is a metadata scan, not a git shadow repository

- **Status:** Accepted
- **Date:** 2026-11-12
- **Deciders:** Repository maintainer, by explicit user decision on 2026-11-12.
- **Consulted:** None recorded.
- **Informed:** None recorded.
- **Spec:** Vikunja #1617. Amends the fingerprint mechanism of #1198.
- **Dependencies:** None.
- **Supersedes:** None (#1198 had no ADR).
- **Superseded by:** None

## Context

The evidence ledger (#1198) compares workspace fingerprints around each tool call to tell an edited, untested turn from a verified one. The result is advisory only: a "completion is unverified" note at the end of a turn.
The fingerprint was a git tree built with `git add --force -A` in a new shadow repository per turn. Ignored content was deliberately included.
In a workspace with a 21 GB `target/`, the baseline took about 3 minutes and wrote 6 to 8 GB before the turn's first tool call could start. Turns that were killed left their repositories behind: 62 GB on one machine.
How should the ledger detect workspace changes without copying the workspace?

## Decision drivers

- The first tool call must not wait minutes for an advisory check.
- Nothing may grow without bound in the state directory.
- Ignored files must still count as observed content.
- A missed change is worse than an extra warning.

## Decision

We will fingerprint the workspace by hashing each entry's metadata: relative path, type, size, mtime, and on Unix the inode and mode, plus each symlink's target.
The scan skips only `.git` and `.daimonos` directories and includes ignored files.
Files modified within 2 seconds before the scan also have their content hashed. This covers the coarse filesystem clock, where two quick same-size writes can share an mtime. Git handles this "racy" case the same way.
The scan writes nothing. An unreadable workspace root yields `None`, which the ledger reports as `Unknown`.
Each process removes the legacy `<state>/evidence` shadow-repository store once, in the background.

## Alternatives

- **Keep content hashing, but reuse one shadow repository per workspace:** later turns are fast, but the first turn still copies every byte, and the store grows with the workspace.
- **Exclude build output such as `target/` from the snapshot:** fast, but it breaks the "ignored content counts" rule, and the list of build directories is open-ended.
- **File-change events (inotify through the `notify` crate):** almost no cost per call, but events can overflow, arrive late, or miss files in newly created directories. The watcher is long-lived state. It would save about 0.2 s per call.
- **Turn the check off:** removes the cost and the signal.

## Consequences

- A revert can count as a change, because the mtime moved. This can produce an extra "unverified" note, never a missed change.
- A write that keeps the old mtime and size (`cp -p`, `rsync -a`, `touch -d`), made more than 2 seconds before the scan, is not detected.
- A scan costs about 0.2 s (debug build) for 43,000 files. Its cost grows with the number of files, not their size.
- A running turn of an older build loses its shadow repository when the legacy store is removed. Its evidence becomes `Unknown`.

## Confirmation

- `cargo test evidence::` covers: no writes under the state directory, same-tick same-size rewrites, ignored files, created and deleted files, an unreadable workspace, and removal of the legacy store.

## Anchors

_Anchors:_ src/evidence.rs::Worktree::fingerprint, src/evidence.rs::scan, src/evidence.rs::record_entry, src/evidence.rs::remove_legacy_store

## Revisit when

A supported workspace reaches millions of files and the scan takes seconds. File-change events may then pay for their complexity.

## Open questions

None blocking.
