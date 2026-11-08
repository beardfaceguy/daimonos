# Remaining dbunk work

Date: 2026-10-28
Status: Package plan, ADRs 021–032, and all four G0 interface groups approved by the user on 2026-10-29 UTC. Operational launch preflight remains required.

## Current checkpoint

- Repository: `/home/beardface/work/daimonos`.
- Current planning checkout: local `master`.
- Verified implementation base: `859194c8cc3aa736e6539db14af6bd370f325e7c`.
- The former planning branch `plan/1575-dbunk-work-packages` still references the earlier base. Do not launch from it.
- The implementation base includes the ACP durability fix. Preserve it in every package.
- FR-06 merged through PR #254. Its PR and post-merge CI passed.
- The previous handoff records #1576 closed and parent #1575 open in project #183. Tracker state was not refreshed during this edit.
- No remaining feature is implemented or assigned.
- The recovery investigation log has unrelated local changes. Preserve them.
- The two original PM inputs and this handoff are local, untracked files. Do not discard them.
- The user approved the package plan, local logging policy, and ADRs 021–032. G0 review remains required before implementation.
- No implementation agent has launched. Tracker writes still require the separate final-review approval.

## Inputs to read

1. `AGENTS.md` and `CONTEXT-MAP.md`.
2. `daimonos-feature-requests-for-dbunk.md`.
3. `daimonos-pm-answers-round-1.md`.
4. Relevant existing ADRs before selecting interfaces or changing architecture.

Keep the two original PM input files unchanged. They remain local and untracked.
The PM answers amend the original requests. Do not silently reinterpret them.

## Planning outcome and review gate

The proposal has 20 implementation packages for 17 remaining requests.
FR-08 has two slices. FR-04 has separate transport, audit, and probe slices.
Capacity is three implementers, A/B/C, plus integrator I. These are roles, not assigned people or agents.
Priority groups remain release gates. They are not parallel ownership boundaries.
The user approved this package plan, role labels, and local-only logging policy at 2026-10-29T03:01:18Z.
Do not launch implementation agents until their applicable ADRs are Accepted and the G0 launch contracts receive human review.
Do not claim any package complete until its integrated, observable acceptance checks pass.

### Agent labels and local-only work logs

This policy covers development-agent notes and progress. It does not change the FR-04 product audit or FR-08 runtime state rules.
No agent writes development notes, progress comments, task changes, or new tasks to Vikunja during implementation.
The integrator writes to Vikunja only after the entire process finishes and the human explicitly approves the proposed content and changes.

| Role | Unique agent label | Required log directory |
|------|--------------------|------------------------|
| Implementer A | `dbunk-impl-a` | `~/.daimonos/agentic_work_logs/dbunk-impl-a/` |
| Implementer B | `dbunk-impl-b` | `~/.daimonos/agentic_work_logs/dbunk-impl-b/` |
| Implementer C | `dbunk-impl-c` | `~/.daimonos/agentic_work_logs/dbunk-impl-c/` |
| Integrator I | `dbunk-integrator` | `~/.daimonos/agentic_work_logs/dbunk-integrator/` |

Each agent's launch instructions must include its exact label, directory, and this policy.
Before work starts, that agent creates its own directory with private permissions:

```bash
umask 077
# Set AGENT_LABEL to the exact label from the assigned row above.
LOG_DIR="$HOME/.daimonos/agentic_work_logs/$AGENT_LABEL"
mkdir -p "$LOG_DIR/artifacts"
chmod 700 "$LOG_DIR" "$LOG_DIR/artifacts"
```

Use these files inside the assigned directory:

- `journal.md`: append timestamped decisions, completed slices, commands, outcomes, blockers, and integration handoffs.
- `checkpoint.md`: maintain the current package, branch/worktree, base/HEAD SHAs, changed files, verified results, and exact next step.
- `artifacts/`: retain command output and test logs. Reference them from the journal instead of copying large output.
- `final-review.md`: nominate important findings for the integrator after reviewing all notes at overall completion.

Save checkpoints after meaningful progress and before long commands, integration handoffs, pauses, or shutdown.
Flush saved checkpoints to disk. On recovery, read the checkpoint and journal before changing code.
Record the runtime session ID and WP ID when a role resumes. Never discard earlier notes or share a directory with another role.
Agents write only their own logs. The integrator reads the implementers' handoff notes and records integration results in its own directory.
I also maintains `integration-ledger.md` for package status, ownership transfers, merge SHAs, dependencies, and the heavy-build queue.
Record launch manifests and their log-directory mappings under I's directory. Do not use Vikunja as the live coordination ledger.
Keep credentials and raw private evidence out of these files. These logs are local and must not enter commits or evidence bundles.
Required code-linked ADRs and glossary updates still belong in their reviewed repository changes.

At overall completion:

1. Every implementer reviews its notes and writes `final-review.md` with important findings, evidence references, risks, and proposed follow-ups.
2. I reviews its own notes and all three implementer reviews. It also records when an agent has no important findings.
3. I prepares `human-review.md` with a concise combined report and the exact proposed Vikunja text, target tasks, and task changes.
4. I presents that report to the human. Silence, package approval, and test success do not authorize a tracker write.
5. Only after explicit approval, I writes the approved items to Vikunja and records that approval and the resulting IDs locally.

Do not upload journals, raw test logs, or rejected findings. Retain local notes until the human authorizes cleanup.
Agents remain unassigned until the existing plan and ADR review gates permit launch.

### G0: integrator preparation before implementation

Prepared review artifacts:

- [Writer, discovery, network, and admission inventories](../contexts/reviewer-runtime/INVENTORIES.md).
- [Proposed interface ledger and remaining launch review](../contexts/reviewer-runtime/INTERFACES.md).
- All twelve dbunk ADRs are Accepted. The user explicitly approved all four interface groups on 2026-10-29 UTC.
- The valid sealed input bundle is `~/.daimonos/agentic_work_logs/dbunk-integrator/sealed-inputs/8f767687c0b1-11901ce73e0b/`.
- Do not use the earlier attempted `1375dca9b223-c416379733d7` bundle. Its manifest used incorrect hash metadata.
- Current PM files match the pre-crash archive and the handoff hashes. No input or requirement changed.

I owns this preparation checkpoint. It is not an extra product feature.

1. Seal the inputs and record the integration base before making worktrees.
2. Complete the internal-writer and network-attempt inventories from the FR estimates below.
3. Review relevant ADRs and propose any new hard-to-reverse decisions before affected implementation.
4. Review a small interface ledger. Names and flags are proposed until that review.
5. Confirm error names, exit codes, counter semantics, and ownership for the listed seams.
6. Recheck installed tools and test dependencies. Do not replace the installed Daimonos binary during package development.
7. Verify each agent's label, private log directory, and local-only logging instructions before launch.

The interface ledger must cover these boundaries, without function bodies:

| Seam | Required contract | Producers and consumers |
|------|-------------------|-------------------------|
| State placement | Explicit root, destination validation, no fallback, internal writes only | WP01 produces. WP02 and WP04 consume. |
| Path policy | Fixed root, per-argument refusal, walks, independent no-follow option, unsafe-tool eligibility | WP03 produces. WP04 and WP14 consume. |
| Tool eligibility | Effective names/schemas, explicit-list intersection, lazy listings, final-answer identity | WP06 produces. WP09, WP10, WP15, WP16, WP17 consume. |
| Final ending | Unchanged supplied schema, full validation, terminal-call interception, retry/turn counters | WP05 produces. WP09, WP10, WP17 consume. |
| Request profile | Explicit five-value configuration, wire-local serialization, no hidden retry/redirect/proxy behavior | WP07 produces. WP08 and WP09 observe. |
| Local audit | Startup metadata, request/tool events, refusal/limit records, redaction, finish/error handling | WP08 produces. WP06, WP09, WP10, WP18, WP20 consume. |
| Worker route | Caller-controlled commands/environment, routing identity, failure/reaping, no local fallback | WP10 produces. WP12 and WP20 verify. |
| Input budget | Logical bytes, index-served content, entry count, cooperative deadlines, named failure | WP18 produces. WP19 consumes. |

