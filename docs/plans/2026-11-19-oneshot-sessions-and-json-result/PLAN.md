# Plan: named one-shot sessions and a JSON result for `daimonos agent`

- **Status:** Approved (maintainer review, 2026-11-20)
- **Date:** 2026-11-20
- **Spec:** Vikunja #1621. Request: [one-shot resume and JSON handoff](../../handoffs/2026-11-20-daimonos-handoff-oneshot-resume-and-json.md).
- **Decisions:** [ADR-036](../../adr/036-oneshot-sessions-and-json-result.md) (Accepted). It depends on ADR-001, ADR-014, ADR-021, and ADR-033.
- **Branch:** `feat/1621-oneshot-sessions-json`

Paths and symbols in this plan were read at `230df9d`. A name marked `(new)` does not exist yet. A name marked `(proposed)` can change in review.

## Goal

A caller can continue a one-shot run on the same provider and model with only the new input, and can read the outcome and the token usage of each run from stdout.
The acceptance checks of the request document are the success measure.

## In scope

1. `daimonos agent --output json`.
2. `daimonos agent --session-id <id>` and `daimonos agent --resume <id>`.
3. The stable-prefix mark for session runs (ADR-036 Decision 7).
4. The `agent_runs` column for the one-shot session id.
5. Documents: `docs/runtime-modes.md`, `CHANGELOG.md`, the session-durability glossary, and the source tree in `AGENTS.md`.

## Out of scope

- The `final_output` field (ADR-036 Decision 14). `--final-output` is not on `master`. This build does not emit the field.
- Structured done, failed, or needs-a-decision outcomes (ADR-024, WP05).
- Explicit-only discovery (ADR-021, WP11).
- The model-request ceiling (ADR-024).
- Reading the task from a file or stdin.
- Retention or a delete command for one-shot sessions.
- Model failover for one-shot runs.
- A change to result pruning, to `chat`, to ACP, or to the session daemon.
- A change of the default of `DAIMONOS_AGENT_PROMPT_CACHE`.

## Design rules

1. A run without the three new options executes the same code path as today and produces the same bytes on stdout. Existing tests pass without a change.
2. One store implementation. One-shot sessions use `SessionStore` with a claimed writer. They do not use the best-effort `SessionStore::save`.
3. A resume reproduces the prefix or refuses. The saved system prompt text is sent as saved. The tool list is computed and compared, never loaded.
4. Session handling is in the one-shot frontend (`src/agent_cmd.rs`) and one new module. The agent loop gets one new input, the stable-prefix mark.
5. Provider wire format stays in `src/providers/*` (ADR-001). The loop sets `Context::stable_prefix_len`. The adapters translate it.
6. With `--output json`, the print path writes to stdout exactly once.
7. No new crate. `sha2`, `hex`, `serde_json`, and `clap` are dependencies today.
8. The new code has no model-facing text.

## Interfaces

Types and signatures only.

**`src/cli.rs::AgentArgs`**
- `session_id: Option<String>`, `--session-id <ID>`. Conflicts with `resume`, `interactive`, `dry_run`.
- `resume: Option<String>`, `--resume <ID>`. Conflicts with `interactive`, `dry_run`.
- `output: AgentOutputFormat (new)`, `--output <text|json>`, default `text`. Conflicts with `interactive`, `dry_run`.

**`src/oneshot_session.rs (new)`**
- `enum SessionRequest { Start(String), Resume(String) }`
- `struct PrefixRecord { provider: String, system: String, tools: Vec<ToolDigest> }`, `struct ToolDigest { name: String, sha256: String }`. Both derive `Serialize` and `Deserialize`.
- `fn tool_digests(tools: &[ToolSchema]) -> Vec<ToolDigest>`. The digest input is the description and the serialized input schema.
- `struct Refusal(RunFailure)`, an error type. One constructor for each reason code of ADR-036 Decision 9.
- `struct OpenSession`. It holds the store, the writer, the next generation, the saved messages, and the prefix record.
- `fn open(store: SessionStore, request: &SessionRequest) -> Result<OpenSession, Refusal>`
- `fn check_prefix(&self, provider: &str, model: &str, tools: &[ToolSchema]) -> Result<(), Refusal>`
- `fn save(&self, model: &str, thinking: &str, messages: &[Message], cwd: &Path, prefix: &PrefixRecord) -> Result<(), String>`

**`src/session_store.rs`**
- `PersistedSession.prefix: Option<PrefixRecord>`, with `#[serde(default, skip_serializing_if = "Option::is_none")]`. `SESSION_PERSIST_VERSION` stays 1.
- `SessionStore::save_oneshot_generation_result (new)`: the writer, the generation, the model, the thinking level, the messages, the working directory, and the prefix record. It returns `std::io::Result<SessionWriteOutcome>`.

**`src/state_root.rs`, `src/paths.rs`**
- `SessionStoreKind::OneShot`, directory name `oneshot`.
- `paths::oneshot_sessions_dir() (new) -> Option<PathBuf>`: `~/.daimonos/oneshot-sessions`.

**`src/agent.rs`**
- `AgentConfig.mark_history_stable: bool (proposed)`, default `false`. When it is true, each request sets `stable_prefix_len` to the message count.

**`src/agent_cmd.rs`**
- `AgentCmdArgs` gets `provider: String`, `session: Option<SessionRequest>`, `sessions_dir: Option<PathBuf>`, `private_state: bool`, and `output: AgentOutputFormat`.
- `struct RunReport (new)`, derives `Serialize`. Its fields are the table of ADR-036 Decision 13.
- `run_agent` keeps its signature. With a session it returns `Err` after it writes the document, when the save failed.

