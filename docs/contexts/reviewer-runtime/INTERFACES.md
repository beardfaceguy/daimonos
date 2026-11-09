# G0 interface ledger

Date: 2026-10-29
Status: Launch contracts approved by the user on 2026-10-29 UTC. Package dependencies and operational preflight still apply.
Base: `859194c8cc3aa736e6539db14af6bd370f325e7c`.
Decisions: Accepted ADRs 021–032. Scope: the approved 20-package [plan](../../handoffs/2026-10-28-dbunk-remaining-work.md).
Coverage: [INVENTORIES.md](INVENTORIES.md).

## Reading this ledger

Every public spelling, new type, exit number, and platform mechanism below is **proposed**, unless identified as existing.
The architecture is Accepted. These spellings and launch contracts still require the G0 review.
`(new)` marks a file or symbol that does not exist. Existing anchors identify inspected source sections.
The integrator owns shared-file adapters. Package owners own the named helper modules and tests from the plan.
Names here are reservations, not additional features or new default behavior.

## Authority and startup order

1. Parse caller-only options without reading workspace or user configuration.
2. Resolve explicit state and network constraints before any writer, provider, exporter, or child constructor.
3. Resolve permitted explicit control files. Apply discovery policy before any implicit read or environment mutation.
4. Validate the state root and internal destinations. Validate the filesystem mechanism before restricted services start.
5. Admit supported services, register selected tools, and freeze effective policy/catalog metadata.
6. Compile the caller's final schema locally and open the requested audit destination before a model request.
7. When enabled, perform the single capability probe before evidence tools. Reuse the exact frozen definitions.
8. Run tools only through admitted routes. Preserve authority and budgets across every delegated boundary.
9. Finalize the local audit and durable session state during ordered shutdown. Never emit diagnostics on structured-output stdout.

State, confinement, read-only, no-follow, no-spill, analytics-off, controlled transport, and offline-tools remain independent options.
An explicit option cannot be weakened by configuration, environment-derived defaults, worker discovery, cwd changes, or retries.
With every new option absent, preserve existing behavior and accepted regression baselines.

## Caller interface reservations

Use CLI options and explicit JSON control files. No new environment alias may widen these controls.
The final field names must appear in help, configuration docs, audit schemas, tests, and the final PM report.

| Package / FR | Proposed caller interface | Validation and startup behavior |
|--------------|---------------------------|---------------------------------|
| WP01/WP02, FR-08 | Global `--state-dir PATH` | Existing writable private directory outside evidence. Missing, unusable, or overlapping roots fail before writers. No HOME/XDG/temp fallback. |
| WP03, FR-03 | `--confine-paths`; optional `--confine-root PATH` | The first selects the workspace root. An explicit root also enables confinement. Canonical root is fixed for the run. Relative arguments use the session working directory (ADR-022 amendment, PM-confirmed 2026-11-07). |
| WP04, FR-07 | `--workspace-read-only` | Deny all evidence mutations and unsupported opaque operations. Internal writes use state. No implicit relocation of a requested evidence edit. |
| WP05, FR-01 | Agent `--final-output PATH` | JSON contains `schema` or `schema_path`, `tool_name`, `tool_description`, `corrective_retries`, and `max_model_requests`. Schema fields are mutually exclusive. Tool-name collisions fail startup. |
| WP06, FR-02 | `--tool-allowlist PATH` | Explicit JSON array of exact exported tool names. Omitted means legacy behavior. `[]` allows none. Unknown names fail before the probe. |
| WP07, FR-04 transport | Agent `--model-profile PATH` | One complete JSON object owns the five required values and controlled request settings. Do not combine partial sources. |
| WP08, FR-04 audit | `--audit-path PATH` | Caller-requested, private JSONL destination outside evidence. Audit is independent of controlled transport. A requested write failure stops the run. |
| WP09, FR-04 probe | Agent `--capability-probe-payload PATH` | Requires structured final output and a complete controlled model profile. Validate payload locally before the one unretried probe. |
| WP10, FR-05 | Agent `--worker-config PATH` | Explicit MCP-stdio worker definitions and explicit child environment. No local execution fallback. Final answer validation stays model-side. |
| WP11, FR-09 | `--ignore-config-files` | Disable every implicit source in D01–D09. Explicit `--config`, instructions, profiles, and worker definitions remain caller-owned inputs. |
| WP12, FR-10 | Container forwards `--workspace`, `--state-dir`, and MCP/tool-side mode | No entrypoint installation or fixed evidence path. Runtime needs only declared writable state. |
| WP13, FR-11 | `--no-output-spill` | Retain caps and paging instructions. Do not create full copies, saved paths, or hidden file pointers. |
| WP14, FR-12 | `--no-follow-symlinks` | Independent of confinement. Return link metadata and target text without reading through it. |
| WP15, FR-13 | `tool-contract` command; explicit worker config when needed | Deterministic JSON export without a model session. Only selected worker discovery may occur. |
| WP16, FR-14 | Schema annotation `x-daimonos-path: true` | Mark actual path slots, including array items and nested stage arguments. Do not infer them from free text. |
| WP17, FR-15 | CLI `--full-tool-schemas`; preserve existing `[mcp].full_tool_schemas` | Serve full self-contained schemas from the effective catalog. Legacy progressive exposure remains the default. |
| WP18, FR-19 | `--tool-timeout-ms N`, `--tool-input-bytes N` | Positive integers. Omitted means no new limit. A configured unsupported operation is refused, not silently exempted. |
| WP19, FR-17 | `--read-sha256` | Add `sha256` metadata only after hashing the complete stored bytes, including paged/truncated reads. |
| WP20, FR-18 | `--offline-tools`, `--analytics-off` | Offline is a process-wide ceiling. Analytics-off skips local collection. No networked model in that offline process. |