Apply Accepted ADR-026's fail-closed requested-audit policy during G0. It is not OTLP's fail-open policy.
Apply Accepted ADR-032's process-wide offline ceiling and same-process networked-model refusal.
Review ADR-024's inclusive model-request accounting and ADR-029's explicit worker discovery during export.
The user accepted these decisions on 2026-10-29 UTC. Public names, errors, and mechanisms still require the G0 interface review.
A new design question pauses only the affected package and its dependents.

### Immutable inputs for every worktree

I creates a content-addressed input bundle outside the worktrees, then copies its files read-only into each worktree.
The original PM files remain unchanged and untracked. No symlink to a mutable shared input is sufficient.
Record these digests in the launch manifest:

```text
8f767687c0b119f3a43081e0b5c6ab1133be89149d93c9b1fbe54bcb7cd6383d  daimonos-feature-requests-for-dbunk.md
11901ce73e0b2257fc06847ea090723dd7df776170816f828ac52cdb61ebd0dd  daimonos-pm-answers-round-1.md
```

Verify the copies at launch, before review, and before integration:

```bash
sha256sum daimonos-feature-requests-for-dbunk.md daimonos-pm-answers-round-1.md
```

A digest mismatch blocks the package. Do not resolve conflicting PM text by guessing.
The launch manifest also records the package ID, owner, agent label, log directory, parent integration SHA, and allowed files.
I publishes a new integration SHA after each accepted package. New branches use that SHA, not stale master or the old planning branch.
Plan review is complete. Prepare worktrees only after the applicable G0 launch gates close.
Do not create tracking subtasks during implementation under the local-only logging policy.


## Package index and dependency graph

Dependencies below are merge prerequisites. `G0` is the reviewed launch checkpoint above.
Every P1 package also waits for the P0 gate, WP04.
Every P2 package waits for the P1 gate, WP10.
Every P3 package waits for the P2 gate, WP17.
FR-03 is the authorized P1 prerequisite pulled into the P0 foundation, not scope expansion.

| Package | Owner | Scope and acceptance responsibility | Direct prerequisites |
|---------|-------|-------------------------------------|----------------------|
| WP01 | A | FR-08: state root and MCP/native writers. Partial AC-08.1, AC-08.2. | G0 |
| WP02 | B | FR-08: remaining frontends and writers. Completes AC-08.1 and AC-08.2. | WP01 |
| WP03 | C | FR-03: fixed-root paths, walks, and tool eligibility. AC-03.1–03.6. | G0 |
| WP04 | A | FR-07: full read-only workspace. AC-07.1–07.2 and S8 concurrent status. | WP02, WP03 |
| WP05 | B | FR-01: structured final output. AC-01.1–01.7 and PM turn-limit cases. | WP04 |
| WP06 | B | FR-02: explicit intersected allowlist. AC-02.1–02.6 and PM nested-call cases. | WP05, WP08 |
| WP07 | C | FR-04 transport, requirements 1–13. Recording tests underlying AC-04.1–04.5, AC-04.7. | WP04 |
| WP08 | A | FR-04 local audit, requirements 18–20. AC-04.8 and event completeness. | WP04 |
| WP09 | C | FR-04 capability probe, requirements 14–17. Completes AC-04.1–04.7. | WP05, WP06, WP07, WP08 |
| WP10 | B | FR-05: caller-owned workers and remote-only routing. AC-05.1–05.5. | WP09 |
| WP11 | A | FR-09: explicit-only configuration. AC-09.1–09.2. | WP10 |
| WP12 | C | FR-10: local restricted container and source identity. AC-10.1–10.2. | WP10 |
| WP13 | B | FR-11: no-spill truncation. AC-11.1. | WP10 |
| WP14 | A | FR-12: independent no-follow policy. AC-12.1–12.2. | WP11, WP03 |
| WP15 | C | FR-13: effective contract dump, version, changelog. AC-13.1–13.2. | WP11, WP12 |
| WP16 | A | FR-14: owned path annotations. AC-14.1. | WP14, WP15 |
| WP17 | C | FR-15: full self-contained schemas. AC-15.1 plus PM startup/probe cases. | WP16, WP05 |
| WP18 | B | FR-19: deadlines, logical-byte and directory-entry budgets. AC-19.1–19.2. | WP17 |
| WP19 | A | FR-17: full stored-byte SHA-256. AC-17.1, including budget failure. | WP18 |
| WP20 | C | FR-18: offline-tools profile and analytics collection off. AC-18.1. | WP17 |

This graph is acyclic. Package numbering is stable, not a requirement to merge numerically.
WP07 and WP08 deliberately have no dependency on FR-01. They remain independently usable without structured endings.
The same-tool capability probe depends on the final tool and effective allowlist, so WP09 waits for both.
The final-answer call always stays on the model side, including after WP10.

### Concurrent waves

A wave is a readiness checkpoint, not a promised duration. I serializes integration within each wave.
A package can start once its prerequisites merge. It need not wait for unrelated work in the same priority group.

| Wave | A | B | C | Exit checkpoint |
|------|---|---|---|-----------------|
| 0 | Writer inventory review | Runtime/lifecycle review | Path/provider/schema review | I reviews G0 and the input manifest. |
| 1 | WP01 | Prepare WP02 fixtures only | WP03 | State-root and confinement PRs pass independently. |
| 2 | Review migration coverage | WP02 | Review escape/unsafe-tool cases | All internal writers have a tested state destination. |
| 3 | WP04 | Review durability regressions | Review restricted mounts | P0 foundation passes actual read-only mounts. |
| 4 | WP08 | WP05 | WP07 | Audit, final output, and transport PRs pass separately. |
| 5 | Review refusal records | WP06 | Prepare probe cases only | Effective lists and nested enforcement pass. |
| 6 | Audit coverage review | Review ending invariants | WP09 | Probe refusal prevents all real review work. |
| 7 | Isolation review | WP10 | Worker/probe recording review | P1 reviewer runtime passes split-process tests. |
| 8 | WP11 | WP13 | WP12 | Config isolation, no-spill, and local image pass separately. |
| 9 | WP14 | Review no-follow coverage | WP15 | No-follow and deterministic contract export pass. |
| 10 | WP16 | Review annotation exclusions | Review dump inventory | All owned path arguments are classified. |
| 11 | Review schema refusal cases | Review default compatibility | WP17 | P2 gate passes full-schema serving and export. |
| 12 | Prepare digest cases only | WP18 | WP20 | Budget cancellation and observed offline behavior pass. |
| 13 | WP19 | Review full-file accounting | Review final combined profile | P3 checks and all cross-feature assertions pass. |
| 14 | Review | Review | Review | I runs the full regression and delivery gate. |

Not every wave uses three implementers for production changes. Do not invent independent work to keep agents busy.
Review and fixture preparation do not authorize production implementation before a dependency merges.

## Exclusive ownership and integration

The package's owner edits only its listed exclusive files and its own test fixtures.
All `(new)` paths below are proposed files, not approved architectural choices.
Legacy files in the estimate inventory are audit targets, not automatic edit permission.

I owns direct edits to these shared entrypoints and contracts:

