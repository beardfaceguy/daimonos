# Task 1335 — Typed errors and durability review, Round 1
Review live code. Proposal: typed Results for daemon store/catalog/load/delete (not found, version, corrupt, permission, IO); stable privacy-safe protocol codes for list/attach/snapshot/persistence, optional additive AttachDenied.code prerequisite before switching; list incomplete warnings; fatal attach rollback. SessionCore tracks canonical DurabilityStatus Saved{through_seq}, Saving, Degraded{code,retryable}, emits dedup event and snapshot; raw OS/path only logs. Save failure nonfatal persistent banner until later success. Bounded retry only retryable; never fresh-create after load failure. Integrate catalog consensus: payload save vs index lag distinct, materialized tombstones, incomplete snapshots. Find sequencing/durability truth/error mapping/privacy problems. End ACCEPT or REVISE.

> Model claude-fable-5-high.

## Claude Fable — Round 1

Review complete. I verified the proposal against the live store, factory, core persistence path, daemon attach/list/stop/eviction paths, the wire protocol, both client codecs (Rust serde and Android kotlinx), and the prior consensus records for 1333/1334/1336/1337/1339. The typed-error and durability architecture is sound and the prior-consensus integration is correctly stated, but I found five blocking problems — two sequencing, one durability-truth, one delete-lifecycle, one protocol-compatibility. Per the protocol I don't write files; append this response verbatim to `agent_review/1335-typed-errors-durability.md`.

## Review Response — Round 1

**Position:** PARTIAL

What the live code establishes: `SessionStore::load` collapses not-found / unreadable / corrupt / future-version into `Option::None` with the underlying `io::Error` discarded unlogged (`src/session_store.rs:152-157`); `save_record` swallows write failures to stderr with no success signal (135-137); the factory maps load-`None` to a string error, and the daemon flattens it to `OpenFailed` → free-text `AttachDenied { reason: "session could not be opened" }` (`src/session_factory.rs:75-90`, `src/session_daemon.rs:248`, 2062). The daemon never falls back to Create for a requested id (`open_session`, session_daemon.rs:207-214) — the never-fresh-create rule already holds server-side and today binds only future client code, since `HeadlessFrontend::attach` has no non-test caller. Fatal-attach rollback deletes only *generated* sessions (`HandshakeSessionGuard` → `rollback_generated_session`, session_daemon.rs:630-641); a Load-opened session survives to idle retention, matching 1333. The proposal's typed store errors, privacy-safe codes, and additive `AttachDenied.code` (1339 option (b)) all have clean landing spots.

### Blocking Concerns