These flags do not exist yet. G0 review must approve or replace their spellings before implementation.
Trusted control files outside evidence are not model-supplied evidence paths. They never grant tools access to their own locations.
With explicit state, internal paths such as audit, trace, token-log, socket, and cache overrides must also remain under state.

## State and filesystem contracts

| Owner | Proposed module and seam | Contract |
|-------|--------------------------|----------|
| WP01 A | `src/state_root.rs` (new): `StateAuthority`, `StateKey`, `WorkspaceId` | Resolve each internal destination from immutable caller authority. Construct private child paths without a fallback root. |
| WP02 B | Existing `paths`, `session_bootstrap`, `agent_runtime`, session stores | Accept the same authority. No new HOME/config lookup in restore, migration, cancellation, or shutdown. |
| WP03 C | `src/path_policy.rs` (new): `FilesystemAuthority`, `AuthorizedRead`, `WalkEntry` | Keep the canonical root and owned directory descriptors. Authorize actual opens and walks, not only path strings. |
| WP04 A | `src/run_policy.rs` (new): `ToolAdmission`, `EffectClass` | Central admission table identifies mediated reads, explicit mutations, and opaque effects. No new architecture for a boolean read-only label. |
| WP14 A | `FilesystemAuthority` extension | Use the same authorization path for independent no-follow semantics and confined walks. |

State layout reservations: `workspaces/<id>/index`, `snapshots`, `pipeline`, `kgl`, `checkpoints`, and `evidence`.
Run/global reservations: `sessions`, `catalog`, `coordination`, `analytics`, `logs`, `tool-output`, `oauth`, `scratch`, and `sockets`.
These are state keys, not caller-controlled relative paths. One shared resolver owns workspace identity derivation.
Do not create or copy the caller's state-root directory if it is missing. Validate and pin it first.
Explicit state precedence covers all W01–W25 consumers, including child scratch paths and SQLite sidecars.

### Proposed supported filesystem mechanism

Use the existing direct `libc` dependency. No additional resolver crate is proposed.
On Linux, use pinned root descriptors with `openat2` resolution constraints for actual access.
Confinement permits links whose resolution stays inside the root. Reject magic links and escapes without falling back to a check-then-open sequence.
No-follow adds a no-symlinks rule. Walk metadata and link target text come from descriptor-relative operations without following the entry.
Read-only admission rejects mutation entrypoints before execution. Future writable targets require validated parent handles, not canonicalization of nonexistent files.

