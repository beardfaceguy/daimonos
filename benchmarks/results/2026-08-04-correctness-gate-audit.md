# Benchmark correctness-gate audit (2026-08-04)

Triggered by discovering that the `07-snapshot-rollback` gate was **vacuous** — it
passes on a pristine tree, so it cannot distinguish a real run from one that did
nothing. Two runs also showed 0 snapshot/edit ops in `~/.daimonos/analytics.db`,
which first looked like silent failures; that reading was later **withdrawn** (see
the re-grade section below — agent-mode `analytics.db` under-records direct tool
calls, Vikunja #136).

## Method
Reset `benchmarks/workspace` to pristine, then execute every task's `workspace` checks
against it. **A check that passes on a pristine tree cannot distinguish success from
inaction.** Response checks were classified by whether they demand a fact obtainable
only by doing the work.

## Results

| task | workspace checks | verdict |
|---|---|---|
| 01-read-understand | 0 | response-only, but demands real field/method names -> discriminating |
| 02-search-usages | 0 | response-only; demands filenames + line numbers -> discriminating |
| 03-edit-rename | 3 | 2 discriminating (grep for the rename); `cargo test -q` vacuous but harmless |
| 04-explore-architecture | 0 | response-only; filenames are guessable for a Rust project -> **weak** |
| 05-execute-tests | 0 | response-only; demands the number `15` -> discriminating |
| 06-git-status | 0 | response-only; demands `7 commits` + commit subject -> discriminating |
| **07-snapshot-rollback** | 1 | **VACUOUS** (fixed here) |
| 08-exec-cargo-test | 0 | response-only; demands `15` -> discriminating |
| 09-exec-git-log | 0 | response-only; demands `7 commits` -> discriminating |
| **10-exec-build-check** | 0 | response-only, **all generic words** (`clippy`, `compil`, `clean`) -> **assertable** |
| **11-exec-multi-command** | 0 | response-only, **all generic words** -> **assertable** |

**8 of 11 tasks have no workspace check at all.** Most survive because they demand a
specific fact (`15` tests, `7 commits`) that cannot be guessed. Tasks **10** and **11**
do not: every keyword is a generic word an agent would use when merely *claiming*
success. They remain unfixed and should be tightened (e.g. demand the clippy warning
count, or the test count).

## Two bugs fixed here

### 1. `07-snapshot-rollback` had a vacuous gate
`! grep -qi toys src/config.rs` asserts the file does **not** contain `toys` — but the
pristine file doesn't either. Success is byte-identical to never-started, so the check
would pass even for a run that did nothing. The companion response check (`snapshot`, `restor`)
is keyword-only and satisfied by merely saying the words.

**Fix:** added `test -n "$(ls -A .daimonos/snapshots 2>/dev/null)"`. This works because
`restore` does **not** delete the snapshot (`remove_dir_all` lives in `delete_impl`, not
`restore_impl`), so a real run leaves the snapshot on disk. Verified:
- fails on a pristine tree (discriminating);
- passes after a snapshot is created (positive control).

**The prompt is deliberately unchanged**, so token/behaviour numbers stay comparable
with lineage F0-F4; only grading tightens.

*Residual gap (documented, not fixed):* a run that snapshots but skips the edit still
passes. Closing that needs a prompt change (leaving a distinct final edit in place),
which would bump the task-set fingerprint.

### 2. `reset_workspace` leaked snapshots across runs
`git clean -fd` cannot remove `.daimonos/snapshots/*`: those dirs contain gitignored
paths (`.cursor/`), so git leaves them, and `-x` would also wipe `target/` and force a
full rebuild every task. Snapshots therefore accumulated across tasks and runs, which:
- polluted workspace-wide searches (snapshot copies of `src/` are extra matches — a
  plausible contributor to `02-search-usages` variance), and
- made "a snapshot exists" unusable as a signal.

**Fix:** explicit `rm -rf .daimonos` in `reset_workspace`.

## Retroactive re-grade of task 07 — WITHDRAWN

An earlier version of this section inferred a retroactive grade from
`~/.daimonos/analytics.db` op counts:

| verdict inferred from analytics op counts | runs |
|---|---|
| would PASS (real snapshot created) | 8 |
| would FAIL (0 ops recorded) | 2 |

and concluded "true correctness on task 07 was 80%, not 100%".

**That conclusion is withdrawn.** In agent mode `~/.daimonos/analytics.db` does not
record *direct* (non-scripted) tool calls — only microcompaction bookkeeping and
`script:<name>` / MCP-bridge calls are logged (Vikunja #136). A run that created a
snapshot and edited through direct tool calls would show **0 ops** here, so the two
"0 op" runs do **not** demonstrate that no work was done.

What is established is narrower: the old gate was **vacuous** and could not verify the
work. The historical pass/fail rate for task 07 under that gate is therefore
**unknown**. Lineage stages F0-F4 report "33/33 correct"; for task 07 that figure is
**unverified** (not necessarily wrong). Token and cost numbers are unaffected.

---

## Follow-up (same day): tasks 10/11 tightened, and a correction

### Correction to this document's earlier framing
I initially wrote that task 10's gate "rewards the wrong answer" because the project
emits 2 warnings while the check accepted `clean`/`ok`/`no error`. **That was too
harsh** — "compiles cleanly, no errors" is defensible: warnings are not errors and
compilation does succeed. The real flaw was **assertability** (every keyword is a word
an agent would use when merely claiming success), not incorrectness.

### Task 10 — calibration matters
First attempt demanded the warning count (`2 warnings`). It failed **0 of 6** historical
runs. Investigation: cargo reports *2 warnings* which between them name *3 methods*, and
the agent said "4 total" in one line and "3 unused methods" in another. **Any specific
count is defensibly ambiguous, so it is a bad discriminator.** Replaced with the
method names, which cannot be produced without running the command:

```json
[{"all":["clippy"]},
 {"any":["apply_discount","find_by_sku","find_by_category"]},
 {"any":["never used","dead[ _]code","unused"]}]
```

### Task 11 — demand real facts
Test count (`15`), a real clippy fact, a real commit subject from `git log --oneline -5`,
and the true clean status.

### Validation of all three (07 / 10 / 11)
| task | real runs passing | fabricated answer passes |
|---|---|---|
| 07-snapshot-rollback | 6/6 | **no** |
| 10-exec-build-check | 6/6 | **no** |
| 11-exec-multi-command | 6/6 | **no** |

Both directions checked: the gates admit genuine work and reject a plausible fabrication.

## Fixture bug, corrected diagnosis

My first explanation for the snapshot leak — "snapshot dirs contain gitignored
`.cursor/` paths so `git clean -fd` skips them" — was **wrong**. The real cause: the two
leaked snapshots were **tracked in the fixture's HEAD**. `git clean` only removes
*untracked* files, and `git checkout -- .` actively **restored** them on every reset.

Consequences that a naive fix would have caused:
- `rm -rf .daimonos` alone leaves the fixture repo **dirty (20 deletions)**, which breaks
  tasks `06`/`09`/`11` — they assert a clean `git status`.

Correct fix applied to the fixture (local, see below):
1. `git rm -r --cached .daimonos`, add `.daimonos/` to the fixture `.gitignore`;
2. **`git commit --amend`** rather than a new commit — tasks `06`/`09` assert exactly
   **7 commits**, and adding one would have broken them.

Verified after the fix: 7 commits, HEAD subject preserved, clean status, and task 07's
gate discriminating after a real `reset_workspace`.

## Reproducibility gap (not fixed)
`benchmarks/workspace/` is **gitignored by the outer repo** (0 tracked files). The
fixture is therefore **local-only and not distributed**, so the lineage's "fixture
commit" comparability criterion cannot be verified across machines. The fixture repair
above is a local change and is deliberately **not** part of this commit.
