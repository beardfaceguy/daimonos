# G0 reviewer-runtime inventories

Date: 2026-10-29
Status: Source inventory complete for review. Enforcement is not implemented.
Base: `859194c8cc3aa736e6539db14af6bd370f325e7c`.
Authority: Accepted ADRs 021–032 and the sealed PM inputs in the [package plan](../../handoffs/2026-10-28-dbunk-remaining-work.md).
Contracts: [INTERFACES.md](INTERFACES.md). Public names and mechanisms there await G0 review.

## Scope and evidence

The inventory covers the inspected first-party startup, service, tool, provider, and shutdown paths.
Pattern scans also include test fixtures. The rows below identify production consumers, not the raw match count.
Private scan artifacts live under `~/.daimonos/agentic_work_logs/dbunk-integrator/artifacts/`.
This is not proof that an arbitrary child process, plugin, or remote server has no additional effects.
A new or unclassified consumer must refuse a restricted profile until its owner inventories and tests it.

Use these disposition terms:

- **Relocate:** use the explicit state authority. Never choose HOME, cwd, or temporary storage as a fallback.
- **Disable:** do not initialize the producer when the relevant option prohibits it.
- **Authorize:** apply the shared filesystem or tool policy before any operation.
- **Refuse:** return the named unsupported-operation error before effects begin.

## Internal writer and supporting-reader inventory

The explicit state profile must cover every enabled writer below, including temporary and SQLite sidecar files.
Workspace-read-only mode separately prohibits effects under the evidence workspace.
Explicit user file-edit operations are not internal state. They remain governed by the filesystem and tool policies.

