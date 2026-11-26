# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- The local container accepts a caller-selected workspace and MCP arguments. `daimonos --version` reports its package, source revision, and independent tool-contract version.
- `--read-sha256` includes complete stored-byte digests on filesystem and pinned Git reads, including paged and bounded results.
- `--offline-tools` serves four local evidence tools over MCP stdio or a Unix socket. It refuses network-capable producers and subprocess tools.
- `--analytics-off` disables local analytics collection for a run and its session daemon. It leaves an explicitly requested local audit active.

## [0.1.8] - 2026-11-24

### Security

- **The agent frontends do not load the workspace `daimonos.toml`** (ADR-039).
  `agent`, `chat`, `acp`, and the session daemon took their configuration from
  `<workspace>/daimonos.toml` when it existed, before the user file. That file
  comes with the workspace, which can be a repository of another party, and
  the configuration names what an agent run starts and where its data goes.
  These modes now load the file that `--config` names, else
  `~/.config/daimonos/config.toml`, else the defaults. Startup prints one line
  when a workspace file exists and is not loaded. To use the workspace file,
  pass `--config <workspace>/daimonos.toml`. The MCP server modes are
  unchanged. Versions up to 0.1.7 are affected.

## [0.1.7] - 2026-11-24

This release has the reviewer-runtime options (#282), the agent env file and
ACP signal fixes (#288), and a security fix for the workspace `agent.env`.

### Security

- **The workspace `agent.env` cannot set connection, approval, or network
  settings** (ADR-038). `<workspace>/agent.env` comes with the workspace, which
  can be a repository of another party, and it ranked above the user file. It
  could set the provider endpoint that receives the requests and the API key
  of the user file, the approval mode and tool lists, and the proxy of the HTTP
  client. These lines are now ignored and reported on stderr:
  `DAIMONOS_AGENT_PROVIDER`, each `DAIMONOS_AGENT_*_BASE_URL` and
  `DAIMONOS_AGENT_*_API_KEY`, `DAIMONOS_AGENT_ENV`, the approval settings, the
  proxy and certificate variables, and the user directory variables (`HOME`,
  `XDG_*_HOME`). Set them in
  `~/.config/daimonos/agent.env`, in the file that `--agent-env` names, or in
  the real environment. Versions up to 0.1.6 are affected.

### Added

- **Cooperative tool budgets (FR-19, ADR-031).** Three default-off, global
  options limit one top-level tool call: `--tool-timeout-ms N` (a deadline),
  `--tool-input-bytes N` (a limit for the file and git content that the call
  reads or scans), and `--tool-max-entries N` (a limit for the directory
  entries that the call walks; the name is proposed).
  - A deadline hit returns `tool_timeout`. A byte or entry hit returns
    `tool_input_limit` with the limit, the configured value, and the consumed
    value. A hit returns no partial result and writes the `limit_hit` audit
    event.
  - `search` in content mode charges the bytes that it scans and stops during
    the scan. `ls` stops the walk at the entry limit.
  - `batch` and `execute_script` share one budget with their children.
  - A subprocess of `exec`, `git_read`, or a plugin is stopped at the deadline
    with its process group.
  - Under an input or entry limit, a tool that cannot meter its input is
    refused with `tool_policy_unsupported`: `exec`, the process plugins, the
    index search, and each tool that is not on the admission list.
    `tool_pipeline` is refused under each budget option. A budget option with
    `--worker-config` is refused at startup (exit `2`).
  - Without the options, each tool is unchanged.
- **`--capability-probe-payload` (FR-04).** A default-off `agent --print`
  option (ADR-027) that sends exactly one unretried capability probe before any
  evidence tool runs, proving the endpoint can drive the same final-answer tool
  the run depends on. It requires `--final-output` and `--model-profile`
  (omitting either is a startup refusal, exit `2`, naming the option) and is
  agent-only (`chat`, `acp`, the session daemon, and `--dry-run` refuse it).
  The caller's minimal payload is validated locally against the final-answer
  schema before anything is sent (invalid payload → exit `2`, no request). The
  probe offers the byte-equivalent frozen run tool catalog; a pass is exactly
  one schema-valid final-answer call and the run proceeds, while every other
  outcome — text only, the wrong tool, two or more calls, invalid arguments, or
  an endpoint error (503, 3xx redirect, timeout) — yields the distinct
  `model_unsupported` exit (`21`) with no further work and no returned call
  executed. The probe is one HTTP attempt regardless of the profile's retry
  setting, counts against the `--final-output` `max_model_requests` ceiling,
  and (with `--audit-path`) is recorded as one `probe_result` event that never
  contains the credential value. No live credentialed endpoint is required. New
  module `src/capability_probe.rs`; model-facing wording in
  `prompts/capability_probe.md`. See ADR-027 and `docs/configuration.md`.
- **Audit reconciliation for probes, limits, and controlled transport
  (FR-04).** The terminal `run_end` audit record now always matches the
  process exit through one source: a failed probe records `model_unsupported`
  / `21`, and a `--final-output` ending records `20`
  (`structured_output_failed`) or `24` (`model_request_limit`). When the
  controlled transport serves a request, its `model_request` audit event now
  carries the observed `http_status`, `retry_index`, and the profile's `wire`,
  and the probe's single endpoint request is recorded on its `probe_result`
  event with the same `http_status`/`retry_index`/`wire` and its
  endpoint-reported `usage` token counts (FR-04 req 19). An unset profile
  credential refuses the run identically with or without the probe flag, and
  sends no request.
- **`--worker-config` (FR-05).** An opt-in, default-off `agent --print` option
  for credential-free worker execution (ADR-028). The model-side process holds
  the credential and reaches the endpoint but executes no tool itself: every
  non-final tool call is routed to a caller-supplied MCP-stdio worker named in
  a JSON control file (`workers` map of name -> `{command, args, env}`) or
  refused, with no local/native/opcode/script fallback. Each worker child is
  launched from a cleared environment (`env -i`) plus only its explicit `env`
  map, so no credential, provider configuration, or inherited variable reaches
  its env, argv, or initialization. Worker tools are discovered before the
  catalog is frozen and exported as `mcp__{worker}__{tool}`, so `--tool-allowlist`
  and `--audit-path` treat them uniformly; the allowlist is their sole run-policy
  gate (worker tools are the caller's sandbox and are not path-checked or refused
  under `--workspace-read-only`). The final-answer tool (`--final-output`) stays
  intercepted and validated model-side and is never forwarded to a worker. A
  malformed/empty control file fails closed at startup (`run_configuration_invalid`,
  exit 2); a required worker that cannot launch, or that exits/stalls during a
  call, aborts the run with the distinct `worker_unavailable` (exit 22), records
  the failed dispatch in the audit under the `worker` route, and runs no later
  tool. Agent-only: refused in `--interactive`/`--dry-run` and absent from the
  `chat`, `acp`, and session-daemon frontends. New module `src/worker_runtime.rs`
  and `mcp_bridge::connect_worker`. See ADR-028 and `docs/configuration.md`.
- **`--audit-path` (FR-04).** A global, default-off option that writes a
  caller-requested, privacy-preserving run audit: one private JSONL file
  (`O_CREAT | O_EXCL`, mode `0600`, never appended) with a versioned
  lifecycle (`run_start` … `run_end`), `model_request`, `tool_call`, and
  `tool_refusal` records sharing one `run_id` and a monotonic `sequence`. It
  is independent of optional analytics and OTLP export and works without
  controlled transport or a model profile. Credentials, URL userinfo/path/
  query, prompts, bodies, headers, and tool argument/result contents never
  enter the record; arguments and results are SHA-256 hashes of canonical
  (sorted-key) bytes, only the credential variable name and base-URL host are
  recorded, and unavailable metadata is explicit `null`. Unlike OTLP's
  fail-open tracing, a requested audit is required: an open/write/finish
  failure (including an existing destination, or exceeding
  `[reviewer.audit]` `max_event_bytes`/`max_events`) stops the run with exit
  `23` (`audit_write_failed`) — an open failure before any model request, a
  mid-run failure before further model or tool work. With `--state-dir` the
  path must be under the state root (else exit `2`, naming the option and
  path); otherwise it must be outside the workspace. New module
  `src/run_audit.rs`. See ADR-026 and `docs/configuration.md`.
  The audit is written only by the one-shot `agent --print` frontend; every
  other frontend that accepts the global option (`chat`, `acp`, the MCP
  server, the session daemon, the interactive agent, and `agent --dry-run`)
  refuses `--audit-path` at startup with exit `2` (`run_configuration_invalid`,
  reason `audit_unsupported_frontend`) rather than run a falsely-audited
  session. The terminal `run_end` record is written before any assistant
  output is published, and its `status`/`exit_code` come from the same
  terminal source as the process exit. The typed `AuditEvent` renders the same
  JSON that is streamed to the file.
- **`--workspace-read-only` (FR-07).** A global, default-off option that
  prohibits every Daimonos-initiated workspace mutation end to end (startup,
  tool calls, teardown). Workspace mutations are refused with
  `workspace_write_forbidden` and opaque, unmediatable tools (`execute_script`,
  arbitrary `exec`, the repair helper, external CLI plugins) with
  `tool_policy_unsupported`, at every dispatch boundary; an allowlist cannot
  override the refusal. `execute_script` is refused before any Starlark runs in
  every frontend — over MCP/daemon and in `daimonos agent` / `chat` / `acp`
  (the shared agent run loop) — not only on the MCP path. Read tools, including
  `git_read`, keep working, and internal state uses `--state-dir`; the
  startup-guarded internal writers are a single registry
  (`run_policy::INTERNAL_WRITERS`, incl. the request-trace directory W11)
  cross-checked against the INVENTORIES.md W-rows by a completeness test.
  Central admission table in `src/run_policy.rs`. See ADR-022 and
  `docs/configuration.md`. Startup refusals use the reserved exit-2 shape
  (`code: run_configuration_invalid`, `reason: workspace_write_forbidden`), the
  same as a state-authority failure. `--workspace-read-only` is part of a
  session daemon's launch identity, so a read-only client spawns a read-only
  daemon and will not attach to one started without it. The request-trace
  startup guard resolves `DAIMONOS_OPENROUTER_TRACE_DIR` through the writer's
  own resolver, so the guard and the writer agree on the destination.
- **Controlled model transport** (`daimonos agent --model-profile <PATH>`,
  ADR-025, FR-04 transport requirements 1-13). An opt-in, profile-scoped request
  transport: one caller-owned JSON object supplies the five required values
  (`provider_label`, `model`, `base_url`, `wire`, `credential_env`) plus optional
  `max_tokens`, `timeout_ms`, `request_retries`, `proxy`, and `extra_body_fields`.
  A partial profile fails at startup naming the missing value(s). Requests carry
  only the fixed body/header set, set automatic tool choice explicitly (never a
  forced choice or provider `strict` flag), send no sampling parameters, use the
  stable endpoint path, and suppress ambient headers, telemetry, redirects, and
  ambient proxies. `max_tokens` is caller-owned with no built-in default:
  mandatory on the `messages` wire (a profile omitting it fails at startup) and
  omitted on the `chat` wire unless the caller set it. `extra_body_fields` are
  validated at startup and refused before any request if they overwrite a
  protected field or carry any sampling, forced-choice, strictness, streaming,
  or multi-choice parameter on either wire. Default off; legacy provider behavior is unchanged. The flag
  is honored only by the one-shot send path and fails closed when combined with
  `--interactive` (the interactive TUI routes through the session daemon, which
  carries no profile). Caller `request_retries` is bounded by the model-request
  ceiling (`100`, ADR-025).
- **Self-contained full tool schemas (FR-15).** A new global `--full-tool-schemas`
  flag (equivalent to `[mcp] full_tool_schemas = true` / `DAIMONOS_MCP_FULL_SCHEMAS=1`)
  turns on an opt-in mode that serves and exports a full, self-contained JSON
  Schema draft 2020-12 document for every tool instead of the default stripped
  placeholders. Before any model request, serving, or export, each schema is
  validated offline (`src/schema_contract.rs`, reusing the ADR-024 validator):
  external references, recursive reference cycles, non-2020-12 documents, and
  cross-tool definition-name collisions are refused with run reason
  `schema_unusable`; an endpoint-specific rejection stays a probe failure, never
  a guessed startup blacklist. Honored on every frontend (MCP server,
  `tool-contract`, agent, chat, ACP) and carried in the session-daemon launch
  identity. Default off; terse/lazy serving is unchanged. See
  [ADR-030](docs/adr/030-self-contained-full-schemas.md).
  - For an agent frontend the option does not change the model request (tool
    schemas are already full) and adds only the startup self-contained check;
    the three switches (`--full-tool-schemas`, `DAIMONOS_MCP_FULL_SCHEMAS`,
    `[mcp] full_tool_schemas`) all run that same check. The check now also covers
    the caller-supplied `--final-output` final-answer schema: it is offered
    unchanged but refused as `schema_unusable` (exit 20, final-output error
    shape) when it contains a recursive reference cycle. The self-contained
    check follows every in-document reference form with proper 2020-12
    URI/anchor semantics: a percent-/`~`-decoded JSON-pointer `$ref`, a
    plain-name `$anchor`, a `$dynamicRef` to a `$dynamicAnchor`, and a `$ref`
    naming the document's or an embedded resource's `$id` (absolute or relative
    to the base, with or without a fragment); a reference it cannot resolve
    inside the document is refused `schema_unusable` and named, never fetched
    (fail closed). Definition-name clash refusal applies only to Daimonos-owned
    definitions; a supplied schema is never refused for a definition name.
  - The self-contained check reads *schema positions only*: a `$ref`-shaped
    object that is the data of a `const`/`enum`/`default`/`examples` keyword is
    not a reference, so it is not followed and such a schema is accepted and
    offered unchanged; a reference under a keyword the check does not recognize
    is read conservatively (fail closed); and two schema positions declaring the
    same `$id` or anchor are refused as `schema_unusable` with the duplicate
    named (the static index never silently replaces an identifier).
  - The self-contained check follows local references *iteratively* (an explicit
    work list, not call recursion), so a large but finite, non-recursive `$ref`
    chain — hundreds of thousands of linked definitions — is analyzed without
    ending the process by a signal and is accepted (exit 0) just as the default
    mode accepts it; a chain that closes into a cycle is still refused
    `schema_unusable`. Only a pathological document with millions of schema
    positions exhausts the static-analysis budget and is refused
    `schema_unusable` (exit 20) — never a crash. That budget is a fixed internal
    guard of 10000000 steps, not a configuration key.
  - An explicit `--full-tool-schemas` flag now wins over the
    `DAIMONOS_MCP_FULL_SCHEMAS` variable for every value: a false-y variable
    (`0`/`false`/`off`/empty) no longer silently masks the flag. Without the
    flag the variable still wins over the `[mcp] full_tool_schemas` config key
    (precedence unchanged).
- **Tool contract (FR-13).** `daimonos tool-contract` prints a deterministic,
  session-free JSON dump of the effective tool contract (contract version,
  effective options, and the effective tools with their schemas), reusing the
  live MCP serving pipeline so the dump equals what the server serves for the
  same inputs. The contract carries its own version (`1.0.0`), reported over MCP
  initialization in `_meta["io.daimonos/tool-contract-version"]` and tracked in
  `docs/tool-contract-changelog.md`. See
  [ADR-029](docs/adr/029-effective-tool-contract.md).
- **Default behavior change (FR-13, no opt-out).** Every MCP `initialize`
  response now carries the additional `_meta` key
  `io.daimonos/tool-contract-version`; this is unconditional (required by
  AC-13.2) and has no option — the existing `server-kind` meta and
  `serverInfo.version` (binary version) are unchanged. The session-free
  `daimonos tool-contract` command also writes no state artifacts, including no
  secure log file in the resolved state directory.
- **Owned path annotations (FR-14).** Every Daimonos-owned tool argument whose
  value is a filesystem path, a glob, or a directory to walk — including the
  elements of path arrays — now carries the standard JSON Schema annotation
  `"x-daimonos-path": true` in the served and exported schemas, so a caller can
  enumerate a tool's path slots without a hand-written list. Opaque
  command/script/argv strings, agent-mail reservation glob patterns, URLs, and
  caller-provided worker / final-answer schemas are deliberately not marked.
- **Default behavior change (FR-14, no opt-out).** The path annotations above are
  added unconditionally to the served schemas (there is no option to disable
  them). They are purely additive metadata: no existing schema key, tool, or
  argument is changed or removed. Because a newly populated schema annotation is
  an additive contract change, the tool-contract version moves from `1.0.0` to
  `1.1.0` (`docs/tool-contract-changelog.md`); the binary version
  (`serverInfo.version`) is unchanged.
- **Scope of the path marks (FR-14).** The marks are caller-facing metadata:
  they are carried by the `tool-contract` dump, `tools/list`, and
  `get_tool_schema`, and are deliberately **not** sent to the model endpoint
  (the schemas handed to the model stay unchanged per FR-15). In the initial
  exposure tier the dump and `tools/list` serve a full schema for 21 of the 35
  tools, so the dump shows the marks only for those tools (6 of which have path
  arguments); for the other 14 tools whose initial-tier schema is stripped to
  `{"type": "object"}`, a caller reads the marks through `get_tool_schema`,
  which returns the complete annotated schema. FR-15 full self-contained schemas
  (WP17) will remove this limitation.
- **Structured final output** (`daimonos agent --final-output <control.json>`,
  ADR-024, FR-01). Opt-in, default off. A run ends only when the model makes a
  single call to a caller-designated final-answer tool whose arguments validate
  against the caller's Draft 2020-12 JSON Schema (validated offline with
  `jsonschema` 0.60.2, format assertion on, external references refused without
  I/O). On success the process emits exactly the validated argument JSON plus
  one newline on stdout and exits 0; a typed failure emits a bounded JSON error
  (`reason`, `validation_errors`, `attempts`, `model_requests`) on stderr and
  exits 20; the inclusive model-request ceiling emits reason `turn_limit` and
  exits 24. Corrective retries are bounded independently of the model-request
  count, a mixed ending executes none of its calls, and the flag is refused on
  frontends that cannot honor it (interactive, dry-run, chat, ACP, daemon).

- **Explicit-only configuration** (`--ignore-config-files`, FR-09, ADR-021).
  A global, default-off run option that refuses every implicit configuration
  source (environment files, config files, agent instructions, skills, default
  MCP/Zed settings, ignore files, and file-defined CLI plugins) so a hostile
  workspace or home file cannot configure an opted-in run. Explicit
  caller-supplied inputs (`--config` and the paths it names, `--agent-env`,
  `--agent-instructions`) remain honored, and each ignored source is reported
  by path at startup. The setting is pinned into a session daemon's launch
  identity.
  Ignore-file suppression (D08) covers every walker an opted-in run can reach:
  content `search`, `search mode="files"` (the filename index), their confined
  `--confine-paths` equivalents, and `snapshot` create/restore.
  The over-broad-root index warm-up preflight honors the policy too, so a
  hostile `.gitignore` cannot change which paths an opted-in run eagerly
  indexes. A process-level test confirms that under the option a hostile
  workspace `agent-instructions.md` never reaches the model request.

- **No-spill tool output** (`--no-output-spill`, config `tool_output.save_full_output = false`;
  ADR-021 / FR-11). Bounds oversized tool results in place: the producer writes
  no full-output spill file and emits no saved-file pointer, on the native,
  process, background-process, reformat, and old-result-pruning routes.
  Background processes (`bg`/`poll`) hold bounded output in memory instead of
  writing an artifact file and advertise no `log` path. Truncated results keep
  explicit truncation plus paging/narrowing guidance. Default behavior (a spill
  file and a `full output saved to` pointer) is unchanged when the flag is
  absent.

- **`--no-follow-symlinks`.** Independent no-follow filesystem policy (FR-12,
  ADR-022): every symlink is reported as a link with its literal target text and
  is never read or walked through, including intermediate directory links and
  links named as a tool/walk scope. Combines with `--confine-paths`; default off
  leaves behavior unchanged.

### Fixed

- **An explicit `--agent-env` file lost to the home `agent.env`** (ADR-037,
  vikunja #1623). Daimonos loads `<workspace>/agent.env` and
  `~/.config/daimonos/agent.env` into the process environment at startup, and
  their values then replaced the values of the file that `--agent-env` or
  `$DAIMONOS_AGENT_ENV` names. A run with `--agent-env` on a host with a home
  `agent.env` used the provider endpoint, the model, and the API key of the
  home file. The order is now: real environment, then the explicitly selected
  file, then the implicit files. With an explicitly selected file, the
  connection settings (provider, base URL, API key) never come from an implicit
  file. A run without an explicit selection does not change.
- **A signal during `daimonos acp` startup killed the process** (vikunja
  #1623). The handlers for SIGTERM, SIGINT, and SIGHUP were installed after the
  startup requests to the provider. They are now installed before those
  requests, and a signal during them ends the process with exit code 0.

### Changed

- **Releases start from CI.** A merge to `master` that changes the version in
  `Cargo.toml` creates the tag and starts the Release workflow. A merge that
  keeps the version releases nothing.

## [0.1.6] - 2026-11-22

This release has the 13 PRs that merged after v0.1.5 (#252 to #263, and #284).

### Added

- **One-shot sessions and a JSON result** (ADR-036, #284).
  - `daimonos agent --session-id <ID>` and `daimonos agent --resume <ID>`:
    named one-shot sessions that a later run continues on the same provider,
    endpoint, and model, with the same system prompt, tool list, and earlier
    messages.
  - `daimonos agent --output json`: one JSON result document on stdout for
    each run, with the outcome and the token usage of that invocation. It is
    also printed after a provider error and a context overflow.
  - `agent_runs.oneshot_session_id`: the named session of a one-shot run.
  - Reference: `docs/runtime-modes.md`, section "One-shot sessions and the
    JSON result".
- **`git_read` tool** (ADR-017, #253): a separate, read-only tool for pinned
  git reads. The `[git_read]` configuration section sets its limits.
- **Strict git pinning** (ADR-018, ADR-019, #254): `git_read.strict_pinning`
  requires explicit full commit identifiers. Status filters are isolated.
- **Caller-owned state root** (ADR-021, #257, #258): `--state-dir PATH` puts
  the state of a run under one root. It covers the MCP tool server, native
  tool mode, `agent`, `chat`, ACP, the session daemon, session import and
  export, OAuth status, and analytics statistics.
- **Path confinement for local reads** (ADR-022, #259): `--confine-paths`
  (root = workspace) and `--confine-root PATH`. The `[reviewer.tool_limits]`
  section sets the symlink limits.
- **ACP approval time limit and permission-mode lists** (ADR-034, #261).
  - `acp.approval_timeout_secs` (default 120) denies a permission request that
    the client does not answer.
  - `DAIMONOS_AGENT_PERMISSION_MODE` (`null`, `allow_list`, `deny_list`) and
    `DAIMONOS_AGENT_PERMISSION_LIST` in the agent env file.

### Changed

- **`exec` of git and gh uses the plugin policy** (#252). The argv form,
  unmapped subcommands, and ACP-streamed calls now run with the non-interactive
  environment of the plugin. Stdout and the exit code do not change.
- **Evidence fingerprint** (ADR-035, #262): the evidence ledger hashes file
  metadata and writes nothing. Before, it copied the workspace into a shadow
  git repository on each turn.

### Fixed

- **ACP thread history stopped saving** (ADR-033, #260). A second process that
  loaded a live session took its writer. One live process now owns a session,
  and a second process is refused.
- **ACP crash recovery** (#255): accepted prompts, completed provider messages,
  and completed tool results are saved at recovery boundaries.
- **ACP approval wait without a limit** (#261): a client that never answered a
  permission request held the session.
- **Leaked evidence store** (#262): killed turns left shadow repositories in
  the state directory. Each process now removes the legacy store once.

### Documentation

- Accepted ADRs 021 to 032 and the reviewer-runtime glossary (#256).
- Vikunja project URLs in `AGENTS.md` (#263).

## [0.1.5] - 2026-10-18

The first release since v0.1.4 (May 2026). It spans about 250 merged PRs. The
binary now also reports its real version: earlier releases still said 0.1.1.

### Added

- **Agent harness.**
  - `daimonos agent` (one-shot), `daimonos chat` (REPL with `--resume`), and
    an opt-in terminal UI.
  - Daemon-owned sessions that survive client disconnects.
  - Providers: Anthropic, native OpenAI Responses, and OpenRouter.
  - Multi-provider routing with live model discovery and failover.
- **Native ACP engine for Zed** (`daimonos acp`).
  - Multi-session, with cross-process `session/load`.
  - Model picker and per-session Thinking (reasoning-effort) selector.
  - Plan updates, terminal cards, diff rendering, and slash commands.
  - Consumes Zed-provided MCP servers.
- **Portable Agent Skills.** `SKILL.md` discovery from `~/.agents/skills` and
  `<workspace>/.agents/skills`, a native `skill` tool, `/name` invocation, and
  Zed Skills UI integration.
- **Outbound MCP.** Servers can be configured from a Claude-style
  `mcpServers` file or Zed's `context_servers`, with OAuth grants.
- **Context management.** Context-window compaction (ADR-002), offload
  handles for large tool output (ADR-007), and `execute_script` LLM sub-calls
  (ADR-008).
- **Coordination and checkpoints.** Agent mail for multi-agent coordination
  (ADR-009), and automatic per-turn workspace checkpoints.
- **Observability.** Privacy-first OpenTelemetry / Langfuse tracing (ADR-006).
- **Tools.**
  - `git` `raw` passthrough for every git subcommand.
  - `gh` `raw` passthrough plus PR, run, and issue commands.
  - Resilient `edit_file` matching, `append_file`, and hybrid lazy indexing.
- **Remote and repository.** A paired remote session gateway with an Android
  controller (debug APK built in CI). Public-facing repository maintenance
  files (`LICENSE`, `CONTRIBUTING.md`, `SECURITY.md`, `CODEOWNERS`).

### Changed

- MCP configuration can use Zed's JSONC `context_servers` directly, and
  `daimonos mcp-config sync -- COMMAND` adapts it for file-only harnesses such
  as Cursor ACP on every launch. Zed is now the default MCP source for new and
  default-only Daimonos configurations. Existing users who want to keep
  `~/.config/daimonos/mcp_servers.json` must set
  `[agent.mcp].servers_file` explicitly. ACP empty-list recovery is limited to
  clients identifying as Zed and is controlled by
  `[acp.mcp].zed_config_fallback`.
- **OpenRouter honors `DAIMONOS_AGENT_THINKING`** (default `medium`), which
  adds billed reasoning tokens. Set `off` to keep the previous behavior.
  OpenRouter requests default `max_tokens` to the model's catalog output
  ceiling.
- **Model-facing prompts and tool descriptions** live in `prompts/` and can be
  overridden.
- **CI.** Runners are pinned (check jobs on `ubuntu-26.04`, release builds
  on `ubuntu-24.04`, so the glibc baseline is unchanged), and actions are
  bumped past Node 20.
- Expanded `.gitignore` to cover common local artifacts.

### Fixed

- **Closing a chat mid-command no longer orphans the command.** When an MCP
  client disconnects or `daimonos mcp`/`acp` gets SIGTERM, SIGINT, or SIGHUP,
  running commands' process groups are terminated. The new setting is
  `[mcp] client_gone_poll_ms`.
- **OpenRouter tool calls keep optional parameters optional**
  (`strict: false`).
- **The idle watchdog no longer kills a request in flight.**
- **Agent turns recover from a mid-stream transport break** (HTTP/2 `CANCEL`,
  connection reset, or abrupt idle disconnect) instead of dead-stopping. The
  provider adapters classify such faults as retryable, so the turn resumes on
  the bounded retry path (vikunja #1418). A recovered turn currently
  re-streams the partial text it had already shown, so a repeated snippet
  mid-turn is expected until the re-stream de-duplication follow-up lands.
- **Release builds install cross targets on the pinned 1.96.0 toolchain**,
  which unbreaks tagged releases.

- Enforced tool allowlist (`--tool-allowlist PATH`, FR-02/WP06): an explicit JSON array of exact exported tool names is the effective policy at every model listing (tools/list, `list_all_tools`) and every dispatch boundary (direct MCP, batch children, Starlark `tool()`/opcode shortcuts, `execute_script`, the agent loop). Omitted = legacy (all tools); `[]` = none; unknown names fail at startup (exit 2 `run_configuration_invalid`). Caller-owned (never widened by workspace/env config); part of the session-daemon launch identity; refusals carry `tool_not_allowed` and reach the `--audit-path` record. The policy also filters `get_tool_schema`, `list_tool_signatures`, the MCP server instructions, the session-daemon socket listing, and the agent's offered catalog, so served/probed/dumped/dispatched sets agree; with `--final-output` an allowlist omitting the final-answer tool fails startup (`tool_allowlist_missing_final_output`); the reported effective list is the intersection with the agent-env `allowed_commands`; config-registered `[tools.*]` tools may be allowlisted. (r3) The model-facing catalog is now filtered at one source (`tool_facade::active_schemas_for_policy`), so `daimonos chat`, `daimonos acp`, and the session-daemon agent — not only `agent --print` — offer the model exactly the allowlist; forwarded MCP tools and the `execute_script` description's embedded Starlark signatures are filtered by the same policy. (r4) The composite opcodes `tool_pipeline` and `lint_repair` now check their inner `tool_id` against the allowlist before running it, so a listed composite can never reach an unlisted registered `[tools.*]` tool; a malformed or unreadable `--tool-allowlist` file fails startup with reason `tool_allowlist_invalid`; remote/worker tool names cannot be allowlisted in this package (deferred to WP10), so an active allowlist removes every forwarded MCP tool from the catalog.