- `src/main.rs`, `src/cli.rs`, `src/config.rs`, `Cargo.toml`, `Cargo.lock`.
- `src/agent.rs`, `src/agent_runtime.rs`, `src/agent_cmd.rs`, `src/headless_frontend.rs`, `src/providers/mod.rs`.
- `src/session.rs`, `src/session_factory.rs`, `src/session_bootstrap.rs`, `src/chat_cmd.rs`, `src/acp_cmd.rs`, `src/session_daemon.rs`, `src/session_store.rs`.
- `src/tools.rs`, `src/tool_facade.rs`, `src/tool_runner.rs`, `src/mcp.rs`, `src/script.rs`, `src/ops/mod.rs`, `src/ops/tool_ops.rs`.
- `src/index.rs`, `src/ops/file_ops.rs`, `src/ops/diff_ops.rs`, `src/ops/snap_ops.rs`, `src/ops/exec_ops.rs`, `src/ops/schema.rs`.
- `src/provisioning.rs`, `src/plugins/mod.rs`, `src/plugins/git_read.rs`, `src/managed_process.rs`.
- `daimonos.default.toml`, `prompts/tool_descriptions.toml`, `src/tool_descriptions.rs`.
- `README.md`, `docs/configuration.md`, `tests/README.md`, `tests/conftest.py`, `.github/workflows/ci.yml`, and existing shared conformance tests.
- Context maps, glossary/ADR changes, this handoff, and launch manifests.

A feature owner supplies the reviewed interface, focused tests, and exact integration requirements.
I writes shared-file adapters in that package's branch/PR, one package at a time.
The PR is not complete until those adapters and the end-to-end tests pass together.
Do not merge helper modules as a claim that the corresponding FR is delivered.

I is a real integration bottleneck. Queue its work explicitly rather than allowing overlapping edits.
If an existing shared file needs a larger implementation, transfer ownership before editing it.
A transfer names one owner and one package, and waits for all other edits to that file to merge.
Only I approves and records the transfer. Do not resolve overlap through concurrent reservation alone.

Each package gets `tests/fixtures/dbunk/wpNN/` (new) for private fixtures when needed.
Do not modify another package's fixtures or shared `tests/conftest.py` without I.
Later owners of a shared new module are explicit in the package details and dependency graph.
No two packages in one wave receive the same exclusive path.
Exclusive grants last only while that package is active. After merge, I holds the path until the next recorded grant.

### Integration and resource discipline

- Use one worktree and branch per package, based on the recorded integration SHA.
- Rebase or merge the accepted prerequisites before requesting final review. Resolve conflicts from both changes' intent.
- Keep each worktree's build artifacts separate. Do not share mutable test binaries between worktrees.
- Run at most one heavy Rust build, link, full suite, or container build on this laptop at a time.
- Use one Cargo job for the serialized verification queue. Source editing and light fixture review can remain concurrent.
- Do not install development binaries, restart active agents, push, or publish images as a side effect of package work.
- Before integration, verify input hashes, the allowed-file diff, the package checks, and the opt-out behavior.
- Record package progress, commands, results, and merge SHAs in I's local integration ledger.
- Amend this handoff only for reviewed scope, ownership, or dependency changes. Do not use it as a development journal.

## Package deliverables: foundation and reviewer runtime

Verification commands below name future tests. Every new test path is marked `(new)`.
They are launch acceptance commands, not claims that those tests already exist or pass.
Each package includes an opt-out regression. Existing default behavior remains unchanged unless the PM explicitly approved an exception.

### WP01 — A: state root for the MCP tool server

- Outcome: explicit state placement works for MCP and native-tool sessions with unusable HOME. The first FR-08 slice is observable through real tools.
- Own: `src/state_root.rs` (new), `src/paths.rs`, `src/logging.rs`, `src/analytics.rs`, `src/snapshot.rs`, `src/checkpoint.rs`, `src/evidence.rs`, `src/pipeline_cache.rs`, `src/coordination/store.rs`, `src/kgl/store.rs`, `src/kgl/autoindex.rs`.
- I wires bootstrap, config, index, process spill paths, and git status metadata consumers. Preserve `StatusView::create` private views and independent cleanup.
- Failure/default: reject missing, unwritable, or escaping roots without fallback. Name the offending option/path. Do not relocate explicit caller/model writes.
- Verify: `python3 -m pytest tests/test_state_directory.py -v` (new). Check AC-08.1 for MCP and AC-08.2, including home/XDG/config/status paths.
- Checkpoint: deliver the complete writer inventory with each MCP/native destination tested. FR-08 remains incomplete until WP02.

### WP02 — B: state root across runtime frontends

- Outcome: agent, ACP, chat, daemon, OAuth, and remaining writers use the explicit state destination. This closes FR-08.
- Own: `src/state_root.rs` transfers from A after WP01 merges, `src/mcp_oauth.rs`, `src/remote_auth.rs`, and `tests/test_state_frontends.py` (new).
- I wires existing session/runtime entrypoints and stores. Reuse the root contract from WP01 rather than add a second state resolver.
- Failure/default: no fallback to HOME, XDG, workspace, or system temp. Preserve history generations, writer epochs, cancellation, compaction provenance, and shutdown draining.
- Verify: `python3 -m pytest tests/test_state_frontends.py tests/test_acp_crash_recovery.py tests/test_session_import_export.py -v`.
- Checkpoint: every writer inventory row has an integration test or a documented disabled/not-applicable path. AC-08.1–08.2 pass across the supported modes.

### WP03 — C: confined paths and safe tool eligibility

- Outcome: root-confined reads and walks reject escapes before accessing content. Relative paths use the current working directory after `set_cwd`. The confinement root remains fixed (ADR-022, amended 2026-11-03).
- Own: `src/path_policy.rs` (new), `tests/test_path_confinement.py` (new). I wires `Session::resolve_path`, native operations, index walks, dispatch, and git consumers.
- Reuse existing git confinement checks as evidence, not as proof that other tools are confined. Review unsafe raw execution and composite eligibility explicitly.
- Failure/default: name the refused argument. Skip/report escaping walk entries. Allow in-root links unless independent no-follow is enabled. Refuse unconfineable allowlisted local tools at startup.
- Verify: `python3 -m pytest tests/test_path_confinement.py tests/test_set_cwd.py tests/test_roots.py tests/test_git_read.py -v`.
- Checkpoint: AC-03.1–03.6 pass, with no outside metadata leakage. Caller-provided workers remain caller-sandboxed and explicitly allowlisted.

### WP04 — A: prohibit workspace writes end to end

- Outcome: read tools succeed on an actual read-only mount and leave writable workspaces unchanged through startup, operation, and shutdown.
- Own: `src/run_policy.rs` (new), `tests/test_read_only_workspace.py` (new). I integrates all direct and delegated dispatch guards.
- Reuse WP01–WP03 destinations and resolution. Do not duplicate writer routing or assume that an allowlist alone prevents startup writes.
- Failure/default: refuse requested workspace mutation, including timestamps, permissions, locks, and teardown effects. Keep current behavior when disabled.
- Verify: `python3 -m pytest tests/test_read_only_workspace.py tests/test_state_directory.py tests/test_git_read.py -v`.
- Checkpoint: AC-07.1–07.2 pass. S8 adds concurrent git status calls with independent private views and verified cleanup. P0 closes here.

### WP05 — B: validated structured final answers

- Outcome: an opt-in agent run emits exactly one validated UTF-8 JSON document and newline, or a distinct documented failure.
- Own: `src/structured_output.rs` (new), `src/schema_validation.rs` (new), `tests/test_structured_output.py` (new). I wires the neutral loop and stdout/error paths.
- Reuse `LlmProvider`, `ToolSchema`, and authoritative message pairing. Select/report the already approved full-validator crate and version. I owns Cargo changes.
- Failure/default: assert date-time and all required 2020-12 keywords. Refuse external references without I/O. Preserve supplied schemas, answer every failed-ending tool call, and keep retries separate from model-request turns.
- Verify: `python3 -m pytest tests/test_structured_output.py -v`; `cargo test structured_output` after the module exists.
- Checkpoint: AC-01.1–01.7 pass on both mock wires. Cover `turn_limit`, zero retries, exhausted retries, mixed calls, remote-read/final-read turns, and success on the final allowed model request.

