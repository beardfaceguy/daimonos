# Configuration Reference

Daimonos uses a TOML config file for tuning indexing, search, and process
behavior. All settings have sensible defaults — configuration is optional.

## Config File Location

Daimonos searches for config in this order:

1. Path passed via `--config` / `-c` flag
2. `daimonos.toml` in the workspace root (not for the agent frontends, see below)
3. `~/.config/daimonos/config.toml`
4. Built-in defaults (equivalent to `daimonos.default.toml` in the repo)

The first file that exists is used. The files are not merged.

The agent frontends (`agent`, `chat`, `acp`, and the session daemon) do not
load `daimonos.toml` from the workspace root (ADR-039). The file comes with the
workspace, which can be a repository of another party. To use it, name it:

```bash
daimonos --config ./daimonos.toml agent "task"
```

When a workspace file exists and is not loaded, startup prints one line on
stderr. The MCP server modes load the workspace file as before.

To start with a custom config, copy the reference file:

```bash
cp daimonos.default.toml ~/.config/daimonos/config.toml
```

Or place it in your project. The MCP server modes find it there. The agent
frontends need `--config`:

```bash
cp daimonos.default.toml /path/to/project/daimonos.toml
```

### Which config file is in use?

To see the discovery order and which file (if any) daimonos will load:

```bash
daimonos --print-config-path            # uses default workspace (.)
daimonos --print-config-path -w /path   # for a specific workspace
daimonos --print-config-path --config /path/to/file.toml
```

It prints each candidate in search order with a `[found]` / `[not found]`
marker and the file that wins (or `built-in defaults` if none exist), then
exits without starting the server. In non-MCP modes daimonos also logs
`config: loaded from …` to stderr at startup (add `--verbose` in `--mcp` mode).

## Caller-owned state (`--state-dir`)

Use `--state-dir PATH` to keep Daimonos-owned frontend state outside the
workspace and HOME. The caller must create a private, writable
state directory that does not overlap the workspace after symlink resolution.
See `StateAuthority` in `src/state_root.rs` and ADR-021 for the boundary.

Daimonos rejects an internal configuration destination outside that root.
It also rejects MCP client roots that overlap the selected state directory.
When selected, automatic HOME config discovery is disabled. Explicit and
workspace config files remain available.

This option does **not** make the workspace read-only or confine tool reads.
Without the option, existing state locations remain unchanged.
`mcp-config` rejects the option with reason `state_mode_unsupported`.

With selected state, agent frontends do not discover HOME agent environments,
user instructions, global skills, or Zed MCP settings. Explicit inputs remain
available. Use `--agent-env` or `DAIMONOS_AGENT_ENV` to select the agent file.
Pass an explicit output directory with `--dump-prompts`.
The session-store layout is defined by `StateAuthority::verify_session_store`.
To avoid HOME writes during startup, selected-state CLI registration checks
executable presence on `PATH` without running version probes. An executable can
still fail when a caller invokes it.

## Confined filesystem access

`--confine-paths` fixes local filesystem reads to the workspace.
`--confine-root PATH` selects a different root and also enables confinement.
The root remains fixed when
the logical working directory changes or an interactive agent starts a daemon.
The Linux implementation requires `openat2` and procfs. It fails closed when
those facilities are unavailable. Other platforms do not yet have an adapter.

Confinement governs filesystem access only. The raw native protocol opcode
`env_get` (17) stays available and can return a variable from the Daimonos
process environment. No MCP or agent tool exposes this opcode. Only the native
protocol client, which already supplies the process environment, can send it.
See ADR-022 and `FilesystemAuthority` in `src/path_policy.rs`.

Confined `read_file`, `ls`, `search`, `diff_files`, and skill reads use the
fixed authority. Native `stat` and `glob` use it too. Automatic walks skip
unsafe entries and report aggregate counts. The index walk limits and
`reviewer.tool_limits` in `daimonos.default.toml` bound traversal.

Tools without a confined adapter are not advertised and cannot run. Explicit
local CLI tools and local stdio MCP servers are refused. `--dump-prompts` and
`session export --output` are refused because their writes are not mediated.
Explicitly forwarded HTTP MCP
servers remain a separate trust boundary. This is not a sandbox for
external services or a read-only workspace; the read-only gate below is a
separate, independent option.

Workspace-discovered control files are read through the authority. Explicit
caller-selected control files remain separate inputs. An explicit `--config`
file is trusted caller input: the control paths it names, such as prompt
overrides, `[agent.mcp] servers_file`, and Zed settings, are also read without
the authority. A workspace `daimonos.toml` does not grant this trust. Do not
pass `--config` a file that the evidence can modify. OAuth credentials, request
tracing, and `--debug-thoughts` require `--state-dir` in confined mode. Global
ignore files outside the root are not read. Confined glob patterns with a parent
component after a wildcard are refused. Procfs symlink stat does not expose
target text.

## Read-only workspace (`--workspace-read-only`)

`--workspace-read-only` prohibits every Daimonos-initiated workspace mutation
for the whole run — startup, tool calls, and teardown. It is independent of
`--confine-paths` and `--state-dir`; any combination is allowed. When absent,
behavior is unchanged (default off).

While it is on:

- Every requested workspace mutation is refused before it runs, with the named
  error `workspace_write_forbidden`. `write_file`, `edit_file`, and
  `append_file` are refused for **every target path** — including paths
  **outside** the workspace — and no write is relocated: a refused write writes
  nowhere. `snapshot restore` is always refused; `snapshot create`/`delete` are
  refused unless the snapshot store is relocated outside the workspace (then
  they only read the workspace and write the relocated store). `kgl_query` and
  `kgl_assert` are refused unless the KGL store is relocated (their store open
  would otherwise create `<workspace>/.kgl`). Refusals apply at every dispatch
  boundary, including each `batch` child.
- Opaque local tools whose effects cannot be mediated are refused with
  `tool_policy_unsupported`: arbitrary `exec`/background processes, the repair
  helper, tool pipelines/registration, `execute_script`, and the external CLI
  plugins (`git`, `gh`, `cargo`, `pytest`, `docker`, `curl`, `shellcheck`,
  `npm`, `discord`). An allowlist entry cannot override these refusals. This
  holds in every frontend: `execute_script` is refused before any Starlark runs
  over MCP and in `daimonos agent` / `chat` / `acp` (the shared agent run loop),
  not only on the MCP path.
- Read and metadata tools keep working: `read_file`, `ls`, `search`,
  `diff_files`, `git_read` (its private status scratch is relocated outside the
  workspace), `workspace_info`, `get_tool_schema`, `list_tool_signatures`,
  `list_all_tools`, `session_stats`, `set_verbosity`, and
  `set_external_session_id`.
- Internal state (logs, analytics, index, snapshots, KGL, checkpoints) is never
  written under the workspace. It uses `--state-dir` when provided. Without
  `--state-dir`, any internal writer whose configured or default destination
  resolves inside the workspace is refused at startup with
  `workspace_write_forbidden` rather than silently written or relocated — e.g.
  a `logging.directory`, `tool_output.directory`, snapshot/process/`git_read`
  status directory configured inside the workspace, enabled per-turn
  `checkpoint`s (whose shadow repo defaults to `<workspace>/.daimonos/
  checkpoints.git`), the structural request-trace directory
  (`DAIMONOS_OPENROUTER_TRACE_DIR`), or the KGL autoindex/observation
  (`DAIMONOS_KGL_AUTOINDEX` / `DAIMONOS_KGL_OBSERVE`). This refusal is
  independent of `--state-dir`. The guarded set is the single registry
  `run_policy::INTERNAL_WRITERS`, cross-checked against the reviewer-runtime
  INVENTORIES.md W-rows by a completeness test.
- Startup refusals use the reserved exit-2 taxonomy: the process prints
  `{"error":{"code":"run_configuration_invalid","reason":"workspace_write_forbidden",...}}`
  and exits 2 — the same shape as a state-authority failure — rather than a
  top-level tool-refusal code. The `DAIMONOS_OPENROUTER_TRACE_DIR` destination
  is resolved exactly as its writer resolves it (verbatim, no `~` expansion), so
  the guard and the writer agree on whether it lands inside the workspace.
- A session daemon carries `--workspace-read-only` as part of its launch
  identity: a read-only client spawns a read-only daemon and refuses to attach
  to a daemon that was started without the option, rather than being served by a
  long-lived process whose admission is not read-only.

The admission table lives in `src/run_policy.rs` (`ToolAdmission`,
`EffectClass`). See ADR-022.

## Local run audit (`--audit-path`)

`--audit-path PATH` turns on a caller-requested, privacy-preserving run audit
(WP08, FR-04). It is **default off**: without the option, behavior is unchanged
and no audit file is written. The audit is independent of optional analytics and
OTLP tracing, and works without controlled transport or a model profile.

The audit is one private JSONL file, one line per event. The file is created
with `O_CREAT | O_EXCL` and mode `0600`; an existing destination is **never**
appended to (that is a write failure, below). Every line is a versioned record
carrying `audit_version`, the shared `run_id`, a monotonic `sequence`, and a
`kind`. The lifecycle is `run_start` first and `run_end` last; in between come
`model_request`, `tool_call`, `tool_refusal`, and (wired by later packages)
`probe_result` and `limit_hit` events.

Privacy rules (ADR-006 D6, ADR-026) — the record never contains:

- a credential **value**; only the credential **variable name** is recorded;
- a model base URL's userinfo, port, path, or query; only its **host**;
- prompt text, request/response/error bodies, or headers;
- tool argument or result **contents**: arguments and results are recorded as
  SHA-256 hashes of their canonical (sorted-key) bytes, never cleartext and
  never a `Debug` string;
- evidence-file contents.

Unavailable metadata is recorded as explicit `null`, never invented.

**Required-write failure contract (fail-closed).** Unlike OTLP tracing, which
fails open, a requested audit is required: if the sink cannot be opened,
written, or finished, the run stops with exit code `23`
(`audit_write_failed`). An open failure stops the run **before any model
request**; a mid-run write failure poisons the sink and aborts before further
model or tool work. The diagnostic never echoes a secret.

**Destination.** The path is resolved before any model work. With an explicit
state root (`--state-dir`), the audit path must be **under** the state root; an
outside path fails startup with exit `2` (`run_configuration_invalid`) and the
error names both `--audit-path` and the offending path. Without `--state-dir`,
the path must be **outside** the workspace (it is private run evidence, not
workspace output). Event bounds come from `[reviewer.audit]` (below).

**Supported frontend scope.** The audit is written only by the one-shot
`daimonos agent --print` frontend. Because a run must never falsely look
audited (ADR-026), every other frontend that accepts the global `--audit-path`
— `chat`, `acp`, the MCP server, the session daemon, the interactive agent, and
`agent --dry-run` — **refuses** the option at startup with exit `2`
(`run_configuration_invalid`, reason `audit_unsupported_frontend`) rather than
ignore it silently. The interactive agent refuses before any session-daemon
launch, so a daemon never runs a falsely-audited session.

## Explicit-only configuration (`--ignore-config-files`)

`--ignore-config-files` refuses every *implicit* configuration source so a
hostile workspace or home file cannot configure an opted-in run. It is a global
run option and is default off: when it is absent, discovery is unchanged.

When it is set, these implicit sources are disabled:

- **Environment files:** a workspace `agent.env` and the
  `~/.config/daimonos/agent.env` (or `$XDG_CONFIG_HOME`) fallback are not read
  or applied.
- **Config files:** a workspace `daimonos.toml` and the HOME/XDG config are not
  read.
- **Agent instructions:** global and workspace `agent-instructions.md` are not
  read.
- **Skills:** the global (`~/.agents/skills`) and workspace (`.agents/skills`)
  roots are not discovered, and no skill executables are registered.
- **MCP servers:** the default Zed `context_servers` settings file is not
  opened.
- **Ignore files:** `.gitignore`, `.ignore`, `.git/info/exclude`, and
  global/parent ignore files do not steer any walker that an opted-in run can
  reach: content `search` and `search mode="files"` (the filename index),
  their confined `--confine-paths` equivalents, and `snapshot` create/restore.
  Built-in hidden-file filtering is not an ignore file and is unchanged.
- **CLI plugins:** only explicitly configured tools are registered; no
  workspace or user plugins.

Explicit, caller-supplied inputs remain honored: an explicit `--config PATH` and
the control paths it names (prompt overrides, `[agent.mcp] servers_file`, Zed
settings), together with `--agent-env`, `$DAIMONOS_AGENT_ENV`, and
`--agent-instructions`. An invalid explicit input fails the run rather than
falling back to another source (ADR-021).

At startup the run prints a one-line JSON effective-policy report to stderr that
names each ignored source by path, without reading its contents, for example:

```json
{"effective_policy":{"ignore_config_files":true,"ignored":[{"kind":"D01","label":"environment file","path":"/work/agent.env"}]}}
```

Under the option a hostile workspace `agent-instructions.md` (D04) is not
layered into the agent system prompt, so it never reaches the model request.
The index warm-up preflight (the over-broad-root guard) is covered by the
ignore-file suppression above, so a hostile `.gitignore` cannot change which
paths an opted-in run eagerly indexes.

The option is part of a session daemon's launch identity: a daemon started with
`--ignore-config-files` is distinct from one started without it, the same way
`--confine-paths` pins a daemon. Reusing a running daemon with a different
setting spawns a new one.

Live remote model discovery is governed separately (see controlled transport
and `DAIMONOS_AGENT_MODELS_DISCOVER`), not by this option.

See ADR-021 and `src/discovery_policy.rs`.

## No-follow symbolic links

`--no-follow-symlinks` reports symbolic links as links and never reads or walks
through them. It is independent of confinement: it can be used alone, and it
can be combined with `--confine-paths` or `--confine-root`. When the option is
absent, behaviour is unchanged (links inside the root resolve normally).

With the option on, a `read_file` of any path containing a symlink component
— the final element or an intermediate directory — is refused with the
`symlink_refused` code and the link's literal, unresolved target text in the
`target` field. The target is never opened or resolved, so no linked content is
returned. Walks for `ls`, `search`, `glob`, and `find` list a link as a link
but do not descend into it.

