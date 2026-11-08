# ADR-022: One filesystem authority governs local reads and mutations

- **Status:** Accepted
- **Date:** 2026-10-28
- **Amended:** 2026-11-03, with explicit maintainer approval. PM confirmed 2026-11-07.
- **Deciders:** Repository maintainer, by explicit user approval on 2026-10-29 UTC.
- **Consulted:** None recorded.
- **Informed:** None recorded.
- **Spec:** Local dbunk requests and PM amendments, sealed in the [package plan](../handoffs/2026-10-28-dbunk-remaining-work.md).
- **Packages:** WP03, WP04, WP14
- **Dependencies:** Accepted ADR-021. Accepted ADR-017 remains the git-read boundary.
- **Supersedes:** None
- **Superseded by:** None

## Context

`Session::resolve_path` joins relative paths to mutable cwd and accepts absolute paths.
Tool implementations and recursive walks perform access through several separate code paths.
dbunk requires fixed-root confinement, no workspace writes, and an independent no-follow option.
Which authority can enforce these properties without claiming that opaque commands are sandboxed?

## Decision drivers

- Authorization must hold at the operation, not only in advertised schemas.
- Root identity must not change with session cwd.
- Confinement and no-follow have different symlink semantics.
- Read-only evidence must remain unchanged through startup and teardown.

## Decision

We will pass a shared run-scoped filesystem authority to every Daimonos-owned filesystem consumer.
It owns path resolution, fixed-root authorization, access permission, walk behavior, and eligibility declarations.
Confinement, no-workspace-writes, and no-follow remain independent opt-in constraints.

Resolve relative arguments against the current session working directory, including after `set_cwd`.
Keep the confinement root fixed. Authorize the resolved object against that root for each operation.
Reject escaping explicit paths before exposing metadata or content. Walks skip and report escaping entries.
Confinement alone permits links whose resolved target stays inside the root.
No-follow reports a link and its target text without opening or walking the target.

Apply read-only permission to all deliberate workspace mutations, including metadata, locks, temporary files, and shutdown effects.
Route internal writes through ADR-021, not through exceptions inside evidence.
Authorization must cover the object actually opened. A canonicalize-then-unrestricted-reopen check is not sufficient evidence against replacement races.
Use handle-relative/identity-preserving operations where needed, or refuse an operation whose boundary cannot be enforced.

Local arbitrary commands, opaque plugins, and scripts are not a filesystem sandbox.
When confinement is enabled, reject an explicitly selected unconfineable local tool at startup, naming it.
Caller-provided workers are caller-sandboxed. Do not path-check or reject their tools solely for lacking local declarations.
Their names remain subject to ADR-023.
Legacy behavior remains unchanged when these options are absent.

## Alternatives

- **Resolve all relative paths from the fixed root:** makes paths independent of `set_cwd`, but breaks expected agent workflows.
- **Validate the startup directory only:** misses later arguments, walks, and cwd changes.
- **Trust path annotations alone:** metadata does not enforce access.
- **Treat in-root symlinks as universally forbidden:** merges two independent requested options.
- **Rely solely on read-only mounts:** useful defense, but does not confine reads or authorize tool names.

## Consequences

Native operations, indexes, and git access need auditable adapters.
Some local tools become unavailable only when confinement requires it.
Relative paths depend on the session working directory. Reproducing a sequence requires its working-directory changes.
Platform support needs explicit verification. Do not promise enforceability on an unsupported resolver.
This is not OS isolation for caller workers or a substitute for their sandbox.

## Confirmation

Run future path-confinement, read-only-workspace, and no-follow suites, plus existing symlink and git tests.
Test direct and intermediate links, cwd changes, path arrays, walks, replacement races, and rejected tools.
After `set_cwd` selects an in-root directory, verify that relative paths use that directory.
Verify that `set_cwd` and later operations refuse escapes and unsafe working-directory replacement.
Use actual read-only mounts and snapshots through startup/calls/shutdown.
Run concurrent private git status views and verify independent cleanup.
Worker tools remain available by name only when the caller explicitly permits them.

## Anchors

_Anchors:_ src/session.rs::Session::resolve_path, src/ops/mod.rs::dispatch_with_progress, src/tool_facade.rs::invoke_with_progress, src/plugins/git_read/runner.rs::ReadRepo, src/plugins/git_read/status.rs::StatusView::create

## Amendment — relative path base (2026-11-03)

The repository maintainer explicitly approved mutable working-directory resolution on 2026-11-03 UTC.
The dbunk PM confirmed this amended FR-03 rule on 2026-11-07 UTC.
This amendment replaces only the prior rule: "Resolve relative arguments against the fixed root, including after set_cwd."
The fixed root remains the security boundary. A working-directory change does not change that boundary.
The authority must refuse an operation if it cannot safely resolve the working directory or the requested object within the root.
Confinement, no-workspace-writes, and no-follow remain independent options.

## Revisit when

A local tool gains filesystem access, a new platform needs support, or stronger OS isolation becomes required.

## Open questions

- The user accepted this decision on 2026-10-29 UTC. Remaining interface questions do not imply implementation approval.
- Interface review must record supported resolver mechanisms and any dependency request before code changes.