### WP06 — B: effective allowlist at every dispatch boundary

- Outcome: explicit restrictions apply to model listings, lazy discovery, direct calls, and every admitted composite inner call.
- Own: `src/run_policy.rs` transfers from A after WP04, `tests/test_tool_allowlist.py` (new). I owns catalog, native/plugin/MCP, and Starlark adapters.
- Reuse `SafetyPolicy::gate` restrictions by intersection. Keep the explicit empty-list case distinct from no explicit list. Preserve caller-owned worker names.
- Failure/default: unknown names and a missing final-answer entry fail startup. Workspace/environment settings cannot widen the list. Every refusal records its reason through WP08.
- Verify: `python3 -m pytest tests/test_tool_allowlist.py tests/test_batch.py tests/test_tool_provisioning.py -v`.
- Checkpoint: AC-02.1–02.6 pass. Nested arbitrary Starlark and `tool("name", ...)` calls cannot bypass the explicit policy.
- Keep historical opt-out behavior tests. Add explicit-mode secure variants through I. Do not claim that old MCP agent-env restrictions are newly enforced when the option is off.

### WP07 — C: exact, opt-in model request transport

- Outcome: recording Chat and Messages endpoints receive only the caller-approved body, headers, timeout, retry count, and explicit proxy behavior.
- Own: `src/providers/anthropic.rs`, `src/providers/openrouter.rs`, `src/providers/router.rs`, `src/providers/request_trace.rs`, `tests/test_request_control.py` (new).
- I owns provider-neutral options in `src/providers/mod.rs`. Keep wire rules provider-local. Do not route Chat Completions into native OpenAI Responses or repurpose session IPC.
- Failure/default: incomplete five-value configuration names missing fields. Reject redirects, hidden retries, ambient headers/proxies, telemetry, and unconfigured extra fields. Preserve legacy provider behavior when disabled.
- Verify: `python3 -m pytest tests/test_request_control.py -v`; `cargo test providers`.
- Checkpoint: transport-level AC-04.1–04.5 and AC-04.7 assertions pass without FR-01. The integrated probe repeats them in WP09.

### WP08 — A: local, privacy-preserving run audit

- Outcome: an opt-in run writes complete JSONL lifecycle, request, tool/refusal, and final-status records to the caller's audit destination.
- Own: `src/run_audit.rs` (new), `tests/test_run_audit.py` (new). I wires provider-neutral events and shared dispatch observations.
- Reuse ADR-006 privacy rules. The audit is separate from optional analytics and OTLP export, and works without FR-01 or WP07.
- Failure/default: redact credentials and URL userinfo/path/query. Hash arguments/results without including contents. Apply the reviewed audit-write failure contract, not OTLP's fail-open policy.
- Verify: `python3 -m pytest tests/test_run_audit.py -v`; include refusal records, configuration-only startup, early failures, durations, and exact secret-value scans.
- Checkpoint: AC-04.8 passes. I later verifies WP06 refusals, WP09 probe results, WP10 route identities, and WP18 limit hits through the same audit API.

### WP09 — C: same-tool capability probe before work

- Outcome: one unretried probe accepts only one valid final-answer call. Every other outcome stops the run before review tools execute.
- Own: `src/capability_probe.rs` (new), `tests/test_capability_probe.py` (new). I wires startup ordering and effective catalog freezing.
- Consume WP05 validation, WP06 names, WP07 transport, and WP08 audit. Text/reasoning beside the sole final call remains allowed.
- Failure/default: return the distinct model-unsupported outcome for text-only, wrong/multiple calls, invalid arguments, schema rejection, 503, or redirect. Do not execute probe tool calls.
- Verify: `python3 -m pytest tests/test_capability_probe.py tests/test_request_control.py tests/test_run_audit.py -v`.
- Checkpoint: all AC-04.1–04.8 and amended exact-tool-set, zero-retry, local-reference, and payload cases pass. No credentialed model call is needed.

### WP10 — B: credential-free worker execution

- Outcome: model-side runs dispatch every non-final tool call only through supplied MCP commands. The final-answer tool remains intercepted locally.
- Own: `src/agent_mcp.rs`, `src/mcp_bridge.rs`, `src/worker_runtime.rs` (new), `tests/test_worker_isolation.py` (new).
- Reuse the existing MCP client and `managed_process::apply_environment` through I. Discover worker tools before freezing the probe/run catalog.
- Failure/default: no credential value reaches worker env, argv, or initialization. Worker exit/unresponsiveness aborts without local fallback or later tool execution. Legacy ACP bridging remains fail-open when this mode is off.
- Verify: `python3 -m pytest tests/test_worker_isolation.py tests/test_capability_probe.py -v` with a recording credential-free worker and a no-network worker container/namespace.
- Checkpoint: AC-05.1–05.5 pass, including caller `compute`, tool identity auditing, cleanup/reaping, and final-only endings. P1 closes here.

## Package deliverables: caller workarounds and verification

### WP11 — A: explicit-only discovery

- Outcome: hostile workspace and home files cannot configure an opted-in run. Explicit caller configuration still works and ignored paths are reported.
- Own: `src/state_root.rs` transfers from B after WP02, `src/agent_env.rs`, `src/env_file.rs`, `src/prompts.rs`, `src/skills.rs`, `src/zed_config.rs`, `tests/test_configuration_isolation.py` (new).
- I wires bootstrap, registrations, and ignore/walk behavior. Use the complete G0 discovery inventory, not only TOML discovery.
- Failure/default: explicit-only mode disables automatic environment files, prompts, instructions, ignore files, user plugins, and remote discovery. An explicit config path remains honored. Opt-out behavior stays unchanged.
- Verify: `python3 -m pytest tests/test_configuration_isolation.py tests/test_cli_prompts.py tests/test_plugins.py tests/test_search.py -v`.
- Checkpoint: AC-09.1–09.2 pass with every recognized discovery kind, including a mixed hostile-config corpus and effective-policy startup report.

### WP12 — C: restricted local container

- Outcome: a locally built image serves MCP from a caller-selected workspace with read-only root, no network, dropped capabilities, and an arbitrary non-root UID.
- Own: `Dockerfile`, `tests/test_container_runtime.py` (new). I owns version/commit command wiring and any reviewed CI or Cargo changes.
- Reuse FR-08 state placement and FR-07 read-only behavior. Include all embedded assets in the build context. The tool side uses the caller's FR-05 command contract.
- Failure/default: require writable private state and perform no runtime download. The executable's source-commit report must match the image label. Do not alter existing binary-version output.
- Verify: `python3 -m pytest tests/test_container_runtime.py tests/test_cli_modes.py -v`; require actual `--read-only --network none --cap-drop ALL --user 12345:12345` execution.
- Checkpoint: AC-10.1–10.2 pass. No registry, image publishing, or publishing CI is permitted.

### WP13 — B: truncation without saved-file pointers

- Outcome: opt-in bounded results contain continuation/narrowing guidance without writing spill files or advertising inaccessible state paths.
- Own: `src/tool_output.rs`, `src/compaction.rs`, `tests/test_no_spill_output.py` (new). I wires managed capture, response wrappers, and frontend propagation.
- Reuse the current output cap and handle contracts. Prevent spills at the producer, not only by removing the final pointer.
- Failure/default: preserve explicit truncation. Never claim a partial result is complete. Preserve existing offload/handle behavior when this mode is disabled.
- Verify: `python3 -m pytest tests/test_no_spill_output.py tests/test_read_write.py tests/test_exec.py -v`; `cargo test tool_output`.
- Checkpoint: AC-11.1 passes across native and process output, with an unchanged state-file inventory and no saved-file pointer.