Probe kernel support before admitting the confined profile. If unavailable, return `filesystem_policy_unsupported` before services start.
On other platforms, reject a requested confined profile until an equivalent tested resolver exists. Ordinary unconfined modes remain unchanged.
The current host is Linux x86_64. The resolver syscall and race guarantees are not yet implemented or certified.
This platform-support restriction needs explicit G0 review.

## Tool policy, catalog, and workers

| Owner | Proposed module and seam | Contract |
|-------|--------------------------|----------|
| WP06 B | `src/run_policy.rs` (new, transferred from WP04): `ToolPolicy`, `ToolDecision` | `Option<set<name>>` distinguishes no allowlist from an explicit empty list. Validate exact names after selected discovery, before model work. |
| WP15 C | `src/tool_contract.rs` (new): `EffectiveToolCatalog`, `ToolContractDocument` | Serving, lazy expansion, export, schema lookup, and the probe read one frozen catalog. |
| WP16 A | `src/path_annotations.rs` (new), adapted to existing `tools::ToolDef` by I | The schema owner marks a path, array item, glob, or walk root. Policy uses the same metadata. |
| WP17 C | `src/schema_contract.rs` (new): `ValidatedToolSchema` | Validate 2020-12 schemas offline. Reject external references, recursive tool definitions, and definition-name collisions. |
| WP10 B | `src/worker_runtime.rs` (new): `WorkerExecutor`, `WorkerRoute` | Reuse MCP stdio clients and lifecycle ownership. Every ordinary tool routes to a selected worker or fails. |

Required method reservations, with signatures only:

- `StateAuthority::resolve(StateKey, Option<WorkspaceId>) -> Result<PathBuf, RunFailure>`.
- `FilesystemAuthority::open_read(&Path) -> Result<AuthorizedRead, ToolFailure>`.
- `FilesystemAuthority::walk(&Path, WalkOptions) -> Result<Walk, ToolFailure>`.
- `ToolPolicy::check(&str) -> Result<ToolDecision, ToolFailure>`.
- `EffectiveToolCatalog::freeze(definitions, policy, filesystem) -> Result<Self, RunFailure>`.
- `EffectiveToolCatalog::export() -> ToolContractDocument`.
- `WorkerExecutor::call(name, arguments, context) -> Result<ToolResult, ToolFailure>`.
- `WorkerExecutor::shutdown() -> Result<(), RunFailure>`.

The integrator adapts existing `mcp::dispatch_local_tool`, `ops::dispatch_with_progress`, `script::dispatch_tool_by_name`, and pipeline stages.
Lower plugin and worker routes enforce the same decision. A cached result is not a policy exemption.
Preserve existing exported MCP names and namespacing. Reject collisions between native, worker, and terminal definitions.
Discovery can initialize explicitly selected workers. It must not discover arbitrary HOME/workspace servers or acquire model credentials.
The caller must separately sandbox workers. Stdio alone is not proof of confinement or offline behavior.

Proposed worker JSON fields: `workers`, with entries containing `name`, `command`, `args`, and `env`.
Commands use an explicit executable. `env` starts empty, not from inherited server configuration or the ambient environment.
Reject explicit environment/argv values that disclose the selected model credential. Never copy model headers, profiles, or secret-bearing model environment into a worker.
All explicitly selected workers are required in worker-only mode. No optional-worker fallback is exposed.
A worker exit, startup failure, unavailable call, or timeout fails the run with `worker_unavailable`.
Session end and failure drain owned worker clients and processes. Preserve existing lease/pool ownership rules.

The model-side terminal tool is a control operation, not a worker tool.
WP06 validates its configured name in the model-visible effective policy. WP10 never sends its arguments to workers.
Batch requires its own admission and admissions for every child. Script and pipeline bindings inherit policy and remaining budgets.
Confinement/read-only admissions remain stronger than an allowlist entry. They can refuse an otherwise listed opaque tool.

Contract JSON reservations: `contract_version`, `tools`, and `effective_options`.
Each tool entry contains `name`, `description`, `input_schema`, and its declared ownership/route metadata.
Proposed initial contract version: `1.0.0`. It is independent of the existing Cargo package version.
Breaking argument/result/name changes require a contract major bump. Document changes in `docs/tool-contract-changelog.md` (new).
Static native export starts no model session. Explicit worker definitions can require worker discovery before export and serving.
Both paths apply identical selection and validation. A network-dependent discovery request is refused in an offline process.