| ID | Producer and opened source anchor | Current destination or trigger | Required restricted-profile disposition | Owner |
|----|-----------------------------------|--------------------------------|-----------------------------------------|-------|
| W01 | `src/main.rs::debug_tokens_log_path` | Optional --debug-tokens startup creates HOME `.config/daimonos/token-debug.log` | Resolve under state before this early helper, or refuse the incompatible request. | WP01 / I |
| W02 | `src/index.rs::WorkspaceIndex::new` | In-memory index and filesystem watcher. No production index-cache file was found. | No disk relocation is required for this constructor. Mediate its reads and walks, and disable prohibited discovery. | WP01, WP03 / I |
| W03 | `src/snapshot.rs::SnapshotStore::create` | Workspace snapshot tree, copied files, and manifest | Relocate internal snapshot storage. Restore remains an explicit mutation governed by read-only policy. | WP01 / I |
| W04 | `src/analytics.rs::AnalyticsStore::new` | SQLite at the configured analytics path | Relocate the DB and its WAL/SHM files. Do not construct it when collection is disabled. | WP01, WP20 / I |
| W05 | `src/coordination/store.rs::CoordinationStore::open_with` | Coordination database and schema writes | Relocate database, locks, journals, and maintenance writes. Read queries can still initialize storage. | WP01 / I |
| W06 | `src/pipeline_cache.rs::PipelineCache::with_config_watching` | In-memory result cache and watcher. No on-disk pipeline cache was found. | No invented cache directory. Mediate watcher reads, cached-result policy, and accounting. | WP01, WP18 / I |
| W07 | `src/kgl/store.rs::KglStore::open_workspace_with` | Workspace `.kgl` store; constructors create directories and tables | Relocate all KGL tables and sidecars. Do not treat a query as a write-free constructor. | WP01 / I |
| W08 | `src/kgl/autoindex.rs::spawn_watcher` | Starts a watcher that updates the KGL store | Thread the relocated store through watch startup and teardown. Disable discovery-triggered work when prohibited. | WP01, WP11 / I |
| W09 | `src/kgl/observe.rs::record_file_op` | Opens the KGL store after file-tool calls | Use the same relocated store. No hidden workspace write from a successful read. | WP01 / I |
| W10 | `src/logging.rs::init` | Rolling local log files from logging config | Resolve under state before logging initialization. Never emit diagnostics to protocol stdout. | WP01 / I |
| W11 | `src/providers/request_trace.rs::write_with_limit` | Explicit structural request-trace directory, lock, and rotation files | Validate under state or refuse the override. Disable tracing in controlled model transport. | WP02, WP07 / I |
| W12 | `src/tool_output.rs::store_full` | Configured spill path, HOME `.daimonos/tool-output`, or temporary fallback | Relocate by default. With no-spill enabled, never open or write the full copy. | WP01, WP13 / I |
| W13 | `src/plugins/git_read/status.rs::StatusView::create` | Temporary copied git metadata for non-mutating status | Relocate private scratch data. Preserve the existing no-mutation and no-network behavior. | WP01, WP04 / I |
| W14 | `src/session_bootstrap.rs::connect_or_spawn` | Starts a daemon that later opens socket, catalog, and history paths | Propagate authority across spawn and fallback paths. No direct environment-file writer was found in this module. | WP02 / I |
| W15 | `src/agent_runtime.rs::run_agent` | Agent history and optional token-log output | Relocate histories and explicit internal log destinations. Refuse out-of-state overrides before startup. | WP02 / I |
| W16 | `src/agent_runtime.rs::run_chat` | Chat histories, checkpoints, and token logs | Use the same run authority as agent and ACP modes. | WP02 / I |
| W17 | `src/agent_runtime.rs::run_acp` | HOME-derived ACP sessions and analytics initialization | Relocate before opening stores. Keep writer epochs, journals, and durable checkpoints intact. | WP02 / I |
| W18 | `src/session_store.rs::SessionStore::write_record` | Session SQLite DB, transaction journals, and migration files | Keep the complete store tree under state. Preserve crash-recovery ordering from the merged repair. | WP02 / I |
| W19 | `src/agent_runtime.rs::run_session_daemon` | Daemon histories, catalog, and Unix socket | Relocate stores and socket paths. Apply the same rule to explicit internal socket/catalog overrides. | WP02 / I |
| W20 | `src/session_catalog.rs::SessionCatalog::open` | Session catalog schema and sidecars | Relocate. Discovery fallback must not reopen the original HOME location. | WP02 / I |
| W21 | `src/checkpoint.rs::CheckpointStore::init` | Workspace `.daimonos/checkpoints.git` shadow Git repository | Relocate its metadata and Git scratch state without changing checkpoint semantics. | WP02 / I |
| W22 | `src/evidence.rs::Worktree::new` | HOME evidence snapshots or temporary fallback | Relocate per-turn repositories and git scratch state. Preserve cleanup behavior. | WP02 / I |
| W23 | `src/mcp_oauth.rs::write_credential` | HOME `.blue_rose/mcp-oauth`, private temp file, atomic replacement | Relocate explicitly enabled OAuth storage or refuse it. No default HOME credential access under explicit state. | WP01 / I |
| W24 | `src/provisioning.rs::build_tool_services` | Plugin construction, index warming, cache watchers, and analytics attachment | Apply authority and producer admission before provisioning. No first-party LSP manager exists in this checkout. | WP01, WP04, WP20 / I |
| W25 | `src/managed_process.rs::apply_environment` | Child HOME/XDG/TMP variables and per-call overlays | Restricted children receive authorized state/temp locations only. Caller overlays cannot restore forbidden destinations. | WP04, WP10, WP20 / I |
| W26 | `src/session.rs::Session::resolve_path` | User-requested file writes and process cwd | Authorize through the filesystem policy. Never reroute an explicit evidence write into state and report it as success. | WP03, WP04, WP14 / I |

| W27 | `src/managed_process.rs::create_artifact` | Background-process output under HOME `.daimonos/process-output`, explicit process path, or temp fallback | Relocate before launch. Include failed-spawn cleanup and no-spill admission where applicable. | WP01, WP04, WP13 / I |

Each constructor's owner must test its direct path and every frontend that calls it.
WAL/SHM files, rotation files, locks, removals, migrations, and failed-operation cleanup count as writes.
The evidence-folder growth issue is a separate deferred task. This work relocates that storage but does not add retention policy.

