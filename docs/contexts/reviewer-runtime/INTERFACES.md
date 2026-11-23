# G0 interface ledger

Date: 2026-10-29
Status: Launch contracts approved by the user on 2026-10-29 UTC. Package dependencies and operational preflight still apply.
Implementation status: WP04 (`--workspace-read-only`, FR-07) is implemented. The reserved spellings `--workspace-read-only`, `src/run_policy.rs::ToolAdmission`/`EffectClass`, and the refusal codes `workspace_write_forbidden` / `tool_policy_unsupported` are now live (`tests/test_read_only_workspace.py`). Startup refusals use the reserved exit-2 taxonomy below: top-level `code` is `run_configuration_invalid` with the specific cause in `reason` (e.g. `workspace_write_forbidden`), matching the state-authority failure shape (the two tool-refusal codes remain tool-result codes, never exit-2 top-level codes). `--workspace-read-only` is part of the session-daemon launch identity (`src/session_bootstrap.rs`: forwarded to a spawned daemon, hashed into the bootstrap fingerprint, and an explicit selection in `launch_identity`), so a read-only client cannot be served by a daemon started without it.

Implementation status: WP08 (`--audit-path`, FR-04) is implemented. The reserved spellings `--audit-path`, `src/run_audit.rs` (`RunAudit`, `AuditFailure`), the exit code `23` / `audit_write_failed`, and the `[reviewer.audit]` bounds (`max_event_bytes`, `max_events`) are live (`tests/test_run_audit.py`). `run_start`, `model_request`, `tool_call`, and `tool_refusal` are wired through the one-shot agent frontend; `probe_result` (WP09) and `limit_hit` (WP18), and the `model_request` HTTP-status/`wire` fields (WP07), are defined in the module and exercised through its public test seam as pending integrator hooks.

Implementation status (WP08, r2): the reserved type `AuditEvent` is now provided in `src/run_audit.rs` (it renders the exact JSON streamed to the file, so all three reserved spellings `RunAudit`/`AuditEvent`/`AuditFailure` are live). `--audit-path` is honored only by `agent --print`; every other frontend that accepts it (`chat`, `acp`, the MCP server, the session daemon, the interactive agent, `agent --dry-run`) refuses at startup with exit `2` (`run_configuration_invalid`, reason `audit_unsupported_frontend`). The terminal `run_end` record is written before any assistant output is published, and its `status`/`exit_code` are derived from the same terminal source as the process exit. Process-level coverage of mid-run-write and finish-time audit failures (both exit `23`), and of each refusing frontend, is in `tests/test_run_audit.py`.
Implementation status (WP08, r3): `run_end.reported_model` is a pending integrator hook, recorded next to the `model_request` HTTP-status/`wire` fields (WP07). The default provider adapter (`src/providers/mod.rs::LlmResponse`) carries no endpoint-reported model identifier and no adapter parses `"model"` from the response body, so `run_end` records `reported_model: null`; wiring it requires a WP07 transport change that exposes the reported model, not a WP08 change.