## Structured final output and controlled requests

WP05 owns `src/structured_output.rs` and the reusable `src/schema_validation.rs` (both new): `FinalOutputSpec`, `FinalOutputState`, and `FinalAttemptFailure`.
Use `jsonschema` version `0.60.2`, the full draft-2020-12 validator selected by Accepted ADR-024.
Compile with default external resolver features disabled. Assert `format: date-time` and permit only self-contained local references.
Acceptance of ADR-024 confirms this named validator dependency. This step does not add it to Cargo.toml.
Any additional crate still needs separate confirmation.

The terminal response is valid only when exactly one call uses the configured final-tool name and its arguments validate.
Text/reasoning alongside that sole call is allowed. Do not send the terminal schema through tool-schema subset rewriting.
For a mixed ending, answer every call in protocol order and execute none of that failed ending's calls.
Corrective messages use `prompts/` text (new entries), not model-facing Rust string literals.
Success emits only the argument JSON and one newline on stdout. Failure emits a bounded JSON error on stderr or an authorized state destination.

Counter reservations:

- `corrective_retries` counts extra final-output attempts after the first ending. A value of 1 permits two ending attempts.
- `final_attempts` counts candidate terminal endings, not ordinary successful evidence-tool rounds.
- `max_model_requests` is an independent optional positive limit for the whole run.
- `model_requests` increments before every HTTP attempt, including the probe and explicit transport retries.
- The probe is exactly one HTTP attempt with no retry, even when later ordinary requests permit retries.
- A request-limit refusal sends no request. It cannot reset a counter or silently select another provider.

The failure JSON contains `reason`, `validation_errors`, `attempts`, and `model_requests`.
Required final-output reasons remain `no_final_call`, `mixed_calls`, `invalid_arguments`, `retries_exhausted`, `endpoint_error`, and `schema_unusable`.
Do not collapse those into one free-text message. The terminal failure records its last validation errors.

WP07 adapts existing `src/providers/anthropic.rs`, `src/providers/openrouter.rs`, and `src/providers/router.rs`.
I owns shared `ControlledModelSpec`, `ControlledRequest`, and `ControlledTransport` seams in existing `src/providers/mod.rs`.
Do not add a parallel controlled provider module or duplicate wire implementation.
The model-profile JSON contains `provider_label`, `model`, `base_url`, `wire`, and `credential_env` as one complete source.
Additional caller controls are `max_tokens`, `timeout_ms`, `request_retries`, `proxy`, and explicit `extra_body_fields`.
`wire` is `chat` or `messages`. Never infer it from model names or URLs.
The credential value comes only from the named variable and never appears in control-file exports or audit records.

No implicit sampling, cache hints, prompt-cache controls, provider strict flags, trace propagation, SDK telemetry, redirects, proxies, or HTTP retries.
Use the exact body/header sets from FR-04. Messages uses the stable `/v1/messages` path and caller-owned `max_tokens`.
Chat uses the caller base URL's stable Chat Completions path. Native OpenAI Responses remains an unchanged separate adapter.
Explicit extra fields cannot overwrite protected fields or introduce forbidden sampling/forced-choice/strict behavior.
Record every attempted request. Do not retry a probe or perform implicit model-discovery calls to fill missing configuration.
Controlled requests can run without final-output mode or audit. Final-output mode can run without this transport.
Auxiliary summary/reformat calls use the same controlled authority and request counter. They must not select a legacy provider or retry policy.
If an auxiliary path cannot honor the exact request contract, refuse before sending with `auxiliary_request_unsupported` metadata.
Do not silently drop history or perform a hidden fallback request after that refusal. Default non-profile compaction remains unchanged.

WP09 owns `src/capability_probe.rs` (new): `ProbeSpec` and `ProbeOutcome`.
The caller supplies a minimal valid final payload. Local schema validation occurs before the endpoint call.
The probe offers the byte-equivalent frozen definitions that the review offers. No dummy-tool substitute is allowed.
It never invokes an evidence tool. Any endpoint error, wrong/multiple call, text-only ending, or invalid arguments yields `model_unsupported`.

## Audit, output, and resource contracts

