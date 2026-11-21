# ADR-036: One-shot agent runs can be named, resumed, and reported as JSON

- **Status:** Accepted
- **Date:** 2026-11-19
- **Deciders:** Repository maintainer, by explicit user decision on 2026-11-20. The maintainer resolved open questions 1 to 5 on the same day.
- **Consulted:** None recorded.
- **Informed:** None recorded.
- **Spec:** Vikunja #1621. Request: [one-shot resume and JSON handoff](../handoffs/2026-11-20-daimonos-handoff-oneshot-resume-and-json.md).
- **Dependencies:** ADR-014, ADR-021, ADR-033 (all accepted).
- **Supersedes:** None. Amends ADR-024 for one option combination (Decision 14).
- **Superseded by:** None

## Context

One-shot `daimonos agent "<task>"` starts a new conversation on each call. It builds the system prompt from the workspace, runs the loop, prints the assistant text, and writes one `agent_runs` row. It does not save the conversation.
An external orchestrator runs Daimonos as a worker. After a failed test stage it wants to send only the failure output to the same conversation on the same model, so that the provider serves the earlier conversation from its prompt cache. It also needs the outcome and the token counts of each run without reading the analytics database.
`chat` saves and resumes through `SessionStore::save`. That path is best-effort, takes no writer, and lets `--model` replace the saved model.

Three facts from the code and the provider documentation limit the design. They come from a reading of the code. No provider request was measured.

1. Prompt caching is off by default (`DAIMONOS_AGENT_PROMPT_CACHE`). When it is on, the agent loop always sends `stable_prefix_len: 0`. The Anthropic adapter then marks only the last tool definition. The system prompt and the messages are not cached on direct Anthropic. The OpenRouter adapter marks the last message for `anthropic/` models. An identical prefix alone does not give a cache read on direct Anthropic.
2. The loop prunes old tool results in place (`microcompact_history`) when the kept results pass `intra_turn_result_budget_tokens` (default 40,000). This rewrites earlier messages. The request says that one-shot mode has no compaction. That is correct for ADR-002 compaction and not for this pruning.
3. Anthropic documents the cache order as tools, then system, then messages. A change of the thinking setting invalidates the message cache. The default cache lifetime is 5 minutes. ([Prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching), read 2026-11-19.)

How can a one-shot run be continued and reported so that a caller can depend on the saved state, the request prefix, and the result?

## Decision drivers

- A run without the new options must not change.
- A resumed request reproduces the earlier prefix, or the run refuses. No silent difference.
- The caller can depend on the save and on the result document.
- One store implementation and one ownership rule (ADR-014, ADR-033).
- The caller does not read Daimonos tables.

## Decision

We will add three opt-in options to one-shot `daimonos agent`: `--session-id <id>`, `--resume <id>`, and `--output json`.

### Sessions