Implementation status: WP10 (`--worker-config PATH`, FR-05) is implemented. The reserved `src/worker_runtime.rs` (`WorkerExecutor`, `WorkerRoute`, plus `WorkerConfig`/`WorkerSpec`) and the worker JSON fields (`workers` -> `name`/`command`/`args`/`env`) are live, routing every non-final call to a selected worker or refusing it with no local fallback (`tests/test_worker_isolation.py`). Workers reuse the MCP-stdio client via `mcp_bridge::connect_worker`, launched credential-free through `env -i` (cleared environment plus only the explicit `env` map), honoring the W25 env-clear discipline for the SDK child; no model credential, provider config, or inherited variable reaches a worker (AC-05.1). Discovery (initialize + tools/list) runs before the catalog freeze; tools export as `mcp__{worker}__{tool}` and are gated only by WP06's `run_policy::ToolPolicy::allows` at the agent-loop boundary (worker tools are the caller's sandbox: not path-checked, not refused under `--workspace-read-only`). The reserved exit `22` / `worker_unavailable` is live for required-worker initialization, call, liveness, and teardown failure (process exit and audit `run_end.exit_code` agree; the lost dispatch is recorded under the `worker` route with reason `worker_unavailable` before `run_end`). A malformed/empty control file exits `2` (`run_configuration_invalid`, reasons `worker_config_invalid`/`worker_config_unreadable`). The final-answer tool is intercepted model-side and never forwarded (D133). `--worker-config` is agent-`--print`-only (refused in `--interactive`/`--dry-run`; absent from chat/ACP/session-daemon). Merge items (not in this base): the WP09 capability probe (AC-05.2 "including the probe") and WP13 `save_full_output` propagation to spawned workers/child sessions.
Implemented: WP14 (`--no-follow-symlinks`, FR-12) is landed — independent no-follow reads/walks through `FilesystemAuthority`; the reserved row below is now in force.
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
- Shipped (WP10 r2) signatures/types, reconciling the reservations above: `WorkerExecutor::call(&self, name: &str, arguments: &Value) -> WorkerDispatch` (no `context`) and `WorkerExecutor::shutdown(&self)` (reaps every owned worker child; no return). Public enum `WorkerDispatch { Served { content, is_error }, NoRoute, Aborted(String) }`. `mcp_bridge` exports `WorkerClient`, `WorkerToolDef { name, description, input_schema }`, and `WorkerCallResult { Served { content, is_error }, Down(String) }`. `WorkerConfig::reject_credential_disclosure(&self, credentials: &[String]) -> Result<(), String>` refuses an explicit worker `env`/`args` disclosure of the model credential before launch (exit 2, `run_configuration_invalid`, reason `worker_credential_disclosure`; a `command` containing `=` is likewise refused at parse). The lost-worker terminal is carried by the typed `AgentResult::worker_lost` flag, not by a substring of `error_message`. Teardown reaps each worker's process group (SIGTERM→grace→SIGKILL) so no child/descendant outlives the run.
- Shipped (WP10 r3, D33): the terminal `run_end` record and the process exit code are produced by the single source `agent_runtime::run_terminal_status`. A post-discovery configuration refusal (an allowlist entry served by no worker) is carried as a typed `AgentResult::config_invalid` (`run_configuration_invalid`, exit 2) and recorded as one `run_end` before `exit_on_config_invalid` exits — never a lone `run_start`. The credential-disclosure refusal runs BEFORE `open_run_audit` (no runtime state), so like every exit-2 configuration refusal it leaves no audit file. Worker loss AND worker-initialization failure both map through the shared `worker_failure` classifier, so the audited `run_end.exit_code` always equals the process exit 22 (`worker_unavailable`).
- Shipped (WP10 r4, D38): the model-facing catalog passes the allowlist policy at a single point — in worker mode `agent_cmd::run_agent` builds the full `worker_catalog` from discovery and offers it filtered by `run_policy::ToolPolicy::allows`, so no frontend offers the model a tool the allowlist does not name (tests `test_r4_offered_catalog_equals_allowlist` / `_with_final_output` / `_without_allowlist`). After discovery, EVERY allowlist entry (not only `mcp__…` names) must name a tool of the frozen catalog of the run — the discovered worker tools plus the final-answer tool when `--final-output` is set; an entry that names none (including a native name such as `read_file`, which dispatches to nothing in worker mode) ends the run at startup through the typed `config_invalid` verdict (`run_configuration_invalid` / `tool_allowlist_unknown_entry`, naming the entry; `run_end` 2 under `--audit-path`) before the first model request (`test_r4_native_allowlist_name_in_worker_mode_fails_at_startup`; non-worker runs are unchanged). In worker mode the offered catalog is the ONLY model-facing tool surface: the `execute_script` Starlark signature dump and outbound MCP tools are structurally absent (native tools are not in `worker_catalog`; `agent_mcp` is `None`), and the agent system prompt does not enumerate the run catalog.
- Shipped (WP10 r5, R1): the worker-INITIALIZATION failure that `agent_cmd::run_agent` returns on the `Err` path is now carried as the typed `worker_runtime::WorkerInitError` (Display `worker_unavailable: <detail>`), and `agent_runtime::worker_failure` DOWNCASTS it rather than matching the `worker_unavailable` substring of an arbitrary anyhow message — the `Err`-path analogue of the `Ok`-path typed `worker_lost` flag. The reviewed exit `22` / `worker_unavailable` and the audited `run_end.exit_code` are unchanged; an unrelated error whose text merely contains the token is no longer mis-classified (unit `worker_init_failure_is_typed_not_message_substring`; process-level `test_k3`/`test_missing_worker` unchanged). R2 (doc only): `mcp_bridge::WorkerClient::shutdown`'s reap guarantee is scoped to Linux (off-Linux teardown relies on the SDK `kill_on_drop` of the direct child, matching `docs/configuration.md`). R3 (doc only): `tests/README.md` states the AC-05.2 namespace leg is skipped on hosts without unprivileged user+net namespaces and that a skip is a gate blocker, not a pass.

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

