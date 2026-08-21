# Daimonos Protocol Specification v0.1

## Overview

Daimonos uses a compact, opcode-based protocol designed to minimize token usage
for LLM agent tool calls. The protocol is exposed as an MCP server (JSON-RPC
over Streamable HTTP) for direct Cursor integration, with an optional compact
binary mode for non-MCP clients.

## Opcode Registry

Each operation is identified by a numeric opcode. The agent can call opcode 255
to retrieve the full registry with argument schemas.

| Op | Name | Args | Returns |
|----|------|------|---------|
| 0 | read | path, offset?, limit? | {content, lines, size} |
| 1 | write | path, content | {ok, size} |
| 2 | patch | path, edits[] | {ok, applied} |
| 3 | ls | path, depth?, glob? | {entries[]} |
| 4 | stat | path | {size, modified, type} |
| 5 | glob | pattern, root? | {files[]} |
| 6 | grep | pattern, path?, glob?, max? | {matches[{file,line,text}]} |
| 7 | find | query, kind?, root? | {results[]} |
| 8 | exec | cmd, args[], cwd?, env? | {exit, out, err?} |
| 9 | bg | cmd, args[], cwd?, env? | {pid, log} |
| 10 | poll | pid | {running, exit?, tail?} |
| 11 | kill | pid | {ok} |
| 12 | snap | tag? | {id, timestamp} |
| 13 | restore | id | {ok} |
| 14 | diff | a, b | {hunks[]} |
| 15 | git | sub, args[] | {structured output} |
| 16 | env_set | key, value | {ok} |
| 17 | env_get | key | {value} |
| 18 | session | workspace? | {id, cwd, env} |
| 255 | schema | op? | {registry or single op schema} |

## Request Format

### MCP Mode (primary)

Standard JSON-RPC 2.0 over MCP. Each opcode maps to an MCP tool name
(`daimonos_read`, `daimonos_write`, etc.) with typed parameters.

### Compact Mode (optional, non-MCP clients)

Length-prefixed MessagePack over Unix socket:

```
[op: u8, ...args]
```

Example: read file at path with offset
```
[0, "src/main.rs", 10, 50]
```

## Response Format

All responses are structured objects. No unstructured text output.

### Success
```json
{"ok": true, "d": <result data>}
```

### Error
```json
{"ok": false, "e": <error code>, "m": <message>}
```

### Error Codes
| Code | Meaning |
|------|---------|
| 1 | Not found |
| 2 | Permission denied |
| 3 | Invalid argument |
| 4 | IO error |
| 5 | Process failed |
| 6 | Timeout |
| 7 | Snapshot not found |

## Session State

The daemon maintains per-session state:
- Working directory (persists across calls)
- Environment variables (persists across calls)
- Active background processes
- Snapshot stack

This eliminates the need for agents to re-send `. "$HOME/.cargo/env" &&`
on every shell call.

## Batch Operations

A single request can bundle multiple operations:

```json
{"batch": [
  [5, "*.rs", "src"],
  [6, "struct.*Error", null, "*.rs"],
  [4, "Cargo.toml"]
]}
```

Returns an array of results in order. Operations are independent (no
cross-referencing between batch items).
