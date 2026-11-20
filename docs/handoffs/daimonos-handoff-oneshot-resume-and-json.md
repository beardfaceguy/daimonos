# Handoff: resumable one-shot sessions and a JSON result for `daimonos agent`

You are working in the daimonos repository. Follow `AGENTS.md` as usual: start in Vikunja, read the accepted ADRs, then the code. This handoff asks for two opt-in additions to the one-shot `daimonos agent` mode. When neither new flag is passed, behavior must stay exactly as it is today.

## Who needs this and why

Konzern (github.com/meetalix/konzern) is a local orchestrator that runs coding work as a pipeline of stages. Its agent stages hand a finished prompt and a model to a **worker agent harness**, and Daimonos is one of the harnesses it will call, as an external program at a pinned version. Konzern's side of that boundary is `src/konzern/agents.py` (`AgentRequest` in, `AgentResponse` out). Konzern's main goal is cutting token cost without losing accuracy.

Two gaps block that today:

1. **Retries resend everything.** When a program stage (tests, lint) fails after an agent stage, Konzern sends the agent the failure output and asks it to try again. Today each `daimonos agent` call starts a new conversation, so the agent pays for the full prompt again and re-reads the files it already read. Konzern has decided to **resume the same session on the same model** for retries, sending only the failure output, so the earlier conversation is served from the provider's prompt cache. When Konzern moves to a stronger model, it starts a fresh session on purpose.
2. **No machine-readable result.** One-shot mode prints only the assistant's text. Konzern needs the outcome and the token counts for every run (its run log records tokens per stage). The only way to get them today is to read Daimonos's internal analytics database (`agent_runs`), which would couple Konzern to Daimonos's schema.

## Process

- File a Vikunja task (project 172, "Agentic CLI tool", or 183, "daimonos-agent", whichever fits your conventions) and reference it in commits.
- Both changes add a public CLI contract and change the one-shot session lifecycle, so write **ADR-036** (status Proposed) before building, in the style of ADRs 021–035. Get the maintainer's approval, then build.
- New crates need confirmation (review checklist). Model-facing text belongs in `prompts/`, not Rust literals.
- Tests in the same change: Rust tests for the new paths, and pytest where the MCP side is touched. Both `cargo test` and `python3 -m pytest tests/ -v` must pass.

## Change 1: name and resume one-shot sessions

