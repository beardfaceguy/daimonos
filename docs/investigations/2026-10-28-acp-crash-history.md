# ACP history loss after an unexpected logout

Date: 2026-10-28
Status: Repair implemented and tested locally. The installed agent remains unchanged.
Repository: `/home/beardface/work/daimonos`

## Report

The laptop unexpectedly returns to the login screen. Applications close.
After Zed restarts, Daimonos ACP sessions can lack recent history.
The user estimates a loss of up to 30 minutes. The exact loss interval is unknown.
Normal session closure appears to preserve history.

## Scope and safeguards

- Investigate Daimonos persistence and crash recovery first.
- Use this local log. Do not access or update external project management systems.
- The desktop logout and history loss may have separate causes.
- Do not assume that normal-close behavior proves crash safety.
- Do not alter live session files or copy private conversation content into this log.
- Preserve the existing branch and unrelated files.

## Initial state — 18:00 UTC

- Branch: `plan/1575-dbunk-work-packages`.
- HEAD: `9dd840c` (`Merge pull request #254 from beardfaceguy/feat/1575-dbunk-reviewer`).
- Three unrelated files are untracked. They remain untouched:
  - `daimonos-feature-requests-for-dbunk.md`
  - `daimonos-pm-answers-round-1.md`
  - `docs/handoffs/2026-10-28-dbunk-remaining-work.md`
- The root context map currently covers git reads only.
- No graphify graph appeared in the indexed paths.
- Repository instructions normally require Vikunja. The user's explicit local-only instruction takes precedence for this investigation.

## Working hypotheses

1. Daimonos saves history only at a turn or session boundary.
2. Daimonos acknowledges updates before it durably stores them.
3. Restore selects an older snapshot or a different session path.
4. Zed loses client-side state that Daimonos cannot restore.
5. A resource failure stops the desktop and agents. This remains separate from persistence correctness.

These are hypotheses, not findings.

## Next checks

1. Read persistence ADRs and scoped repository rules.
2. Trace ACP notifications, history commits, storage writes, and session restore.
3. Inspect installed process and storage metadata without exposing conversation text.
4. Reproduce abrupt termination with an isolated session and synthetic messages.
5. Compare normal closure with abrupt termination.

## Recovery instruction

If this chat stops, read this file first. Preserve unrelated work.
Continue from the latest evidence entry. Do not infer that unrecorded tests passed.

## Contract review — step 2

Read AGENTS.md, scoped rules, strategies, and Accepted ADRs 014, 015, and 016.
ADR-014 makes SQLite the canonical store, with generation and writer-epoch guards.
ADR-015 defines saved, unsaved, saving, degraded, and superseded durability states.
ADR-016 keeps normal turns transactional. Cancellation materializes an in-memory turn journal.
The journal retains authoritative provider messages and completed tool results.
Unfinalized stream text is deliberately absent from persisted model-facing history.
The source still needs inspection to determine whether hard termination bypasses journal materialization.
The script catalog contains a local memory monitor. Do not start a service without a separate decision.
No production code changed. No external tracker was used.

## Source trace — step 3

`SessionCore::capture_persistence` copies `AgentSession::history()`, not the active turn journal.
`SessionCore::persist_current` serializes saves and waits for a SQLite write.
`SessionStore::write_record` writes the payload inside an explicit transaction and commits it.
`SessionStore::connection` selects WAL mode. No explicit synchronous override appeared in the inspected connection code.
The ACP retry path calls `persist_current` only after completion or cancelled finalization.
The normal prompt path still needs its complete source review.

The installed `~/.cargo/bin/daimonos` binary has an October 19 modification time.
The repository HEAD is newer. Do not assume the installed binary matches this checkout.
The binary rejects `--version`, so that command did not identify a build version.
Only one process named daimonos appeared in the metadata sample. Its RSS was about 58 MiB.
That sample does not establish historical memory usage or identify the logout cause.
The home filesystem was 95% used, with about 24 GiB available. It was not full at this sample.
Repowise reported a stale index and unavailable synthesis. Its answer did not establish the persistence behavior.
Continue with live source and isolated tests.

## Finding from source — 2026-10-28T18:08:14Z

`SessionCore::prompt_with_active_turn` holds the AgentSession mutex throughout the provider/tool loop.
It emits the user message and marks durability unsaved before starting that loop.
It calls `commit_staged` or `finalize_cancelled_turn` only when the loop returns or cancellation wins.
Only then does it release the session mutex and call `persist_current`.

