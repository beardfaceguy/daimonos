# ADR-021: Caller-owned run authority controls state and discovery

- **Status:** Accepted
- **Date:** 2026-10-28
- **Deciders:** Repository maintainer, by explicit user approval on 2026-10-29 UTC.
- **Consulted:** None recorded.
- **Informed:** None recorded.
- **Spec:** Local dbunk requests and PM amendments, sealed in the [package plan](../handoffs/2026-10-28-dbunk-remaining-work.md).
- **Packages:** WP01, WP02, WP11, WP13
- **Dependencies:** Accepted ADR-014 and ADR-015. Preserve existing ADR-020 implementation behavior without claiming its acceptance.
- **Supersedes:** None
- **Superseded by:** None

## Context

State paths currently derive from HOME, XDG, workspace configuration, and temporary-directory fallbacks.
Configuration discovery tries explicit, workspace, and user locations in sequence.
dbunk needs a writable private state root while evidence and the container root remain read-only.
It also needs an independent explicit-only discovery option.
How will one run retain caller authority without changing process-global environment or legacy defaults?

## Decision drivers

- Internal writes must not depend on writable HOME or evidence.
- Explicit configuration must not silently fall back after an error.
- Concurrent sessions must not share a mutable process-global root.
- State placement and configuration discovery are independent caller options.

## Decision

We will resolve caller authority once, before discovery-dependent services or internal writers start.
Pass an immutable run-scoped authority through provisioning and runtime constructors.
Use the same resolved state destination for every internal writer and private temporary artifact.

An explicit state root must exist, be writable, and remain outside evidence after symlink resolution.
Reject an explicit internal destination outside that root, naming the option and path.
This includes audit, OAuth, traces, spill files, private git views, indexes, and session stores.
Do not relocate caller-requested tool writes. Their authorization belongs to ADR-022 and ADR-023.

Explicit-only discovery uses only sources that the caller names.
It disables automatic workspace/home TOML, environment files, prompts, instructions, skills, ignores, plugin config, and remote discovery.
Invalid explicit inputs fail rather than selecting another source.
Report effective policy and ignored source paths without reading their contents.

The same authority carries an independent no-spill output option.
When selected, producers must not save the full result before returning bounded output or publish a saved-file pointer.
Existing in-memory handle behavior remains separate under Accepted ADR-007.
All options preserve legacy behavior when absent.
Do not change HOME/XDG globally, add a singleton root, or migrate existing histories automatically.
Reuse store formats, writer epochs, generations, and the merged checkpoint/drain behavior.
Public option/type spellings remain proposed until interface review.

## Alternatives

- **Set HOME for the process:** conflates state with discovery and leaks across sessions.
- **Change each writer separately:** preserves fallbacks and makes omission likely.
- **Copy state into evidence:** breaks read-only mounts and contaminates evidence.
- **One mandatory reviewer profile:** prevents independent use of the two requested options.

## Consequences

Every writer needs an explicit destination or a disabled/not-applicable inventory entry.
Constructor changes cost more initially than environment redirection.
One authority removes hidden fallbacks and supports isolated worker processes.
Unspecified discovery remains compatible for ordinary users.

## Confirmation

Run future `tests/test_state_directory.py`, `tests/test_state_frontends.py`, and `tests/test_configuration_isolation.py`.
Use hostile HOME/XDG/workspace inputs and verify that only explicit configuration applies.
Snapshot all internal destinations and reject escaping audit/status paths.
Run existing ACP crash recovery and session import/export tests after relocation.
Different sessions in one process must retain independent roots.

## Anchors

_Anchors:_ src/paths.rs::state_dir, src/paths.rs::agent_sessions_dir, src/config.rs::load, src/config.rs::search_candidates, src/agent_env.rs::AgentEnv::load, src/provisioning.rs::build_tool_services

## Revisit when

A new internal writer or automatic discovery source appears, or sessions need explicit state migration.

## Open questions

- The user accepted this decision on 2026-10-29 UTC. Remaining interface questions do not imply implementation approval.
- The final option names and complete writer/discovery inventory need interface review before implementation.