No-follow is enforced through the same fixed-root filesystem authority as
confinement (`openat2`/procfs on Linux; it fails closed when those are
unavailable), so selecting it pins resolution to the workspace root and shares
confined mode's startup requirements. `git_read` reads git objects rather than
the filesystem, so a stored tree symlink is already reported as a `120000`
blob whose content is the literal target text, without resolving through it.
Operator configuration readers (skills, prompts, the agent environment file,
editor configuration, and ignore files) follow links by design even under
no-follow, because they read the operator's own startup files rather than
workspace evidence presented to the model.
See ADR-022 and `FilesystemAuthority` in `src/path_policy.rs`.

## Agent Environment File

`daimonos agent`, `daimonos chat`, and `daimonos acp` use a separate,
dotenv-style `agent.env` for provider credentials and agent behavior. The file
is required; its location is selected in this order:

1. `--agent-env <path>`
2. `$DAIMONOS_AGENT_ENV`
3. `~/.config/daimonos/agent.env` when neither `--state-dir` nor confinement (`--confine-paths` or `--confine-root`) is selected.

After reading the selected file, non-empty process variables named
`DAIMONOS_AGENT_*` override values from the file. Empty or whitespace-only
process values do not erase file values. Effective precedence is therefore:

```text
CLI behavior flags (for example --model) > process DAIMONOS_AGENT_* values
> selected agent.env file values
```

Daimonos also loads two **implicit** files into the process environment at
startup: `<workspace>/agent.env`, then `~/.config/daimonos/agent.env`. They set
only variables that the environment does not have. With `--state-dir` the home
file is not read. With confinement only the workspace file is read.

The workspace file is untrusted input: it comes with the workspace (ADR-038).
These assignments in `<workspace>/agent.env` are ignored and reported on stderr:

- Connection settings: `DAIMONOS_AGENT_PROVIDER`, each
  `DAIMONOS_AGENT_*_BASE_URL`, and each `DAIMONOS_AGENT_*_API_KEY`.
- The selector `DAIMONOS_AGENT_ENV`.
- Approval settings: `DAIMONOS_AGENT_APPROVAL_MODE`,
  `DAIMONOS_AGENT_ALLOWED_COMMANDS`, `DAIMONOS_AGENT_DENIED_COMMANDS`,
  `DAIMONOS_AGENT_PERMISSION_MODE`, and `DAIMONOS_AGENT_PERMISSION_LIST`.
- Proxy and certificate variables: `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`,
  `NO_PROXY`, `SSL_CERT_FILE`, and `SSL_CERT_DIR`.