`AgentSession::begin_turn_journal` stores the suffix in an `Arc<Mutex<TurnJournalState>>`.
`finalize_cancelled_turn` moves this suffix into canonical messages. No disk-backed journal appeared in this path.
Therefore the inspected code has no mid-turn crash checkpoint for authoritative tool results.
A process kill can lose an entire long active turn, not just unfinalized stream text.
This is a source finding. A synthetic process-level reproduction is the next confirmation.
Completed-turn recovery still needs an independent control test.

The installed binary SHA-256 is `f13d5ecfbfed88d0c72a06312fd2958c557a3aee1400a393021c78b357001b75`.
The currently running daimonos executable has the same hash.
That observation does not prove which binary Zed launches for ACP.

## Synthetic probe prepared

Reusable script: `/home/beardface/work/agentic_scripts/acp-crash-recovery/probe.py`.
The catalog and README document its isolation and usage.
Installed-binary experiment directory: `/home/beardface/.local/state/daimonos-crash-probe/installed-20261028T181323Z`.
Each case writes a durable result before the next case starts.
The probe signals only its own child processes. No live session store is modified.

## Installed-binary reproduction confirmed — 2026-10-28T18:15:04Z

The four-case synthetic ACP probe completed successfully against the installed October 19 binary.
Its SHA-256 matches the live daimonos executable sampled earlier.

| Case | Prior completed turn | Active user prompt | Completed assistant step | Completed tool result |
|------|----------------------|--------------------|--------------------------|-----------------------|
| Completed turn, then SIGKILL | Restored | Not applicable | Not applicable | Not applicable |
| Active turn, then SIGKILL | Restored | Lost | Lost | Lost |
| Active turn, explicit session/cancel | Restored | Restored | Restored | Restored |
| Active turn, normal completion | Restored | Restored | Restored | Restored |

Before terminating the active process, the local provider received the completed read_file result.
This proves that the missing tool result was authoritative, not merely partial streamed text.
The saved payload still contained only the two baseline messages while that turn was active.
Cancellation or normal completion increased the payload to six messages.
Each synthetic SQLite database returned `ok` from its quick check.
ACP session/load replay matched the persisted markers after process restart.

This reproduces a Daimonos durability gap without Zed.
It does not prove the cause of the laptop's logout or corruption of already saved turns.
It can explain a long loss interval if the agent remains inside one long tool-running turn.
Do not claim that this explains multiple earlier completed prompts without additional evidence.
EOF and SIGTERM checks remain next, followed by a comparison with the existing newer debug build.
No installed binary or production source was changed.

## Graceful transport termination also loses the active turn — 2026-10-28T18:16:14Z

Artifact directory: `/home/beardface/.local/state/daimonos-crash-probe/installed-signals-20261028T181504Z`.
Both SIGTERM and stdin EOF returned process exit status 0.
Both restored only the two baseline messages, despite a confirmed completed tool result in the active turn.
The active prompt, completed assistant step, and completed tool result were absent from storage and ACP replay.
The synthetic SQLite databases remained healthy.

The shutdown source signals cancellation and awaits MCP bridge shutdown.
It does not await each active turn's cancelled finalization and persistence before returning.
This creates a second failure path beyond uncatchable SIGKILL.
An ordinary transport shutdown can finish before the cancelled turn becomes durable.
A proposed repair must address both durable in-flight checkpoints and graceful shutdown draining.
No repair is implemented or approved in this investigation.

The probe now hashes binaries incrementally to avoid loading large debug binaries into memory.

## Newer debug-build comparison — 2026-10-28T18:18:28Z

All six cases completed against the existing October 27 debug build.
Artifact directory: `/home/beardface/.local/state/daimonos-crash-probe/debug-20261028T181614Z`.
SIGKILL, SIGTERM, and stdin EOF each lost the active turn's authoritative messages.
Explicit cancellation and normal completion preserved those messages.
Previously completed history survived every tested termination case.
Each synthetic SQLite quick check returned `ok`.

The existing debug build was not rebuilt. This avoids compilation load during the logout investigation.
Its exact source commit is not established. The current inspected source independently contains the same failure paths.
The installed binary and the newer debug build therefore show the same observed behavior.
A configured-installation metadata check is next. It reads no live conversation payloads.

## Actual Zed configuration confirmed — 2026-10-28T18:20:44Z