## Discovery inventory

| ID | Source anchor | Implicit input today | Explicit-state profile | Ignore-configuration profile | Owner |
|----|---------------|----------------------|------------------------|------------------------------|-------|
| D01 | `src/env_file.rs::load_default` | Workspace and HOME/XDG `agent.env` | No HOME fallback | Do not open implicit files or inject their variables | WP11 / I |
| D02 | `src/config.rs::search_candidates` | Explicit config, workspace TOML, HOME/XDG config | No HOME candidate | Only the caller's explicit config path is eligible | WP01, WP11 / I |
| D03 | `src/agent_runtime.rs::load_agent_env` | Explicit or default agent configuration | No default HOME file | Require caller inputs for configured behavior. Preserve ordinary defaults when the option is off. | WP02, WP11 / I |
| D04 | `src/prompts.rs::load_agent_instructions` | Global and workspace instructions | No global HOME instructions | Only an explicit instruction path or caller-supplied text | WP11 / I |
| D05 | `src/skills.rs::discover` | Global and workspace skill roots | No global HOME discovery | No implicit skills or executable registrations | WP02, WP11 / I |
| D06 | `src/agent_mcp.rs::load_configured_specs` | Configured files plus default Zed MCP settings | No default HOME/Zed file | Only explicitly selected server definitions | WP11, WP10 / I |
| D07 | `src/zed_config.rs::context_server_specs` | HOME/XDG Zed settings and server entries | No automatic settings read | Do not open the default settings file | WP11 / I |
| D08 | `src/ops/file_ops.rs::grep` | Ignore-file behavior during evidence walks | Not a state-path decision | Do not let implicit ignore files alter tool behavior | WP11, WP14 / I |
| D09 | `src/config.rs::register_tools` | File-defined CLI plugins | Explicit trusted configuration only | No workspace/user plugin registration | WP11, WP06 / I |
| D10 | `src/providers/openrouter.rs::list_models` | Live model discovery after provider construction | Not disabled merely by relocating state | Controlled transport and offline profiles must separately prohibit hidden discovery | WP07, WP20 / I |

Ignoring configuration must happen before environment loading, service provisioning, or tool registration.
The startup report names recognized ignored candidates without opening their content to produce the report.
Package tests must place hostile fixtures at every recognized candidate location.
Explicit caller control files are not untrusted evidence paths. Their authority and allowed state writes remain separate.

## Network producer inventory

The offline-tools ceiling applies to the entire process, including construction, discovery, polling, failure handling, and shutdown.
A model-side process with network access is separate from an offline tool-side process.
Local analytics collection and remote telemetry export have different controls.

