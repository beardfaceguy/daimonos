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
  `docs/configuration.md`.

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