Metadata artifact: `/home/beardface/.local/state/daimonos-crash-probe/installation-20261028T1818.json`.
Zed's `daimonos` agent launches `/home/beardface/.cargo/bin/daimonos` with the `acp` argument.
Its configured executable hash matches the installed binary used in the reproduction.
The live process also has an ACP argument and uses that executable.
This ties the reproduced behavior to the configured agent, not merely an unrelated checkout.
The live session store is `~/.daimonos/acp-sessions/sessions.sqlite3`.
Only filesystem metadata was inspected. No live conversation payload was opened or changed.
The store had no WAL or SHM file in this sample. That alone does not indicate a fault.
Zed config names an OpenRouter tracing variable. Its value and trace payloads were not inspected.

Daimonos cannot be ruled out. It independently reproduces the missing-history symptom.
This does not establish the logout trigger or loss across multiple previously completed prompts.
A shutdown drain alone cannot protect against SIGKILL. Durable in-flight checkpoints also need a reviewed design.
No production code or installed binary changed. No external tracker was accessed.

## Investigation checkpoint — 2026-10-28T18:21:28Z

The initial investigation is complete. No repair was attempted.
All six termination/control cases ran across the installed binary experiments.
All six cases also ran against the existing newer debug build.
Every case confirmed successful baseline persistence and restart replay.
The probe process environments already exclude inherited DAIMONOS and XDG variables.
Synthetic artifacts remain under `~/.local/state/daimonos-crash-probe/`.
The reusable probe, metadata inspector, README, and catalog entry remain under `~/work/agentic_scripts/acp-crash-recovery/`.

Recommended repair sequence:
1. Retain process-level regression tests for active-turn termination.
2. Make graceful shutdown await cancelled finalization and its durable save.
3. Design durable checkpoints for accepted prompts and authoritative completed provider/tool messages.
4. Keep unfinalized stream text separate from provider history.
5. Preserve ADR-014 writer epochs and generations, plus ADR-016 tool-call/result invariants.

Until repaired, explicit cancellation followed by its completion response preserved the tested authoritative state.
Waiting for normal turn completion also preserved it.
Closing the transport during an active turn did not preserve it in these tests.
These mitigations cannot protect against unexpected process death.

Unverified questions:
- The desktop logout trigger is still unknown. No OOM, compositor, or login-manager cause was established.
- Loss across several previously completed prompts was not reproduced.
- Physical power-loss durability was not tested.
- The exact commit of the pre-existing debug binary was not established.
- Real conversation payloads and provider traces remain uninspected.

_Anchors:_ src/session_core.rs::SessionCore::prompt_with_active_turn, src/session_core.rs::SessionCore::capture_persistence, src/agent.rs::AgentSession::begin_turn_journal, src/agent.rs::AgentSession::finalize_cancelled_turn, src/acp_cmd.rs::shutdown_session_handles_with, src/acp_cmd.rs::replay_history, src/session_store.rs::SessionStore::write_record

## Repair authorized — 2026-10-28T18:42:04Z

The user requested shutdown draining and durable in-flight checkpoints.
The desktop logout/OOM investigation remains separate. Do not induce OOM or a logout.
The user suspects loss of prior completed prompts but has no proof.
Retain the completed-turn kill control while testing the repairs.
Continue local-only tracking and frequent durable log updates.
Normal turns must remain transactional. Recovery must omit unfinalized streamed text.

## Shutdown drain slice — 2026-10-28T18:51:38Z

Created branch `fix/acp-crash-durability`. Unrelated untracked files remain untouched.
Added a regression test that failed because teardown returned before cancelled-turn finalization.
TurnController now notifies waiters when its active permit is released.
ACP teardown awaits that release and bridge shutdown under the existing bounded deadline.
The permit already spans the terminal persistence write.
All four focused transport-teardown tests pass.

Checkpoint design: reuse the existing persisted session record as a recovery projection.
Do not introduce a second table, a separate queue, or an unbounded save worker.
Keep live canonical history staged until the existing terminal commit.
Save authoritative message boundaries through the existing writer epoch and generation checks.
Materialize pending tool calls with the existing interruption policy for restart validity.
Do not persist individual streamed chunks. Do not rebuild or replace the installed binary yet.

## Checkpoint first slice — 2026-10-28T18:59:28Z

The accepted-prompt test failed before the provider started because storage was empty.
It now passes. SessionCore installs an awaited recovery-save hook and saves the prompt before publishing the user event.
The hook reuses writer fencing, capture generations, bounded retries, and degraded durability reporting.
No schema or session-format migration is required.
ADR-020 records the recovery-projection design with Proposed status. It is not reviewer-accepted.
The next regression uses the synthetic process probe to require completed tool results after SIGKILL.

