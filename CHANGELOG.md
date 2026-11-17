# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
    `schema_unusable` (exit 20) — never a crash.
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