1. `--session-id <id>` starts a new named session. `--resume <id>` continues one. The two options conflict with each other, with `--interactive`, and with `--dry-run`. The id uses the `SessionStore` id alphabet, with at most 128 characters (proposed). The top-level `--session-id` keeps its meaning: it filters `--stats`. The position separates the two options: before `agent`, or after it.
2. The session uses the `SessionStore` type in its own directory: `~/.daimonos/oneshot-sessions`, or `sessions/oneshot` under a selected state root (new `SessionStoreKind::OneShot`). `chat --list` and `chat --resume` do not see these sessions.
3. `PersistedSession` gets one optional field, the **prefix record**: the provider name, the system prompt text, and the ordered tool list with a SHA-256 digest of each tool's description and input schema. The model and the thinking level use the existing fields. The record version stays 1.
4. A new session claims the writer with no expected generation. A resume loads the record and claims the writer at the loaded generation. `claim_writer` takes the owner lock, so a second live process is refused (ADR-033).
5. A resume uses the saved provider, model, and thinking level. A `--model` or `--provider` value that differs from the record is refused. The system prompt is the saved text. It is not rebuilt. The run computes its tool list and compares it with the record. A difference in order, name, or digest is refused, and the message names the first tool that differs.
6. The resumed conversation is the saved messages plus the new input as one user message. A session run has no ADR-002 compaction policy. Result pruning (Context, fact 2) does not change.
7. In a session run, the loop marks the full message list as the stable prefix (`stable_prefix_len` = message count) on each request. The provider adapter translates that (ADR-001). This has no effect while prompt caching is off. A run without a session option keeps the value 0.
8. The run saves the session after the loop returns, for each stop reason, through the claimed writer at the next generation. Any result other than `Saved` is a failure and the process exits with code 1 (proposed).
9. A refusal before the run exits with code 2 and prints the existing `run_configuration_invalid` JSON on stderr. Reason codes (proposed): `session_id_invalid`, `session_exists`, `session_not_found`, `session_owned_elsewhere`, `session_changed`, `session_not_resumable`, `session_model_mismatch`, `session_provider_mismatch`, `session_tools_changed`.
10. A context overflow ends the run with `stop_reason: error` and `context_overflow: true`. The run does not compact and does not retry. The caller starts a new session.
11. Each invocation writes one `agent_runs` row, as today. The row gets a nullable column `oneshot_session_id` (proposed), added through `migrate_schema`.

### JSON result

12. `--output json` prints one JSON document and one newline on stdout when the loop returned a result, for each stop reason. It prints nothing else on stdout. A run that fails before the loop starts prints no document. The exit code rules of `check_agent_result` do not change.
13. The document has these fields (names proposed):

| Field | Type | Content |
|---|---|---|
| `schema_version` | integer | `1` |
| `session_id` | string or null | The id of `--session-id` or `--resume` |
| `resumed` | boolean | True for `--resume` |
| `session_saved` | boolean or null | Null without a session option |
| `session_error` | string or null | The save failure |
| `provider`, `model` | string | The provider and model that served the run |
| `stop_reason` | string | `StopReason::as_str`, the same value as the `agent_runs` row |
| `error_message` | string or null | From the result |
| `context_overflow` | boolean | From the result |
| `final_text` | string | The text blocks of the last assistant message of this invocation |
| `turns` | integer | The same value as the `agent_runs` row |
| `usage` | object | `input_tokens`, `output_tokens`, `reasoning_output_tokens` (or null), `thinking_bytes`, `cache_read_tokens`, `cache_write_tokens`, `cost_usd`. This invocation only. |
| `evidence` | object | `status`: `unknown`, `no_mutations`, `verified`, or `unverified` |
| `duration_ms` | integer | Wall time of the invocation |

14. With the structured final output of ADR-024, the envelope is the only stdout document. A field `final_output` holds the validated document, or null. The exit codes of ADR-024 do not change. This rule applies when one build has both options.
15. Without `--output json`, a resumed run prints only the assistant text of this invocation.

## Alternatives

- **The caller reads `agent_runs`:** no change in Daimonos, but it couples the caller to an internal schema.
- **Put one-shot sessions in the chat store directory:** one directory less. `chat --resume` can then continue the session with compaction and another model, and the chat save takes no writer.
- **Rebuild the system prompt and refuse if it differs:** no stored prompt text. A change of a skill or an instruction file in the workspace then blocks each retry.
- **Store the full tool schemas and reuse them:** a resume survives a tool change, but the model is then offered tools that the runtime does not have.
- **Let `--model` win, as `chat` does:** the cache is lost without a message.
- **Record the id in `external_session_id`:** no schema change, and `--stats --session-id` works. The outer runtime owns that column through `DAIMONOS_AGENT_SESSION_ID`, so two ids can collide.
- **Save only after a successful run:** the id stays free after a failure. The caller pays again for the work of the failed attempt.
- **Leave the cache intent at 0:** less change. On direct Anthropic a resume then reads only the tool definitions from the cache.
- **A JSON event stream:** more than the caller needs.