WP08 owns `src/run_audit.rs` (new): `RunAudit`, `AuditEvent`, and `AuditFailure`.
Proposed format: one private JSONL file per run, one versioned object per event, and a terminal run-status event.
Events carry `audit_version`, `run_id`, `sequence`, and `kind` plus only the fields allowed for that kind.
Kinds: `run_start`, `model_request`, `probe_result`, `tool_call`, `tool_refusal`, `limit_hit`, and `run_end`.
This event layout is a proposed interface choice, not a change to the accepted local-audit ownership decision.

- Run fields: provider label, requested/reported model, base-URL host, wire, credential variable name, effective policy/root, and final status/exit code.
- Model fields: request sequence, HTTP status, explicit retry index, probe flag, and reported usage when available.
- Tool fields: exported name, route, argument/result SHA-256, allowed/refused, reason, elapsed duration, and limit outcome.
- Hash compact UTF-8 JSON values with stable key ordering. Result hashes cover the result delivered at the dispatch boundary, not a Debug representation.
- Never include credential values, request/response bodies, full evidence content, forwarded headers, or raw endpoint error bodies.
- Open with private permissions and fail if the requested destination cannot be safely created. Do not silently append another run to the same file.
- Enforce configured event-size and event-count limits. Requested audit failure stops further execution and produces a nonzero failure.
- Write the successful terminal audit event before emitting the structured success JSON.
- A process kill can leave an incomplete audit. Do not claim that this recorder supplies crash recovery for active model turns.

WP13 extends existing `tool_output::bound_text` with an output policy.
`save_full_output=false` skips full-copy creation and saved-path metadata entirely, including failure and script/pipeline routes.
Truncated responses retain paging or query-narrowing guidance. They never advertise a nonexistent saved file.
User-requested explicit file writes remain ordinary tool operations, not spill output.

WP18 owns `src/tool_budget.rs` (new): `ToolBudget`, `BudgetReceipt`, and `LimitFailure`.
Method reservations: `ToolBudget::check_deadline()`, `charge_input(bytes)`, and `remaining()` return typed results.
A call starts with an absolute deadline and a remaining logical-input counter at admission.
Children share those values. A script, batch, pipeline stage, cache hit, or worker route never receives a fresh top-level budget.
Charge logical uncompressed bytes consumed by a read, scan, parser, or full digest. Do not substitute output size or physical disk-sector reads.
Cached results need a validated accounting receipt and current policy admission. Otherwise bypass the cache or refuse the operation.
Cooperative loops check before reads and traversal steps. Limits cannot wait until an unbounded read or traversal completes.
Owned subprocess deadlines use existing `ManagedChild::terminate` to terminate and reap the process group.
A non-terminable worker or an opaque input scan is refused when the corresponding budget is enabled.
No cooperative guarantee is claimed for an unowned remote process.

Proposed `ToolFailure` fields: `code`, `message`, `tool`, `argument`, and optional `limit`, `configured`, `consumed`, and `retryable`.
A timed-out or input-limited call returns `complete=false` and no success-shaped partial result.
These fields are policy/error metadata, not a rewrite of every existing tool's successful payload.
Audit records the named limit and the refusal. Error text is bounded and does not contain raw evidence.

WP19 owns `src/file_digest.rs` (new) and the existing pinned-git runner.
It adds `sha256` to the existing read payload or binary metadata when enabled.
Use all stored bytes from the authorized file handle or pinned git blob, not normalized text, a page, or decoded lossy output.
A complete digest may accompany truncated text. A partial digest never appears.
If limits prevent completion, return `tool_input_limit` or `tool_timeout` with no `sha256` value.
`git_read` operations that cannot expose/count a complete blob stream refuse input-budget mode until supported.
Do not weaken existing binary withholding or strict full-commit pinning to provide a hash.

## Errors and exit reservations

These code names and numbers are proposed. Existing ordinary CLI behavior stays unchanged outside the selected modes.
Do not present generic `anyhow` text as the machine-readable contract.

