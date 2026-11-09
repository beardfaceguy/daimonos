# ADR-033: A live session has one owning process

- **Status:** Accepted
- **Date:** 2026-11-08
- **Deciders:** Repository maintainer (chose this option on 2026-11-08).
- **Consulted:** None recorded.
- **Informed:** None recorded.
- **Spec:** Vikunja #1607 (history-loss investigation; root cause in comment 1414).
- **Supersedes:** None. Amends the resume rule of ADR-014.
- **Superseded by:** None

## Context

ADR-014 rotates the writer epoch when a runtime resumes a session at the current
generation. That makes a delayed save from an exited runtime terminally
superseded. It assumes the previous runtime is gone.

Zed can start a new `daimonos acp` process and send `session/load` for a thread
that another live process still serves. The new process claims the epoch at the
current generation, so the claim succeeds. The live process keeps serving the
user, but every later save is `Superseded`. The superseded state is sticky, the
WARN carried no session id, and ACP showed nothing. Hours of work were lost in
four observed threads (Vikunja #1607). The claiming process often exits within
minutes, but its epoch row remains, so the live process never recovers.

Which process may write a session that two live processes hold?

## Decision drivers

- The process the user is typing into must keep saving.
- A lost save must never be silent.
- Ownership must end automatically when a process exits or crashes, without a
  lease timer or a cleanup job.
- The existing epoch fence must keep rejecting delayed writes from exited
  runtimes.
- The refusing side must say which process owns the session (maintainer
  requirement: report the owner's PID).

## Decision

We will make the first live owner keep the session.

1. `SessionStore::claim_writer` takes an exclusive, non-blocking `flock` on
   `<store>/locks/<session-id>.lock` before it rotates the epoch. The lock
   directory is 0700, the file is 0600, opened with `O_NOFOLLOW` and
   `O_CLOEXEC`, so tool child processes do not keep it after `exec`.
2. The owner writes its PID into the lock file. The `SessionWriter` holds the
   lock for its lifetime. The OS releases it when the last clone drops or the
   process exits, including on a crash.
3. If another process holds the lock, the claim fails with
   `SessionStoreError::OwnedElsewhere { pid }`. ACP `session/load` returns that
   error to the client, with the owner's PID. The refused process does not
   touch the epoch, so the owner keeps saving.
4. The epoch fence from ADR-014 stays. It still rejects a delayed write from a
   runtime that exited after a new owner claimed the session.
5. A superseded or degraded save is visible. ACP sends an agent-message notice
   when durability becomes `Superseded` or `Degraded`. The WARNs
   `session_writer_superseded` and `session_payload_save_exhausted` carry the
   session id.

The lock applies to every frontend that claims a writer: ACP and the session
daemon. The compatibility chat store (`SessionStore::save`) takes no writer and
is unchanged (ADR-014). A load of a session that is live in the same process
still uses the live handle and takes no lock.

## Alternatives

- **Claim on the first prompt, not on load:** a replay-only load no longer
  steals ownership, but alternating prompts in two windows still split the
  history.
- **Keep last-loader-wins and only warn:** reports the loss but does not
  prevent it.
- **Lease row with heartbeat in SQLite:** needs a timer, clock tolerance, and
  stale-lease rules. A crashed owner blocks the session until the lease expires.
- **POSIX `fcntl` record locks:** not inherited across `fork`, but released when
  any descriptor for the file closes in the process, for example when reading
  the owner PID.
- **Merge histories from both processes:** no safe merge rule exists for
  diverged agent histories.

## Consequences

- A second process cannot take over a live session. Zed shows the load error
  in the window that tried to open the thread, with the owner's PID.
- A user who wants to continue in the second window must close the thread in
  the owning window or stop that process first.
- After an owner exits or crashes, the next load succeeds at once. A stale
  epoch row can no longer block a session.
- A child forked during the owner's release holds a copy of the lock until its
  `exec`. A reclaim in that window is briefly refused.
- `flock` is reliable on local filesystems only. Stores on network filesystems
  are not supported for live ownership.
- Protection starts only when every process runs a build with this change. An
  older binary takes no lock and can still claim a session.
- The v3 daemon attach protocol has no owner field. Daemon clients get
  `SessionOpenFailed`, and the claim WARN names the owner PID.

## Confirmation

- `session_store::tests::live_writer_refuses_second_claim_and_reports_owner_pid`:
  a second claim is refused with this process's PID, and a claim succeeds after
  the owner drops.
- `tests/test_acp_session_ownership.py::test_second_process_load_is_refused_with_owner_pid`:
  a second `daimonos acp` process gets a `session/load` error with the owner's
  PID, the owner keeps saving, and the load succeeds after the owner exits. The
  installed pre-fix binary fails this test (the load is not refused).
- `acp_cmd::tests::durability_notice_warns_only_when_saving_has_stopped_or_failed`.
- The ADR-014 epoch tests still pass after an explicit lock release:
  `reopened_writer_epoch_rejects_old_save_and_delete`,
  `writer_only_reservation_can_be_reclaimed_after_failed_creation`, and
  `session_core::tests::reopened_writer_epoch_rejects_delayed_old_core_save`.
- `tests/test_acp_crash_recovery.py` restarts load the session after SIGTERM,
  EOF, cancel, and SIGKILL, so the OS releases the lock on exit.

## Anchors

- `src/session_store.rs::SessionStore::claim_writer`
- `src/session_store.rs::SessionStore::acquire_owner_lock`
- `src/session_store.rs::SessionWriter`
- `src/acp_cmd.rs::durability_notice`

## Revisit when

- Zed can tell an agent server that a thread moved to another process, so
  ownership could transfer instead of being refused.
- Session stores must live on a network filesystem.
- The daemon attach protocol gains an owner or diagnostic field.

## Open questions

- Why Zed starts new `daimonos acp` process pairs and reloads threads that are
  live elsewhere. This needs a Zed-side investigation (Vikunja #1607).
