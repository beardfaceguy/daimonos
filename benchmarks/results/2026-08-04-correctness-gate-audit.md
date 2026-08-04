# Benchmark correctness-gate audit (2026-08-04)

Triggered by discovering that two `07-snapshot-rollback` runs were graded **correct**
while doing no work at all (0 snapshot ops, 0 edit ops in `~/.daimonos/analytics.db`).

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
passed for runs that did nothing. The companion response check (`snapshot`, `restor`)
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

## Retroactive re-grade of task 07 (from analytics)
| verdict under fixed gate | runs |
|---|---|
| would PASS (real snapshot created) | 8 |
| **would now FAIL (no snapshot)** | **2** |

**True correctness on task 07 was 80%, not 100%.** Lineage stages F0-F4 all report
"33/33 correct"; that figure is **overstated**. Token and cost numbers are unaffected.