### WP14 — A: independent no-follow reads and walks

- Outcome: every link is reported as a link with its target text, even when its target stays inside the root. Walks do not traverse links.
- Own: `src/path_policy.rs` transfers from C after WP03, `tests/test_no_follow.py` (new). I wires filesystem and git metadata/link consumers.
- Reuse the fixed-root resolver. Keep confinement and no-follow as independent options. Test git metadata links separately from stored tree entries.
- Failure/default: no linked target content is returned. Confinement alone still permits in-root links. Both options can be enabled together.
- Verify: `python3 -m pytest tests/test_no_follow.py tests/test_symlinks.py tests/test_path_confinement.py tests/test_git_read.py -v`.
- Checkpoint: AC-12.1–12.2 pass, including intermediate directory links, walks, and independent-mode combinations.

### WP15 — C: deterministic tool contract and version

- Outcome: a session-free JSON dump matches the effective MCP definitions for the same inputs. Initialization reports the separate contract version.
- Own: `src/tool_contract.rs` (new), `docs/tool-contract-changelog.md` (new), `tests/test_tool_contract.py` (new). I wires catalog export, MCP initialization, and CLI.
- Reuse `tool_facade::active_schemas`, the registry, and provisioning. Freeze the effective-catalog inputs at G0. Do not export a second handwritten tool list.
- Failure/default: report invalid selection/configuration before output. Keep dump ordering deterministic. Preserve the existing binary-version contract, separate from WP12 source identity.
- Verify: `python3 -m pytest tests/test_tool_contract.py tests/test_handshake.py tests/test_tool_provisioning.py tests/test_tool_list_changed.py -v`.
- Checkpoint: AC-13.1–13.2 pass across workspace, plugin availability, tier, policy, and schema-mode variants. Breaking tool changes require a contract-version change and changelog entry.

### WP16 — A: owned path annotations

- Outcome: the effective schema dump identifies every owned path/glob/walk argument, including array elements, and marks no non-path argument.
- Own: `src/path_annotations.rs` (new), `tests/test_path_annotations.py` (new). I owns central and plugin schema edits.
- Use WP15's effective inventory. A path annotation describes semantics, not just an argument named path.
- Failure/default: do not classify opaque Starlark/code strings as paths. Do not rewrite caller-owned workers or the caller's final-answer schema.
- Verify: `python3 -m pytest tests/test_path_annotations.py tests/test_tool_contract.py -v` with a reviewed positive/negative argument inventory.
- Checkpoint: AC-14.1 passes for every owned effective tool, including nested arrays. Coordinate contract-version changes through WP15.

### WP17 — C: self-contained full-schema serving

- Outcome: opt-in serving and export use full, self-contained 2020-12 schemas rather than stripped placeholders. Supplied schemas remain unchanged.
- Own: `src/schema_contract.rs` (new), `tests/test_full_schemas.py` (new). I wires the existing full-schema path and startup selection.
- Consume the reusable validator from `src/schema_validation.rs`, owned by B since WP05. C does not edit it independently.
- Failure/default: detect external refs, recursion, or other statically unusable schemas before the model call. Endpoint-only rejection remains a probe failure. Do not guess endpoint support or rewrite schemas.
- Verify: `python3 -m pytest tests/test_full_schemas.py tests/test_tool_contract.py tests/test_tool_provisioning.py tests/test_capability_probe.py -v`.
- Checkpoint: AC-15.1 and amended owned/supplied-schema cases pass. Default stripped/full behavior outside this option remains unchanged. P2 closes here.

### WP18 — B: deadlines and input accounting

- Outcome: opted-in calls stop at caller deadlines or logical-input/entry limits and return named errors rather than silent partial success.
- Own: `src/tool_budget.rs` (new), `tests/test_tool_budgets.py` (new). I wires dispatch, native/index readers, and managed process cancellation/reaping.
- Meter bytes returned from cached indexes, not just filesystem reads. Count directory entries separately. Keep output truncation distinct from input limits.
- Failure/default: refuse tools whose relevant budget cannot be enforced, but only when that budget is enabled. A timeout wrapper alone is insufficient if work continues underneath.
- Verify: `python3 -m pytest tests/test_tool_budgets.py tests/test_exec.py tests/test_search.py tests/test_read_write.py tests/test_git_read.py -v`.
- Checkpoint: AC-19.1–19.2 pass with timing bounds, stopped child processes, index paths, typed failures, and WP08 audit records. Disabled limits preserve current behavior.

### WP19 — A: full stored-byte digests

- Outcome: enabled filesystem and pinned-git reads report SHA-256 of all stored bytes, even for paged or truncated output.
- Own: `src/file_digest.rs` (new), `src/plugins/git_read/runner.rs`, `tests/test_file_digests.py` (new). I wires filesystem results and the git plugin surface.
- Consume WP18 accounting. Hash raw stored bytes, not decoded, normalized, truncated, or displayed text.
- Failure/default: if a budget prevents hashing the complete file, return the named failure. Never label a partial digest as a full-file digest.
- Verify: `python3 -m pytest tests/test_file_digests.py tests/test_tool_budgets.py tests/test_git_read.py -v`; compare with `sha256sum`, including oversized and binary stored bytes.
- Checkpoint: AC-17.1 passes for normal, paged, truncated, and pinned-git reads. Digest reads obey the same deadline and logical-input policy.

### WP20 — C: observed offline-tools behavior

- Outcome: eligible read-tool sessions perform no outbound connection attempt, and analytics collection is disabled rather than only upload.
- Own: `src/offline_tools.rs` (new), `src/observability.rs`, `src/providers/request_trace.rs`, `src/analytics.rs` transfers from A after WP01, `tests/test_offline_tools.py` (new).
- I wires discovery, registrations, OAuth, KGL network paths, and bootstrap. Do not assume FR-07 or an unreachable endpoint prevents network attempts.
- Failure/default: refuse unavailable offline capabilities instead of silently using network fallback. Keep the local FR-04 audit distinct from disabled analytics/export.
- Verify: `python3 -m pytest tests/test_offline_tools.py tests/test_analytics.py tests/test_observability.py tests/test_kgl.py -v` in an attempt-observing network namespace or equivalent harness.
- Checkpoint: AC-18.1 passes with DNS/connect observation, internal discovery/export checks, and the G0 network inventory. Apply the reviewed model-side/tool-side profile scope.

## ADR inventory

The user accepted ADRs 021–032 on 2026-10-29 UTC. Their architectural decisions now govern the packages.
The user explicitly completed G0 interface review. Apply package dependencies and operational preflight before implementation.
ADR dependencies describe design relationships, not a requirement to enable every related runtime option together.
The existing PM inputs remain local and untracked. Their sealed hashes above identify the spec for local review.
Publishing an ADR PR requires an approved way to share that spec. Do not add the PM files to Git automatically.

| ADR | Decision | Packages |
|-----|----------|----------|
| [021](../adr/021-caller-owned-run-authority.md) | Caller-owned state and discovery authority | WP01, WP02, WP11, WP13 |
| [022](../adr/022-shared-filesystem-authority.md) | Shared filesystem authorization with independent confinement/read-only/no-follow constraints | WP03, WP04, WP14 |
| [023](../adr/023-explicit-tool-policy.md) | Explicit policy at every direct and delegated dispatch boundary | WP06 |
| [024](../adr/024-validated-terminal-output.md) | Validated model-side terminal calls, offline full validator, and bounded counters | WP05 |
| [025](../adr/025-controlled-model-transport.md) | Independent exact request/HTTP profile | WP07 |
| [026](../adr/026-caller-requested-local-audit.md) | Independent local audit and accepted required-write failure semantics | WP08 |
| [027](../adr/027-frozen-catalog-capability-probe.md) | One unretried probe against frozen run definitions | WP09 |
| [028](../adr/028-worker-only-execution.md) | Credential-free worker execution without local fallback | WP10 |
| [029](../adr/029-effective-tool-contract.md) | Shared effective serving/export catalog, separate version, owned path metadata | WP15, WP16 |
| [030](../adr/030-self-contained-full-schemas.md) | Full self-contained schemas without rewriting caller definitions | WP17 |
| [031](../adr/031-cooperative-tool-budgets.md) | Cooperative delegated budgets and complete stored-byte digest semantics | WP18, WP19 |
| [032](../adr/032-offline-tools-authority.md) | Process-wide offline-tools network ceiling and collection disablement | WP20 |

