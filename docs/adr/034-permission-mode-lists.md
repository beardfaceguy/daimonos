# ADR-034: Permission-mode lists decide which tool calls ask for approval

- **Status:** Accepted
- **Date:** 2026-11-12
- **Deciders:** Repository maintainer, by explicit user decisions on 2026-11-12 (scope, precedence, remote-tool naming, and config location).
- **Consulted:** None recorded.
- **Informed:** None recorded.
- **Spec:** Vikunja #1616 (ACP turn hung on an unanswered `session/request_permission`).
- **Dependencies:** ADR-003 (ACP MCP bridge and remote-tool naming).
- **Supersedes:** None
- **Superseded by:** None

## Context

`DAIMONOS_AGENT_APPROVAL_MODE` has three values. `auto` never asks. `interactive` asks for destructive native tools and every remote `mcp__*` tool. `paranoid` asks for every tool.
An "Always Allow" answer for a remote tool lasts only for the process, because the exposed name is sanitized, truncated, and de-duplicated (#990). It can point to a different server later.
Zed starts new daimonos processes often, so the operator gets asked again for the same harmless read-only remote tools.
The existing `DAIMONOS_AGENT_ALLOWED_COMMANDS` and `DENIED_COMMANDS` lists decide whether a tool may run at all. They do not decide whether it asks.
How should an operator choose, per tool, which calls ask for approval, including remote MCP tools, without trusting unstable exposed names?

## Decision drivers

- Let the operator skip prompts for trusted tools, or ask only for risky ones.
- Never let an unstable remote-tool name grant or skip approval for a different server.
- Keep the existing block lists authoritative.
- Keep approval settings in one file.

## Decision

We will add `DAIMONOS_AGENT_PERMISSION_MODE` with the values `null`, `allow_list`, and `deny_list`, plus the comma list `DAIMONOS_AGENT_PERMISSION_LIST`, in `agent.env`.

- `null` (or unset) keeps today's behavior: `approval_mode` decides.
- `allow_list`: listed tools run without asking. Every other tool asks.
- `deny_list`: listed tools ask. Every other tool runs without asking.
- The lists cover all tools, native and remote.
- When the mode is not `null`, the list replaces `approval_mode`'s ask decision. `DENIED_COMMANDS` and `ALLOWED_COMMANDS` still block first. The in-process "Always Allow" set still skips a prompt.
- Native tools are listed by name. Remote tools are listed as `server/tool`: the forwarded server name plus the server's own tool name. The MCP bridge records this key when it registers a route (`McpBridge::permission_key`). The exposed `mcp__...` name never matches.
- A trailing `*` is a prefix match. Every other pattern matches exactly.
- A remote tool without a known `server/tool` key always asks, in both list modes. This fails closed where no bridge supplies identities (for example the `chat` and `agent` CLI modes).

`SafetyPolicy` owns the decision (`gate_keyed`). `CanonicalToolLifecycle` resolves remote keys from the session's live bridge slot, so a bridge refresh also refreshes the keys.

## Alternatives

- **Exposed-name lists:** simpler, but the #990 risk lets an entry match a different server.
- **Persist "Always Allow" for remote tools:** has the same naming risk and gives no deny-style list.
- **Apply lists only in `interactive` mode:** two settings would interact in ways that are hard to predict. The operator chose replacement.
- **A `[permissions]` table in `config.toml`:** allows comments per entry, but splits approval settings across two files.
- **Match by server URL or command instead of name:** more stable against renames, but harder to write by hand. The server name is under the operator's control in the client config.

## Consequences

`deny_list` with a short list is permissive: every native tool not listed, including `exec`, runs without asking. The operator chose this behavior explicitly.
Renaming a server in the client config changes its key. Entries for the old name stop matching. In `deny_list` mode such a tool then runs without asking.
Remote tools in modes without a bridge always ask under either list mode.

## Confirmation

- `cargo test safety::` covers both list modes, `server/tool` matching with `*`, unknown remote identity, and block-list precedence.
- `cargo test permission_mode_and_list_reach_the_safety_policy` covers `agent.env` parsing and validation.
- `cargo test permission_key_maps_exposed_name_to_server_slash_tool` covers the bridge identity.
- `pytest tests/test_acp_permission.py` drives the real binary: an `allow_list` entry runs a remote tool with no permission request, and a `deny_list` entry asks.

## Anchors

_Anchors:_ src/safety.rs::SafetyPolicy::gate_keyed, src/safety.rs::PermissionPolicy, src/agent_env.rs::AgentEnv::to_safety_policy, src/mcp_bridge.rs::McpBridge::permission_key, src/session_core.rs::CanonicalToolLifecycle::with_bridge_slot

## Revisit when

A client forwards a stable server identity (for example a URL or an ID) that is better than its configured name, or operators need per-argument rules (for example `exec` of specific programs).

## Open questions

None blocking. Glob syntax beyond a trailing `*` is out of scope.