**Today:** `agent_cmd::run_agent` (`src/agent_cmd.rs`) builds `initial = vec![Message::user(task)]`, runs `agent::run`, prints assistant text, and records an `agent_runs` analytics row. The conversation is not saved. `chat` already persists and resumes through `SessionStore::save` / `load` (`src/session_store.rs`, `src/chat_cmd.rs`, Vikunja #963). Reuse that store; don't add a second one.

**Requested behavior** (flag spellings are proposals; settle them in the ADR):

- `daimonos agent --session-id <id> "<task>"` runs a new one-shot session under the caller's id, and saves the conversation and the model when the run ends. If a session with that id already exists, fail with a clear error; never overwrite.
- `daimonos agent --resume <id> "<new input>"` loads the saved conversation, appends the new input as a user message, runs, and saves again.
- **Same model only.** A resumed session runs on the model it was saved with. If `--model` is passed and differs, fail with a clear error. (`chat --resume` lets `--model` win; for one-shot callers a silent model change would throw away the cache the resume exists for.)
- **Cache-friendly prefix.** The resumed request must start with the same bytes as the earlier one: the same system prompt, the same tool schemas in the same order, and the same earlier messages. Today the system prompt is rebuilt per run by `prompts::agent_system_for_workspace`, which reads the workspace. The agent has usually just edited that workspace, so a rebuilt prompt can differ and miss the cache. Save the system prompt (or what's needed to rebuild it exactly) with the session, and reuse it on resume. If the tool schema set differs on resume, fail rather than continue on a silently different prefix.
- **Saving must not be best-effort here.** `SessionStore::save` logs and swallows failures, which suits chat. When `--session-id` or `--resume` is used, the caller depends on the save, so a failed save must exit non-zero.
- **Context growth.** One-shot mode runs with no compaction (`AgentConfig::default()`). Keep it that way for resumed sessions: compaction rewrites the prefix and defeats the cache. If a resumed run hits a context overflow (`AgentResult.context_overflow`), end with that as the stop reason so the caller can start a fresh session.
- **Concurrency.** One writer per session id. Use the existing generation / owner-lock rules (ADR-015, ADR-033) to reject a concurrent resume of the same id instead of interleaving.
- **State placement.** Under a caller-owned state root (ADR-021), the session store must live under that root.
- **Analytics.** Keep recording one `agent_runs` row per invocation, with the session id recorded so a caller can total a session's attempts.

## Change 2: `--output json` for one-shot runs

**Requested behavior:**

- `daimonos agent --output json ...` prints exactly **one JSON document and a newline** on stdout, and nothing else on stdout. The assistant's text goes inside the document; logs and diagnostics stay on stderr.
- Print the document whenever a run started, **including when it failed** (provider error, refusal, context overflow), so the caller can still record the tokens spent. Only argument or configuration errors that stop a run from starting may skip it (stderr message, non-zero exit). Keep today's exit-code semantics from `check_agent_result`.
- Suggested fields (settle names and types in the ADR, version them, and document them in `docs/runtime-modes.md`):

```json
{
  "schema_version": 1,
  "session_id": "konzern-run-42-code",
  "resumed": true,
  "provider": "anthropic",
  "model": "claude-...",
  "stop_reason": "end_turn",
  "error_message": null,
  "context_overflow": false,
  "final_text": "Changed greet() to return 'hi'; tests pass.",
  "turns": 5,
  "usage": {
    "input_tokens": 1830,
    "output_tokens": 412,
    "reasoning_output_tokens": 0,
    "cache_read_tokens": 41200,
    "cache_write_tokens": 2100,
    "cost_usd": 0.0731
  },
  "evidence": { "workspace_changed": true, "verifier_passed": true },
  "duration_ms": 18450
}
```

- `usage` covers **this invocation only**, not the whole session. Konzern totals attempts itself.
- `model` is the model that actually served the run. One-shot mode has failover off (`failover_models` is empty by default), so today that's the requested model. If failover is ever enabled for one-shot runs, report every model that served a call.
- `evidence` comes from `AgentResult.evidence` (Vikunja #1198); include whatever it reliably reports.

**Open question for the ADR:** how this combines with ADR-024's structured final output (WP05), which emits only the validated document on stdout. One option is to keep `--output json` as the envelope and put the validated document in a `final_output` field when both are enabled. Decide it in ADR-036 rather than leaving it to the implementation.

## Acceptance checks

- Without the new flags, one-shot output and behavior are unchanged (existing tests pass unmodified).
- `--session-id` saves a session; reusing an existing id fails without overwriting.
- `--resume` continues the saved conversation. A test with a mock provider shows the resumed request's system prompt, tools and earlier messages are byte-identical to the first run's, and only the new user message is added.
- `--resume` with a different `--model` fails with a clear error.
- A failed save under `--session-id` or `--resume` exits non-zero.
- `--output json` prints one parseable document on stdout for success, provider error and context overflow, with per-invocation usage that matches the `agent_runs` row.
- A concurrent second `--resume` of the same id is rejected.

## Out of scope here

These are already planned elsewhere or tracked separately:

- Structured done / failed / needs-a-decision outcome: ADR-024, WP05.
- Explicit-only discovery, so a caller's prompt isn't joined by the workspace's own AGENTS.md and skills: ADR-021, WP11.
- Model-request ceiling: ADR-024.
- Recording the model on each `agent_runs` row, and reading the task from a file or stdin.

## When you're done

Report back with: the ADR number and status, the Vikunja task, the PR, the final flag names, and an example JSON document from a real run.