WP12's local image applies ADR-021, ADR-022, and ADR-028 boundaries. It adds no registry or separate worker protocol decision.
WP13's no-spill option applies ADR-021 authority while preserving ADR-007's default handle behavior.
WP19's additive digest uses ADR-031 accounting and completion rules. It does not need a second hashing backend.
No separate product ADR is needed merely to run bounded development-agent commands from a supervisor script.
Any proposed persistent supervisor service or new product lifecycle would need its own review before implementation.

## Architectural inventory and invariant checks

Accepted decisions to reuse:

- ADR-001: neutral core, wire-local provider behavior. WP07 must not convert the native OpenAI Responses path into Chat Completions.
- ADR-003: existing MCP client, namespacing, leases, and legacy fail-open behavior. WP10's opt-in fail-closed route is a new scoped decision.
- ADR-006: metadata-only optional observability. WP08's caller-requested local audit is separate, not a change to OTLP's failure policy.
- ADR-007: default-off offload handles. WP13 must preserve current behavior when its no-spill option is disabled.
- ADR-010: runtime options cannot widen host policy. Explicit run restrictions intersect existing restrictions.
- ADR-014, ADR-015, ADR-016: SQLite ownership/generations, session durability states, and authoritative cancelled-turn history.
- ADR-017: separate non-destructive git_read with no passthrough, writes, or network.

ADR-018, ADR-019, and ADR-020 are Proposed. They are not accepted architectural authority.
Their existing merged behavior remains a regression baseline. Do not revert strict git behavior or crash durability while awaiting review.
G0 records any acceptance blockers and links the new decisions before affected implementation starts.
This document does not self-accept ADRs or prescribe unreviewed type names.

Current source sections opened for planning include:

- `paths::state_dir`, `paths::agent_sessions_dir`, `config::load`, `Session::resolve_path`.
- `ToolRegistry::run`, `tool_facade::active_schemas`, `tool_facade::invoke_with_progress`.
- `AgentMcp`, `agent_mcp::connect`, `LlmProvider`, `ToolSchema`, `MultiProvider`.
- `managed_process::apply_environment`, `managed_process::run`, `ToolOutputConfig::resolved_directory`, `StatusView::create`.

The FR estimates below remain an inventory of additional consumers. G0 opens and checks every assigned consumer before finalizing its edit list.
No module extraction is mandatory merely because a proposed new path appears in this plan.

## Final integration and delivery gate

I owns this gate after WP01–WP20 merge. It is not an extra feature package.

1. Verify the two input digests and the accepted interface/ADR ledger.
2. Run every package command against the integrated tree. New tests must be collected, not silently deselected.
3. Run the ACP crash-recovery regression after state, loop, worker, and shutdown integration.
4. Run the default-off matrix without reviewer options across MCP, agent, ACP, chat, and daemon paths touched by the change.
5. Run the combined reviewer profile with fake Chat and Messages endpoints and a credential-free no-network worker.
6. Verify read-only mounts, workspace snapshots, local state destinations, audit redaction, and exact schema/allowlist exposure together.
7. Verify logical budgets and full-file digests together. Verify offline tools with observed connection attempts.
8. Check source identity, contract version, changelog, embedded image assets, and restricted local-container behavior.
9. Update docs and seeds if applicable. Record verified package status in I's local integration ledger.
10. Submit the reviewed changes through the authorized PR process. Watch every triggered workflow to completion.
11. Collect all four final reviews and prepare I's consolidated `human-review.md`.
12. Obtain explicit human approval before I writes the approved summary or task changes to Vikunja.

Serialized full checks:

```bash
cargo fmt -- --check
cargo clippy --all-targets -j 1 -- -D warnings
cargo test -j 1
python3 -m pytest tests/ -v
```

Use an isolated Python environment with the repository's declared test dependencies and required local tools.
A missing Docker/namespace capability or skipped mandatory acceptance check is a blocker, not proof of completion.
No live credentialed endpoint test is permitted without separate human approval.
No publication, Linear migration, unrelated cleanup, dependency upgrade, agent restart, or binary installation is part of this plan.

## How to read the file estimates

These are planning estimates, not approved edit lists or complete dependency inventories.
`(new)` marks a candidate file that does not exist yet. Proposed module names are not architecture decisions.
Existing paths were checked against the local source tree. Some audit targets may require no edits.

Most new caller options also affect `src/cli.rs`, `src/config.rs`, `daimonos.default.toml`, and `docs/configuration.md`.
Changes to instructions or schemas may also affect `prompts/tool_descriptions.toml` and `src/tool_descriptions.rs`.
Relevant new architectural decisions require records under `docs/adr/`; filenames are not selected here.
These shared files require an integration owner. Repeated paths do not imply parallel-safe edits.
FR-06 is already merged and FR-16 is withdrawn, so neither has a remaining implementation estimate.

## P0: finish the read-only foundation

### FR-08

Explicit internal state directory. Route all Daimonos state beneath it. Skip automatic home configuration. Reject unusable roots and escaping destinations without fallback. Name the offending option and path.

#### Estimated files affected

- Bootstrap: `src/cli.rs`, `src/main.rs`, `src/config.rs`, `src/paths.rs`.
- Runtime consumers: `src/session.rs`, `src/session_factory.rs`, `src/session_bootstrap.rs`, `src/agent_runtime.rs`, `src/agent_cmd.rs`, `src/chat_cmd.rs`, `src/acp_cmd.rs`, `src/session_daemon.rs`.
- State writers: `src/logging.rs`, `src/analytics.rs`, `src/index.rs`, `src/snapshot.rs`, `src/checkpoint.rs`, `src/session_store.rs`, `src/tool_output.rs`, `src/managed_process.rs`, `src/pipeline_cache.rs`, `src/evidence.rs`, `src/coordination/store.rs`, `src/kgl/store.rs`, `src/kgl/autoindex.rs`.
- Additional audit: `src/mcp_oauth.rs`, `src/remote_auth.rs`, `src/plugins/git_read/status.rs`, `src/plugins/git_read/runner.rs`.
- Tests: `tests/test_state_directory.py` (new), `tests/test_logging.py`, `tests/test_analytics.py`, `tests/test_git_read.py`, `tests/test_session_import_export.py`. A complete writer inventory is still required.
### FR-07

Optional prohibition on workspace writes. Include startup, tools, permissions, deliberate timestamp changes, locks, and teardown. Depends on FR-08 and FR-03. Verify actual read-only mounts and unchanged workspace snapshots.

#### Estimated files affected

- Guards: `src/ops/mod.rs`, `src/ops/file_ops.rs`, `src/ops/exec_ops.rs`, `src/ops/snap_ops.rs`, `src/tool_runner.rs`, `src/mcp.rs`, `src/tool_facade.rs`, `src/script.rs`, `src/safety.rs`, `src/plugins/mod.rs`.
- Automatic writes: `src/main.rs`, `src/session.rs`, `src/evidence.rs`, `src/index.rs`, `src/snapshot.rs`.
- Tests: `tests/test_read_only_workspace.py` (new), `tests/test_read_write.py`, `tests/test_snapshots.py`, `tests/test_batch.py`, `tests/test_git_read.py`. Coordinate these edits with FR-08 and FR-03.