| CLI exit | Stable run code | Trigger |
|----------|-----------------|---------|
| 0 | `ok` | Normal success. Structured mode emits only its validated argument JSON. |
| 1 | Existing legacy error | Existing non-profile failures. Do not renumber them. |
| 2 | `run_configuration_invalid` | Missing/conflicting controls, unknown allowlist entry, unusable state, or unsupported requested profile |
| 20 | `structured_output_failed` | Final-output failure with a required FR-01 reason and counters |
| 21 | `model_unsupported` | Capability probe failed. No evidence tool starts. |
| 22 | `worker_unavailable` | Required worker initialization, call, liveness, or teardown failure |
| 23 | `audit_write_failed` | Requested audit cannot open, append, or finish |
| 24 | `model_request_limit` | Inclusive request counter prevents the next attempt |
| 25 | `controlled_endpoint_error` | Controlled transport failure outside a probe or final-output failure context |

Exit 2 includes detailed reason codes: `state_unusable`, `state_overlaps_workspace`, `state_mode_unsupported`, `unknown_tool`, `tool_name_collision`,
`filesystem_policy_unsupported`, `offline_profile_incompatible`, `model_profile_incomplete`, and `schema_unusable` before a request.
`state_mode_unsupported` is a temporary WP01 refusal for frontends whose state is not yet relocated. WP02 removes that limitation as it adds support.
Under final-output mode, `schema_unusable` uses exit 20 and the required final-output error shape instead.
A probe endpoint failure uses exit 21. A later controlled endpoint failure in final-output mode uses exit 20 with `endpoint_error`.
Audit failure takes precedence over reporting successful completion. Preserve the original failure code as bounded secondary metadata when available.

Tool refusal codes: `tool_not_allowed`, `path_outside_root`, `workspace_write_forbidden`, `symlink_refused`,
`tool_policy_unsupported`, `tool_timeout`, `tool_input_limit`, and `offline_tool_forbidden`.
`symlink_refused` includes the link's target text as metadata without reading its target.
Confined walks report skipped links and reasons. They do not return outside content or metadata.
Do not include an unbounded path listing, filesystem error dump, or secret value in error messages.

## Offline and capability admission

WP20 owns `src/offline_tools.rs` (new): `NetworkAuthority` and `ProducerAdmission`.
Offline admission occurs before providers, discovery, OTLP, OAuth, MCP IP transports, remote tools, gateways, or child producers initialize.
Model profiles requiring IP networking are incompatible with the same offline process.
Native stdio and Unix IPC are not outbound IP grants. Arbitrary stdio children still require proven isolation or refusal.
Analytics-off prevents local event collection as well as store initialization. OTLP-off remains a separate producer control.
Document every N01–N21 disposition and update that inventory with any new producer.

The first supported confined backend is proposed as Linux/openat2 using existing libc.
No-follow on an unsupported backend must also refuse rather than use a race-prone check-then-open fallback.
Opaque process input budgets are unsupported unless a tested adapter accounts their logical input.
Budgeted remote workers are refused when their deadline cannot terminate owned work or their input accounting cannot be trusted.
These refusals preserve guarantees. They are not permission to omit ordinary no-budget worker operation.

## Configuration and compatibility reservations

New configuration groups: `reviewer.final_output`, `reviewer.controlled_model`, `reviewer.audit`, and `reviewer.tool_limits`.
Explicit caller options own active security controls. Config can supply operational caps, but cannot widen explicit restrictions.
Audit limits: `max_event_bytes`, `max_events`. Schema/input limits: `max_schema_bytes`, `max_validation_errors`.
All implementation caps belong in `daimonos.default.toml` and typed config, never unbounded allocations or hidden constants.
Their default numeric values receive package-level configuration review. They are not protocol guarantees.

Preserve existing MCP/client namespacing, progressive defaults, native providers, log privacy, process cleanup, and session durability.
Do not change unrelated features or introduce a second provider wire/protocol implementation merely to avoid adapters.
The worker protocol remains MCP stdio. WP12 adds no private daemon protocol or image-publication scope.
The separate evidence-retention todo is not part of these packages.

## Integration seams and package trace