- User directory variables: `HOME`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME`,
  `XDG_DATA_HOME`, and `XDG_CACHE_HOME`.
- Loader variables such as `LD_PRELOAD` and `PATH`, as before. These are
  refused from the user file too.

Each name is compared without letter case. Set them in the user file, in the
file that `--agent-env` names, or in the real environment. Each other line of
the workspace file applies, and tool child processes inherit it.

When the file is selected explicitly, with `--agent-env` or with
`$DAIMONOS_AGENT_ENV` in the real environment, a value from an implicit file
ranks below the selected file (ADR-037):

```text
CLI behavior flags > real environment DAIMONOS_AGENT_* values
> explicitly selected agent.env file > implicit agent.env files
```

- A value from an implicit file fills only a setting that the selected file
  omits or leaves blank.
- A value from an implicit file never fills a connection setting:
  `DAIMONOS_AGENT_PROVIDER`, each `..._BASE_URL`, and each `..._API_KEY`. Put
  these into the selected file or into the real environment. A run that lacks
  one stops at startup and names it.
- Without an explicit selection, nothing changes: the selected file is the home
  file, and each process value overrides it.
- A selector that the user file sets (`DAIMONOS_AGENT_ENV=...` in
  `~/.config/daimonos/agent.env`) is not an explicit selection. The workspace
  file cannot set the selector.
- The rule covers `DAIMONOS_AGENT_*` settings that the agent env file defines,
  and `DAIMONOS_SKILL_DIR`. Five variables are read from the process
  environment only, so a line in the selected file has no effect on them:
  `DAIMONOS_AGENT_MODELS_DISCOVER`, `DAIMONOS_AGENT_MAX_TOKENS`,
  `DAIMONOS_AGENT_ERROR_RESUME`, `DAIMONOS_AGENT_AUTO_CONTINUE`, and
  `DAIMONOS_AGENT_SESSION_ID`.
- Tool child processes inherit the process environment, with the values of the
  implicit files. A Daimonos process that a tool starts sees those values as
  values of the real environment. The session daemon of `agent --interactive`
  does not: it loads the implicit files itself.

Required effective values remain
`DAIMONOS_AGENT_PROVIDER`, `DAIMONOS_AGENT_MODEL`,
`DAIMONOS_AGENT_BASE_URL`, `DAIMONOS_AGENT_APPROVAL_MODE`,
`DAIMONOS_AGENT_API_KEY`, and `DAIMONOS_AGENT_COMPACTION`. A required key may
come from either the file or the process environment; startup fails clearly
when it is absent from both. All existing provider, approval-mode, and
compaction validation applies after the override merge.

Supported providers are `anthropic`, `openrouter`, and native `openai`.
Native OpenAI uses the Responses API (not Chat Completions); a direct
GPT-5.6 Sol configuration is:

```dotenv
DAIMONOS_AGENT_PROVIDER=openai
DAIMONOS_AGENT_MODEL=gpt-5.6-sol
DAIMONOS_AGENT_BASE_URL=https://api.openai.com/v1
DAIMONOS_AGENT_API_KEY=sk-...
```

### Controlled model transport (`--model-profile`)

`daimonos agent --model-profile <PATH>` is an opt-in, profile-scoped request
transport (ADR-025, FR-04 requirements 1-13). Default off: without the flag,
agent mode keeps its current provider behavior unchanged. It is designed for
callers that must know and test exactly what is sent to a model endpoint.

The path is ONE JSON object that is the sole source of the model request
configuration. The five identity values are all required; a partial profile
fails at startup naming every missing value. None of the five may be combined
with another source, so `--model-profile` cannot be used together with
`--model` or `--provider`.

| Field | Required | Meaning |
|-------|----------|---------|
| `provider_label` | yes | A caller label for the run. |
| `model` | yes | The exact model identifier sent on the wire. |
| `base_url` | yes | The endpoint base URL. |
| `wire` | yes | `"chat"` (Chat Completions, `{base_url}/chat/completions`) or `"messages"` (Anthropic Messages, `{base_url}/v1/messages`). Never inferred from the model or URL. |
| `credential_env` | yes | Name of the environment variable holding the credential. The value is read only from this variable and never appears in logs or exports. |
| `max_tokens` | messages: yes; chat: no | Caller-owned output ceiling. There is no built-in default: on the `messages` wire it is mandatory, so a `messages` profile that omits it fails at startup naming the field; on the `chat` wire it is sent only when the caller set it and omitted entirely otherwise. |
| `timeout_ms` | no (default 60000) | Request timeout, in milliseconds. |
| `request_retries` | no (default 0) | Caller-configured retry count. The HTTP layer performs no hidden retries; every attempt is recorded. Bounded by the model-request ceiling (`100`, ADR-025); a larger value is refused at startup. |
| `proxy` | no | An explicit proxy URL. Proxy environment variables are otherwise ignored. |
| `extra_body_fields` | no | Explicit, caller-configured extra body fields, validated at startup. A key is refused before any request if it overwrites a protected field (`model`, `max_tokens`, `messages`, `tools`, `tool_choice`, `system`) or is any sampling, forced-choice, strictness, streaming, or multi-choice parameter on either wire (e.g. `temperature`, `top_p`, `top_k`, `min_p`, `frequency_penalty`, `presence_penalty`, `repetition_penalty`, `seed`, `logit_bias`, `n`, `best_of`, `logprobs`, `top_logprobs`, `function_call`, `parallel_tool_calls`, `strict`, `response_format`, `stream`, `stream_options`). No allowlist of permitted extras is defined, so only keys outside that denylist and the protected set are accepted. |

The retry ceiling (`100`) and the fallback `timeout_ms` (`60000`) are fixed constants of the
controlled transport. They are not `daimonos.default.toml` settings: ADR-025 defines the ceiling as a
safety invariant, not an operator setting, and the timeout applies only when the caller omits the field.

The flag is mutually exclusive with `--model`, `--provider`, and `--interactive`.
The controlled transport is honored only by the one-shot send path; the
interactive TUI routes model requests through the session daemon, which carries
no profile, so combining `--model-profile` with `--interactive` is refused at
startup (fail closed) rather than silently using the legacy transport.

Any top-level field beyond this set fails startup. In this mode the request body
contains only the fixed field set, carries no sampling parameters, sets
automatic tool choice explicitly whenever tools are offered (never a forced
choice and never a provider `strict` flag), uses only the stable endpoint path,
and sends only the fixed header set (the credential header, `Content-Type`,
`anthropic-version` on Messages, a fixed Daimonos `User-Agent`, plus standard
transport headers). Ambient organization/project/custom headers, tracing or
telemetry export, HTTP redirects (a 3xx is an endpoint error), and ambient
proxies are all suppressed. A client-construction error fails the profile
rather than falling back to the default client.

```json
{
  "provider_label": "anthropic",
  "model": "claude-opus-5-5",
  "base_url": "https://api.anthropic.com",
  "wire": "messages",
  "credential_env": "ANTHROPIC_API_KEY",
  "max_tokens": 4096,
  "timeout_ms": 30000,
  "request_retries": 0
}
```

### Tool approval lists (`DAIMONOS_AGENT_PERMISSION_MODE`)

`DAIMONOS_AGENT_PERMISSION_MODE` is optional. It decides which tool calls ask
for approval (ADR-034):

| Value | Behavior |
|-------|----------|
| `null` (default, or unset) | `DAIMONOS_AGENT_APPROVAL_MODE` decides. |
| `allow_list` | Tools in `DAIMONOS_AGENT_PERMISSION_LIST` run without asking. Every other tool asks. |
| `deny_list` | Tools in `DAIMONOS_AGENT_PERMISSION_LIST` ask. Every other tool runs without asking. |

When the mode is `allow_list` or `deny_list`, the list replaces the ask decision
of `DAIMONOS_AGENT_APPROVAL_MODE`. `DAIMONOS_AGENT_DENIED_COMMANDS` and
`DAIMONOS_AGENT_ALLOWED_COMMANDS` still block tools first. An "Always Allow"
answer still skips later prompts for that tool.

`DAIMONOS_AGENT_PERMISSION_LIST` is a comma list:

- A native tool uses its name, for example `read_file` or `exec`.
- A remote MCP tool uses `server/tool`: the MCP server name from the client
  (for example the Zed `context_servers` key) and the server's own tool name,
  for example `notion/notion-get-self`. The exposed `mcp__...` name never
  matches.
- A trailing `*` matches any suffix, for example `notion/*`, or `*` for all
  tools. Other patterns must match exactly.

A remote tool whose server identity is unknown always asks. This applies in
modes without an ACP MCP bridge.

```dotenv
DAIMONOS_AGENT_PERMISSION_MODE=allow_list
DAIMONOS_AGENT_PERMISSION_LIST=read_file,search,notion/notion-get-self,linear/*
```

### Agent Skills (`DAIMONOS_SKILL_DIR`)

Daimonos discovers portable Agent Skills from `~/.agents/skills` by default.
Override that global root in `~/.config/daimonos/agent.env` (or the selected
agent env file):

```dotenv
DAIMONOS_SKILL_DIR=/absolute/path/to/skills
```

The value must be absolute; `~/...` is expanded. An unset or blank value uses
the default. Relative values are rejected with a warning and no global skills
are loaded. Project skills are also discovered from
`<workspace>/.agents/skills`; a project skill overrides a same-named global
skill.

Each skill is `<root>/<name>/SKILL.md`: Markdown with YAML frontmatter. Only
`name` and `description` are required. Unknown frontmatter keys are tolerated
for cross-harness portability; `disable-model-invocation: true` is an optional
Zed-compatible extension that hides the skill from the model catalog while
leaving manual `/name` activation available. Names use lowercase ASCII letters,
digits, and hyphens, with a 64-byte limit. A `SKILL.md` may be at most 100 KiB.
Supporting files should use paths relative to the skill directory.

```markdown
---
name: code-review
description: Review a change for correctness and security.
---

# Code review

Review the requested change and report findings by severity.
```

At session startup Daimonos adds only eligible names and descriptions to the
system prompt; bodies remain on disk until the model calls `skill` or the user
enters `/code-review [arguments]`. This is separate from
`agent-instructions.md`, which remains always-on.

Resource limits are configured separately in `daimonos.toml`:

```toml
[agent.skills]
max_skills = 128
max_file_bytes = 102400
catalog_max_bytes = 16384
description_warning_bytes = 1024
```

| Setting | Default | Description |
|---------|---------|-------------|
| `max_skills` | `128` | Maximum valid skill directories considered in each global or workspace discovery root. |
| `max_file_bytes` | `102400` | Maximum bytes read from one `SKILL.md` during discovery or activation. |
| `catalog_max_bytes` | `16384` | Maximum metadata-only Agent Skills catalog appended to the system prompt. |
| `description_warning_bytes` | `1024` | Description size above which discovery emits a warning. |

For `gpt-5.6-sol`, daimonos reports the documented 1,050,000-token context
window and 128,000-token maximum output capability. The model is text-only;
ACP image prompts are rejected before provider dispatch. OpenAI tool loops
retain encrypted reasoning continuation state locally with `store=false`.

This makes one-off headless overrides possible without rewriting a complete
temporary file:

```bash
DAIMONOS_AGENT_MODEL=anthropic/claude-opus-4.8 \
DAIMONOS_AGENT_APPROVAL_MODE=auto \
daimonos agent "review this change"
```

### Reasoning effort (`DAIMONOS_AGENT_THINKING`)

Optional. Sets the model's reasoning effort for both the one-shot
`daimonos agent` path and the `daimonos acp` prompt path. Valid values, in
ascending order: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`
(case-insensitive). Defaults to `medium` when the key is absent, so existing
`agent.env` files are unaffected. An unrecognized value fails startup with a
message naming the key and listing the valid levels.

```dotenv
DAIMONOS_AGENT_THINKING=high
```

How the level maps onto the wire request is provider-specific. On native
OpenAI the level is sent as `reasoning.effort`; note that its Responses
contract tops out at `xhigh`, so `max` is mapped conservatively onto the
same `xhigh` wire value — the two are therefore indistinguishable at that
provider (pick `xhigh` unless you specifically want `max` to mean the
maximum on a provider that exposes a distinct level). Provider defaults and
the Anthropic adaptive thinking behavior are unchanged by this key.

On OpenRouter the level is sent as the unified `reasoning` object:
`reasoning.effort` with the same level name, or `reasoning.enabled=false`
for `off`. OpenRouter turns the effort into each upstream's native control.
For Anthropic models that is a reasoning budget carved out of `max_tokens`,
so OpenRouter requests default `max_tokens` to the model's catalog output
ceiling. Reasoning tokens are billed as output.

In `daimonos acp` this key is only the starting level. Zed's Thinking
dropdown changes it per session (see `docs/zed-acp-setup.md`).

### Provider prompt caching (`DAIMONOS_AGENT_PROMPT_CACHE`)

Optional and default-off. Set `DAIMONOS_AGENT_PROMPT_CACHE=on` to enable an
explicit ephemeral cache breakpoint:

- Anthropic marks the final tool definition, caching the stable tool-schema
  prefix.
- OpenRouter marks the final cacheable content block in the latest conversation
  message for `anthropic/*` models. The explicit marker keeps OpenRouter's
  normal upstream routing; Daimonos does not use the top-level cache option that
  restricts routing to direct Anthropic. Other OpenRouter model families are
  left unchanged.
- Native OpenAI ignores this setting because its prefix caching is automatic.

The first request can pay a cache-write premium. Repeated requests with the
same prefix then use cheaper cache reads, so this favors tool-loop turns and
can cost more for a one-call response. A four-run direct-Anthropic Opus 4.8
Task 04 experiment reduced fresh input 75.3% and cost 43.1% overall; at matched
three-call behavior, warm-cache cost fell approximately 54.7%.

OpenRouter advances its breakpoint to the latest cacheable message each turn.
The prior prefix can be read from cache, while the newly appended suffix is
written as the next cache entry and can incur the provider's write premium.
This differs from direct Anthropic's fixed tool-schema breakpoint.

```dotenv
DAIMONOS_AGENT_PROMPT_CACHE=on
```

## Settings

### `[index]` — Workspace Indexing

Daimonos indexes relative file paths for `search` with `mode = "files"`.
Content search remains a direct filesystem grep and does not use this index.
Construction is O(1); population follows the configured rollout mode.

| Setting | Default | Description |
|---------|---------|-------------|
| `mode` | `"hybrid"` | `eager` always warms long-lived sessions, `lazy` waits for file search, and `hybrid` heuristically warms small or marked projects |
| `max_depth` | `20` | Maximum directory traversal depth |
| `skip_extensions` | *(see below)* | File extensions to skip (known binary formats) |
| `max_files` | `50000` | Hard cap on retained indexed paths. `0` uses an internal 50,000-file safety cap |
| `max_walk_entries` | `100000` | Hard cap on all filesystem entries visited by preflight and index walks; must be at least the effective `max_files` |
| `guard_overbroad_roots` | `true` | Gate eager indexing on a signal: a root larger than the preflight budget is indexed only if it has a `project_markers` entry |
| `project_markers` | `.git`, `.hg`, `.svn`, `Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`, `Gemfile`, `pom.xml`, `build.gradle`, `CMakeLists.txt`, `Makefile` | Filenames marking a root as a real project for the gate |

Hybrid mode uses `guard_overbroad_roots` as a bounded warm-start heuristic:

- **Within `max_files`** (small) -> index it, whatever it is.
- **Larger than `max_files`** -> warm only if a `project_markers` entry is
  present at the root.

This stops daimonos from crawling an over-broad directory it inherited as cwd
(commonly `$HOME` — an editor window with no project open — but equally a NAS
mount or a downloads dir), which has been observed to balloon a single
instance to ~1.3 GB RSS. A skipped warmup is not an empty-search failure:
the first file search performs a bounded population and reports `partial`
coverage when it reaches the cap. After population, a watcher marks the warm
index dirty so the next search refreshes external changes.

The default `skip_extensions` list covers images, audio, video, archives,
compiled objects, fonts, databases, and office documents. Directories named
`.git`, `node_modules`, and `target` are always skipped.

```toml
[index]
mode = "hybrid"
max_depth = 20
max_files = 50_000
max_walk_entries = 100_000
skip_extensions = [
  "png", "jpg", "jpeg", "gif", "webp", "ico", "bmp", "svg",
  "mp3", "mp4", "avi", "mov", "mkv", "flac", "wav", "ogg", "webm",
  "zip", "tar", "gz", "bz2", "xz", "7z", "rar", "zst",
  "exe", "dll", "so", "dylib", "o", "a", "lib",
  "wasm", "pyc", "pyo", "class",
  "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx",
  "sqlite", "db", "mdb",
  "ttf", "otf", "woff", "woff2", "eot",
]
```

### `[search]` — Search Limits

| Setting | Default | Description |
|---------|---------|-------------|
| `default_grep_max` | `100` | Max results for content search (grep) when not specified by caller |
| `default_find_max` | `20` | Max results for file search (trigram) when not specified |

```toml
[search]
default_grep_max = 100
default_find_max = 20
```

### `[process]` — Command Execution

| Setting | Default | Description |
|---------|---------|-------------|
| `poll_tail_lines` | `20` | Number of trailing output lines returned by `poll` for background processes |
| `exec_output_max_chars` | `100000` (100 KB) | Max characters of exec stdout/stderr before auto-truncation |
| `exec_stream_chunk_bytes` | `8192` (8 KB) | Read size for live foreground-exec updates sent to ACP clients |
| `extra_path` | *(none)* | Additional directories to prepend to `PATH` for exec/bg commands |
| `max_background_processes` | `16` | Maximum running or stopping background processes owned by one session |
| `termination_grace_ms` | `2000` | Grace between TERM and KILL for an owned process group |
| `output_memory_bytes` | `1048576` (1 MiB) | Maximum bytes retained in memory per stdout/stderr stream while reading |
| `artifact_max_bytes` | `104857600` (100 MiB) | Maximum bytes retained in one private background-output artifact |
| `artifact_directory` | `~/.daimonos/process-output` | Optional private process-artifact directory override |
| `default_timeout_secs` | `0` | Foreground/plugin deadline; `0` disables it |
| `inherit_env` | *(reviewed list)* | Exact ambient parent variables inherited before session/per-call overrides |
| `inherit_env_prefixes` | `LC_`, `XDG_` | Ambient parent prefixes inherited by managed children |

Managed execution bounds output while it is read, owns Unix process groups,
and retires them with TERM followed by KILL after the configured grace.
Background output uses random exclusive `0600` files under a `0700` directory;
the returned `log` field is the supported way to discover the path.

Auto-truncation keeps the first and last lines of output with a
`[N lines, M chars truncated]` notice in the middle. This prevents large build
outputs from consuming excessive tokens.

Common tool directories (`~/.cargo/bin`, `~/.local/bin`, etc.) are
auto-detected and added to `PATH`. Use `extra_path` for non-standard
locations:

```toml
[process]
poll_tail_lines = 20
exec_output_max_chars = 100_000
exec_stream_chunk_bytes = 8_192
extra_path = ["/opt/custom/bin", "/usr/local/go/bin"]
```

### `[tool_output]` — Model-visible tool results

Every text result returned to an agent or MCP client is bounded before it
enters model history. When a result exceeds either limit, Daimonos stores the
complete text in a private managed file and returns a UTF-8-safe head/tail
preview containing its path. Storage failures fail open: the successful tool
result remains unchanged.

The caller-owned `--no-output-spill` flag (config `save_full_output = false`,
ADR-021 / FR-11) bounds oversized results in place instead: no full copy is
written and no saved-file pointer is emitted, including on the reformat route
(below). The preview still marks the truncation explicitly and tells the model
to page (`offset`/`limit`) or narrow its query to retrieve more. Use it when the
caller's state directory is not readable by the agent, so a saved-file pointer
would only mislead it. Old-result pruning behaves the same way under this mode:
results are still pruned to reclaim tokens, but with no stored copy and no path.
Default behavior is preserved when the flag is absent; the flag cannot be
weakened by configuration.

Before each provider call, older successful tool results are also pruned
newest-first against a deterministic token budget. Errors and the configured
number of recent results are preserved. Large string arguments from old
`write_file` and `edit_file` calls are truncated without changing paths or
other metadata.

| Setting | Default | Description |
|---------|---------|-------------|
| `directory` | `~/.daimonos/tool-output` | Managed full-output directory; files are mode `0600` under a mode `0700` directory |
| `max_bytes` | `51200` | Maximum UTF-8 bytes in one model-visible text result |
| `max_lines` | `2000` | Maximum lines in one model-visible text result |
| `retention_days` | `7` | Delete older managed files during subsequent writes |
| `intra_turn_result_budget_tokens` | `40000` | Approximate newest-first budget for prior successful results |
| `intra_turn_keep_recent_results` | `5` | Most-recent successful results always preserved |
| `old_argument_max_chars` | `2000` | Threshold for truncating old edit/write string arguments |
| `save_full_output` | `true` | Save a full copy and emit a saved-file pointer for bounded output. `false` (CLI `--no-output-spill`, ADR-021/FR-11) bounds in place with paging/narrowing guidance and no spill file or pointer. |

```toml
[tool_output]
max_bytes = 51_200
max_lines = 2_000
retention_days = 7
intra_turn_result_budget_tokens = 40_000
intra_turn_keep_recent_results = 5
old_argument_max_chars = 2_000
save_full_output = true
```

### `[loop_detector]` — Tool retry-storm detection

Deterministic, LLM-free detection of tool retry storms inside a single agent
turn (vikunja #1197, adapted from Octomind). Each complete parallel batch of
tool calls plus results is one detector *round*. Every `(call, result)` pair is
fingerprinted; a round containing any novel pair counts as progress and resets
the windows, so legitimate repeated polls or staged reads never trigger it.
Uninterrupted identical repetition first injects a bounded corrective hint
(rotating sections from the `loop_steer` prompt) into the next request as
ephemeral system context. If the model changes its call-set, the detector
de-escalates; if it keeps repeating the exact call-set, reminders back off
exponentially until a hard circuit breaker ends the turn before paying for
another generation. Steers and breaker trips are recorded in analytics as
`context:loop_detector`.

| Setting | Default | Description |
|---------|---------|-------------|
| `enabled` | `true` | Master switch; `false` removes the detector from the agent loop |
| `repeat_threshold` | `3` | Steer once an identical `(call, result)` pair repeats this many consecutive rounds; `0` disables |
| `no_novelty_rounds` | `3` | Steer after this many consecutive rounds with no novel pair; `0` disables |
| `circuit_breaker_rounds` | `12` | Hard-stop the turn after this many consecutive no-novelty rounds; `0` disables (steers only) |

```toml
[loop_detector]
enabled = true
repeat_threshold = 3
no_novelty_rounds = 3
circuit_breaker_rounds = 12
```

### `[mcp]` — MCP server (`--mcp`)

These settings apply only when running as an MCP server over stdio (Cursor,
Zed, etc.). Socket-mode daemon behavior is unchanged.

| Setting | Default | Description |
|---------|---------|-------------|
| `idle_timeout_secs` | `600` | Exit cleanly after this many seconds with **no** MCP requests (`list_tools`, `call_tool`, …). Protects against orphaned processes when an editor leaks stdin. Set `0` to disable. Overridable with `DAIMONOS_IDLE_TIMEOUT_SECS`. |
| `client_gone_poll_ms` | `250` | How often the stdio server checks whether the client stopped reading stdout, because it exited or closed the pipe. When that happens, and also on SIGTERM, SIGINT, or SIGHUP, running commands' process groups get SIGTERM, then SIGKILL after `[process] termination_grace_ms`, so they don't outlive the session. Closing only stdin is **not** a disconnect: in-flight calls still finish and reply. Set `0` to disable. |
| `startup_logs` | `false` | When `false`, omit benign informational lines on stderr during MCP startup and idle shutdown (plugin registration, indexer stats, watchdog messages). Some MCP hosts (notably **Cursor**) classify **all** subprocess stderr as `[error]` in the UI. Use `--verbose`, `[mcp] startup_logs = true`, or `DAIMONOS_LOG_STARTUP=1` when debugging daimonos. |

```toml
[mcp]
idle_timeout_secs = 600
startup_logs = false
```

### `tool-contract` command — effective tool contract dump

```bash
daimonos tool-contract                 # dump for the default workspace (.)
daimonos tool-contract -w /path        # dump for a specific workspace
```

`daimonos tool-contract` prints the effective tool contract as deterministic
JSON on stdout and exits, without starting a model run or a session store
(FR-13). The document is a function of the same inputs that shape the served
catalog — workspace context checks, exposure tier, schema mode
(`[mcp] full_tool_schemas` / `DAIMONOS_MCP_FULL_SCHEMAS`), and confinement — so
its `tools` array equals what the MCP server serves over `tools/list` for those
inputs. Each entry carries `name`, `description`, `input_schema`, and
registry-derived `tier`/`route` metadata; the top level carries the standalone
`contract_version` and the `effective_options` that produced the dump. The same
version is reported over MCP initialization in
`_meta["io.daimonos/tool-contract-version"]`, separate from the binary version.
Changes are tracked in `docs/tool-contract-changelog.md` (see ADR-029).

#### Owned path annotations (`x-daimonos-path`)

Every Daimonos-owned tool argument whose value is a filesystem path, a glob, or
a directory to walk is marked in the tool's input schema with the standard
annotation `"x-daimonos-path": true` (FR-14). The marker means the same thing in
every tool: *this string is a filesystem location Daimonos resolves against the
run root.* For an array of paths the marker sits on the array's `items` schema,
so the path **elements** are marked. A caller can therefore enumerate a tool's
path slots directly from its schema, with no hand-written per-tool list.

The annotation is unconditional (there is no option to disable it) and purely
additive: it adds the marker and changes no other schema keyword. It is
**semantic, not spelling** — these are deliberately *not* marked:

- opaque command, argv, or script-code strings (`exec.command`/`exec.args`,
  `execute_script.code`, raw `git.args`);
- agent-mail reservation glob patterns (`reserve_paths.paths`,
  `release_reservations.paths`, `check_conflicts.paths`), which are opaque
  reservation keys never resolved against the filesystem;
- URLs, revisions, search patterns, identifiers, and enums;
- caller-provided MCP worker tools and the caller's final-answer schema.

The marks are caller-facing metadata applied in the shared serving pipeline
(`src/tools.rs::tool_definitions`), so the `daimonos tool-contract` dump, the
live `tools/list`, and `get_tool_schema` carry identical marks for the same
inputs. The marks are **not** sent to the model endpoint: the schemas the agent
hands to the model are preserved unchanged (FR-15), because a strict endpoint
may reject an unknown keyword.

**Tier limitation (today).** The dump and `tools/list` carry a full schema only
for the tools whose schema is served in full in the initial exposure tier — 21
of the 35 tools today. The path marks therefore appear in the dump only for
those 21 tools (6 of which have path arguments). For the other 14 tools the
initial-tier schema is stripped to `{"type": "object"}`, so a caller reads their
path marks through `get_tool_schema`, which always returns the complete
annotated schema. FR-15 full self-contained schemas (WP17) will serve every
schema in full and remove this limitation; AC-14.1 is not claimed for the
stripped tools from the dump alone.

ADR-022 performs the actual path enforcement through a separate path
(`src/path_policy.rs`); it does not yet consult the owned-argument table in
`src/path_annotations.rs`, which today is read only by the schema annotator. See
ADR-029 and FR-14.

#### Full, self-contained tool schemas (`--full-tool-schemas`)

By default `list_tools` and the `tool-contract` dump serve a full JSON Schema
only for the tools whose schema is exposed in the initial tier, and strip the
rest to `{"type": "object"}` (progressive exposure, to save prefix tokens). The
opt-in **full-schema mode** instead serves and exports the full, self-contained
schema for *every* tool (FR-15). Enable it in any of three ways, which follow a
fixed precedence: the CLI flag wins over the environment variable, which wins
over the configuration key.

```bash
daimonos --full-tool-schemas tool-contract   # global CLI flag — wins over env and config
# or
export DAIMONOS_MCP_FULL_SCHEMAS=1            # environment — wins over the config key
# or
[mcp]
full_tool_schemas = true                      # daimonos.toml — lowest precedence
```

The mode is purely additive and opt-in: with it off, the terse/lazy placeholder
serving is unchanged. With it on, every served/exported schema is a valid,
self-contained Draft 2020-12 document: no reference to another file or URL (only
local `$defs`/`$ref`), no recursive reference cycle, and no definition name that
would clash when a caller combines tool schemas. These are checked offline
before any model request, MCP full serving, or export (`src/schema_contract.rs`,
reusing the Draft 2020-12 validator in `src/schema_validation.rs`); a schema that
fails is refused with run reason `schema_unusable` (ADR-030). A schema that is
valid and self-contained but which a *particular* model endpoint rejects is not
refused here — that surfaces at the capability probe, never as a guessed startup
blacklist. Caller-supplied worker and final-answer schemas are preserved and
offered unchanged, not rewritten to a vendor subset.

The flag is honored on every frontend that accepts it: the MCP server serves
full schemas over `tools/list`; `tool-contract` exports them; the agent, chat,
ACP, and session-daemon frontends enforce the self-contained gate before the
first model request; and the session daemon carries the mode in its launch
identity, so an interactive client requesting it cannot silently attach to a
daemon started without it. Context: ADR-030.

For an agent frontend (agent/chat/ACP/daemon) the option does **not** change the
tool schemas sent to the model — those are already full and unchanged — so it
adds only the startup self-contained check; the model request is identical with
and without it. The three switches follow a fixed precedence and all run that
same check once the mode is on: an explicit `--full-tool-schemas` flag turns the
mode **on for every value** of `DAIMONOS_MCP_FULL_SCHEMAS` (so a false-y
variable such as `0`/`false`/`off`/empty cannot silently mask the flag); without
the flag, `DAIMONOS_MCP_FULL_SCHEMAS` wins over `[mcp] full_tool_schemas = true`
(a false-y variable disables the mode even when the config key enables it). The
check covers the native tool catalog
(served, exported, and sent to the model) and the caller-supplied `--final-output`
final-answer schema, which is accepted and offered unchanged but refused
(`schema_unusable`, exit 20, final-output error shape) if it contains a
recursive reference cycle. The self-contained check resolves every in-document
reference form with proper Draft 2020-12 URI/anchor semantics (not a
string-prefix test): a JSON-pointer `$ref` (percent-decoded, then `~1`/`~0`), a
plain-name `$anchor`, a `$dynamicRef` to a `$dynamicAnchor`, and a `$ref` that
names the document's or an embedded resource's `$id` (absolute or relative to
the base URI, with or without a fragment). A reference it cannot resolve inside
the document is refused `schema_unusable` and named, never fetched (fail
closed). The cross-tool definition-name clash rule applies only to
Daimonos-owned definitions; a supplied schema is never refused for a definition
name. Context: ADR-030.

The check reads **schema positions only**. A reference keyword (`$ref`,
`$dynamicRef`) has a meaning only in a schema; a `$ref`-shaped object that is the
value of a data keyword (`const`, `enum`, `default`, `examples`) is data, not a
reference, so it is not followed and such a schema is accepted and offered
unchanged. A reference under a keyword the check does not recognize is read
conservatively (fail closed), so a cycle hidden there is still refused. If two
schema positions declare the same `$id` (resolving to one URI) or the same
anchor (one base URI and name), the document is refused `schema_unusable` with
the duplicate named: the static index never silently replaces an identifier.
Context: ADR-030.

A **very deep** schema is analyzed without ever ending the process by a signal.
The self-contained check follows local references iteratively (an explicit work
list, not call recursion), so a large but finite, non-recursive `$ref` chain —
hundreds of thousands of linked definitions — is analyzed and **accepted**
(exit 0) exactly as the default placeholder mode accepts it; a chain that closes
into a cycle is still refused `schema_unusable`. Only a genuinely pathological
document with millions of schema positions exhausts the static-analysis budget
and is refused `schema_unusable` (exit 20) — never a crash. (A schema whose JSON
nesting exceeds the JSON parser's own depth limit is rejected by the parser
before this check runs, in both modes.) Context: ADR-030.

The static-analysis budget is a **fixed internal guard of 10000000 steps**, not
a configuration key: there is no TOML setting or environment variable to raise
or lower it. A document that would make the check visit more than that many
schema positions is refused `schema_unusable` (exit 20) rather than running
unbounded; every realistic schema stays far below the limit. Context: ADR-030.

### `[session]` — Daemon-owned agent sessions

These limits apply to transport-independent agent sessions shared by ACP,
the local TUI/UDS client, and future remote clients.

| Setting | Default | Description |
|---------|---------|-------------|
| `socket_path` | workspace-derived | Optional local Unix socket override; by default a stable canonical-workspace hash allows distinct projects to run simultaneously. |
| `max_active_tool_calls` | `16` | Maximum in-flight tool calls tracked by one session. Must be greater than zero. |
| `max_sessions` | `64` | Maximum live sessions retained by one interactive session daemon. Must be greater than zero. |
| `max_clients_per_session` | `4` | Maximum clients simultaneously attached to one session. Must be greater than zero. |
| `event_queue_capacity` | `256` | Maximum canonical events queued per attached client. Must be greater than zero. |
| `snapshot_entries` | `2000` | Maximum transcript entries and tool calls retained independently in a full attach snapshot. Must be greater than zero. |
| `replay_events` | `512` | Maximum canonical session events retained for reconnect delta replay. |
| `approval_timeout_secs` | `30` | Maximum seconds a daemon approval may wait before it is denied safely. |
| `max_tool_event_output_bytes` | `65536` | Maximum UTF-8 bytes retained from one tool result in canonical events; range: 32 through one eighth of `max_frame_bytes`. |
| `accept_error_backoff_ms` | `100` | Delay before retrying a recoverable local Unix listener accept failure. |
| `idle_retention_secs` | `300` | Seconds before unloading a detached idle core while preserving its durable record for reattach; `0` keeps cores resident. |
| `session_list_page_size` | `50` | Maximum entries returned by one daemon session-list page. |
| `session_list_preview_bytes` | `256` | Maximum UTF-8 bytes retained in each local session-list preview. |
| `session_list_cursor_bytes` | `128` | Maximum accepted opaque daemon session-list cursor size. |
| `session_list_snapshot_entries` | `1000` | Maximum rows retained in one connection-bound listing snapshot. |
| `session_list_snapshot_global_capacity` | `256` | Maximum live listing snapshots per trust domain across all connections; the oldest same-domain snapshot is invalidated first. |
| `session_list_snapshot_ttl_secs` | `60` | Lifetime of one connection-bound listing snapshot. |
| `session_catalog_path` | beside daemon database | Optional shared SQLite metadata catalog path. |
| `session_catalog_busy_timeout_ms` | `5000` | SQLite writer wait under multiprocess contention. |
| `session_store_busy_timeout_ms` | `5000` | SQLite writer wait for canonical session payload operations. |
| `session_archive_max_bytes` | `67108864` | Maximum bytes accepted from one imported session archive. |
| `session_catalog_pending_entries` | `1024` | Maximum distinct latest-wins pending catalog mutations per daemon. |
| `session_catalog_write_batch` | `64` | Maximum mutations applied by one blocking worker batch. |
| `session_catalog_query_timeout_ms` | `6000` | Maximum catalog query/reconciliation wait; must exceed the SQLite busy timeout. |
| `session_catalog_query_concurrency` | `2` | Maximum blocking catalog queries/reconciliation jobs per daemon. Timed-out jobs retain a permit until SQLite returns. |
| `session_catalog_fallback_entries` | `256` | Maximum payload files examined by one incomplete-catalog fallback. |
| `session_catalog_reconcile_entries` | `128` | Maximum catalog/directory entries repaired per reconciliation pass. |
| `session_catalog_reconcile_interval_ms` | `100` | Shared minimum interval between reconciliation passes across daemon processes. |
| `session_catalog_full_rescan_secs` | `60` | Interval between full drift sweeps after a workspace catalog reaches completeness. |
| `session_catalog_tombstone_retention_secs` | `300` | Minimum retention for confirmed materialized deletion rows. |
| `persistence_retry_attempts` | `3` | Total payload-save attempts before persistence remains degraded. |
| `persistence_retry_initial_backoff_ms` | `50` | Initial delay between retryable payload-save failures. |
| `persistence_retry_max_backoff_ms` | `1000` | Maximum exponential payload-save retry delay. The serialized gate is held for every configured delay plus each blocking write attempt. |
| `persistence_eviction_extension_secs` | `300` | Maximum extra idle retention granted to dirty/degraded sessions before forced eviction. |
| `persistence_final_save_timeout_secs` | `5` | Maximum wait for one retention or shutdown final-save pass. |
| `shutdown_grace_secs` | `5` | Maximum wait for daemon-owned prompt and client tasks during shutdown. |
| `client_command_timeout_secs` | `10` | Maximum wait for a local frontend to receive a daemon command result. |
| `bootstrap_timeout_secs` | `15` | Maximum wait for an automatically started session daemon to accept connections. |
| `bootstrap_retry_interval_ms` | `50` | Delay between automatic session-daemon connection attempts. |
| `reconnect_attempts` | `4` | Maximum automatic reconnect attempts after resumable revocation or transport loss. |
| `reconnect_initial_backoff_ms` | `100` | Initial automatic reconnect delay. |
| `reconnect_max_backoff_ms` | `1000` | Maximum exponential reconnect delay. |
| `switch_attach_retry_attempts` | `3` | Candidate attach attempts when rapidly switching back to a session whose prior connection may still be tearing down. |
| `switch_attach_retry_backoff_ms` | `50` | Delay between transient client-limit switch retries. |
| `remote_pairing_ttl_secs` | `300` | Lifetime of one single-use remote pairing claim. |
| `remote_pairing_wait_secs` | `300` | Maximum wait for local approval of a pairing request. |
| `remote_auth_timeout_secs` | `10` | Maximum time for the first remote authentication frame. |
| `remote_heartbeat_interval_secs` | `30` | Idle interval before sending a WebSocket ping. |
| `remote_heartbeat_timeout_secs` | `10` | Maximum wait for activity after a heartbeat ping. |
| `remote_max_messages_per_second` | `30` | Per-connection WebSocket text/control-frame rate limit. |
| `remote_max_connections` | `4` | Global concurrent remote WebSocket limit. |
| `remote_admission_attempts_per_minute` | `6` | WebSocket upgrade limit per source IP. |
| `remote_max_unauthenticated_per_ip` | `2` | Concurrent pre-authentication sockets allowed per source IP. |
| `remote_max_admission_peers` | `4096` | Maximum source addresses retained by remote admission accounting. |
| `remote_max_paired_devices` | `64` | Maximum paired devices retained during one daemon lifetime. Re-pairing replaces that device's previous ticket. |
| `max_frame_bytes` | `1048576` | Maximum newline-delimited JSON frame size. |
| `max_prompt_bytes` | `131072` | Maximum UTF-8 bytes in one prompt; its worst-case JSON-escaped event must fit within `max_frame_bytes`. |
| `max_label_bytes` | `256` | Maximum UTF-8 bytes in a client label. |
| `max_identifier_bytes` | `128` | Maximum UTF-8 bytes in session, client, request, approval, and option identifiers. |
| `max_ticket_bytes` | `1024` | Maximum authentication ticket bytes. |
| `max_runtime_value_bytes` | `4096` | Maximum UTF-8 bytes in a string runtime-option value. |
| `max_capabilities` | `16` | Maximum requested capabilities in one attach. |

Idle eviction runs on the `idle_retention_secs` interval, so a forced eviction
can occur up to one sweep interval after `persistence_eviction_extension_secs`
expires.

Canonical session payloads live in `sessions.sqlite3`. Portable archives use a
versioned Daimonos envelope because ACP, MCP, and provider APIs do not define a
session-export standard:

```bash
daimonos session import session.json
daimonos session export <session-id> --format json
daimonos session export <session-id> --format json --output session.json
```

Import requires a top-level `session_id` and rejects an existing id. Export
refuses to overwrite an existing output file.
Daemon and ACP runtimes claim a new writer epoch when opening a session.
SQLite rejects delayed saves and deletes from every superseded epoch regardless
of their numeric capture generation.

Automatic reconnect treats only typed `event_queue_lagged` revocation and
transport loss as resumable. `session_stopped`, `attachment_replaced`, and
reason-only revocations from older daemons are terminal; core logic never
parses free-text reasons. After recovery, the TUI warns that a command written
immediately before disconnect may need verification.

```toml
[session]
# socket_path = "~/.daimonos/custom-session.sock"
max_active_tool_calls = 16
max_sessions = 64
max_clients_per_session = 4
event_queue_capacity = 256
snapshot_entries = 2000
replay_events = 512
approval_timeout_secs = 30
max_tool_event_output_bytes = 65536
accept_error_backoff_ms = 100
idle_retention_secs = 0
session_list_page_size = 50
session_list_preview_bytes = 256
session_list_cursor_bytes = 128
session_list_snapshot_entries = 1000
session_list_snapshot_global_capacity = 256
session_list_snapshot_ttl_secs = 60
session_catalog_busy_timeout_ms = 5000
session_store_busy_timeout_ms = 5000
session_archive_max_bytes = 67108864
session_catalog_pending_entries = 1024
session_catalog_write_batch = 64
session_catalog_query_timeout_ms = 6000
session_catalog_query_concurrency = 2
session_catalog_fallback_entries = 256
session_catalog_reconcile_entries = 128
session_catalog_reconcile_interval_ms = 100
session_catalog_full_rescan_secs = 60
session_catalog_tombstone_retention_secs = 300
persistence_retry_attempts = 3
persistence_retry_initial_backoff_ms = 50
persistence_retry_max_backoff_ms = 1000
persistence_eviction_extension_secs = 300
persistence_final_save_timeout_secs = 5
shutdown_grace_secs = 5
client_command_timeout_secs = 10
bootstrap_timeout_secs = 15
bootstrap_retry_interval_ms = 50
reconnect_attempts = 4
reconnect_initial_backoff_ms = 100
reconnect_max_backoff_ms = 1000
switch_attach_retry_attempts = 3
switch_attach_retry_backoff_ms = 50
remote_pairing_ttl_secs = 300
remote_pairing_wait_secs = 300
remote_auth_timeout_secs = 10
remote_heartbeat_interval_secs = 30
remote_heartbeat_timeout_secs = 10
remote_max_messages_per_second = 30
remote_max_connections = 4
remote_admission_attempts_per_minute = 6
remote_max_unauthenticated_per_ip = 2
remote_max_admission_peers = 4096
remote_max_paired_devices = 64
max_frame_bytes = 1048576
max_prompt_bytes = 131072
max_label_bytes = 256
max_identifier_bytes = 128
max_ticket_bytes = 1024
max_runtime_value_bytes = 4096
max_capabilities = 16
```

Catalog reconciliation advances at most one configured batch per shared
`session_catalog_reconcile_interval_ms`. A drift sweep needs approximately
`ceil((payload files + catalog rows) / session_catalog_reconcile_entries)`
successful list-triggered passes; every page remains explicitly `incomplete`
until the sweep finishes. Timed-out blocking jobs retain their concurrency
permit until SQLite or the bounded directory scan returns.

### `[tui]` — Interactive terminal frontend

These limits apply to `daimonos agent --interactive`. History is process-local
and is not persisted across launches.

| Setting | Default | Description |
|---------|---------|-------------|
| `history_entries` | `100` | Maximum submitted prompts retained for Up/Down navigation. Valid range: 1–10,000. |
| `scrollback_entries` | `2000` | Maximum entries retained independently for transcript lines and tool cards. Valid range: 1–50,000. |

```toml
[tui]
history_entries = 100
scrollback_entries = 2000
```

### `[acp]` — Native agent protocol (`acp`)

These settings apply to native ACP integrations such as Zed.

| Setting | Default | Description |
|---------|---------|-------------|
| `session_list_page_size` | `50` | Maximum saved sessions returned by one `session/list` response. When more remain, the response includes an opaque continuation cursor. Must be greater than zero. |
| `approval_timeout_secs` | `120` | Maximum seconds a `session/request_permission` may go unanswered. Daimonos then denies the tool call and tells the model that the client did not answer. The limit applies while the client is connected. Must be greater than zero. |

```toml
[acp]
session_list_page_size = 50
approval_timeout_secs = 120
```

#### `[acp.mcp.oauth_servers.<name>]` — outbound OAuth policy

A *non-secret policy* is routing/configuration metadata: the endpoint URL,
OAuth opt-in and local profile, optional public client ID and scopes. It does
**not** include access/refresh tokens or client secrets. Policies belong in
Daimonos config; grants belong in `~/.blue_rose/mcp-oauth/`.

Outbound MCP OAuth is opt-in per endpoint. `mcp auth login` performs
protected-resource and authorization-server discovery, authorization-code +
PKCE in a browser with a localhost callback, and writes a private grant.
When ACP starts (or a new session rebuilds its bridge), a capability-protected
loopback proxy forwards MCP traffic to the configured HTTPS endpoint, refreshes
expiring grants, and prevents bearer tokens from reaching the SDK, Zed, tool
schemas, or redirect targets. Other servers remain available when one requires
authorization. A live Notion consent/fetch test still requires the user's
workspace selection and has **not** been run.

```toml
[acp.mcp.oauth_servers.notion]
url = "https://mcp.notion.com/mcp"
profile = "meetalix"
# client_id = "..." # Daimonos client ID, never assumed to be Zed's
# scope = "..."
```

Policies match canonical HTTPS endpoint URLs, require unique URL/profile
pairs, and reject **all** forwarded headers on OAuth-enabled endpoints.
Keep server-specific headers in non-OAuth configurations; this boundary prevents
an arbitrary upstream header from carrying sensitive data past the proxy. `daimonos mcp auth login
notion` displays an authorization URL and attempts to open it in the browser;
select the intended workspace in the consent screen. `daimonos mcp auth status
notion` reports nonsecret grant status; `daimonos mcp auth logout notion`
removes *only Daimonos's* local grant. Private files under
`~/.blue_rose/mcp-oauth/` use a mode `0700` directory and mode `0600` files
with atomic replacement; they are not encrypted at rest. Never put a token in
Zed settings or the TOML policy. Authorization must be initiated outside an ACP
agent turn. Reconnect or start a new ACP session after login to discover tools.

#### `[acp.mcp]` — MCP-server bridge

Zed forwards every configured context server to the ACP agent on `session/new`
and `session/load`. When the bridge is enabled, daimonos connects to each as an
MCP client, discovers its tools, and exposes them to the model as
`mcp__{server}__{tool}` alongside the native daimonos tools (ADR-003, #990).
Native tools always win a name collision. A server that fails to start,
connect, or list tools is skipped (fail-open) — the session still gets the
remaining servers and all native tools. Remote tool calls flow through the same
permission hooks as native destructive tools and are attributed in analytics
under their `mcp__…` name.

By default, identical transport configurations are initialized once and leased
from a process-wide pool across ACP sessions. Tool names/routes and analytics
attribution remain per session; the last lease shuts the client down. Disable
pooling for context servers whose internal state must be isolated per chat.

| Setting | Default | Description |
|---------|---------|-------------|
| `enabled` | `true` | Master switch. When `false`, forwarded `mcp_servers` are ignored and the `mcp` agent capability is not advertised. |
| `allow_stdio` | `true` | Accept + advertise stdio-transport servers (`command`/`args`/`env`). |
| `allow_http` | `true` | Accept + advertise HTTP-transport servers (`url`/`headers`). |
| `shared_pool_enabled` | `true` | Reuse identical initialized clients across ACP sessions in one process. Set `false` for servers requiring per-chat state isolation. |
| `init_timeout_secs` | `10` | Per-server budget for the `initialize` + `tools/list` handshake. Exceeding it skips that server. Must be > 0 when enabled. |
| `call_timeout_secs` | `60` | Per remote `tools/call` budget. On timeout the model gets an error tool result and the turn continues. Must be > 0 when enabled. |
| `shutdown_timeout_secs` | `5` | Maximum MCP runtime/child shutdown wait. Expiry is logged and ACP teardown continues. Must be > 0 when enabled. |
| `teardown_timeout_secs` | `10` | Overall budget for concurrently shutting down every session bridge after ACP transport loss. Must be > 0 when enabled. |
| `max_servers` | `32` | Upper bound on forwarded servers connected per session. Must be > 0 when enabled. |
| `max_concurrent_connects` | `8` | Maximum server handshakes in flight at once. Results are registered in forwarded order for deterministic tool names. Must be > 0 when enabled. |
| `max_tools_per_server` | `128` | Upper bound on tools registered from any single server. Must be > 0 when enabled. |

```toml
[acp.mcp]
enabled = true
allow_stdio = true
allow_http = true
shared_pool_enabled = true
init_timeout_secs = 10
call_timeout_secs = 60
shutdown_timeout_secs = 5
teardown_timeout_secs = 10
max_servers = 32
max_concurrent_connects = 8
max_tools_per_server = 128
```

### `[reviewer.audit]` — Local run audit bounds

Bounds for the caller-requested local run audit (`--audit-path`, WP08/FR-04).
They apply only when the audit is enabled.

```toml
[reviewer.audit]
# Maximum serialized bytes for one audit event. A larger event is a required-
# audit failure (exit 23), never a silent truncation.
max_event_bytes = 65536
# Maximum number of audit events recorded for one run.
max_events = 100000
```

Exceeding either bound is a required-audit write failure (exit `23`,
`audit_write_failed`), consistent with the fail-closed contract above — the run
stops rather than silently dropping or truncating a record.

### `[logging]` — Runtime Diagnostics

Daimonos writes structured JSON logs to a rotating file independently of the
ACP/MCP transport. Protocol stdout is never used for logging. Warning and error
events are also mirrored to stderr so hosts such as Zed can surface failures.
Both outputs allow only tracing targets owned by Daimonos; dependency events
and raw MCP child-process stderr are dropped because they may contain secrets.
The log directory is restricted to mode `0700` and matching log files to
`0600` on Unix.

Daimonos-owned events intentionally exclude prompts, API keys, MCP headers,
tool arguments, and file contents. They do retain operational metadata needed
for diagnosis, including absolute workspace paths, session IDs, MCP server
names, and tool names.

| Setting | Default | Description |
|---------|---------|-------------|
| `enabled` | `true` | Enables persistent structured logging. Initialization failure is fail-open and reported on stderr. |
| `level` | `info` | File filter: `trace`, `debug`, `info`, `warn`, `error`, or `off`. |
| `stderr_level` | `warn` | Stderr filter. ACP/MCP stdout remains protocol-only. |
| `directory` | `$XDG_STATE_HOME/daimonos/logs` or `~/.local/state/daimonos/logs` | Rotated log directory. `~` is expanded. |
| `file_prefix` | `daimonos` | Rotated filename prefix. |
| `rotation` | `daily` | `hourly`, `daily`, or `never`. |
| `max_files` | `14` | Maximum retained rotated files. Must be greater than zero. |
| `resource_interval_secs` | `15` | Process telemetry interval. Set to `0` to disable. |

```toml
[logging]
enabled = true
level = "info"
stderr_level = "warn"
# directory = "~/.local/state/daimonos/logs"
file_prefix = "daimonos"
rotation = "daily"
max_files = 14
resource_interval_secs = 15
```

Each process emits lifecycle, ACP session, workspace-index, MCP bridge, remote
tool timing, and shutdown events. `daimonos::telemetry` snapshots include PID,
uptime, RSS, thread count, open file descriptors, cumulative CPU scheduler
ticks, and interval CPU-tick deltas. To investigate a hot loop, temporarily set
`level = "debug"` and follow the newest file in the configured directory.

### `[observability]` — OTLP/Langfuse Tracing

Optional OpenTelemetry export provides a causal trace view of agent prompts,
LLM generations, tools, and context management (ADR-006). It is disabled by
default and does not replace local SQLite analytics or secure process logs.
Export uses a bounded background queue: a full queue drops spans rather than
blocking an agent turn, and initialization/export/shutdown failures are
fail-open.

Only tracing spans with the dedicated `daimonos::observability` target may
cross the OTLP boundary. Existing diagnostic events can contain local paths and
are explicitly excluded. Prompt, source, command, tool payload, model output,
thinking, headers, and credentials are not exported under defaults.

See **[observability.md](observability.md)** for the operational runbook: cloud
and self-host setup, the smoke test, credential rotation, sampling, retention,
overhead budgets, troubleshooting, disable/rollback, and comparing models and
token-saving strategies.

| Setting | Default | Description |
|---------|---------|-------------|
| `enabled` | `false` | Enables OTLP trace export. Missing credentials disable export without failing the runtime. |
| `endpoint` | `http://localhost:3000/api/public/otel/v1/traces` | Exact OTLP/HTTP traces endpoint. Langfuse Cloud endpoints are region-specific. |
| `basic_auth` | `true` | Sends Basic Auth credentials. Set false for an unauthenticated local collector. |
| `basic_auth_username_env` | `LANGFUSE_PUBLIC_KEY` | Environment-variable name containing the Basic Auth username/public key. |
| `basic_auth_password_env` | `LANGFUSE_SECRET_KEY` | Environment-variable name containing the Basic Auth password/secret key. |
| `environment` | `development` | Deployment environment resource label. |
| `release` | unset | Optional release identifier. |
| `sample_ratio` | `1.0` | Parent-based sampling ratio from `0.0` through `1.0`. |
| `max_queue_size` | `2048` | Maximum completed spans awaiting export. |
| `max_batch_size` | `512` | Maximum spans per request; cannot exceed queue size. |
| `batch_delay_ms` | `5000` | Maximum delay before exporting a partial batch. |
| `flush_timeout_ms` | `3000` | Bounded shutdown/flush timeout. |

```toml
[observability]
enabled = false
endpoint = "http://localhost:3000/api/public/otel/v1/traces"
basic_auth = true
basic_auth_username_env = "LANGFUSE_PUBLIC_KEY"
basic_auth_password_env = "LANGFUSE_SECRET_KEY"
environment = "development"
# release = "v0.1.1"
sample_ratio = 1.0
max_queue_size = 2048
max_batch_size = 512
batch_delay_ms = 5000
flush_timeout_ms = 3000
```

Credential values are read only from the named environment variables and are
never included in configuration dumps or initialization errors. Langfuse's
generic OTLP base endpoint ends in `/api/public/otel`; because Daimonos
configures the signal-specific traces exporter directly, its endpoint includes
the required `/v1/traces` suffix.
Content capture is not currently configurable: export is unconditionally
metadata-only. A future redacted content mode will add an explicit opt-in only
when its privacy tests and limits ship with it.

### Offline evidence workers (ADR-032)

Start the tool process with `daimonos --workspace REPO --offline-tools mcp`.
For local IPC, add `--socket PATH` after `mcp`. `tool-contract` also accepts the profile.
Use a separate process for the model. Keep the run audit on that side.

The worker admits `ls`, `read_file`, `search` (content and filenames), and `workspace_info`.
A caller allowlist can narrow that set. Selecting an incompatible tool or runtime mode stops startup with `offline_profile_incompatible` (exit 2).
A direct call to an unsupported tool returns `offline_tool_forbidden`. This includes `git_read`, subprocess tools and composite calls.

Offline mode disables analytics collection, OTLP export, KGL producers, model-assisted output formatting, and plugin startup probes.
It does not grant filesystem confinement or promise a read-only workspace. Use the filesystem flags for those controls.
Retain a no-network container or namespace for defense in depth. See the [network producer inventory](contexts/reviewer-runtime/INVENTORIES.md) for the scope and acceptance check.

### `[analytics]` — Token & Latency Tracking

Daimonos records every MCP tool call (request/response token estimates,
execution time, redirect/filter/dedup signals) to a SQLite database for
cross-session reporting. Pass `--analytics-off` to disable collection for one run,
even if the configuration enables it. The flag does not disable OTLP export or
a requested local audit. It does not block network access.

| Setting | Default | Description |
|---------|---------|-------------|
| `enabled` | `true` | Master switch. When `false`, nothing is tracked and the SQLite file is never opened. |
| `db_path` | `~/.daimonos/analytics.db` | Path to the SQLite store. Created on first run with WAL mode. |
| `retention_days` | `90` | Records older than this are auto-pruned roughly every 100 inserts. |

```toml
[analytics]
enabled = true
# db_path = "~/.daimonos/analytics.db"
retention_days = 90
```

The data is exposed three ways:

- **`session_stats` MCP tool** — query `session`, `history`, or `daily`
  scope. The `session` scope echoes the live `external_session_id` so
  the agent can confirm correlation is wired up. `history` and `daily`
  accept an optional `external_session_id` argument that filters the
  result set to a single agent-runtime session.
- **`workspace_info` MCP tool** — includes a compact `analytics`
  summary block (calls / req_tokens / resp_tokens / saved_tokens /
  redirects / filters / dedup_hits / db_path / external_session_id).
- **`daimonos --stats`** — human-readable report. Pass
  `--session-id <id>` to filter the history/daily blocks to a single
  agent-runtime session id (useful with
  `claude --session-id $SID` / `DAIMONOS_AGENT_SESSION_ID=$SID`).

### Correlating with agent-runtime sessions

Daimonos's analytics rows include an optional `external_session_id`
column so per-tool-call tokens can be joined post-hoc with the agent's
own usage logs. Two ways to attach the id:

1. **Bootstrap from environment** — set `DAIMONOS_AGENT_SESSION_ID`
   before launching daimonos. The MCP server reads it once at startup
   and stamps every analytics row in that connection with that id.
   Most useful when you control the launch (CI, benchmark harnesses,
   `claude --session-id $SID`).
2. **Mid-session via MCP tool** — call
   `set_external_session_id({"id": "<sid>"})`. Subsequent rows pick up
   the new id. Pass `""` to clear. Useful when the agent is launched
   by an editor (Cursor, Claude Code Desktop) and the user can't set
   environment variables on the daimonos subprocess.

```bash
# Bench-harness pattern: same UUID on both sides.
SID=$(uuidgen)
DAIMONOS_AGENT_SESSION_ID=$SID claude --session-id "$SID" \
    --mcp-config .cursor/mcp.json --strict-mcp-config \
    --tools "" "Refactor the login flow"

# Later, query just this session's daimonos-side cost:
daimonos --stats --session-id "$SID"
```

### `[discord]` — Discord Integration Foundation

These settings define bot-token auth, allowlists, and token-budget limits for
Discord integration. Phase 1 defaults are conservative: integration disabled,
deny-by-default allowlists, and read-only behavior.

| Setting | Default | Description |
|---------|---------|-------------|
| `enabled` | `false` | Enables Discord integration behavior and startup validation |
| `bot_token_env_var` | `"DISCORD_BOT_TOKEN"` | Env var name used to load the bot token |
| `api_base_url` | `"https://discord.com/api/v10"` | Discord REST API base URL |
| `allow_guild_ids` | `[]` | Explicitly allowed guild IDs (Discord snowflakes) |
| `allow_channel_ids` | `[]` | Explicitly allowed channel IDs (Discord snowflakes) |
| `max_messages_per_call` | `100` | Max messages returned by a single read/search call |
| `max_message_chars` | `4000` | Per-message text truncation cap |
| `max_response_chars` | `32000` | Total response payload cap for Discord tool output |
| `read_only_default` | `true` | Keeps write-style actions disabled by default |
| `rate_limit_max_retries` | `2` | Max retry attempts when Discord responds with HTTP 429 |
| `rate_limit_max_sleep_ms` | `10000` | Max per-retry sleep duration (ms) when honoring retry hints |

```toml
[discord]
enabled = false
bot_token_env_var = "DISCORD_BOT_TOKEN"
api_base_url = "https://discord.com/api/v10"
allow_guild_ids = []
allow_channel_ids = []
max_messages_per_call = 100
max_message_chars = 4_000
max_response_chars = 32_000
read_only_default = true
rate_limit_max_retries = 2
rate_limit_max_sleep_ms = 10_000
```

### `[kgl]` — Knowledge-Graph Layer

Tunables for the KGL code+intent knowledge graph. KGL is itself gated on the
`DAIMONOS_KGL_AUTOINDEX` / `DAIMONOS_KGL_OBSERVE` environment variables; these
values govern its SQLite access and background file-watcher when enabled.

| Setting | Default | Description |
|---------|---------|-------------|
| `busy_timeout_ms` | `5000` | SQLite busy-timeout (ms) on every KGL store connection. With WAL journaling (enabled automatically) this lets the watcher's writer and the `kgl_query`/`kgl_assert` readers wait briefly for a lock instead of erroring with `SQLITE_BUSY`. |
| `max_watches` | `4096` | Hard cap on inotify watches the KGL file-watcher registers, bounding `fs.inotify.max_user_watches` usage. |
| `debounce_secs` | `2` | Debounce window for coalescing change bursts into at most one graph rebuild. |
| `orient_max_matches` | `12` | Max task-matching defs `kgl_query orient` expands in one bundled call (each adds its edges + dependents), bounding the orient response size. |
| `find_max` | `200` | SQL LIMIT applied to every `kgl_query find` result set; caps how many nodes a broad LIKE query can materialise in one call. |
| `blast_radius_max` | `500` | Hard node cap for `blast_radius` BFS; stops dense call graphs from exhausting CPU/memory during a transitive traversal. |
| `skip_dirs` | `["target", ".git", ".jj", "node_modules", ".kgl", "graphify-out"]` | Directory base names never walked when detecting/indexing a substrate or registering watches. |

```toml
[kgl]
busy_timeout_ms = 5000
max_watches = 4096
debounce_secs = 2
orient_max_matches = 12
find_max = 200
blast_radius_max = 500
skip_dirs = ["target", ".git", ".jj", "node_modules", ".kgl", "graphify-out"]
```

### `[prompts]` — Model-Facing Prompt Overrides

The text that steers daimonos's behavior is embedded in the binary as defaults
but can be overridden at runtime without recompiling. Prompt-file contents
**replace** their built-in defaults. The tool-description catalog is a partial
overlay: omitted tools and variants keep their embedded values. Unset/empty keys
use the embedded default; unreadable files warn and fall back. `~` expands to
`$HOME`. Prompt files are sent to the model verbatim, so do **not** put comments
inside them. See `prompts/README.md` for the committed defaults and guidance.

```toml
[prompts]
# agent_system     = "~/.config/daimonos/prompts/agent_system.md"
# mcp_instructions = "~/.config/daimonos/prompts/mcp_instructions.md"
# kgl_hint         = "~/.config/daimonos/prompts/kgl_hint.md"
# summary          = "~/.config/daimonos/prompts/summary.md"
# loop_steer       = "~/.config/daimonos/prompts/loop_steer.md"
# cancelled_turn   = "~/.config/daimonos/prompts/cancelled_turn.md"
# skill_catalog    = "~/.config/daimonos/prompts/skill_catalog.md"
# tool_descriptions = "~/.config/daimonos/prompts/tool_descriptions.toml"
```

| Key | Used by | Purpose |
|-----|---------|---------|
| `agent_system` | `daimonos agent` / `chat` / ACP | Core agent system prompt (tool-use strategy, `execute_script` preference). |
| `mcp_instructions` | `daimonos --mcp` | Server instructions sent to the MCP host, including the terse-output directive that affects output token cost. |
| `kgl_hint` | `daimonos --mcp` (KGL auto-index only) | Nudge to orient via the knowledge graph before reading source. |
| `summary` | context compaction | System prompt for the summarizer that replaces evicted turns. |
| `loop_steer` | `daimonos agent` / `chat` / ACP | Corrective steer rotated by the deterministic loop detector. |
| `cancelled_turn` | `daimonos chat` / ACP | Safety note retained for cancelled turns and unresolved tool calls. |
| `skill_catalog` | `daimonos agent` / `chat` / ACP | Introduction prepended to the bounded metadata-only Agent Skills catalog. |
| `tool_descriptions` | MCP / `agent` / `chat` / ACP | Partial TOML overlay for full/terse tool descriptions and nested `[tool.parameters]` JSON Schema property descriptions. |

**Getting the baseline defaults**: the defaults are embedded in the binary, so
you don't need the source to see or copy them:

```bash
daimonos --print-prompt mcp_instructions      # print one default to stdout
daimonos --dump-prompts                        # scaffold all eight resources into
                                               #   ~/.config/daimonos/prompts/
daimonos --dump-prompts /path/to/dir           # ...into a custom directory
daimonos --dump-prompts --force                # overwrite existing files
```

`--dump-prompts` writes the seven `<name>.md` prompts and
`tool_descriptions.toml` (skipping existing files unless `--force`), then prints
a ready-to-paste `[prompts]` block. Start from these so an override begins at —
and can be diffed against — the baseline.

**Additional agent instructions**: `daimonos agent`, `daimonos chat`, and ACP
append `~/.config/daimonos/agent-instructions.md` to the resolved
`agent_system` prompt when that file exists. If `$XDG_CONFIG_HOME` is set, it
replaces `~/.config`. Override the file for a run with the global flag:

```bash
daimonos agent "task" --agent-instructions /path/to/rules.md
daimonos chat --agent-instructions /path/to/rules.md
daimonos acp --agent-instructions /path/to/rules.md
```

The additional file is appended verbatim with a blank-line separator. A missing
default file is silently ignored. An explicit unreadable override — or a default
file that exists but cannot be read — is an error, preventing configured rules
from being silently omitted. This does not apply to `daimonos --mcp`.

**Warning**: these change how the agent behaves. Removing the `execute_script`
guidance or the terse directive typically **increases** token usage. Override
deliberately and prefer editing copies over the committed defaults so you can
compare against the baseline.

`summary` is also overridable via the `DAIMONOS_AGENT_SUMMARY_PROMPT` agent-env
variable, which takes precedence over `[prompts].summary`. Precedence:
`DAIMONOS_AGENT_SUMMARY_PROMPT` > `[prompts].summary` > embedded `summary.md`.

### `[tools.<id>]` — Tool Plugins (Advanced)

Register external tools for the tool runner system. Most users don't need
this — the built-in plugins for git, cargo, gh, and docker are auto-registered
when the binaries are found on `PATH`.

```toml
[tools.x07]
bin = "/path/to/x07"
source_pattern = "**/*.x07.json"
manifest = "x07.json"
```

## Environment Variables

These are not part of the config file but affect daimonos behavior:

| Variable | Description |
|----------|-------------|
| `DAIMONOS_IDLE_TIMEOUT_SECS` | Overrides `[mcp] idle_timeout_secs` when set to a parseable integer. `0` disables the idle watchdog. Used by tests. |
| `DAIMONOS_LOG_STARTUP` | When non-empty and not `0` / `false` / `no`, enables MCP stderr startup diagnostics before config load (same family as `--verbose` / `[mcp] startup_logs`). |
| `DAIMONOS_AGENT_SESSION_ID` | Optional agent-runtime session id (e.g. a UUID matching `claude --session-id`). When set, every analytics row recorded by this daimonos process is tagged with this id, and `daimonos --stats --session-id <id>` / `session_stats {external_session_id: <id>}` will filter to it. The `set_external_session_id` MCP tool can override this at runtime. |
| `DAIMONOS_AGENT_SUMMARY_PROMPT` | Overrides the context-compaction summarizer prompt. Takes precedence over `[prompts].summary` and the embedded default. |
| `DISCORD_BOT_TOKEN` | Default Discord bot token variable when `[discord].bot_token_env_var` is unchanged. |
| `PATH` | Daimonos inherits the launching process's `PATH` for exec/bg commands |

## Performance Tuning Tips

**Large monorepos**: increase `max_depth`, `max_files`, and
`max_walk_entries` together if important paths fall outside reported
coverage. Add project-specific generated/binary extensions to
`skip_extensions` to spend the path budget on useful files.

**Slow first file search**: use `mode = "hybrid"` or `"eager"` to warm
long-lived sessions, or narrow `path` and `glob` filters. Later searches reuse
the warm path index until its watcher reports a filesystem change.

**Truncated exec output**: if you're losing important build output, increase
`exec_output_max_chars`. The default of 100 KB covers most cases.

### OpenRouter structural request tracing (opt-in)

To retain the tool schemas sent on future OpenRouter requests, set
`DAIMONOS_OPENROUTER_TRACE_DIR` in the **Daimonos process environment** (not
`agent.env`) and restart the agent. For example:

```sh
export DAIMONOS_OPENROUTER_TRACE_DIR="$HOME/.local/state/daimonos/openrouter-traces"
```

The parent directory must exist. The trace directory is created with mode 0700;
existing directories must be private and owned by the current user. Trace files
use mode 0600. On Unix the final directory component and files cannot be
symlinks, files cannot be hard-linked, and opens/rotation are relative to a
validated directory handle. Parent symlinks are resolved at directory open;
this does not isolate tracing from other processes running as the same user.
Use a trusted local filesystem (filesystem IO itself is not time-bounded).
Each JSONL record has a local UUID/timestamp, model, streaming flag,
message count, and the exact outgoing `tools` array. It excludes messages,
tool-call arguments/results, URLs, HTTP headers, and API keys. **Schemas may still
contain private descriptions, defaults or examples: treat these files as sensitive.**

`requests.jsonl` rotates into `requests.jsonl.1` at 16 MiB (at most 32 MiB of
retained payload); oversized records are skipped, not truncated. Lock contention
or trace IO failures skip the record and emit a generic warning without failing
the model request. Unset the variable to disable tracing and delete both files
to remove retained data. Rotation replaces old history; archive needed evidence
securely before it is rotated out.

This captures Daimonos's outgoing schema, not OpenRouter's downstream transformed
request. It cannot recover older requests, prove downstream behavior, or correlate
raw model responses; the UUID is local, not a provider request ID.

## File evidence digests

Pass `--read-sha256` to add a lowercase SHA-256 digest to `read_file` and `git_read` with `command: show_file`. The digest covers all stored bytes, including original line endings. The offset, line limit, output cap, and unchanged-file cache do not change its meaning.

A text result carries `sha256`. A binary `read_file` result carries `binary`, `size`, and `sha256` without content. A Git binary or output-limit refusal can carry `metadata.sha256`. The Git digest identifies the pinned blob bytes, not the current worktree or the Git object header. Strict-pinning rules still apply.

Supply `--tool-input-bytes` and `--tool-timeout-ms` to bound this work. A one-line page can require a full file read. A budget failure returns no digest. When `read_file` detects a change on the opened handle, it returns `file_changed`; retry the read. This check is not a filesystem snapshot. Large filesystem reads still buffer the file; use a byte limit for untrusted sizes.

Digest mode bypasses model-assisted reformatting for these two tools. A Git output cap too small for the complete metadata error stops startup with `digest_metadata_cap`. Without `--read-sha256`, the existing read shapes and Git cap behavior stay unchanged.

## Read-only git (`git_read`)

`git_read.strict_pinning` is false by default. Enable it to require explicit
40-character SHA-1 commit identifiers. `diff` and `ancestry` then require `a` and
`b`. Other permitted operations require `rev`. Names, ranges, implicit HEAD,
short hashes, and annotated-tag object identifiers are rejected. `refs` and
`worktree_status` are refused under this policy. Default revision resolution remains unchanged.

`log` and `show_commit` accept `include_body=true`. Only that opt-in adds the
`body` field to commit metadata. The existing response cap still applies.
Bodies preserve Git's body text, including any trailing newline. Do not trim them before comparisons.

The runner disables configured clean, smudge, and process filters in both modes.
This is an approved security-fix exception to the unchanged-default rule.
`worktree_status` does not apply those external transformations. It can report a
modified file where ordinary Git reports a clean filtered file, including Git LFS cases.
Do not interpret this result as filter-aware status. Built-in Git conversions can still apply.
[ADR-019](adr/019-ignore-repository-filters.md) records the reason and the human approval.
Status runs against private, frozen metadata with no filter definitions or include directives.
Later changes to repository configuration cannot add executable filters to that process.
`git_read.status_directory` can select an existing metadata directory outside the workspace.
The default is the system temporary directory. The private directory is removed after the call.
`git_read.max_status_metadata_bytes` defaults to 64 MiB and bounds the copied metadata.
Unsupported filter keys or oversized configuration fail closed before reads.
Strict pinning is not required for process hardening. It adds immutable input selection.


The `[git_read]` section bounds local, pinned git reads. `GitReadConfig` in
`src/config.rs` defines the defaults. `daimonos.default.toml` lists every key.
ADR-017 records the safety boundary.

| Key | Controls |
|-----|----------|
| `max_output_bytes` | Captured bytes per stream and serialized response bytes |
| `max_file_bytes` | Blob size before content reads |
| `max_lines` | Lines in `show_file` and `blame` |
| `max_entries` | Tree entries, refs, status entries, and changed files |
| `max_results` | Grep matches |
| `max_commits` | Log entries |
| `max_patch_bytes` | UTF-8 bytes in a patch preview |
| `max_input_bytes` | Request size and repository metadata size |
| `max_alternate_dirs` | Reachable alternate object stores |

Each value must be positive. File and patch caps must not exceed the output cap.
The output cap must fit the minimal structured error response.
The effective response cap is the smaller of `git_read.max_output_bytes` and `tool_output.max_bytes`.
Oversized responses fail before the shared output handler can spill them to disk.
Caller limits can reduce a cap, but cannot increase it.
`max_alternate_dirs` counts the primary store and its reachable alternate stores.
The tool name `git_read` is reserved. A generic CLI configuration cannot replace it.

`repo` names a repository root relative to the workspace, not the session's
current directory. Git directories and alternate stores must also stay inside
the workspace after symlink resolution. `worktree_status` ignores submodules.
The tool requires git with `--no-lazy-fetch` support. Older git does not register it.
History reads support bare repositories. `worktree_status` rejects them with `bare_repository`.
Grep rejects stderr diagnostics instead of reporting a potentially incomplete search as successful.
Malformed nested ranges return `rejected_argument`.

Every successful result includes `repo` and `commits`. Annotated tags retain
their object SHA and report their peeled commit SHA separately. Tags that name
non-commit objects report a null commit SHA. `log` accepts one revision or an
explicit `a..b` range. `diff` requires `a` and `b`, or `rev=a..b`.
`show_commit` compares a merge commit with its first parent.

`offset` starts at zero. `next_offset` continues a truncated page with the same
arguments. Blame uses one-based `start_line` and `end_line`. Its continuation
keeps those arguments and changes only `offset`. Patch previews do not support
continuation. A truncated preview is not a complete patch.

Failures return `error.kind` and `error.message`. Required kinds are
`unknown_revision`, `path_not_found`, `object_missing`, `binary_file`,
`repo_outside_workspace`, and `rejected_argument`. Binary errors include metadata,
not content. Additional kinds describe invalid repositories, I/O failures,
unavailable tools, and git errors. Oversized captures or responses return
`output_limit` with `truncated: true`. They never return partially parsed records.

Agent mode can grant `git_read` without granting `git`:

```dotenv
DAIMONOS_AGENT_ALLOWED_COMMANDS=read_file,search,workspace_info,git_read
```

MCP mode does not enforce this agent allowlist. MCP clients must expose only the
required read tools. Do not expose `exec`, `batch`, or `execute_script` to a
strictly read-only reviewer. Their internal calls do not apply the agent policy.

## Active-turn crash recovery

When chat persistence is enabled, SessionCore writes recovery projections at authoritative turn boundaries.
`DAIMONOS_CHAT_PERSIST` controls whether ACP sessions have persistence.
Checkpoint saves reuse the configured persistence retries and writer-generation guards.
They do not store individual streamed tokens or commit the live turn early.
The session file format remains unchanged.

ACP shutdown drains cancelled turns and MCP bridges within `[acp.mcp].teardown_timeout_secs`.
A timeout does not remove the latest completed recovery checkpoint.
Storage failures remain visible as degraded durability until a full save succeeds.

[ADR-020](adr/020-durable-turn-checkpoints.md) explains the design and rollback behavior.
[The glossary](contexts/session-durability/CONTEXT.md) defines checkpoint and canonical history.

## Structured final output (`--final-output`)

Opt-in (FR-01 / ADR-024). `daimonos agent --final-output <PATH>` makes a one-shot `--print` run end with a single schema-validated JSON document on stdout. The control file is JSON with these keys:

- `schema` *or* `schema_path` (mutually exclusive; exactly one required) — the JSON Schema draft 2020-12 document for the final answer.
- `tool_name`, `tool_description` — the final-answer tool the model sees. The schema is offered unchanged (no strict-subset rewrite). `tool_name` must not collide with a registered tool (startup exit 2).
- `corrective_retries` — extra ending attempts after the first (dbunk uses 1).
- `max_model_requests` (optional) — inclusive ceiling on model requests for the whole run; exceeding it fails with reason `turn_limit` (exit 24).

Success emits only the validated argument JSON and one newline (exit 0). A failed ending writes a bounded JSON error object to stderr with `reason`, `validation_errors`, `attempts`, and `model_requests` (exit 20). An external schema reference is refused before any model request (`schema_unusable`, exit 20). Default off leaves agent mode unchanged.

## `--tool-allowlist PATH` (FR-02, WP06)

Enforce an explicit tool allowlist for a run. `PATH` is a JSON file holding an array of exact exported tool names, e.g. `["read_file", "search", "workspace_info"]`.

- **Default off.** Omitting the flag keeps legacy behavior (all tools allowed).
- An empty array `[]` grants no tools (deny-all), distinct from omitting the flag.
- The model sees **exactly** the allowlisted tools in every listing, including lazy/`list_all_tools` listings.
- Every call is checked before it runs, at every direct, batch, Starlark, `execute_script`, and remote/worker dispatch boundary; a composite must itself be listed and its inner calls are each checked.
- An entry matching no known tool fails the run at startup (exit 2, `run_configuration_invalid`, reason `tool_allowlist_unknown_entry`). With `--final-output`, the designated final-answer tool must be listed explicitly (never added automatically).
- Caller-owned: read only from this flag, never from workspace/user config or the environment; it can only narrow existing restrictions (the effective set is their intersection).
- At startup the effective allowlist is reported on stderr: `{"effective_tool_allowlist": [...]}`. This is the intersection with the run's active legacy restriction (the agent-env `allowed_commands`), i.e. the set the run will actually permit — not necessarily the raw list.
- Scope of the report (FR-02, ADR-023, r3 decision): the `effective_tool_allowlist` reflects the allowlist intersected with the agent-env `allowed_commands` only. The rest of `SafetyPolicy::gate` — `denied_commands`, permission-mode patterns, and approval mode — is NOT folded into this report (a session-free full-gate evaluation at startup is not a small change, and permission/approval yield an *ask*, not an exclusion). Those checks still refuse at the dispatch boundary regardless, so enforcement is unaffected; only the one-line report may list a name that `denied_commands` would additionally block.
- The policy filters every tool-name surface, not just execution: `tools/list`, `list_all_tools`, `get_tool_schema`, `list_tool_signatures`, the MCP server instructions, the session-daemon socket listing, and the agent's offered catalog. An unlisted tool is never served, probed, or dumped.
- Agent-family catalogs (r3 fix, ADR-023 R1/R2): the model-facing catalog of `agent --print`, `daimonos chat`, `daimonos acp`, and the interactive/session-daemon agent is built from one source (`tool_facade::active_schemas_for_policy`), which the allowlist filters — so every one of those frontends offers the model exactly the allowlist (no per-frontend filter). Forwarded/outbound MCP tools offered to the model obey the same allowlist. The embedded Starlark signature block inside the `execute_script` tool *description* is filtered too, so under an active allowlist it never names an unlisted tool (e.g. `write_file`).
- With `--final-output`, if the designated final-answer tool is omitted from the allowlist the run fails at startup (exit 2, `run_configuration_invalid`, reason `tool_allowlist_missing_final_output`). It is never added automatically.
- A config-registered custom local tool (`[tools.*]` in `daimonos.toml`) is a dispatchable name and may be allowlisted. Workspace/user config can neither set nor disable the allowlist (`--tool-allowlist` is the sole source).
- Refusals carry the `tool_not_allowed` code and are recorded in the `--audit-path` record.
- Honored on every frontend and carried in the session-daemon launch identity, so daemon reuse cannot bypass it.
- Composite delegation (r4 fix, ADR-023 R1): the composite opcodes `tool_pipeline` and `lint_repair` run an inner registered tool named by their `tool_id` argument. That inner name is now checked against the allowlist before the tool runs, so a *listed* `tool_pipeline`/`lint_repair` can never reach an *unlisted* registered `[tools.*]` tool (PM Q-FR-02-1). A refused inner tool never runs and the refusal carries `tool_not_allowed`, naming the inner tool.
- Remote and worker tool names (r4 note, R2): in this package the allowlist may name only native tools and config-registered `[tools.*]` local tools. Remote MCP (`server/tool`) forwarded names cannot be placed on the allowlist yet — listing one fails startup as an unknown entry. Caller-supplied worker tool names (`mcp__{worker}__{tool}`), however, MAY be allowlisted when `--worker-config` is set (WP10, FR-05): they are validated against the discovered worker catalog after discovery, and any allowlist entry that names no tool of the frozen run — including a native name such as `read_file`, which dispatches to nothing in worker mode — ends the run at startup (exit 2, `run_configuration_invalid` / `tool_allowlist_unknown_entry`, naming the entry) before the first model request. Consequently, while an allowlist is active every forwarded/outbound MCP tool is removed from the model catalog and refused at dispatch (there is no allowlist entry that could admit it).
- A malformed `--tool-allowlist` file (not valid JSON, or not a JSON array of strings) and an unreadable file both fail the run at startup (exit 2, `run_configuration_invalid`, reason `tool_allowlist_invalid`); the read/parse error is reported and the file is never silently treated as “no list”.

## Same-tool capability probe (`--capability-probe-payload`)

Opt-in (FR-04 requirements 14–17 / ADR-027). `daimonos agent --print --capability-probe-payload <PATH>` sends **exactly one** unretried probe request before any evidence tool runs, to prove the endpoint can drive the *same* final-answer tool the run depends on. `PATH` is a JSON file holding one minimal, schema-valid payload for the final-answer tool.

- **Default off.** Without the flag, behavior is unchanged and no probe is sent.
- **Requires `--final-output` (FR-01) and `--model-profile` (controlled transport).** Omitting either is a startup refusal: exit `2`, `{"error":{"code":"run_configuration_invalid","reason":"capability_probe_requires_final_output"|"capability_probe_requires_model_profile",...}}`, naming the offending option. The probe flag is **agent-only**: `chat`, `acp`, and the session daemon reject it as an unexpected argument; `--dry-run` cannot send a probe and is refused (it already conflicts with `--final-output` at parse).
- **Local payload validation first.** The payload is parsed and validated against the final-answer schema **locally, before anything is sent**. An unreadable file, invalid JSON, or a payload that fails the schema is a caller configuration error (exit `2`, reason `capability_probe_payload_invalid`), and no request is sent.
- **Frozen catalog.** The probe offers the byte-equivalent frozen run tool catalog (the final-answer tool plus every tool the run will offer); no dummy substitute. It instructs the model to call the final-answer tool once with the payload. Text/reasoning beside the single final call is allowed.
- **One pass, everything else stops.** A pass is exactly one schema-valid call to the final-answer tool. Every other outcome — text only, the wrong tool, two or more calls, invalid arguments, or an endpoint error (a 503, a 3xx redirect, or a timeout) — yields the distinct `model_unsupported` exit (`21`) with `{"error":{"code":"model_unsupported","reason":...}}` on stderr and **no further work** (the agent loop never starts; no evidence tool and no returned probe call is executed). The probe is exactly one HTTP attempt even when the run's profile permits retries.
- **Counts against the ceiling.** The single probe request counts against the `--final-output` `max_model_requests` ceiling (the loop is seeded with the probe's request), so the probe cannot smuggle an extra request past the inclusive limit.
- **Audited.** With `--audit-path`, the verdict is recorded as one `probe_result` event (`status` `model_supported`/`model_unsupported`, plus `reason` on failure); the credential value never appears. No live credentialed endpoint is required. The probe request's `http_status`, `retry_index`, and the profile's `wire` are recorded on that same `probe_result` event (the probe is one request to the model endpoint). Its endpoint-reported token counts are recorded on that event as `usage` (the same `{input_tokens, output_tokens}` shape as `model_request.usage`, zero counts when the endpoint reports none). A failed probe also makes the terminal `run_end` record its `exit_code` 21 / `status` `model_unsupported`, matching the process exit. For the agent loop's own requests over the controlled transport, each `model_request` event carries the observed `http_status`, `retry_index`, and `wire`.

Model-facing probe wording lives in `prompts/capability_probe.md` (the system-turn framing) and `prompts/capability_probe_directive.md` (the user-turn directive template, into which only the final-tool name and the payload are interpolated as data). See `src/capability_probe.rs` and ADR-027.
## Restricted local container (FR-10)

Build from a pinned Git tree, not a dirty worktree. The build can download base images, packages, and locked Cargo dependencies. It does not publish an image. Run these commands at the repository root (Docker, Git, and Python 3.11+):

```sh
rev=$(git rev-parse HEAD)
version=$(git show "$rev:Cargo.toml" | python3 -c 'import sys,tomllib; print(tomllib.load(sys.stdin.buffer)["package"]["version"])')
git archive "$rev" Dockerfile .dockerignore Cargo.toml Cargo.lock daimonos.default.toml src prompts |
  docker build --build-arg SOURCE_COMMIT="$rev" --build-arg VERSION="$version" -t "daimonos-local:$rev" -
