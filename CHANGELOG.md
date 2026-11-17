# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
