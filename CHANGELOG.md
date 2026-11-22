# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
