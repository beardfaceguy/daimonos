# ADR-039: The agent frontends do not load the workspace `daimonos.toml`

- **Status:** Accepted
- **Date:** 2026-11-24
- **Deciders:** Repository maintainer, by explicit user decision on 2026-11-24 (the recommended option was shown and accepted).
- **Consulted:** None recorded.
- **Informed:** None recorded.
- **Spec:** Vikunja #1627.
- **Dependencies:** ADR-021 (state root), ADR-022 (confinement), ADR-038 (the workspace `agent.env` is untrusted).
- **Supersedes:** None
- **Superseded by:** None

## Context

Without `--config`, Daimonos took its configuration from the first file that exists: `<workspace>/daimonos.toml`, then the user file `~/.config/daimonos/config.toml`. The first file won completely. The user file was not read when the workspace had a file.

The workspace file comes with the workspace, and a workspace is often a checked-out repository of another party. The configuration decides more than tuning. For the modes that run a model with tools, it names the MCP servers that the run starts, the tools that are registered, and where logs and telemetry go.

ADR-038 made the workspace `agent.env` unable to set the settings that matter for trust. The configuration file had no such rule.

Must a mode that runs a model load a configuration file that comes with the workspace?

## Decision drivers

- A workspace must not decide what an agent run starts or where its data goes.
- The user's own configuration must apply in each workspace.
- A user who wants the workspace file must have a simple way to accept it.
- The MCP server modes are started by a client that the user configured for that workspace. Their behavior must not change without need.

## Decision

We will not load `<workspace>/daimonos.toml` in the agent frontends: `agent`, `chat`, `acp`, and the session daemon.

- These modes load the file that `--config` names, else the user file, else the built-in defaults. `--config <workspace>/daimonos.toml` is the opt-in.
- Only the command line supplies `--config`. No environment variable and no file selects the configuration file, so a workspace cannot opt in for the user.
- When a workspace file exists and is not loaded, startup prints one line on stderr that names the file and the option. Under confinement the line is printed only for a file that the filesystem authority can read, which is the check of the loader on that path.
- The rule applies on each load path: the default path, `--state-dir`, and confinement. Explicit-only mode (`--ignore-config-files`) already loads no implicit file.
- The session daemon gets the `--config` path of the frontend that starts it, as before.
- The other modes (the MCP server over stdio or a socket, the legacy socket daemon, `stats`, `tool-contract`, `mcp-config`, `mcp auth`, `session`) load the workspace file as before.
- `session` (import and export of session archives) runs no model and starts no configured process. It reads three limits from the configuration: the archive size limit, the preview size, and the store busy timeout. The location of the session store does not come from the configuration file.
- `--print-config-path` reports the candidates of the mode that it is run with.

## Alternatives

- **Ignore single settings of the workspace file, as ADR-038 does for `agent.env`.** It keeps harmless tuning. It loses: the configuration has many sections, and each new section would need a trust decision. A missed setting is a silent gap.
- **Load the user file first and let the workspace file fill what it omits.** It loses for the same reason: the workspace would still supply each setting that the user did not set.
- **A store of trusted workspaces with a prompt.** It is the usual design of editors. It loses on cost now: it needs state, a prompt, and a rule for unattended runs. It can be added later, and the option of this decision stays valid with it.
- **Apply the rule to the MCP server modes too.** It loses against the last driver. See Consequences.

## Consequences

- An agent run in a workspace that relied on its `daimonos.toml` now uses the user file or the defaults. The stderr line names the file. The fix for the user is `--config`.
- The user file now applies in a workspace that has its own file. Before, it did not.
- A project cannot ship agent configuration that applies without an action of the user.
- The MCP server modes still load the workspace file. A client that starts `daimonos mcp` in a workspace of another party gets the configuration of that workspace. This decision does not cover that case.
- An editor that starts `daimonos acp` and relied on a workspace file must pass `--config`.

## Confirmation

- `pytest tests/test_workspace_config.py` drives the real binary: `agent` does not load the workspace file and prints the notice; `--config` applies it; the user file applies when a workspace file exists; the rule holds with `--state-dir` and with `--confine-paths`; `acp`, `chat`, and a directly started session daemon follow it; the MCP server loads the file as before.
- The tests for `--state-dir`, confinement, `acp`, `chat`, and the session daemon use a workspace file that the startup validation rejects. The run must succeed without `--config` and must stop with the validation error when `--config` names the file. So a test cannot pass because a message is missing.
- `cargo test agent_frontends_are_exactly_the_modes_that_run_a_model` pins the predicate for each mode. `cargo test search_candidates_order_and_explicit` and `cargo test load_from_workspace_file` cover the candidate list and the loader.
- Mutations, each run on the final tree: `Agent`, `Chat`, `Acp`, or `SessionDaemon` removed from `RuntimeMode::is_agent_frontend`; the rule removed from the confined loader; the rule removed from the default and `--state-dir` loader. Each fails a process test.
- Not covered by a test: the session daemon that `agent --interactive` starts as a child. It gets the mode `SessionDaemon` and the `--config` path of its parent, which are the two inputs that the direct test covers.

## Anchors

_Anchors:_ src/cli.rs::RuntimeMode::is_agent_frontend, src/config.rs::ImplicitConfigFiles, src/config.rs::search_candidates, src/config.rs::load, src/config.rs::load_confined

## Revisit when

A workspace trust store exists, or the MCP server modes need the same rule.

## Open questions

1. Should the MCP server modes stop loading the workspace file by default? Not decided.