## Consequences

- A session record holds the system prompt and the conversation, with file content, in a 0700 directory and 0600 files. This ADR adds no retention and no delete command. The store grows.
- A resumed run uses the old system prompt. A changed skill or instruction applies only to a new session.
- A change of a tool schema (an upgrade, a configuration change, a new workspace signal) makes existing sessions non-resumable.
- A cache read is not guaranteed. The provider lifetime can end (5 minutes by default on Anthropic). Result pruning rewrites messages. A coordination notice or a loop-detector steer changes the system text for one request.
- With prompt caching on, each request of a session run pays the provider's cache-write price for its new part. A session that is never resumed costs more than a run of today.
- A failed run consumes the session id.
- `agent_runs` has three id columns.
- `--session-id` has two meanings. The position before or after `agent` selects the meaning.
- With `--output json`, no code on the print path may write to stdout.

## Confirmation

- Existing one-shot tests pass without a change.
- A mock-provider test: run with `--session-id`, then `--resume`. The system prompt, the tool array, and the earlier messages of the resumed request are byte-identical to the last request of the first run. One user message is added.
- Tests for each refusal of Decision 9, with the record unchanged after the refusal.
- A test with a held owner lock: the second `--resume` exits with code 2.
- A test with a failing store: the process exits non-zero, and the document has `session_saved: false`.
- Tests of the stdout bytes with `--output json` for success, provider error, and context overflow. One of them runs a tool whose child process writes to stdout.
- A test compares `usage`, `stop_reason`, and `turns` with the `agent_runs` row.
- An Anthropic request-builder test: a session run marks the last message.
- A state-root test: the session database is under `sessions/oneshot`.
- A CLI parse test: the top-level `--session-id` and `agent --session-id` parse as two separate values.
- `cargo test` and `python3 -m pytest tests/ -v` pass.

## Anchors

_Anchors:_ src/agent_cmd.rs::run_agent, src/agent_runtime.rs::run_agent, src/agent_runtime.rs::check_agent_result, src/cli.rs::AgentArgs, src/session_store.rs::PersistedSession, src/session_store.rs::SessionStore::claim_writer, src/session_store.rs::SessionStore::load_result, src/session_store.rs::SessionStore::write_record, src/state_root.rs::SessionStoreKind, src/state_root.rs::StateAuthority::verify_session_store, src/agent.rs::run, src/agent.rs::AgentResult, src/tool_output.rs::microcompact_history, src/providers/mod.rs::Context, src/providers/mod.rs::StopReason::as_str, src/providers/anthropic.rs::build_request_with_cache, src/providers/openrouter.rs::apply_prompt_cache, src/prompts.rs::agent_system_for_workspace, src/analytics.rs::AnalyticsStore::record_agent_run, src/analytics.rs::migrate_schema, src/evidence.rs::EvidenceLedger::status, src/chat_cmd.rs::resolve_model

## Revisit when

- One-shot runs get model failover. The `model` field must then name each model that served a call.
- The one-shot session store needs retention.
- A caller needs a resume across a tool-schema change.

## Open questions

The maintainer resolved these on 2026-11-20. Each answer keeps the rule of the Decision section:

1. **Store directory.** A separate directory with the same store type (Decision 2).
2. **Cache intent.** In scope. Session runs mark the message list as the stable prefix (Decision 7).
3. **Save after a failed run.** The run saves for each stop reason (Decision 8).
4. **Option name.** `--session-id` on `agent` (Decision 1). A probe with clap 4.6.1 showed that the top-level option and the `agent` option parse as two separate values.
5. **Result pruning.** It stays on in session runs (Decision 6).

These are open. They did not block acceptance:

6. The reason codes, the field names, the column name, the 128-character limit, and exit code 1 for a failed save are proposed.
7. After a failed run, the saved history can end with a user message. The next resume then sends two user messages in sequence. It is not verified that each provider accepts this.