| ID | Producer and opened source anchor | Possible network or delegated effect | Required offline-tools behavior | Owner |
|----|-----------------------------------|--------------------------------------|----------------------------------|-------|
| N01 | `src/providers/openrouter.rs::OpenRouterProvider::complete` | Chat Completions model request | Refuse a networked provider before construction. Controlled model-side transport remains outside this process profile. | WP07, WP20 / I |
| N02 | `src/providers/openrouter.rs::OpenRouterProvider::fetch_catalog` | Live model/context discovery | No catalog fetch, fallback discovery, or DNS attempt | WP20 / I |
| N03 | `src/providers/anthropic.rs::AnthropicProvider::complete` | Messages model request | Refuse in the offline process | WP20 / I |
| N04 | `src/providers/openai.rs::OpenAiProvider::complete` | Native Responses request | Refuse in the offline process. Preserve the native adapter when offline mode is off. | WP20 / I |
| N05 | `src/providers/mod.rs::ProviderTimeouts::client` | HTTP client construction and proxy behavior | No offline consumer may instantiate an unrestricted client and rely on later call filtering | WP20 / I |
| N06 | `src/agent_runtime.rs::discover_models` | Startup provider probing and discovery | Disable before the factory/probe path. An incompatible explicit request fails startup. | WP20 / I |
| N07 | `src/observability.rs::ObservabilityRuntime::initialize` | OTLP exporter, background batches, flush, and shutdown | Never construct an exporter. No queued upload or shutdown flush may exist. | WP20 / I |
| N08 | `src/analytics.rs::AnalyticsStore::new` | Local collection, not an identified upload path | Explicit analytics-off skips store creation and event production. Do not confuse it with disabling OTLP. | WP20 / I |
| N09 | `src/mcp_bridge.rs::create_client_with_proxy` | MCP HTTP/SSE endpoint and implicit transport behavior | Refuse IP transports before client initialization | WP20 / I |
| N10 | `src/mcp_bridge.rs::connect` | OAuth proxy startup during bridge initialization | Do not start the proxy or contact its upstream | WP20 / I |
| N11 | `src/mcp_oauth/login.rs::discover` | OAuth metadata probes, challenge handling, token endpoints | Refuse login/refresh/discovery in the offline process | WP20 / I |
| N12 | `src/mcp_oauth/proxy.rs::start_proxy` | Loopback listener and upstream requests | Refuse. Loopback HTTP is still IP networking, not a permitted Unix IPC exception. | WP20 / I |
| N13 | `src/plugins/discord.rs::DiscordPlugin::new` | Discord API requests and possible pagination | Refuse remote tools before client construction | WP20 / I |
| N14 | `src/remote_gateway.rs::serve` | HTTP/WebSocket gateway listening and request handling | Refuse the IP gateway profile. Ordinary local stdio/Unix operation remains separate. | WP20 / I |
| N15 | `src/managed_process.rs::run` | CLI tools can fetch, upload, resolve names, or spawn network-capable descendants | A scrubbed environment is insufficient. Refuse unsupported producers or use a demonstrated supported isolation path. | WP04, WP20 / I |
| N16 | `src/tool_runner.rs::ToolPlugin::run_command_with_config` | Configured tools and arbitrary user binaries | Refuse unclassified tools in restricted profiles | WP04, WP20 / I |
| N17 | `src/mcp_bridge.rs::create_client_with_proxy` | Stdio server commands can install packages or use the network | Do not infer offline safety from stdio transport. Require explicit admission and demonstrated child isolation, or refuse. | WP10, WP20 / I |
| N18 | `src/plugins/mod.rs::register_builtin_plugins` | Plugin availability probes can execute external programs during startup | Decide restricted-profile eligibility before probing an arbitrary binary. Do not certify startup from later per-call refusal. | WP04, WP20 / I |
| N19 | `src/plugins/git_read/runner.rs::ReadRepo::output` | Git promisor/lazy-fetch and helper behavior | Reuse existing hardening. Re-run missing-object and unreachable-remote tests under attempt observation. | WP20 / I |

| N20 | `src/tool_output.rs::reformat_text` | Optional output-reformat model call, separate from the main agent request | Refuse under offline-tools before any call. Controlled mode must mediate this request or refuse it without a legacy fallback. | WP07, WP20 / I |
| N21 | `src/agent.rs::AgentSession::compact` | Automatic summary model call with its own retry loop | Gate before sending. Controlled request accounting and retry rules cover every HTTP attempt. Offline mode permits no auxiliary request. | WP07, WP20 / I |

N15 includes the git write/raw plugin, Cargo, npm, gh, curl, Docker, pytest plugins, and shellcheck wrappers.
Their external programs are not certified safe merely because a particular subcommand sounds read-only.
Batch, pipeline, and Starlark calls inherit the same admissions and cannot bypass the ceiling.
No independent updater/downloader service appeared in the inspected first-party path. Do not infer that external programs have none.

## Dispatch and tool-eligibility inventory

