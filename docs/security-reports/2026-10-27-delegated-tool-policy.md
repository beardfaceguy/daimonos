# Delegated tool policy audit

Date: 2026-10-27
Origin: Vikunja #1573. Report only. Enforcement remains unchanged.

## Agent Starlark finding

The agent checks `SafetyPolicy` for the outer `execute_script` call.
Inner tool dispatch does not repeat that check.

The runtime reproduction tests a restrictive allowlist and a `write_file` denylist.
A direct write is blocked in both cases. A delegated write succeeds.
The script is:

```python
result = write_file("delegated.txt", "written")
```

Run the deterministic reproduction:

```bash
cargo test policy_audit_script_inner_writes_bypass_allowlist_and_denylist -- --nocapture
```

This test passes when it reproduces the current unsafe behavior.
It checks that `blocked.txt` is absent and `delegated.txt` contains `written`.
It uses the real agent loop and a provider that returns fixed tool calls.
No external model or credentials are required.

## MCP batch and Starlark finding

MCP mode ignores the agent allowlist and denylist.
A runtime test sets both variables:

```dotenv
DAIMONOS_AGENT_ALLOWED_COMMANDS=read_file,search,git_read,batch,execute_script
DAIMONOS_AGENT_DENIED_COMMANDS=write_file
```

Direct `write_file`, batch `write_file`, and scripted `write_file` all succeed.
This is not a batch-only escape. MCP permits the direct write too.
Batch is MCP-only. The agent catalog does not offer it.

Run the MCP reproduction:

```bash
python3 -m pytest tests/test_git_read.py::test_policy_audit_mcp_delegates_ignore_agent_env -v
```

The batch payload is:

```json
{"ops":[{"tool":"write_file","arguments":{"path":"batch.txt","content":"written"}}]}
```

## Mitigation and scope

The requested reviewer allowlist excludes `execute_script`, so the agent blocks it.
Keep that restriction. Do not grant delegate tools to a strictly read-only agent.
MCP clients must filter their exposed tools and their calls.
Exclude `batch` and `execute_script`, as well as direct write tools.

The git_read change does not fix either policy boundary.
A separate task must decide the enforcement model.

_Anchors:_ src/script.rs::dispatch_tool_by_name, src/agent.rs::run, src/agent.rs::policy_audit_script_inner_writes_bypass_allowlist_and_denylist, tests/test_git_read.py::test_policy_audit_mcp_delegates_ignore_agent_env
