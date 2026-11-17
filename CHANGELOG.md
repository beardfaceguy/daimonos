# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
