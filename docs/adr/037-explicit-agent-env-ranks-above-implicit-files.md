# ADR-037: An explicitly selected agent env file ranks above the implicit `agent.env` files

- **Status:** Accepted
- **Date:** 2026-11-23
- **Deciders:** Repository maintainer, by explicit user decision on 2026-11-23 ("do all three", after the options were shown).
- **Consulted:** None recorded.
- **Informed:** None recorded.
- **Spec:** Vikunja #1623.
- **Dependencies:** ADR-021 (state root: no home `agent.env`), ADR-022 (confinement: workspace `agent.env` only).
- **Supersedes:** None
- **Superseded by:** None

## Context

Two mechanisms read `agent.env` files.

- `env_file` runs first, for each runtime mode. It reads the **implicit** files `<workspace>/agent.env` and `~/.config/daimonos/agent.env` and sets each variable that the process environment does not have.
- `agent_env` then reads the **selected** file for `agent`, `chat`, `acp`, and the session daemon. The selection is `--agent-env`, else `$DAIMONOS_AGENT_ENV`, else the home file. Non-empty process values named `DAIMONOS_AGENT_*` override the values of the selected file.

The second step cannot tell a value of the launcher from a value that the first step set. So a value of an implicit file replaced the value of the file that the caller named with `--agent-env`.

Measured on 2026-11-23: with a home `agent.env`, a run with `--agent-env <file>` sent its requests to the endpoint, with the model and the API key, of the home file. An unattended caller that passes `--agent-env` (an orchestrator, a test) got the settings of the developer host.

How must an explicitly selected agent env file rank against the implicit files?

## Decision drivers

- A caller that names a file must get the settings of that file.
- A launcher that sets a variable in the real environment must keep its override (the ACP `env` block of an editor).
- A run without an explicit selection must not change.
- No credential of a host file may reach an endpoint that an explicit file names, and the reverse.

## Decision

We will rank the settings of an agent run in this order, highest first:

1. CLI behavior options, for example `--model`.
2. The real environment: a non-empty `DAIMONOS_AGENT_*` variable that the launcher supplied.
3. The explicitly selected file.
4. The implicit `agent.env` files.

A file is explicitly selected when `--agent-env` names it, or when `$DAIMONOS_AGENT_ENV` of the real environment names it. A selector that an implicit file supplies is not an explicit selection.

- `env_file` records the name of each variable that it sets (`env_file::implicit_keys`). `agent_env` uses that record to split the process values into real values and implicit values.
- With an explicit selection, an implicit value only fills a setting that the selected file omits or leaves blank.
- With an explicit selection, an implicit value never fills a **connection setting**: `DAIMONOS_AGENT_PROVIDER`, each `..._BASE_URL`, and each `..._API_KEY`, also of a secondary provider. These come from the selected file or from the real environment. So the API key of one file is never sent to the endpoint that another file names. A run that lacks a required connection setting stops before a request, and the message names the settings that were not used. The message does not hold a value.
- Without an explicit selection, the behavior does not change. The selected file is then the home file, and each process value overrides it.
- The session daemon does not inherit the implicit values of the process that starts it. It loads the implicit files itself, so it has its own record.
- The rule covers the settings that `agent_env` parses: `DAIMONOS_AGENT_*` and `DAIMONOS_SKILL_DIR`.

## Alternatives

- **Do not load the implicit files when a file is explicitly selected.** This is the cleanest isolation. It loses: an implicit file also supplies variables that `agent_env` does not parse (see Consequences) and variables for tool child processes. The loss of those would change more runs.
- **Let the explicit file win over the real environment too.** It loses: an editor that starts `acp` with an `env` block and an `--agent-env` option would lose its override, against the rule of `env_file` that the launcher wins.
- **Keep the order and change the documents.** It loses: a named file that a host file overrides cannot give a reproducible run.
- **Clear the environment in each caller.** It works for a test. It loses as a product rule: each caller must know the rule, and a caller with a home file that omits it sends credentials to the wrong endpoint.

## Consequences

- A run with `--agent-env` on a host with a home `agent.env` changes: the named file now wins for each setting that it sets. This is the intended change.
- A setting that the named file omits still comes from an implicit file, except a connection setting. A caller that needs a closed set of settings uses `--state-dir`, which reads no home file (ADR-021).
- A named file that relied on the home file for its API key or base URL now fails at startup. The fix is to put the setting into the named file or into the real environment.
- Without an explicit selection, nothing changes, including the order of the two implicit files. This decision does not cover that order (Vikunja #1624).
- Tool child processes inherit the process environment, with the values of the implicit files, as before.
- Not covered: variables that the product reads directly from the process environment and that `agent_env` does not parse. They are `DAIMONOS_AGENT_MODELS_DISCOVER`, `DAIMONOS_AGENT_MAX_TOKENS`, `DAIMONOS_AGENT_ERROR_RESUME`, `DAIMONOS_AGENT_AUTO_CONTINUE`, and `DAIMONOS_AGENT_SESSION_ID`. For these, a line in the selected file has no effect, as before, and a value of an implicit file or of the real environment applies.
- The record of implicit variables is process state. A process that another Daimonos process starts, and that inherits its environment, sees the implicit values as real values. The session daemon is the one such child, and its start removes them.

## Confirmation

- `pytest tests/test_agent_env_precedence.py` drives the real binary with two provider endpoints. It covers: the explicit file against the home file, against the workspace file with `--state-dir`, the real environment above the explicit file, an omitted setting filled from the home file, `$DAIMONOS_AGENT_ENV` of the real environment, a selector from an implicit file, the unchanged run without a selection, and the session daemon of an interactive run.
- The same file covers the connection settings: an explicit file without an API key does not get the key of the home file, an explicit file without a base URL does not use the endpoint of the home file, the real environment can supply either, and the confined run with both ways to name the file.
- `cargo test agent_env::` covers the order in `layered_vars`, the fill rule, `is_connection_setting`, and `explicit_selection`.
- `cargo test daemon_command_does_not_pass_on_implicit_agent_env_values` covers the environment of the session daemon.

## Anchors

_Anchors:_ src/env_file.rs::implicit_keys, src/agent_env.rs::AgentEnv::load, src/agent_env.rs::layered_vars, src/agent_env.rs::explicit_selection, src/agent_env.rs::fill_unset_from_implicit, src/agent_env.rs::is_connection_setting, src/session_bootstrap.rs::daemon_command

## Revisit when

A second Daimonos child process inherits the environment of its parent and reads agent settings, or a caller needs an explicit file that reads no implicit file without `--state-dir`.

The ACP signal change of the same pull request is not a part of this decision. The session daemon and the MCP stdio server still install their signal handlers after their startup work (Vikunja #1625).

## Open questions

1. Should the five directly read variables in Consequences move into `agent_env`, so that the selected file can set them? Not decided.
2. The rules for a run without an explicitly selected file are outside this decision. Vikunja #1624 tracks them.