**B1 — On-disk save order is not payload order; `Saved{through_seq}` requires a monotonic write guard, not just typed results.** Two live windows let an older captured payload overwrite a newer one. (a) `set_runtime_config` captures history under the session lock (session_core.rs:726), releases it (731), then crosses an await on `client_user_message_ids` (732) before persisting (733) — and it holds `lifecycle`, which `prompt` never takes, so a full fast turn can begin, complete, and persist newer history inside that window; the config path then writes the older capture last. (b) The prompt path releases the turn slot (`drop(active_turn)`, 1145) *before* persisting (1148), so the next turn is admissible while the previous persist is still pending. `SessionPersistence::save` serializes writes (state mutex held across `save_acp`, 556-572) but not their order. Any `DurabilityStatus` derived from "last save that returned Ok" can then report `Saved{through_seq=S_new}` while the file holds older bytes — the exact durability-truth failure this task exists to prevent. Required: capture `through_seq` atomically with the payload capture, and have the persistence layer skip/refuse writes whose seq is below the last successfully written seq (per-core in-memory guard is sufficient; it is the payload-level analogue of 1336's generation guard). Bounded retries must re-capture current state, never re-write a stale capture.

**B2 — `through_seq` semantics must be pinned to what the payload can actually claim; three live facts constrain it.** (a) There is no atomic capture point today: history is snapshotted at session_core.rs:1109, `assistant_outcomes` are read later inside `persist` (867-870), and `AssistantDone` is emitted between them (1143). Define the capture point (e.g. read the router sequence under the dispatch lock at payload capture, the way `subscribe_and_capture` does at 399-412) or `through_seq` is fiction. (b) The payload is a lossy, *intentionally incomplete* projection: a cancelled turn never persists (`history_snapshot` is `None` when `turn` is `None`, 1109), so the cancelled turn's `UserMessage` event may never be covered by any save — `through_seq` can lag `latest_seq` until the next successful save, and the design must say whether that renders as unsaved-changes (it genuinely is unsaved data that eviction will destroy) and must not let the banner logic treat lag alone as Degraded. (c) Sequence numbers are core-instance-local and restart near 0 on every load (`initial_snapshot` seq = `events.latest_sequence()` of a fresh router, session_core.rs:975); `DurabilityStatus`/`through_seq` must never be persisted into the payload or the 1336 catalog nor compared across core instances.

**B3 — Degraded + idle eviction or shutdown silently destroys the only remaining copy, and nothing triggers "later success" for an idle session.** `evict_idle_sessions` → `finish_removed_entry` and `end_session` never persist (session_daemon.rs:478-548, 555-628); shutdown ends sessions with `delete_persisted=false` and no save. That is safe today only because turn-end saves are assumed durable; under the proposal's own model, a Degraded session's memory is ahead of its disk. Idle retention can evict a Degraded session with zero clients attached — nobody ever sees the banner — and `shutdown_grace` ends it the same way. Separately, saves are only triggered by turn end and config change, so "banner until later success" has no trigger for an idle session unless the design schedules retries. Required: (i) a stated terminal rule — final save attempt in the eviction/shutdown path, or refusal/deferral of eviction while Degraded (bounded, since disk-full may never recover — pick and bound it), or an explicitly accepted, logged loss; and (ii) a stated retry trigger and backoff for "bounded retry only retryable."

**B4 — Delete failure is unrecoverable in the live sequencing, and one existing path already violates the privacy rule.** `end_session` removes the entry from the map *before* `finish_removed_entry` attempts `delete_persisted` (session_daemon.rs:571-601, 616-618). On delete failure the client receives code `session_delete_failed` with the **raw `io::Error` string on the wire** (`message: error.to_string()`, session_daemon.rs:1123 — fix this in the same change), `SessionEnding` was already emitted, and a retry of `StopSession` returns `changed: false` because the session is gone from the map — the payload file becomes a permanent orphan that re-lists as durable. Typed delete errors alone cannot fix this; the design must resequence: attempt payload deletion before removal commits (failure → session stays resident and the operation is retryable), or make deletion of non-resident sessions expressible in the protocol. With the 1336 integration, the materialized tombstone must be recorded only on confirmed payload deletion — which requires a delete path that can actually confirm or retry.

**B5 — A new `SessionEvent` variant is not additive, so the dedup durability event as proposed breaks version-skewed clients.** `SessionEvent` is `#[serde(tag = "type")]` (session_protocol.rs:238-239) with no unknown-variant tolerance; the Rust client's `recv` turns a parse failure into a connection-fatal `TransportError` (client_transport.rs:273), and the Android codec's sealed interface with `@JsonClassDiscriminator("type")` throws on an unknown discriminator — `ignoreUnknownKeys = true` (ProtocolCodec.kt:11) covers unknown *fields* only. Skew is real in both directions: the daemon is a long-lived separately-spawned process (the stale-binary caveat is documented for ACP), and the Android app ships independently. This is exactly why 1339 chose an additive optional *field* for `AttachDenied.code`; an enum *variant* does not enjoy that property. Required, pick one and state it: a protocol version bump (old clients are cleanly denied at attach, session_daemon.rs:771-781); unknown-event tolerance in both clients landing *before* the daemon ever emits the new variant (an ordering constraint the plan must name); or carrying durability as an additive field on an existing carrier. The snapshot half is safe as an additive `#[serde(default)]` field — and `SnapshotState::apply` (session_daemon.rs:1734) must fold the durability event so late attachers and gap-sync snapshots carry the banner, with dedup keyed on status *transition*, not per-save (otherwise every turn appends an event).

### Non-Blocking Suggestions

**S1** — Typed store errors must come with actual logging: `load` currently discards the `io::Error` entirely (`.ok()?`) — there is no log to put the raw OS detail *in* — and `save_record` uses `eprintln!` rather than `tracing`. Route both through the tracing subscriber so "raw OS/path only logs" has a real sink.

**S2** — In the typed error set, keep the version policy asymmetric per the 1336 B1 pattern: a *newer* on-disk `version` is a distinct `Version` error (never rewritten, never reported as corrupt or not-found), while corrupt means parse failure of a current-version file — which, given the atomic temp+rename writer, implies external interference and deserves its own code.

**S3** — The workspace-mismatch load failure (raw `PathBuf` equality, session_factory.rs:83-90) currently disappears into the generic `OpenFailed` flattening. Give it its own stable code, and note it is the same comparison the 1336 B4 consensus requires to become canonical — land the code and the canonicalization coherently.

**S4** — List incomplete warnings: `list()` silently skips unreadable/unparseable files (session_store.rs:184-191). The warning carrier should be a count or boolean on the `SessionList` response (additive field, safe in both skew directions since no type uses `deny_unknown_fields`), never filenames or paths — consistent with the trust-gating from 1337.

**S5** — "Payload save vs index lag distinct" should be stated as an invariant: `DurabilityStatus` is computed exclusively from payload save results; catalog upsert lag, catalog incomplete, or tombstone reconciliation state must never set Degraded — they surface only through the 1336 `incomplete=true` listing marker.

**S6** — Create-mode sessions persist an empty record at open (session_factory.rs:221) with the failure swallowed. Under the new model this is the natural birth of a Degraded status (creation proceeds, banner shows) — state that explicitly so implementers don't make initial-save failure fatal to session creation, which would contradict "save failure nonfatal."

### Responses to Prior Points

Not applicable — Round 1. Prior-consensus integration is correctly restated: the additive `AttachDenied.code` prerequisite matches 1339 option (b); fatal attach rollback correctly preserves the generated-vs-loaded asymmetry (a loaded payload is never deleted on attach failure — keep it that way); materialized tombstones and incomplete snapshots match 1336 B8/S4.

**Verdict:** REVISE

---

**REVISE** — the architecture needs no redesign: B1/B2 are the sequencing and truth-definition work that makes `Saved{through_seq}` honest, B3/B4 are lifecycle rules the durability model newly obligates, and B5 is a wire-compatibility decision that must be named before the event ships. All five have small, stateable resolutions; an ACCEPT with those commitments reaches consensus without a further round from me.

## Daimonos response — Round 2

Accept B1-B5 and S1-S6:
- Create an atomic persistence capture under appropriate session/router locks containing payload plus core-instance-local through_seq. A per-core serialized latest-wins writer rejects lower seq; retry recaptures newest state. Never persist/compare through_seq across core instances. Cancelled/unsaved event lag shows `Unsaved` distinct from `Degraded`; status options become Saved, Unsaved, Saving, Degraded.
- Retry retryable failures via bounded exponential backoff/coalesced latest capture. Degraded/Unsaved sessions are not normally evicted. Shutdown/retention performs a bounded final save/retry within grace; if still failing, log structured data-loss risk and retain payload file, but process shutdown may lose in-memory delta (explicit unavoidable outcome). Memory retention has configurable max extension to avoid immortal sessions.
- Resequence StopSession(delete): attempt and confirm payload deletion before committing removal; failure leaves session resident/retryable and sends redacted stable code. Catalog tombstone only after confirmed unlink.
- Choose protocol version bump for durability event/timeline changes; old clients denied cleanly. Snapshot adds status and SnapshotState folds transition events. Dedup only transitions. AttachDenied.code can land earlier as additive field without bump.
- Structured tracing, asymmetric version, canonical workspace mismatch, bounded warning counts, payload-only durability, and nonfatal degraded initial create all accepted.

Nuance: atomic capture must avoid deadlock across AgentSession/router/outcome maps; implement one SessionCore method with documented lock order and tests rather than callers assembling it. Any blocker left?

## Claude Fable — Round 2

Consensus is reached — all five blocking concerns were ACCEPTed with concrete revision plans that match what round 1 required, and I verified the one live-code question the nuance raises: `SessionEventRouter::emit` runs handlers synchronously while holding the `dispatch` lock (session_core.rs:324-372), which confirms the deadlock vector is real and shapes the lock-order rule. Here is my response for the worker to append verbatim to the log.

## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns

None. B1-B5: resolved, per the ACCEPT commitments in the worker's Round 2 message.

### Non-Blocking Suggestions

**S7 — The `Saving` status must not defeat B5's transition-only dedup.** With four statuses, every healthy save cycle is `Saved → Saving → Saved` — two "transitions" per turn, which re-creates the per-save event spam the dedup rule exists to prevent. Recommend: `Saving` is visible in snapshots/polls only and never emits a timeline event; events fire only on transitions between the durable-truth classes (`Saved`/`Unsaved`/`Degraded`). Alternatively, drop `Saving` from the event vocabulary entirely and keep it a snapshot-only field.

**S8 — The new tunables belong in `Config` per repo rules.** Backoff bounds, retry cap, shutdown-grace save budget, and the max retention extension for Degraded/Unsaved sessions must land in `daimonos.default.toml` + the relevant config section (configurable-limits rule), not as constants. The retention extension also satisfies the bounded-collections rule for the daemon's session map — good that it's already in the plan.

### Responses to Prior Points

Re B1/B2 ACCEPT: resolved — atomic capture with core-instance-local `through_seq`, per-core serialized latest-wins writer rejecting lower seq, retry-recaptures-newest, and never persisting/comparing `through_seq` across core instances is exactly the required shape. The `Unsaved` status for cancelled/unsaved event lag correctly separates "data exists only in memory" from "save machinery is failing."

Re B3 ACCEPT: resolved — bounded backoff with coalesced latest capture supplies the missing retry trigger; eviction deferral with a configurable max extension plus a bounded final save at shutdown/retention, ending in a structured data-loss log with the payload file retained, is a complete and honest terminal rule. Losing the in-memory delta at process exit after a bounded best effort is indeed unavoidable and correctly named as such.

Re B4 ACCEPT: resolved — confirm-unlink-before-removal-commit makes delete retryable and makes the 1336 tombstone recordable only on confirmed deletion; the redacted stable code also fixes the raw `io::Error` currently on the wire at session_daemon.rs:1123.

Re B5 ACCEPT: resolved — version bump for the event/timeline change with clean denial of old clients, additive snapshot `status` with `SnapshotState` folding transition events, and `AttachDenied.code` landing earlier as an additive field is the coherent split of the three options. See S7 for the one dedup refinement.

Re S1-S6 ACCEPT: recorded.

Re the nuance (R-nuance): the single-`SessionCore`-method approach is correct, and the live code pins three constraints the documented lock order must state. (1) `emit` invokes handlers synchronously under the router's `dispatch` lock (session_core.rs:324-372), so event handlers must never acquire `AgentSessionShared` state locks, and the capture method must never be reachable from inside a handler — otherwise capture (state locks → `dispatch`) and emit (`dispatch` → handler → state locks) deadlock by inversion. (2) Ordering for honesty: read `through_seq` from the router no later than the state capture it accompanies, relying on the mutation-before-emit invariant so `through_seq` can only understate, never overclaim; add a test that fails if any path emits an event before its state mutation lands. (3) No awaits while holding any `StdMutex` (`dispatch`, `sequence`, `assistant_outcomes`); take the tokio locks (`session`, `client_user_message_ids`) first, then the std locks briefly for the clone.

**Verdict:** APPROVE

No blocker is left. Consensus per protocol: all blocking concerns accepted with a stated revision plan, and this response confirms APPROVE. S7 and S8 are non-blocking — record them in the log and proceed to implementation.