FR-08 must include the FR-06 private status metadata directory. Reviewer suggestion S8 requests two concurrent status calls with independent views and cleanup.

## P1: reviewer runtime

### FR-01

Full 2020-12 final-output validation, including asserted date-time. Preserve the supplied schema. Separate corrective-ending retries from the model-request turn limit. Final output stays model-side. Report `turn_limit` distinctly.

#### Estimated files affected

- Ending policy/output: `src/agent.rs`, `src/agent_runtime.rs`, `src/agent_cmd.rs`, `src/headless_frontend.rs`, `src/providers/mod.rs`.
- Dependency: `Cargo.toml`, `Cargo.lock`. Possible extraction: `src/structured_output.rs` (new, proposed).
- Wire compatibility: `src/providers/anthropic.rs`, `src/providers/openai.rs`, `src/providers/openrouter.rs`. Provider-specific code stays in provider modules.
- Tests: `tests/test_structured_output.py` (new), schema/model fixtures under `tests/fixtures/`, and agent/provider Rust tests.
### FR-02

Explicit allowlist intersects existing restrictions. Apply it to listings and every dispatch, including composite inner calls. Empty allows nothing. Unknown names and a missing final-answer entry fail startup.

#### Estimated files affected

- Listings/dispatch: `src/tools.rs`, `src/tool_facade.rs`, `src/tool_runner.rs`, `src/mcp.rs`, `src/agent.rs`, `src/agent_mcp.rs`, `src/safety.rs`.
- Nested calls/registration: `src/script.rs`, `src/ops/tool_ops.rs`, `src/plugins/mod.rs`.
- Tests: `tests/test_tool_allowlist.py` (new), `tests/test_batch.py`, `tests/test_tool_pipeline.py`, `tests/test_tool_provisioning.py`, `tests/test_tool_list_changed.py`. Dispatch files overlap FR-03, FR-05, and FR-07.
### FR-03

Fixed-root path confinement for local tools and walks. Refuse unsafe local tools at startup when listed. Caller-provided workers remain caller-sandboxed and allowlisted.

#### Estimated files affected

- Resolution/walks: `src/session.rs`, `src/ops/file_ops.rs`, `src/index.rs`, `src/ops/diff_ops.rs`, `src/ops/snap_ops.rs`.
- Eligibility/dispatch: `src/tools.rs`, `src/tool_facade.rs`, `src/mcp.rs`, `src/tool_runner.rs`, `src/script.rs`, `src/plugins/mod.rs`.
- Git boundary audit: `src/plugins/git_read.rs`, `src/plugins/git_read/runner.rs`, `src/plugins/git_read/status.rs`.
- Tests: `tests/test_path_confinement.py` (new), `tests/test_roots.py`, `tests/test_set_cwd.py`, `tests/test_symlinks.py`, `tests/test_search.py`, `tests/test_ls.py`, `tests/test_git_read.py`. Share path-policy decisions with FR-12.
### FR-04

Split into strict request transport, audit, and capability-probe packages. Transport and audit work independently of FR-01. The probe requires FR-01. Use recording fake endpoints and preserve legacy provider behavior when disabled.

#### Estimated files affected

- Transport: `src/providers/mod.rs`, `src/providers/anthropic.rs`, `src/providers/openai.rs`, `src/providers/openrouter.rs`, `src/providers/router.rs`, `src/providers/request_trace.rs`.
- `src/client_transport.rs` carries session IPC, not model HTTP. It is not an FR-04 HTTP implementation target.
- Probe/lifecycle: `src/agent.rs`, `src/agent_runtime.rs`, `src/agent_cmd.rs`.
- Audit integration: `src/observability.rs`, `src/tool_runner.rs`, `src/tool_facade.rs`. Possible module: `src/run_audit.rs` (new, proposed). Keep the run audit distinct from analytics.
- Tests: `tests/test_request_control.py`, `tests/test_capability_probe.py`, `tests/test_run_audit.py` (all new), plus provider Rust tests and recording fixtures under `tests/fixtures/`.
### FR-05

Caller-provided MCP workers, explicit credential-free environments, remote-only tool routing, and fail-closed worker failure. Never forward the final-answer call to a worker.

#### Estimated files affected

- Connections/lifecycle: `src/agent_mcp.rs`, `src/mcp_bridge.rs`, `src/session_factory.rs`, `src/managed_process.rs`.
- Routing: `src/agent.rs`, `src/agent_runtime.rs`, `src/agent_cmd.rs`, `src/tool_facade.rs`. Audit `src/agent_env.rs` for credential propagation.
- Tests: `tests/test_worker_isolation.py` (new), `tests/conftest.py`, and worker fixtures under `tests/fixtures/`. Preserve FR-01 final-answer interception.

The human approved adding a full JSON Schema validator dependency. Select and report its name/version during that work. No validator was added in FR-06.

## P2: remove caller workarounds

### FR-09

Explicit-only configuration. Ignore automatic workspace/user configuration, environment files, registrations, instructions, prompts, and ignore files. Report ignored paths. Honor explicit caller configuration.

#### Estimated files affected

- Discovery: `src/main.rs`, `src/cli.rs`, `src/config.rs`, `src/agent_env.rs`, `src/env_file.rs`.
- Instructions/registrations: `src/prompts.rs`, `src/skills.rs`, `src/plugins/mod.rs`, `src/agent_mcp.rs`, `src/session_factory.rs`, `src/session_bootstrap.rs`.
- Ignore/config behavior: `src/index.rs`, `src/ops/file_ops.rs`, `src/zed_config.rs`.
- Tests: `tests/test_configuration_isolation.py` (new), `tests/test_cli_modes.py`, `tests/test_cli_prompts.py`, `tests/test_plugins.py`, `tests/test_search.py`. Inventory every automatic discovery path before implementation.
### FR-10

Locally buildable image with configurable workspace/mode and matching source-commit labels. Test non-root, read-only root, dropped capabilities, no network, and writable state. No publishing.

#### Estimated files affected

- Image and version wiring: `Dockerfile`, `src/cli.rs`, `src/main.rs`, `Cargo.toml` if build metadata requires it.
- Build-input audit: `daimonos.default.toml`, `prompts/tool_descriptions.toml`, and other embedded assets must enter the Docker build context. Their contents need not change.
- Tests/docs: `tests/test_container_runtime.py` (new), `tests/test_cli_modes.py`, `tests/README.md`, `README.md`. A local-build CI check may affect `.github/workflows/ci.yml`; no publishing workflow is planned.
### FR-11

Optional truncation without spill files or saved-file pointers. Give paging or narrowing guidance instead.

#### Estimated files affected

- Output formatting/storage: `src/tool_output.rs`, `src/tool_runner.rs`, `src/mcp.rs`, `src/agent.rs`, `src/managed_process.rs`, `src/ops/exec_ops.rs`, `src/ops/file_ops.rs`.
- Existing offload/handle behavior audit: `src/compaction.rs`. Preserve current behavior when the option is off.
- Tests: `tests/test_no_spill_output.py` (new), `tests/test_read_write.py`, `tests/test_exec.py`, and Rust tests in `src/tool_output.rs`.
### FR-12

Independent no-follow option. Report links and their target text without reading through them. Do not traverse links during walks.

#### Estimated files affected

- Reads/walks: `src/ops/file_ops.rs`, `src/index.rs`, `src/session.rs`.
- Policy eligibility/integration: `src/tools.rs`, `src/tool_runner.rs`, `src/plugins/mod.rs`; audit `src/plugins/git_read/runner.rs` against the shared link policy.
- Tests: `tests/test_symlinks.py`, `tests/test_read_write.py`, `tests/test_search.py`, `tests/test_ls.py`, `tests/test_no_follow.py` (new). Coordinate the shared resolution logic with FR-03.
### FR-13

