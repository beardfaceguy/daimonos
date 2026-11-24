# ADR-038: The workspace `agent.env` cannot set connection, approval, or network settings

- **Status:** Accepted
- **Date:** 2026-11-24
- **Deciders:** Repository maintainer, by explicit user decision on 2026-11-24 (the recommended option was shown and accepted).
- **Consulted:** None recorded.
- **Informed:** None recorded.
- **Spec:** Vikunja #1624.
- **Dependencies:** ADR-037 (an explicitly selected agent env file ranks above the implicit files).
- **Supersedes:** None
- **Superseded by:** None

## Context

Daimonos loads two implicit files into the process environment at startup: `<workspace>/agent.env`, then the user file `~/.config/daimonos/agent.env`. A variable that is already set is not overwritten, so the workspace file ranks above the user file.

The two files do not have the same origin. The user wrote the user file. The workspace file comes with the workspace, and a workspace is often a checked-out repository of another party. `env_file` already treats the file as untrusted input: it refuses variables that change how programs load, such as `LD_PRELOAD` and `PATH`.

That list did not cover the settings of the agent itself. The workspace file could set the provider, the base URL, and the API key. It could set the approval mode and the lists that block tools or skip approval. It could set the proxy variables of the HTTP client. Each of these overrode the user file. It could also set `XDG_CONFIG_HOME`, and the user `config.toml` is found through that variable after the file is loaded.

Which settings must the workspace `agent.env` not be able to set?

## Decision drivers

- A workspace must not decide where the requests of the agent go or which credential they carry.
- A workspace must not lower the approval rules that the user chose.
- A workspace must not decide where the user's own configuration is found.
- A workspace file that sets harmless values (a model name, a feature switch, variables for the tools of the project) must keep its function.
- A refused line must be visible to the user.

## Decision

We will ignore these assignments in `<workspace>/agent.env`, and report them on stderr by name:

1. **Connection settings:** `DAIMONOS_AGENT_PROVIDER`, and each `DAIMONOS_AGENT_*_BASE_URL` and `DAIMONOS_AGENT_*_API_KEY`, also of a secondary provider.
2. **The selector** `DAIMONOS_AGENT_ENV`.
3. **Approval settings:** `DAIMONOS_AGENT_APPROVAL_MODE`, `DAIMONOS_AGENT_ALLOWED_COMMANDS`, `DAIMONOS_AGENT_DENIED_COMMANDS`, `DAIMONOS_AGENT_PERMISSION_MODE`, and `DAIMONOS_AGENT_PERMISSION_LIST`.
4. **Proxy and certificate variables:** `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`, `SSL_CERT_FILE`, and `SSL_CERT_DIR`.
5. **User directory variables:** `HOME`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_DATA_HOME`, and `XDG_CACHE_HOME`.

Each name is compared without letter case.

- The user file, the file that `--agent-env` names, and the real environment can set each of them.
- The rule is in `env_file::workspace_refusal`. It applies on each path that loads the workspace file, also under confinement.
- One message for the file names each variable and its class. It does not print a value. It is printed in each runtime mode, on stderr.
- The check sees the name after the line is parsed: without an `export` prefix, without surrounding whitespace, and with the quotes of the value removed. A line that is not a valid assignment sets nothing.
- Each other assignment of the workspace file applies as before, and still ranks above the user file.

## Alternatives

- **Do not load the workspace file.** It is the most complete rule. It loses: the file is a documented way to set variables for a project and its tools.
- **Rank the user file above the workspace file for these settings.** A workspace could then still supply a setting that the user file omits, for example a proxy. It loses against the first two drivers.
- **Ask the user to trust a workspace before its file is loaded.** It needs a store of trusted workspaces and an interactive step. Agent runs are often unattended. It loses on cost now. It can be added later.
- **Refuse every `DAIMONOS_AGENT_*` variable.** Simpler to state. It loses: it removes harmless project settings such as the model, and it does not cover the proxy variables.

## Consequences

- A project that sets its base URL, API key, provider, approval mode, or proxy in `<workspace>/agent.env` on purpose stops working that way. The message names the line. The setting must move to the user file, to a file named with `--agent-env`, or to the real environment.
- A workspace file can still set the model and other agent settings outside the four classes, and variables for tool child processes.
- This decision covers `agent.env` only. Other files of a workspace are outside it.
- The network names come from the sources of the locked crates: `hyper-util` 0.1.20 reads `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and `NO_PROXY` in both letter cases, and `rustls-native-certs` 0.8.4 reads `SSL_CERT_FILE` and `SSL_CERT_DIR`. A new client library can add names.
- The rule is a list of names. A variable outside the five classes still applies from the workspace file and is inherited by tool child processes. This includes variables that change how a tool of the project runs.
- Still settable from the workspace file, by decision: the model and the other agent settings outside the classes (thinking, compaction, context window, summary model and summary prompt, model list, prompt cache), `DAIMONOS_SKILL_DIR`, and the five settings that are read from the process environment only (`DAIMONOS_AGENT_MODELS_DISCOVER`, `DAIMONOS_AGENT_MAX_TOKENS`, `DAIMONOS_AGENT_ERROR_RESUME`, `DAIMONOS_AGENT_AUTO_CONTINUE`, `DAIMONOS_AGENT_SESSION_ID`). A workspace already supplies instruction text for the model (`AGENTS.md`, skills), so the summary prompt and the skill root are not a new kind of input.

## Confirmation

- `pytest tests/test_workspace_agent_env.py` drives the real binary: the endpoint and credential, the selector, three ways to loosen approvals, four proxy variables, the user configuration directory, a lower-case line and an `export` line, a setting that still applies, and two controls (the user file can set a proxy; a tool call runs when the user file allows it).
- The same file covers each path that loads the workspace file: the default path, `--state-dir`, and `--confine-paths`. With `--ignore-config-files` the file is not read.
- `cargo test env_file::` covers `workspace_refusal` for each class and letter case, the plan for both sources, and the spellings of a line.
- Mutations: removing one class from `workspace_refusal`, a case-sensitive comparison, or the user-file trust on the confinement path each fail a process test.

## Anchors

_Anchors:_ src/env_file.rs::workspace_refusal, src/env_file.rs::plan, src/env_file.rs::Source, src/env_file.rs::load_confined

## Revisit when

A workspace trust store exists, or the HTTP or TLS library changes the variables that it reads.

## Open questions

1. Should the workspace file be able to set the model, and the other agent settings that stay allowed? The model can raise the cost of a run. Not decided.