**`src/agent_runtime.rs::run_agent`**
- It resolves the store directory. For a resume, it compares an explicit `--model` or `--provider` with the record before it builds the provider.

**`src/main.rs`**
- An `oneshot_session::Refusal` from the agent command exits through `exit_for_state_failure`. No other error path changes.

**`src/analytics.rs`**
- `AgentRunRecord.oneshot_session_id: Option<String>`. One `ALTER TABLE agent_runs ADD COLUMN oneshot_session_id TEXT` in `migrate_schema`.

## Error and edge behavior

| Case | Result |
|---|---|
| Id is empty, has a character outside `[A-Za-z0-9_-]`, or has more than 128 characters | Exit 2, `session_id_invalid`. No store access. |
| `--session-id` and the id exists | Exit 2, `session_exists`. The record does not change. |
| `--resume` and the id does not exist | Exit 2, `session_not_found`. |
| Another live process owns the id | Exit 2, `session_owned_elsewhere`, with the owner PID when it is known. |
| The generation changed between load and claim | Exit 2, `session_changed`. |
| The record has no prefix record, or a version that this build cannot read | Exit 2, `session_not_resumable`. |
| `--resume` with a `--model` that differs from the record | Exit 2, `session_model_mismatch`. The message names both models. |
| `--resume` with a `--provider` that differs from the record | Exit 2, `session_provider_mismatch`. |
| The tool list differs in count, order, name, or digest | Exit 2, `session_tools_changed`. The message names the first tool that differs. |
| The store directory is not usable | Exit 2, `state_unusable`. |
| Provider error, refusal, or `max_tokens` in a session run | The session is saved. The document is printed. The exit code follows `check_agent_result`. |
| Context overflow | `stop_reason: "error"`, `context_overflow: true`. No compaction and no retry. The session is saved. Exit 1. |
| The save fails or does not return `Saved` | The document has `session_saved: false` and `session_error`. Exit 1. Without `--output json`, stderr names the failure. |
| The thought log fails | As today: an error after analytics. With `--output json` the document is printed first. |
| `--output json` and the run fails before the loop | No document. A message on stderr. Non-zero exit. |
| A tool child process writes to stdout | The bytes do not reach the stdout of `daimonos`. A test proves it. |
| The process is killed during a run | No save. The owner lock is released by the OS. A new `--session-id` run with that id can start, because no record exists. |
| A resume after a failed run whose history ends with a user message | The new input is added as one more user message. See Risks. |

Every refusal prints the `run_configuration_invalid` JSON on stderr with the reason code, and prints nothing on stdout.

## Slices

Each slice is one red-green sequence of tests and ends with `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`.

1. **JSON result.** `--output json` for success, provider error, and context overflow. `session_id` and `session_saved` are null. Check: the stdout byte tests and the comparison with the `agent_runs` row.
2. **Start a session.** `--session-id`, the store directory, the prefix record, the save, `session_exists`, `session_id_invalid`, the failed-save exit code, the analytics column. Check: a record exists after the run, and a second run with the same id is refused with the record unchanged.
3. **Resume a session.** `--resume`, the prefix check, the mismatch refusals, the owner-lock refusal, and the output of only the new assistant text. Check: the mock-provider test of the request prefix.
4. **Stable-prefix mark.** `mark_history_stable` in session runs. Check: the Anthropic request-builder test, and a test that a run without a session keeps the value 0.
5. **Process tests and documents.** A pytest file `tests/test_agent_oneshot_sessions.py (new)` runs the binary against a local fake OpenRouter endpoint: start, resume, concurrent resume, selected state root, and JSON stdout. Then the documents of "In scope" item 5.

## Rollout

- No flag. The three options are opt-in.
- The `agent_runs` column is added by the existing idempotent migration. An older binary ignores the column.
- The session record field is optional. An older binary reads a record that has it, because `PersistedSession` does not deny unknown fields.
- Rollback: revert the change. The directory `oneshot-sessions` and the column stay and have no effect.
- Last step: one real run with the configured provider gives the example JSON document for the report. It costs a small amount of provider usage.

## Risks

| Risk | Response |
|---|---|
| The dbunk branches change `src/agent_cmd.rs`, `src/cli.rs`, and `src/agent_runtime.rs` (PR #282). A later merge conflicts. | Keep the change in the new module where possible. The call sites in the shared files stay small. |
| Two user messages in sequence after a failed run. It is not verified that each provider accepts this. | Slice 3 adds a test of the request for this case with the Anthropic and OpenRouter request builders. If an adapter rejects it, stop and ask for a decision. |
| A cache read does not occur in practice (lifetime, pruning, notices). | The plan does not promise a cache read. The real run of the last step reports `cache_read_tokens` of a resume. |
| A code path writes to stdout in JSON mode. | The process test compares the complete stdout with one parsed document. |
| The store grows without limit. | Named in the documents. Retention is out of scope. |

## Review checklist

- [ ] Each acceptance check of the request document maps to a test of slices 1 to 5.
- [ ] Each Confirmation item of ADR-036 has a test.
- [ ] Each row of "Error and edge behavior" has a test, or the row says why not.
- [ ] `cargo test` and `python3 -m pytest tests/ -v` pass.
- [ ] Existing one-shot tests are unchanged.