Separately versioned tool contract, stable effective-schema dump, MCP version, and changelog. Dump inputs must match those that shape the served list.

#### Estimated files affected

- Effective definitions/export: `src/tools.rs`, `src/tool_descriptions.rs`, `src/tool_facade.rs`, `src/mcp.rs`, `src/session.rs`, `src/provisioning.rs`.
- Command/config wiring: `src/cli.rs`, `src/main.rs`, `src/config.rs`. Possible contract module: `src/tool_contract.rs` (new, proposed).
- Tests/docs: `tests/test_tool_contract.py` (new), `tests/test_handshake.py`, `tests/test_tool_provisioning.py`, `tests/test_tool_list_changed.py`, `docs/tool-contract-changelog.md` (new). Keep binary-version output unchanged.
### FR-14

Mark every owned path/glob/walk argument, including array elements. Exclude opaque code and caller-owned worker schemas.

#### Estimated files affected

- Schema definitions: `src/tools.rs`, `src/ops/schema.rs`, `prompts/tool_descriptions.toml`, `src/tool_descriptions.rs`.
- Plugin schema audit: `src/plugins/git_read.rs`, `src/plugins/git.rs`, `src/plugins/docker.rs`, `src/plugins/pytest.rs`, `src/plugins/shellcheck.rs`, `src/plugins/x07.rs`, `src/plugins/mod.rs`. Audit other plugin schemas before claiming complete coverage.
- Tests/docs: `tests/test_path_annotations.py` (new), `tests/test_tool_contract.py` (new under FR-13), and the contract documentation. Coordinate schema edits with FR-13 and FR-15.
### FR-15

Optional full-schema serving/export mode. Validate self-contained, nonrecursive 2020-12 schemas. Preserve supplied schemas. Distinguish detectable startup schema errors from endpoint rejection at the probe.

#### Estimated files affected

- Serving/rendering: `src/tools.rs`, `src/tool_descriptions.rs`, `src/tool_facade.rs`, `src/mcp.rs`, `src/agent_runtime.rs`.
- Validation integration: `src/agent.rs`, `src/providers/mod.rs`, and the FR-01 validator implementation. Reuse the existing full-schema serving path before introducing another mode.
- Tests: `tests/test_full_schemas.py` (new), `tests/test_tool_contract.py` (new under FR-13), `tests/test_tool_provisioning.py`, and schema fixtures under `tests/fixtures/`. Reuse the approved validator rather than assume a second dependency.

## P3: verification and resource controls

### FR-17

Full stored-byte SHA-256 for filesystem and git reads, including paged/truncated responses. Fail if an enabled budget prevents hashing the full file.

#### Estimated files affected

- Read/hash paths: `src/ops/file_ops.rs`, `src/index.rs`, `src/plugins/git_read.rs`, `src/plugins/git_read/runner.rs`, `src/plugins/git_read/tests.rs`.
- Result/schema documentation: `src/tools.rs`, `prompts/tool_descriptions.toml`, `src/tool_output.rs` if wrappers must preserve the digest field.
- Tests: `tests/test_file_digests.py` (new), `tests/test_read_write.py`, `tests/test_git_read.py`. Coordinate full-file accounting with FR-19 and paging behavior with FR-11.
### FR-18

Optional offline-tools profile and analytics collection off. Disable internal network activity and discovery too. Observe connection attempts, not just successful connections.

#### Estimated files affected

- Startup/profile: `src/main.rs`, `src/config.rs`, `src/session.rs`, `src/session_factory.rs`, `src/agent_runtime.rs`, `src/plugins/mod.rs`, `src/tools.rs`.
- Collection/export: `src/analytics.rs`, `src/observability.rs`, `src/providers/request_trace.rs`.
- Discovery/network audit: `src/agent_mcp.rs`, `src/mcp_bridge.rs`, `src/mcp_oauth.rs`, `src/remote_gateway.rs`, `src/kgl/autoindex.rs`, `src/kgl/substrate_repowise.rs`, `src/kgl/substrate_graphify.rs`, `src/kgl/substrate_x07.rs`. This list needs a complete connection-attempt inventory.
- Tests/docs: `tests/test_offline_tools.py` (new), `tests/test_analytics.py`, `tests/test_observability.py`, `tests/test_kgl.py`, and network-policy documentation (new, filename TBD).
### FR-19

Optional deadlines, logical input-byte budgets including indexes, and separate directory-entry limits. Stop external processes at their deadline. Refuse unmeterable tools only when the relevant budget is enabled. Record named limit failures.

#### Estimated files affected

- Per-call lifecycle: `src/tool_runner.rs`, `src/tool_facade.rs`, `src/mcp.rs`, `src/agent.rs`, `src/script.rs`, `src/managed_process.rs`.
- Input accounting: `src/ops/file_ops.rs`, `src/index.rs`, `src/ops/exec_ops.rs`, `src/plugins/git_read.rs`, `src/plugins/git_read/runner.rs`, `src/plugins/git_read/status.rs`.
- Capability/audit integration: `src/tools.rs`, `src/plugins/mod.rs`, and the FR-04 audit implementation.
- Tests: `tests/test_tool_budgets.py` (new), `tests/test_exec.py`, `tests/test_search.py`, `tests/test_read_write.py`, `tests/test_git_read.py`, and managed-process Rust tests.

## Binding exclusions and delivery rules

- FR-16 is withdrawn. No registry, image-publishing, or publishing-CI work.
- No live credentialed model smoke test without human approval.
- No default regression except the already approved FR-06 filter-status security exception.
- No schema rewriting or guessed endpoint-support filtering.
- Internal-state relocation does not relocate explicit caller/model file writes.
- Existing accepted ADRs govern their boundaries. New decisions need appropriate records.
- ADR-018 and ADR-019 remain Proposed. Merge alone does not imply architectural acceptance.
- Keep development notes and status in the assigned local agent log directories.
- Only I may write approved final findings or task changes to Vikunja after overall completion and explicit human approval.
- Defer the Linear port until overall completion.

## Existing coarse tracking buckets

- #1577: FR-08 state directory → WP01, WP02.
- #1578: FR-07 workspace write protection → WP04.
- #1579: P1 policy, confinement, and workers → WP03, WP06, WP10.
- #1580: P1 final output, transport, probe, and audit → WP05, WP07, WP08, WP09.
- #1581: P2 requests → WP11–WP17.
- #1582: P3 requests → WP18–WP20.

These buckets are reference IDs, not a live agent-work ledger. Track WP01–WP20 in I's local integration ledger.
Do not split, create, reassign, or update tracker tasks during implementation.
At overall completion, I proposes any needed tracker changes with the final report and waits for explicit human approval.

## Planning validation

The document-only checks found:

- All 17 remaining FRs have primary acceptance owners across WP01–WP20.
- The package dependency graph has 20 nodes and no cycle or unknown prerequisite.
- Repeated exclusive paths occur only between dependency-ordered packages.
- No independent packages share an exclusive file grant.
- The 28 existing exclusive paths were opened and checked. The 34 missing owned paths are explicitly marked new.
- Both original PM input digests remain unchanged.
- The visible source anchors resolve in the implementation base.
- The prior investigation log and three original untracked inputs/handoff states remain preserved.

These checks validate the plan structure, not the future implementation.
No proposed test, feature implementation, tracker split, worktree launch, or agent assignment occurred.
Package-plan, ADR, and G0 launch-contract reviews are complete. Check operational readiness before each affected implementation wave.

_Anchors:_ src/paths.rs::state_dir, src/session.rs::Session::resolve_path, src/tool_runner.rs::ToolRegistry::run, src/tool_facade.rs::active_schemas, src/managed_process.rs::apply_environment, src/plugins/git_read/status.rs::StatusView::create