Implementation status (WP15, `dbunk/wp15-tool-contract`): implemented and tested. `src/tool_contract.rs` holds `EffectiveToolCatalog` (`freeze`/`export`/`to_json`) and `ToolContractDocument`; the `daimonos tool-contract` command prints the deterministic dump session-free. The dump reuses the live serving pipeline (`tools::tool_definitions`, `initial_exposed_tools`, the context/confinement filter, `render_list_tool`), so the exported array equals `tools/list` for the same inputs. `CONTRACT_VERSION` is `1.0.0`, reported over MCP initialization in `_meta["io.daimonos/tool-contract-version"]` (stdio and socket) and tracked in `docs/tool-contract-changelog.md`. Note for I: when WP06 merges, apply its `ToolPolicy` filter to both serving and `freeze()` so the dump continues to equal serving; the binary-version contract and WP12 source identity are deliberately untouched here.

Implementation status (WP16, `dbunk/wp16-path-annotations`): implemented and tested. `src/path_annotations.rs` holds one reviewed `path_arguments(tool)` table and `annotate(tool, &mut schema)`; it is applied once in the shared serving pipeline (`tools::tool_definitions`), so the `tool-contract` dump, `tools/list`, and `get_tool_schema` carry identical `x-daimonos-path` marks. It is deliberately NOT applied to the agent/model catalog (`tool_facade::active_schemas_for_policy`): the schemas sent to the model endpoint stay unchanged (FR-15), proven on both wires by `tests/test_path_annotations.py::test_model_request_omits_owned_path_annotation` (I3/D28). In the initial tier the dump serves full schemas for 21 of 35 tools, so it shows marks only there (6 tools with path args); the other 14 tools' marks are read via `get_tool_schema` until WP17 (full schemas), covered by `test_get_tool_schema_carries_marks_for_every_tool_including_on_demand`. The marker is unconditional and additive. It marks scalar path properties and, for arrays, the `items` node (path elements). It is semantic not spelling: opaque command/argv/script strings, agent-mail reservation glob patterns (`reserve_paths`/`release_reservations`/`check_conflicts`.`paths`), URLs, revisions, and caller worker/final-answer schemas are not marked. This additive schema change moved `CONTRACT_VERSION` to `1.1.0` (`docs/tool-contract-changelog.md`). Note for I: `path_annotations::path_arguments` is the reviewed owned-path table, intended as the shared source for ADR-022, but today only the schema annotator reads it — ADR-022 enforcement runs through `path_policy` and does not yet consult it; wire it in before claiming a single source.

Status addendum (r2 coverage): the "stdio and socket" claim is now test-proven on both transports — `tests/test_tool_contract.py::test_contract_version_reported_over_socket_initialization` asserts `_meta["io.daimonos/tool-contract-version"]` over `--mcp-socket`, alongside the stdio assertions. Confinement parity (`--confine-paths`: dump tool names equal `tools/list`, a confinement-refused tool absent from both), the on-demand tier gap (`diff_files` absent from dump and fresh `tools/list`), and the session-free guarantee beyond the workspace (no state/log/analytics artifact in the resolved state directory or an isolated HOME) are each covered. The session-free fix suppresses the secure log file for the `tool-contract` runtime mode in `src/main.rs`.

## Structured final output and controlled requests

WP05 owns `src/structured_output.rs` and the reusable `src/schema_validation.rs` (both new): `FinalOutputSpec`, `FinalOutputState`, and `FinalAttemptFailure`.

> Status (WP05, run wp05-20261117): implemented and wired behind `daimonos agent --final-output`. The model-request ceiling emits reason `turn_limit` in its JSON while using exit code 24 (`model_request_limit`). Acceptance suite `tests/test_structured_output.py` passes on both the Chat Completions and Messages mock wires. Validator dependency `jsonschema =0.60.2` (default features off) added per the human's FR-01 approval.
The `daimonos agent --final-output` control file (`schema`/`schema_path`, `tool_name`, `tool_description`, `corrective_retries`, `max_model_requests`) is the complete FR-01 caller interface; the reserved `reviewer.final_output` configuration group names no additional fields until package-level configuration review.
> Status (WP05, run wp05-20261117-r3): independent-review fixes landed. Every failed ending that contains tool calls (including a single `invalid_arguments` call) now pairs each call with an ordered `is_error` result, so a corrective turn is wire-valid on both endpoints; the acceptance mocks enforce the strict unanswered-call rule (HTTP 400). The `--final-output` control file is validated before the agent env file is read (startup-order step 3), so a config/collision error exits 2 regardless of host agent.env. `--final-output` is refused (exit 2) on interactive, dry-run, chat, ACP, and session-daemon frontends. The no-final-decision fallback reports the run's real `attempts`/`model_requests`. Open reconciliation (R5): the WP09 FR-04 capability probe must call `FinalOutputState::record_request` so the one probe request counts against the same inclusive ceiling.
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
I owns the shared controlled-transport seams in existing `src/providers/mod.rs`. The shipped symbols are `ControlledModelSpec`, `ControlledRequest`, `ControlledProfile`, `ControlledHttp`, `controlled_client`, and `controlled_send` (the earlier reserved name `ControlledTransport` was not used; the transport is realized as `controlled_send` + `ControlledHttp` + `controlled_client`).
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