| Packages | Existing seam to adapt | Required integrated check |
|----------|------------------------|---------------------------|
| WP01/WP02 | `main::main`, `provisioning::build_tool_services`, `agent_runtime` frontends | No writer or discovery fallback precedes explicit authority |
| WP03/WP04/WP14 | `Session::resolve_path`, file ops, walks, git reads, effectful opcodes | Actual opened handles stay authorized. Root and no-follow semantics cannot be bypassed. |
| WP05/WP09 | `AgentSession::staged_prompt_message`, completion events, provider request options | Exactly one terminal call, correct error results, bounded retries, and probe-first execution |
| WP06/WP10 | Direct MCP, opcodes, Starlark, pipeline, plugins, and bridge hooks | One explicit policy and worker-only ordinary tool routing |
| WP07/WP08 | Provider adapters, audit hooks, startup diagnostics, shutdown | Exact requests and privacy-safe local records without changing default providers |
| WP11/WP12 | Startup env/config, instructions, skills, worker discovery, container entrypoint | Explicit-only discovery and a restricted local MCP tool-side container |
| WP13 | Tool-output postprocessing, pipeline/script aggregation, response metadata | No spill file and no saved-file pointer on any selected route |
| WP15/WP16/WP17 | `tools::ToolDef`, all export/list/schema paths, MCP initialization | One effective versioned contract, correct path metadata, and full self-contained schemas |
| WP18/WP19/WP20 | Cooperative loops, process groups, caches, opaque routes, network constructors | Limits stop work, hashes are complete, and offline profiles make zero IP attempts |

Each WP retains the exact AC mapping and verification commands in the package plan. This table does not replace those checks.
FR-06 remains completed. FR-16 remains withdrawn. No package adds image publication or a new worker protocol.
WP17 must not reduce the caller's final schema to an endpoint subset or silently apply native-tool recursion restrictions to it.
The final schema uses WP05's full validator. Endpoint rejection remains observable through the exact capability probe.

WP15 also reserves global `--version` output with package version, source commit, and tool-contract version.
Use build metadata for the source commit, not a runtime git lookup. The local image labels must match that metadata.
Any required new build metadata file is marked `(new)` in its implementation task before editing.

## G0 review and readiness

Prepared for review:

- W01–W27 writer/supporting-reader consumers, D01–D10 discovery sources, and N01–N21 network/delegated producers.
- Shared policy boundaries, support/refusal matrix, public options, helper signatures, errors, counters, and ownership.
- A private immutable bundle at `~/.daimonos/agentic_work_logs/dbunk-integrator/sealed-inputs/8f767687c0b1-11901ce73e0b/`.
- Both bundle files match the current source files and the pre-crash recovery archive byte-for-byte.
- The earlier attempted `1375dca9b223-c416379733d7` bundle has invalid hash metadata. Never use it for launch.

Fresh tool preflight found Git 2.56.0, Rust/Cargo 1.96.0, Python and pytest 9.1.1, Node, Docker, and Clang.
System npm and ShellCheck are absent. Temporary copies remain in `target/test-tools/bin`.
For existing full suites, use `PATH="$PWD/target/test-tools/bin:$PWD/.venv/bin:$PATH"`.
No package dependency was installed. No compiler, full test suite, Docker image, or restricted runtime test ran during G0 preparation.
Docker command availability alone does not certify daemon access or AC-10.1.

Human launch review completed at 2026-10-29T04:41:57Z:

1. Approve or revise the public option/control-file spellings and the reserved type/method names, including auxiliary-request refusal.
2. Approve or revise the error shapes, audit event schema, contract version, and distinct exit numbers.
3. Approve or revise Linux/openat2 as the first confined backend, with explicit unsupported-platform refusal.
4. Review the unsupported-tool and opaque-worker budget admissions. Ordinary no-budget worker mode remains in scope.

The user explicitly approved all four groups. The integrator may open the relevant package gates after operational preflight.
ADR acceptance and this ledger's human review are complete. No further design approval is required for the reviewed scope.
The first wave needs WP01/WP03 launch contracts closed. No implementation agent is assigned or launched by preparing these artifacts.
No development notes or task updates may go to Vikunja before the previously agreed final approval.

_Anchors:_ src/main.rs::main, src/provisioning.rs::build_tool_services, src/session.rs::Session::resolve_path, src/providers/mod.rs::CompleteOpts, src/managed_process.rs::ManagedChild::terminate, src/agent.rs::AgentSession::staged_prompt_message, src/mcp.rs::dispatch_local_tool, src/ops/mod.rs::dispatch_with_progress, src/tool_output.rs::bound_text
