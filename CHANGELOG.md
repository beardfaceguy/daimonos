# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- **Agent turns no longer stop when the provider drops out mid-stream.**
  Several mid-stream provider failures were classified fatal, so the turn
  ended with a "Provider request failed." style notice (in Zed, a thought
  line) until the user typed a nudge. Each is now classified transient, so
  the existing in-generation retry and turn-level resume recover it inside the
  same turn:
  - a stream that closes before its terminal event (all three providers);
  - Anthropic in-band `overloaded_error`, `api_error`, `rate_limit_error` and
    `timeout_error` events;
  - OpenRouter mid-stream error chunks (`finish_reason: "error"`), classified
    by `error_type`;
  - OpenAI `error` / `response.failed` events with `server_error` or
    `rate_limit_exceeded`.
- **OpenRouter `finish_reason: "length"` maps to max tokens.** It was treated
  as an unknown stop reason, so any output-cap truncation through OpenRouter
  ended the turn as a provider error.

### Changed

- The ACP engine now shows provider recovery (retry, failover, turn resume) as
  a thought chunk in Zed, carrying only the bounded diagnostic class. Overload
  and `503` errors have their own diagnostic: "Provider is overloaded or
  temporarily unavailable."

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