| Boundary or group | Source anchor | Required contract |
|-------------------|---------------|-------------------|
| MCP direct calls and activation | `src/mcp.rs::dispatch_local_tool` | Apply effective policy before activation, use tracking, or dispatch |
| Opcode direct calls and every batch child | `src/ops/mod.rs::dispatch_with_progress` | Authorize every child before effects. The outer batch name grants nothing to a child. |
| Starlark builtins and `tool()` | `src/script.rs::dispatch_tool_by_name` | Use the same policy, worker route, and inherited budget |
| Pipeline stages and cached results | `src/ops/tool_ops.rs::tool_pipeline` | Check admissions before stage execution or cache reuse |
| Model-facing discovery | `src/tools.rs::all_tools` | Export only the effective catalog, including lazy and full-schema paths |
| CLI plugins | `src/tool_runner.rs::ToolRegistry::run` | Preserve policy and process ownership at this lower boundary |
| Remote and worker calls | `src/agent_mcp.rs::AgentMcp::dispatch_hook` | No local fallback. Preserve exported names and record the serving route. |
| Terminal model output | `src/agent.rs::AgentSession::staged_prompt_message` | Validate on the model side. Never forward the terminal call to a worker. |

| Tool class | With confinement or workspace-read-only enabled | With no-follow enabled | With tool budgets enabled |
|------------|-----------------------------------------------|------------------------|---------------------------|
| Native file reads, search, listing, metadata | Admit only through shared filesystem handles and confined walks | Report link metadata and never traverse links | Cooperative deadline and logical-byte accounting required |
| `git_read` | Inventory repo, git directories, alternates, and scratch paths. Do not rely solely on its workspace-level canonical check. | Reject unsupported link-sensitive paths rather than bypassing the option | Deadline can use owned process teardown. Refuse an operation whose logical input cannot be accounted. |
| File mutations and `set_cwd` | Deny workspace mutations. A changed cwd never changes the fixed confinement root. | Apply independent no-follow checks | A budget does not grant mutation authority |
| `batch` | Admit only after every child's policy enforcement exists | Children use the same filesystem authority | Children share the parent deadline and remaining bytes |
| `execute_script`, arbitrary exec, repair helpers, external plugins | Refuse when their effects cannot be mediated. An explicit allowlist entry cannot override this refusal. | Do not imply a sandbox from pathname checks alone | Refuse unsupported accounting or teardown |
| `tool_pipeline` and `diff_files` | Admit only stages/reads that use the shared boundaries | All path inputs and internal opens are covered | Charge cached logical input. No fresh child budget. |
| Coordination, session stats, KGL, and skill loading | Internal effects require relocated state. Evidence/config reads still require authority. | Do not exempt hidden reads | Prove support or refuse the affected operation |
| Remote/caller workers | Admission follows explicit worker and policy contracts, not generic read-only labels | Do not claim host-level confinement for an opaque remote server | Refuse unsupported logical-byte accounting or non-terminable deadlines |
| Schema/introspection helpers | No implicit server discovery. Return only the frozen effective catalog. | Mark actual path arguments, not free-text values | Bound contract work and perform no evidence scan |

## Verification ownership

- WP01/WP02 tests replace HOME/XDG with unwritable paths and require all enabled internal writes to stay under explicit state.
- WP03/WP04/WP14 test linked roots, ancestor substitution, hostile walks, linked nonexistent write targets, and unsupported tool refusal.
- WP06 tests direct, batch, script, pipeline, lazy activation, plugins, and remote routes against the same effective policy.
- WP18/WP19 test cooperative stops, process-group teardown, cached/composite accounting, and complete digest failure when the budget is insufficient.
- WP20 tests the full admitted read catalog while observing DNS, socket/connect, and child attempts from startup through shutdown.
- Observation must demonstrate zero attempted outbound IP activity, not just failed requests under a disconnected network.
- A source row with no test mapping remains an integration blocker. New consumers update this inventory in the same change.

_Anchors:_ src/provisioning.rs::build_tool_services, src/session_bootstrap.rs::connect_or_spawn, src/agent_runtime.rs::run_acp, src/session_store.rs::SessionStore::write_record, src/providers/request_trace.rs::write_with_limit, src/mcp_bridge.rs::create_client_with_proxy, src/observability.rs::ObservabilityRuntime::initialize, src/ops/mod.rs::dispatch_with_progress, src/managed_process.rs::run
