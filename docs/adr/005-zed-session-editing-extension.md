# ADR-005: Zed session retry and truncation extension

**Date:** 2026-07-20
**Status:** Accepted
**Tracks:** Vikunja #1014; enables Zed-fork #994

## Context

ACP 1.2 / schema 1.4 does not define retrying the latest turn or truncating
history when a user edits an earlier message. Zed has internal UI hooks for
both operations, but external ACP connections cannot implement them through
standard methods.

## Contract

Daimonos advertises three boolean `AgentCapabilities._meta` keys:

- `zed.dev/clientUserMessageIds`
- `zed.dev/sessionRetry`
- `zed.dev/sessionTruncate`

When the first flag is present, Zed adds an opaque string under
`PromptRequest._meta["zed.dev/clientUserMessageId"]`. Daimonos keeps these IDs
in surviving user-turn order and persists them with the session.

Two namespaced JSON-RPC requests are accepted:

- `_zed/session/retry` with `{ "sessionId": string }`, returning the standard
  `PromptResponse`. The latest user turn is rerun against history before that
  turn; its prior assistant/tool output is replaced.
- `_zed/session/truncate` with
  `{ "sessionId": string, "clientUserMessageId": string }`, returning `{}`.
  The selected user turn and all later history/IDs are removed immediately.

Unknown sessions/message IDs return errors. Clients that do not use these
capabilities see unchanged standard ACP behavior.

## Lifecycle and persistence

Retry builds from a cloned pre-turn history and commits only after completion,
so cancellation leaves the prior committed turn intact. Truncation persists
immediately. Client IDs use `serde(default)` in the existing versioned session
record, keeping older files readable.

Compaction can evict old user turns. After each completed prompt and before
truncation, Daimonos trims the oldest IDs until their count matches surviving
non-summary user turns.

## Compatibility

Method names and metadata are namespaced because this is not standard ACP.
Zed exposes its retry/truncate hooks only when the corresponding agent
capabilities are true.