_WP07 status (implemented, transport only):_ the provider-neutral seams
(`Wire`, `ControlledModelSpec`, `ControlledRequest`, `ControlledProfile`,
`ControlledHttp`, `controlled_client`, `controlled_send`) live in
`src/providers/mod.rs`; the wire-local serializers in `src/providers/anthropic.rs`
(Messages) and `src/providers/openrouter.rs` (Chat); `ControlledProvider` in
`src/providers/router.rs`. The caller flag is `daimonos agent --model-profile PATH`
(`src/cli.rs`, validated in `src/agent_runtime.rs`), mutually exclusive with
`--model`/`--provider`/`--interactive`. It is honored only by the one-shot send
path; combining it with `--interactive` fails closed at startup, because the
interactive TUI routes through the session daemon which carries no profile
(`chat`/`acp` expose no such flag). Caller `request_retries` is bounded by the
named model-request ceiling `CONTROLLED_REQUEST_RETRY_CEILING` (`100`, ADR-025).
Transport-level AC-04.1-04.5 are asserted by
`cargo test providers::controlled_transport_tests` (exact body/headers, the
timeout against a slow endpoint, explicit-proxy routing, the 400-on-forced
`tool_choice` endpoint, and fixed headers under an active tracing provider on
both wires); AC-04.7, the opt-out regression, the fail-closed `--interactive`
refusal, and the ambient-proxy-ignored counting-proxy check by
`tests/test_request_control.py`. `max_tokens` is caller-owned with no built-in
default: mandatory on the `messages` wire (a profile omitting it is refused at
startup) and omitted on the `chat` wire unless the caller set it.
`extra_body_fields` are validated at startup against `PROTECTED_BODY_FIELDS` and
a complete `FORBIDDEN_BODY_FIELDS` denylist (every sampling, forced-choice,
strictness, streaming, and multi-choice parameter on both wires); no allowlist
of permitted extras is defined. The capability probe (AC-04.6)
and audit record remain WP09/WP08; the integrated probe repeats AC-04.1-04.5.

WP09 owns `src/capability_probe.rs` (new): `ProbeSpec` and `ProbeOutcome`.
The caller supplies a minimal valid final payload. Local schema validation occurs before the endpoint call.
The probe offers the byte-equivalent frozen definitions that the review offers. No dummy-tool substitute is allowed.
It never invokes an evidence tool. Any endpoint error, wrong/multiple call, text-only ending, or invalid arguments yields `model_unsupported`.