## Authoritative message checkpoint slice — 2026-10-28T19:04:10Z

The process-level SIGKILL regression failed after prompt-only persistence: assistant and tool steps were absent.
The awaited hook now runs after completed provider messages and each completed tool result.
The same real-process regression now passes, including the completed tool result and excluding unfinished stream text.
Retry turns install the same hook with replacement metadata, not a new user-turn identity.
Callbacks release the in-memory journal lock before awaiting blocking persistence.
Each boundary waits for its save. There is no per-token queue or thread spawning.
The checked-in synthetic helper and pytest regression no longer depend on the external script catalog.
The installed binary remains unchanged. Broader regression checks are next.

## First full regression pass — 2026-10-28T19:11:03Z

The six checked-in real-process tests pass for SIGKILL, SIGTERM, EOF, explicit cancellation, normal completion, and completed-turn SIGKILL.
The tests require authoritative assistant/tool recovery and preserve the completed baseline.
They exclude unfinished stream text from interrupted-turn replay.
The full Rust suite passed: 2018 passed, 10 ignored, 0 failed.
Clippy passed with warnings denied before this full run.
Builds used one cargo job. Rust tests used two test threads to limit resource pressure.
The installed binary and active Zed session remain unchanged.
A metadata/failure-path review and the full Python suite remain before completion.

## Python regression pass — 2026-10-28T19:15:04Z

The full Python suite passed: 392 passed, 1 skipped, 0 failed.
The run included all six new synthetic crash/control cases.
Output is saved at `target/acp-crash-pytest.log`.
A final targeted review is checking compaction metadata and persistence failure reporting.
Do not replace the installed binary with this debug build without a separate installation step.

## Final review additions — 2026-10-28T19:24:33Z

A targeted compaction test exposed stale client-message IDs in a recovery snapshot.
The hook now aligns IDs and outcomes with the actual captured history after compaction.
The regression failed before that fix and now passes.
A failure-injection test confirms degraded reporting and a later successful checkpoint.
The live canonical history remains unchanged by both failed and successful recovery-only saves.
Degraded status deliberately remains visible until a full terminal save, per ADR-015.

Added a session-durability glossary and context-map entry.
ADR-020 remains Proposed. No reviewer acceptance is claimed.
The shared check_docs.py tool is absent on this machine. Scoped anchors will be checked directly.
All suites must run again after these final changes.

## Repair verification complete — 2026-10-28T19:32:01Z

Branch: `fix/acp-crash-durability`, based on `9dd840c`.
The repair is ready in this checkout. It is not installed or pushed.

Final checks after the compaction fix:
- Rust: 2020 passed, 10 ignored, 0 failed.
- Python: 392 passed, 1 skipped, 0 failed.
- Clippy: all targets passed with warnings denied.
- Formatting and git diff whitespace checks passed.
- Six process-level cases cover SIGKILL, SIGTERM, EOF, cancellation, completion, and previously completed history.
- New unit tests cover accepted-prompt persistence, teardown draining, compaction metadata, and storage failure reporting.

The runtime log files are `target/acp-crash-rust.log` and `target/acp-crash-pytest.log`.
The shared documentation checker is unavailable. New source anchors were checked directly against the checkout.
ADR-020 records the design with Proposed status. No independent reviewer acceptance is claimed.

Implementation files:
- `src/acp_cmd.rs`: await active-turn finalization during bounded teardown and configure retry checkpoints.
- `src/agent.rs`: awaited recovery hooks at authoritative journal boundaries.
- `src/session_core.rs`: recovery captures, writer/generation guards, bounded retries, metadata alignment, and idle notification.
- `tests/acp_crash_probe.py`: portable, isolated synthetic ACP probe.
- `tests/test_acp_crash_recovery.py`: real-process regression assertions.

Safety and remaining limits:
- No OOM or desktop logout was induced.
- No live session payload or installed executable was modified.
- No external tracker was accessed.
- Unfinished stream text remains outside recovery history.
- External side effects cannot be atomic with a session save. A kill before the next durable boundary can still interrupt an action.
- Storage failure remains visible as degraded durability. A shutdown deadline can expire.
- Physical power-loss testing and the unproved loss of multiple previously completed prompts remain separate work.

To use the fix, install a binary from this branch and restart the ACP agent.
The currently running agent keeps its old code until restart.
Keep this session intact until its current turn completes or explicit cancellation finishes.
A local commit preserves this repair and this log. No remote push is authorized.