docker run --rm --pull=never --network none "daimonos-local:$rev" --version
```

The OCI labels `org.opencontainers.image.version` and `org.opencontainers.image.revision` describe the executable. The MCP `serverInfo.version` still reports the package version. The tool contract has its own version. A native build without a supplied commit reports `source_commit unknown`; it does not query the runtime workspace.

Start the worker with a private, writable state mount. The UID and GID of that mount must match the process. Mount only the evidence the worker should see. Keep model credentials and the Docker socket outside the container.

```sh
docker run --rm -i --pull=never --read-only --network none --cap-drop ALL \
  --user 12345:12345 --security-opt no-new-privileges \
  --mount "type=bind,src=$PWD,dst=/review,readonly" \
  --tmpfs /state:rw,noexec,nosuid,nodev,uid=12345,gid=12345,mode=0700 \
  "daimonos-local:$rev" --workspace /review --state-dir /state \
  --workspace-read-only --ignore-config-files mcp
```

For `--worker-config`, use `docker` as the worker command and pass the arguments above as individual array entries. The host runs the model; the container serves the same MCP stdio protocol. Do not allocate a TTY. The no-argument image command still serves MCP in `/workspace`. A different workspace requires only CLI arguments, not an entrypoint override.

To verify an existing local image against the current checkout, set `DAIMONOS_TEST_IMAGE=daimonos-local:$rev` and run `python3 -m pytest tests/test_container_runtime.py tests/test_cli_modes.py`. Without the variable, pytest skips the Docker checks. A skip is not a successful container check.

## Credential-free worker execution (`--worker-config`)

Opt-in (FR-05 / ADR-028). `daimonos agent --worker-config <PATH>` makes a
one-shot `--print` run execute **no tool in the model-side process**. The
model-side process holds the credential and reaches the model endpoint; every
non-final tool call is routed to a caller-supplied MCP-stdio *worker* or
refused. There is no local/native/opcode/script fallback. The final-answer tool
(`--final-output`, FR-01) stays intercepted and validated model-side and is
never forwarded to a worker. Default off leaves agent mode byte-for-byte
unchanged (the legacy ACP bridge remains fail-open).

The control file is JSON:

```json
{ "workers": {
    "toolside": { "command": "/abs/daimonos", "args": ["--mcp", "-w", "/evidence"],
                  "env": { "PATH": "/usr/bin" } },
    "compute":  { "command": "/abs/compute", "env": {} }
} }
```

- `workers` — a required, non-empty map of worker name to definition. Each entry
  has a required `command` (string), optional `args` (array of strings), and
  optional `env` (object of string→string). Names are sorted for a deterministic
  catalog and worker order.
- **Credential-free by construction.** Each worker child is launched from a
  *cleared* environment (`env -i`) plus **only** its explicit `env` map, so no
  model credential, provider configuration, or inherited ambient variable
  reaches it (AC-05.1). Executable lookup is caller-controlled: pass an absolute
  `command`, or include `PATH` in `env`.
- **Discovery before freeze.** All workers are connected and their tools
  discovered (`initialize` + `tools/list`) before the model-visible catalog is
  frozen. Tools are exported as `mcp__{worker}__{tool}`, exactly like the
  forwarded bridge, so the `--tool-allowlist` (FR-02) and `--audit-path`
  (FR-04) treat them uniformly. The model-visible catalog is the
  worker-discovered tools only; no native in-process tool is offered.
- **Admission.** A worker tool's only run-policy gate is the `--tool-allowlist`
  check at the agent-loop dispatch boundary (`run_policy::ToolPolicy::allows`).
  Worker tools are the caller's sandbox: they are **not** path-checked and are
  **not** refused by `--workspace-read-only` (PM FR-05). The caller must
  separately sandbox the workers (stdio alone is not proof of confinement).
- **Fail-closed.** A malformed, unreadable, or empty control file fails at
  startup with `run_configuration_invalid` (exit 2) before any model request or
  worker launch. A required worker that cannot launch or handshake aborts the
  run with `worker_unavailable` (exit 22) and never falls back to a local tool.
- **No-fallback liveness.** A worker that exits, closes stdio, or stalls during
  a call ends the run with the distinct `worker_unavailable` failure (exit 22);
  the failed dispatch is recorded in the `--audit-path` record (under the
  `worker` route with reason `worker_unavailable`) and **no further tool runs**.
- **Agent-only.** `--worker-config` is honored only by the one-shot `--print`
  path. It conflicts with `--interactive` and `--dry-run` (refused at startup by
  the argument parser), and the `chat`, `acp`, and session-daemon frontends do
  not carry it at all (unexpected-argument error). An entry in `--tool-allowlist`
  that names a worker-scoped tool (`mcp__…`) is cross-checked after worker
  discovery and before the first model request: an entry that no worker serves
  fails the run at startup (exit 2, `run_configuration_invalid`, reason
  `tool_allowlist_unknown_entry`, naming the entry), never silently later at
  dispatch. Native and final-answer names are validated earlier at parse.
- **No credential disclosure.** Beyond the cleared environment, a worker whose
  explicit `env` value or `args` entry *discloses* the resolved model credential
  is refused before any worker starts (exit 2, `run_configuration_invalid`,
  reason `worker_credential_disclosure`). The diagnostic names the worker and
  the location (an env value, or `args[index]`) but never the credential value.
- **Launcher.** The cleared-environment launcher (`env`) is resolved by
  ABSOLUTE path (`/usr/bin/env`, then `/bin/env`), never through an inherited
  `PATH`. A `command` containing `=` is rejected at parse (it would be misparsed
  by `env` as an assignment). On a platform that ships no `env` at those paths,
  credential-free worker execution is unavailable and the launch fails with
  `worker_unavailable` (exit 22).
- **Reaping.** Every owned worker child is reaped through the managed lifecycle
  on every ending (normal, worker-abort, final-output failure): teardown signals
  the worker's process group `SIGTERM`, waits a bounded grace, then `SIGKILL`s
  the group — so no worker child (or descendant, e.g. an `unshare` wrapper)
  outlives the run even when a graceful `shut_down` cannot reap it. On non-Unix
  platforms reaping relies on the SDK's `kill_on_drop` at process exit.

See ADR-028 and `tests/test_worker_isolation.py`.

## Cooperative tool budgets (FR-19, ADR-031)

Three default-off, global command-line options bound the resources a single
top-level tool call may consume. They are caller-owned (never read from
`daimonos.toml`) and, when none is set, every tool behaves exactly as before.

- **`--tool-timeout-ms N`** — an absolute deadline, in milliseconds, for the
  call. A hit returns the reviewed `tool_timeout` code with no partial result.
- **`--tool-input-bytes N`** — a logical input-byte ceiling for the call. It
  counts the file content that a tool reads or scans, and the git content that
  `git_read` returns. It never counts the displayed or truncated output bytes.
  A hit returns `tool_input_limit`.
- **`--tool-max-entries N`** *(proposed)* — a directory-entry ceiling for the
  call (e.g. `ls` walking). A hit returns `tool_input_limit` naming
  `directory entries`. (The reviewed interface row has no separate code for the
  entry limit; integrator decision D56 reuses `tool_input_limit`.)

Each value must be a positive integer; `0` is rejected at startup (exit `2`).

**No partial success.** A limit hit is a typed tool failure naming which limit
was hit, the configured value, and the consumed value — never a truncated
"successful" result.

**One budget per call, shared by composites.** A `batch` and an
`execute_script` share one budget with their children: a child inherits the
remaining allowance and the same absolute deadline and cannot open a fresh
top-level budget, so delegation cannot multiply the allowance.

**Stopped work, not a stopped wait.** For `exec` and `git_read` the deadline is
handed to the managed subprocess, which reaps the whole process group
(`SIGTERM`, bounded grace, then `SIGKILL`) — so no child or grandchild outlives
the `tool_timeout` result. On non-Unix platforms the subprocess stop relies on
the managed lifecycle's `kill_on_drop`.

**Output truncation is separate.** `--no-output-spill` bounds the *displayed*
output of a result; it is not an input budget and a truncated result is never a
budget failure.

**Workers.** Worker-routed tool calls (`--worker-config`, WP10) run in a
separate process that this process's budget cannot observe, so the deadline
cannot stop that work and its input cannot be accounted. Rather than silently
ignore a budget option in worker mode, a budgeted worker route is refused at
startup with exit `2` (`run_configuration_invalid`, reason
`tool_budget_worker_unenforceable`). Worker mode with no budget option is
unchanged.

Every frontend that builds a session from configuration honours the budget
(the MCP server, `agent --print`, the interactive agent / session daemon,
`chat`, `acp`, `batch`, and `execute_script`). For the interactive agent the
budget is part of the session-daemon launch identity, so an attach to a daemon
started under a different (or absent) budget is refused.

See ADR-031 and `tests/test_tool_budgets.py`.

#### Tool budgets — enforcement scope (WP18, FR-19)

The default-off options `--tool-timeout-ms`, `--tool-input-bytes`, and
`--tool-max-entries` are cooperative:

- **Deadline.** Each tool call has the deadline. `read_file`, `ls`, and `search`
  check it at work boundaries: between files, and for each read chunk of a
  content search. A subprocess of `exec`, `git_read`, or a plugin is stopped at
  the deadline with its process group (Linux; on other platforms the process
  default timeout applies). A passed deadline returns `tool_timeout`. The check
  is not preemptive: one slow system call can pass the deadline before the next
  check.
- **Input bytes.** `search` in content mode charges the bytes that it scans and
  stops during the scan. `read_file` checks the file size before the read and
  charges the whole file, also when `offset` and `limit` select a part.
  `git_read` charges the bytes of the content that it returns, after git ended.
  That is an approximation of the git content read: it does not stop git early.
- **Entries.** `ls` charges each directory entry during the walk and stops at
  the limit.
- **Refused under an input or entry limit.** With `--tool-input-bytes` or
  `--tool-max-entries`, a tool is admitted only when it meters its input or
  reads no workspace or external content. Each other tool is refused with
  `tool_policy_unsupported`: `exec`, the process plugins (`git`, `gh`, `cargo`,
  `npm`, `pytest`, `docker`, `shellcheck`, `curl`), `edit_file`, `snapshot`,
  `workspace_info`, tools from configuration, outbound MCP tools, and `search`
  in files mode (the index does not report the content that it reads). With
  `--confine-paths`, the offered tools `read_file`, `ls`, and `search` in
  content mode stay available.
- **Refused under each budget option.** `tool_pipeline`, and a run with
  `--worker-config` (at startup).

Output truncation (`--no-output-spill`, `tool_output`) is a separate mechanism,
never reported as a budget limit hit.