> Status (WP09, run wp09-20261117): implemented. `src/capability_probe.rs` exposes `ProbeSpec` (the probe's identity/request/credential/payload materials) and `ProbeOutcome`; the agent-only `--capability-probe-payload PATH` flag runs the single unretried probe in `agent_cmd::run_agent` after the run tool catalog is frozen and before the agent loop, offering the exact byte-equivalent `tools` vector (no dummy substitute). Startup refuses the flag (exit 2, naming the option) unless both `--final-output` and `--model-profile` are present, and on `--dry-run` and the non-agent frontends (clap rejects the agent-only flag there). The payload is validated locally via `FinalOutputSpec::classify` on a synthetic final call before any request (invalid → exit 2, zero requests). A pass is exactly one schema-valid final-answer call; any other outcome exits 21 (`model_unsupported`) with no further work. The probe is one HTTP attempt (`request_retries` forced to 0) and its verdict is recorded through `RunAudit::record_probe_result` as one `probe_result` event with no credential value. R5 reconciliation (the WP05 note above): the one probe request now counts against the shared `max_model_requests` ceiling — `agent_cmd` seeds the loop's `FinalOutputState` with it via the new `AgentConfig::prior_model_requests`, so `FinalOutputState::record_request` accounts the probe before the loop's first request (exit 24 when `max_model_requests` is already spent). Verified by `tests/test_capability_probe.py`.

> Status (WP09, run wp09-20261117-r2): integrator review gaps closed. The terminal `run_end` record is now reconciled through the single source `agent_runtime::run_terminal_status`: a failed probe records `status: model_unsupported` / `exit_code: 21` (G1) and a final-output ending records `exit_code` 20 (`structured_output_failed`) or 24 (`model_request_limit`) matching the process exit (G2). The WP07 controlled transport now feeds the audit: `controlled_send` fills an additive `providers::ControlledObservation` (http status, retry index, wire), surfaced to the agent loop through the new defaulted `LlmProvider::take_controlled_observation()` (legacy adapters return `None`), so a loop `model_request` records the observed `http_status`, `retry_index`, and the profile's `wire` (G3). `run_end.reported_model` stays a pending WP07 hook (the controlled response carries no reported model). The probe's one request to the model endpoint is recorded by enriching its single `probe_result` event with `http_status`, `retry_index`, and `wire` (G4, amending decision I-01: no separate `model_request` is emitted for the probe, avoiding a loop-owned `request_sequence` collision). `record_model_request` gained a trailing `wire` field and `record_probe_result` gained trailing `http_status`/`retry_index`/`wire` fields. Verified by `tests/test_capability_probe.py` (G1-G5) and `tests/test_run_audit.py`.

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

> Status (WP09, run wp09-20261117-r4): audit field-set governance (integrator decision D34). Adding a field to an audit record is a backward-compatible change under `audit_version` "1"; `AUDIT_VERSION` is NOT bumped for a field addition (only a field removal or a changed field meaning would require a new version signal). As of this run the complete top-level key set of each record below is, besides the common envelope `audit_version` / `run_id` / `sequence` / `kind`:
> - `model_request`: `request_sequence`, `http_status`, `retry_index`, `probe`, `wire`, `usage`.
> - `probe_result`: `status`, `reason`, `http_status`, `retry_index`, `wire`, `usage`.
> - `probe_result.usage` (WP09, r6): the endpoint-reported token counts of the one probe request (FR-04 req 19, decision D43); same `{input_tokens, output_tokens}` object as `model_request.usage`, defaulting to zero counts when the endpoint reports none. A compatible field addition under `audit_version "1"` (no `AUDIT_VERSION` bump).
> A later change to either field set must update these two lines and `tests/test_capability_probe.py::test_audit_model_request_and_probe_result_field_sets_are_fixed` together.

WP13 extends existing `tool_output::bound_text` with an output policy.
`save_full_output=false` skips full-copy creation and saved-path metadata entirely, including failure and script/pipeline routes.
Truncated responses retain paging or query-narrowing guidance. They never advertise a nonexistent saved file.
User-requested explicit file writes remain ordinary tool operations, not spill output.

_Status (WP13, implemented):_ `--no-output-spill` (global CLI flag) sets `tool_output.save_full_output = false`. `src/tool_output.rs::bound_text`, `reformat_text`, and `microcompact_history` honor it by bounding/pruning in place with `NO_SPILL_MARKER`/`NO_SPILL_PRUNED_MARKER` and never calling `store_full`. Wired for the MCP and agent frontends on `master`. Background/managed-process output (`ManagedBackground::spawn`, `src/ops/exec_ops.rs::{bg,poll,kill}`) also honors it: under no-spill the output is held in a bounded in-memory buffer, `bg` emits no `log` path, and `poll` returns the same bounded tail via `ManagedBackground::read_tail` (WP10 worker routing must still propagate `save_full_output` to spawned child sessions when built — see WP13 handoff). Script/pipeline/batch aggregation routes go through the same `bound_text`/`bounded_text_tool_result` seam. Spellings `--no-output-spill` and `save_full_output` are now in use.

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

_Status (WP18, implemented; state after the code review of 2026-11-23):_

- **Options.** `--tool-timeout-ms N`, `--tool-input-bytes N`, and `--tool-max-entries N` are global, default-off options. `0` is refused at startup with exit code `2`. `--tool-max-entries` is not in the reviewed row above: the name is **(proposed)** (integrator decision D56, owner decision open).
- **Codes.** A deadline hit returns `tool_timeout`. A byte or entry hit returns `tool_input_limit` and names the limit, the configured value, and the consumed value. An entry hit has no code of its own (D56). A tool that cannot enforce a requested input or entry budget is refused with the reviewed code `tool_policy_unsupported` (D60). WP18 adds no refusal code.
- **Admission under an input or entry budget.** `tool_budget::tool_input_budget_admitted` is an explicit list. A tool is admitted only when it meters its input (`read_file`, `ls`, `search` in content mode, `git_read`) or reads no workspace or external content (write-only tools, composites, session state, tool-catalog introspection, agent mail, reservations, plan state). Each other tool name is refused, also an outbound MCP tool, a tool from configuration, and a name that a later package adds. `search` in files mode (the index) is refused: the index does not report the content that it reads.
- **Deadline.** One scope in `mcp::dispatch_tool_inner` covers each registry tool. Native readers check the deadline at work boundaries. A subprocess of `exec`, `git_read`, or a plugin is terminated with its process group through `ManagedChild::terminate`. `tool_pipeline` is refused under each budget option. A run with `--worker-config` and a budget option is refused at startup (`run_configuration_invalid`, reason `tool_budget_worker_unenforceable`).
- **Metering.** `search` in content mode charges the bytes that it scans and stops during the scan. `ls` charges each entry during the walk and stops at the limit. `read_file` checks the file size before the read and charges the content. `git_read` charges the bytes of the content that it returns, after the subprocess ended: this is an approximation of the git content read (D61).
- **Composites.** `batch` and `execute_script` share one budget with their children. `execute_script` holds the budget in its script context and sets it on the session only inside each locked inner operation.
- **Audit.** A limit hit writes the reserved `limit_hit` event. The budget code records the hit on the session, and the run loop reads that record. It does not parse the result text of a tool.
- **Names.** `ToolBudget` with `check_deadline()`, `charge_input(bytes)`, `charge_entries(n)`, and `remaining()`. The reserved names `BudgetReceipt` and `LimitFailure` are not used: `path_policy::ToolFailure` carries a failure (D61).
- **Open.** No process test runs the chat or ACP frontend with a budget option. The `tool-contract` dump does not name the budget options.
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

Exit 2 includes detailed reason codes: `state_unusable`, `state_overlaps_workspace`, `workspace_write_forbidden`, `state_mode_unsupported`, `unknown_tool`, `tool_name_collision`,
`filesystem_policy_unsupported`, `offline_profile_incompatible`, `model_profile_incomplete`, and `schema_unusable` before a request.
WP09 (capability probe) adds three exit-2 reasons under the same `run_configuration_invalid` code: `capability_probe_requires_final_output` and `capability_probe_requires_model_profile` (the probe flag is present but `--final-output` / `--model-profile` is missing), and `capability_probe_payload_invalid` (the caller's minimal payload is an unreadable file, invalid JSON, or fails the final-answer schema — refused locally before any request).
WP11 (FR-09, ADR-021) adds exit-2 reason `config_invalid`: the explicit-only loader (`--ignore-config-files`) emits it for an unreadable or invalid explicit `--config` and selects no other source.
`state_mode_unsupported` is a temporary WP01 refusal for frontends whose state is not yet relocated. WP02 removes that limitation as it adds support.
`workspace_write_forbidden` is the WP04 startup refusal when an internal writer's destination resolves inside the workspace under `--workspace-read-only`; it reuses the one FR-07 spelling for both the startup (exit-2) reason and the tool-refusal code below, so operators see a single name for every prohibited workspace write.
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

Implementation status (WP17, `dbunk/wp17-full-schemas`): implemented and tested. The global `--full-tool-schemas` flag enables the opt-in full-schema mode (equivalent to the existing `[mcp] full_tool_schemas` / `DAIMONOS_MCP_FULL_SCHEMAS` seams, set in `src/main.rs` after config load). `src/schema_contract.rs` (new) holds `ValidatedToolSchema::validate`/`validate_catalog`, the offline static gate (ADR-030): it reuses WP05's `src/schema_validation.rs` `CompiledSchema` for external-reference and non-2020-12 refusal, adds local recursive-`$ref`-cycle detection and cross-tool `$defs`/`definitions` name-collision detection, and maps every failure to run reason `schema_unusable`. `tool_contract::validate_self_contained` runs the gate over the frozen catalog inside `EffectiveToolCatalog::freeze` when full mode is on (so the `tool-contract` dump refuses an unusable schema), and `agent_runtime::enforce_full_schema_contract` runs it over the model-facing catalog before the first request on the agent/chat/ACP/session-daemon frontends. The MCP server already serves full schemas via `render_list_tool`; the session daemon carries the mode in its launch identity (`src/session_bootstrap.rs` `daemon_args` + `bootstrap_fingerprint`). Default off leaves terse/lazy serving unchanged. AC-15.1 is proven by `tests/test_full_schemas.py` (every full-dump schema validates against 2020-12 — authoritatively by the binary gate, a zero exit being the pass — and has no external `$ref`; the owned-path marks now appear in the dump for every tool; opt-out regression; MCP/agent/help frontend coverage) plus the Rust seam tests in `schema_contract.rs`, `tool_contract.rs`, and `session_bootstrap.rs`. Note for I: `CONTRACT_VERSION` is deliberately NOT bumped — WP17 changes no tool name/argument/schema/result shape, only adds a serving control and a validation gate over schemas the config/env mode already produced (`docs/tool-contract-changelog.md` Notes). Endpoint-only rejection (`model_unsupported`) is a WP09 merge item; the verify command runs without `tests/test_capability_probe.py` on this base. r6 (review R2, integrator D53): the global `--full-tool-schemas` flag now wins over `DAIMONOS_MCP_FULL_SCHEMAS` for every value — a false-y variable no longer silently masks the flag — via an additive `#[serde(skip)]` `McpConfig::full_tool_schemas_forced` set in `src/main.rs` and read first in `config::effective_full_tool_schemas` (the single chokepoint the dump, MCP server, model-running frontends, and the session-daemon launch identity all consume); without the flag the variable still wins over the config key. The static-analysis step budget (10000000) is a fixed internal guard, not a configuration key.

Implementation status (WP17 r3, `dbunk/wp17-full-schemas`): the recursion detector in `src/schema_contract.rs` was upgraded from a `#`/`#/`-prefix string check to proper Draft 2020-12 URI/anchor resolution (ADR-030 "proper resource semantics, not a string-prefix check"). It now resolves and detects cycles through every in-document reference form: a JSON-pointer `$ref` with correct decoding (percent-decode, then `~1`/`~0`), a plain-name `$anchor`, a `$dynamicRef` to a `$dynamicAnchor`, and a `$ref` that names the document's or an embedded resource's `$id` (absolute or relative to the base URI, with or without a fragment); a diamond (two references to one definition) stays linear. Per integrator decision D41 (fail closed) a reference the WP05 validator accepted that the detector cannot resolve inside the document is refused `schema_unusable` with the reference named, never fetched (new error `SchemaContractError::UnresolvableReference`). The same detector runs for owned and supplied schemas, so T3 adds both-wire process tests (`tests/test_full_schemas.py`) for a supplied schema recursive through `$anchor` and through its own `$id` (exit 20, `schema_unusable`, final-output error shape, zero model requests); S1/S2 are unchanged. No new dependency and no public run-reason/exit-code change. Note for the owner (open point): forwarded/outbound MCP tool schemas join the model-facing catalog without this static gate (runtime-fetched after the native gate); unchanged by r3.

Implementation status (WP17 r4, `dbunk/wp17-full-schemas`): the `src/schema_contract.rs` detector now walks the document with TYPED traversal for both the identifier index and the cycle search (integrator decision D46). A reference keyword (`$ref`/`$dynamicRef`) is followed only in a schema position; `const`/`enum`/`default`/`examples` hold data and are never descended or indexed (fixes the false refusal of a schema whose data merely looks like a `$ref`); a single-schema, array-of-schemas, or name->schema-map keyword is descended by its 2020-12 shape; and every other (unknown) keyword is walked conservatively (fail closed, so a cycle under it is still refused). The static index never replaces an entry: a second `$id` resolving to one URI, or a second anchor with one base URI and name, at a different location is refused `schema_unusable` and named (new error `SchemaContractError::DuplicateIdentifier`) — this closes the false accept where a decoy `$id`/anchor in data masked a real recursive resource. A property or definition whose NAME equals a data keyword is still a schema position and still refused when recursive (the name inside a name->schema map is never a keyword). No new dependency, no public run-reason/exit-code change (`DuplicateIdentifier` maps to the existing `schema_unusable`), and no change to the WP05 validator, `AGENTS.md`, or the forwarded-MCP path. Proven by `tests/test_full_schemas.py` r4 both-wire process cases and the `src/schema_contract.rs` V1–V4 unit tests (including a cycle-through-every-2020-12-schema-keyword table).

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

Implementation status: the WP11 / FR-09 `--ignore-config-files` row is implemented as the reviewed spelling — a global, default-off option that disables D01–D09 while honoring explicit `--config`/`--agent-env`/`--agent-instructions`, pinned into the session-daemon launch identity, with a startup effective-policy report. See `src/discovery_policy.rs` and `tests/test_configuration_isolation.py`. The index warm-up preflight (`src/index.rs::should_warm_index`/`preflight_within_budget`) is threaded with the discovery policy, and instruction (D04) suppression is observed at the process boundary on the recorded model request to a local fake provider (see `tests/test_configuration_isolation.py`).

_Anchors:_ src/main.rs::main, src/provisioning.rs::build_tool_services, src/session.rs::Session::resolve_path, src/providers/mod.rs::CompleteOpts, src/managed_process.rs::ManagedChild::terminate, src/agent.rs::AgentSession::staged_prompt_message, src/mcp.rs::dispatch_local_tool, src/ops/mod.rs::dispatch_with_progress, src/tool_output.rs::bound_text

Implementation status: WP06 (`--tool-allowlist PATH`, FR-02) is implemented. `src/run_policy.rs::ToolPolicy` (`Option<BTreeSet<String>>`: absent = legacy, `Some(empty)` = deny-all) is the effective policy consulted by name at every listing and dispatch boundary: `mcp::handle_list_tools_request` and the `list_all_tools` handler (model sees exactly the allowlist), `mcp::dispatch_tool_inner` (direct MCP + batch children + Starlark non-opcode), the `batch`/`execute_script` special cases, `script::dispatch_tool_by_name` (Starlark opcode + generic `tool()`), and the agent loop's own dispatch chokepoint (opcode tools bypass `dispatch_tool_inner`). Unknown entries fail at startup with exit-2 `run_configuration_invalid` reason `tool_allowlist_unknown_entry` (the final-answer tool must be listed explicitly). Refusals carry `tool_not_allowed` (already in `run_audit::REFUSAL_CODES`, recorded as `tool_refusal`). Caller-owned (`#[serde(skip)]`), never widened by workspace/env config, and part of the session-daemon launch identity (`session_bootstrap.rs`). Read-only/confinement admissions remain stronger and may still refuse an otherwise-listed tool.

Implementation status (WP06, r3 review fixes): the model-facing catalog is now filtered at a single source. `tool_facade::active_schemas_for_policy` takes the `ToolPolicy` and returns only allowlisted tools, so `agent --print`, `daimonos chat`, `daimonos acp` (`agent_tools`), and the session daemon (`session_factory`) all offer the model exactly the allowlist with no per-frontend `retain` (the retain in `agent_cmd.rs` is removed). Forwarded/outbound MCP tools offered to the model are filtered by the same policy. The `execute_script` tool *description*'s embedded Starlark signature block uses `script::tool_signatures_filtered` under the policy, so it never names an unlisted tool (ADR-023 served==dispatched, R1/R2). The `effective_tool_allowlist` report scope is the allowlist intersected with the agent-env `allowed_commands` only; `denied_commands`/permission-mode/approval are enforced at dispatch but not folded into the report (r3 decision; a session-free full-gate evaluation is not a small change). Collision/name-gathering callers of `active_schemas_for_policy` (ACP native-name sets, final-output name-collision check) pass an unrestricted policy by design — they must see the full native catalog. Caller-owned worker names in the startup known-catalog remain deferred to WP10.

Implementation status: WP06 (r2 review fixes) extends the above. The explicit policy now also filters schema lookup (`get_tool_schema`), the Starlark signature dump (`list_tool_signatures`), the MCP server-instructions signature section, the session-daemon socket listing, and the agent-print offered catalog — so the served, discovered, probed, dumped, and dispatched sets agree (ADR-023). With `--final-output`, an allowlist that omits the designated final-answer tool fails at startup with exit-2 `run_configuration_invalid` reason `tool_allowlist_missing_final_output`; the name is never added automatically (PM Q-FR-02-2). The reported `effective_tool_allowlist` is the intersection with the run's active legacy restriction (the agent-env `allowed_commands`), i.e. the set the run will actually permit. Config-registered custom local tools (`[tools.*]`) are part of the known catalog and may be allowlisted.

Implementation status: WP06 (r4 review fixes) extends the above. Exit-2 taxonomy addition: a malformed or unreadable `--tool-allowlist` file fails at startup with `run_configuration_invalid` reason `tool_allowlist_invalid` (joining `tool_allowlist_unknown_entry` and `tool_allowlist_missing_final_output` as the three reserved WP06 exit-2 reasons; `tests/test_tool_allowlist.py::test_malformed_allowlist_file_fails_at_startup` and `::test_unreadable_allowlist_file_fails_at_startup`). Delegation (ADR-023 R1, PM Q-FR-02-1): the composite opcodes `tool_pipeline` and `lint_repair` (and the raw-protocol `tool_run`) now check their inner `tool_id` against the policy (`src/ops/tool_ops.rs` → `session.policy.admit(tool_id)`) before `ToolRegistry::pipeline`/`::repair`/`::run`, so a listed composite cannot reach an unlisted registered tool; the refused inner tool never runs (`tests/test_tool_allowlist.py::test_tool_pipeline_inner_tool_not_on_allowlist_is_refused`, `::test_lint_repair_inner_tool_not_on_allowlist_is_refused`, and the `ops::tool_ops` unit tests). In this package the allowlist names only native and config-registered `[tools.*]` tools; remote/worker names defer to WP10, and an active allowlist therefore removes every forwarded MCP tool from the catalog.

