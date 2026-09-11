# Daimonos optimization benchmark lineage

This is the durable index for optimization benchmarks. Append a stage for
every optimization implementation; never replace an older row. Raw run
directories remain local, while each stage links to a committed report.

> ## ⚠️ Correctness figures for stages F0–F4 are OVERSTATED
>
> Every F-series stage reports "33/33 correct". That is **wrong** for
> `07-snapshot-rollback`, whose gate was **vacuous** until 2026-08-04: it asserted
> `! grep -qi toys src/config.rs`, but the *pristine* file doesn't contain `toys`
> either — success was byte-identical to never-started. Two of ten runs created **no
> snapshot and made no edit** (0 ops in `~/.daimonos/analytics.db`) yet scored
> correct. Retroactive re-grade: **task 07 was 80% correct, not 100%**, i.e. there
> was an invisible ~20% silent-failure rate.
>
> **Token, cost and call-count figures in F0–F4 are unaffected** — only the
> correctness column is. The −70% caching result (F4 vs F3) and the F3-vs-F0
> deltas still stand.
>
> Also fixed at the same time: leaked snapshots were **tracked in the fixture's
> HEAD**, so `git checkout -- .` restored them on every reset (which is why
> `git clean -fd` never removed them). They polluted workspace-wide searches — a
> plausible contributor to the variance in `02-search-usages`.
>
> Because the gate changed, **correctness is not comparable across 2026-08-04**;
> token/cost remains comparable (the task *prompt* was deliberately left
> unchanged). Tasks `10` and `11` were also tightened, having previously been
> satisfiable by generic claims. Details:
> [`2026-08-04-correctness-gate-audit.md`](2026-08-04-correctness-gate-audit.md).
>
> Caveat on reproducibility: `benchmarks/workspace/` is **gitignored by the outer
> repo** (0 tracked files), so the fixture is local-only and not distributed. The
> "fixture commit" criterion below cannot currently be verified across machines.

The machine-readable source is `benchmarks/optimization-lineage.json`.
`benchmarks/benchmark_db.py` synchronizes it and available raw runs into the
private local `~/.daimonos/benchmarks.db` SQLite database.

## Comparison rules

A delta is valid only when provider, exact model, thinking level, task-set
fingerprint, fixture commit, correctness gate, and aggregation method match.
Targeted experiments are tracked separately from full-suite stages.

Every stage records:

- immutable stage and parent stage;
- feature/config change;
- scope fingerprint and run count;
- binary or commit hash;
- correctness, token, cost, and wall-time metrics;
- immediate delta from its parent;
- cumulative delta from the original comparable baseline;
- per-task regressions, even when aggregate results improve.

## Full 11-task lineage

Scope fingerprint: `agent-full11-v1 / direct Anthropic / claude-opus-4-8 /
thinking=medium / compaction=off / 3 passes`.

| Stage | Change | Mean total tokens | Mean cost | Mean wall | Correct | Immediate delta | Cumulative delta |
|---|---|---:|---:|---:|---:|---|---|
| F0 | Pre-1193/1194 baseline | 416,652 | $2.2555 | 145.7s | 33/33 | baseline | baseline |
| F1 | Universal tool-output bounding + intra-turn microcompaction | 427,628 | $2.3062 | 146.7s | 33/33 | tokens +2.63%; cost +2.25%; wall +0.69% | same |
| F2 | Context diagnostics + opt-in tool-prefix caching | 402,699 | $0.6856 | 156.0s | 33/33 | tokens -5.83%; cost -70.27%; wall +6.36% | tokens -3.35%; cost -69.60%; wall +7.09% |
| F3 | Post-round #126 (`a31e470`), cache OFF — parent F0 | 414,881 | $2.2486 | 145.0s | 33/33 | tokens -0.43%; cost -0.30%; wall -0.46% | tokens -0.43%; cost -0.30%; wall -0.46% |
| F4 | #126 with tool-prefix caching ON — parent F3 | 409,465 | $0.6770 | 146.0s | 33/33 | tokens -1.31%; cost **-69.89%**; wall +0.69% | tokens -1.73%; cost **-69.98%**; wall +0.23% |

F0/F1 report:
[`1193-1194-anthropic-medium-comparison.md`](1193-1194-anthropic-medium-comparison.md).

Important F1 per-task upward blips retained in that report:

- Task 04 explore architecture: **+32.89%** tokens
- Task 09 exec git log: **+50.12%** tokens

These remain visible even though other tasks improved.

### F3 / F4 — end-of-round (#126) and the cache toggle

F3 and F4 branch off **F0** (not the F1/F2 intermediate chain) to measure the
*cumulative* #122–#126 round on binary `a31e470` (#126), and to isolate the
Anthropic tool-prefix cache on that single binary:

- **F3 (cache OFF) vs F0:** token/cost-neutral — the −0.4% token / −0.3% cost
  delta is smaller than F3's own ~13% run-to-run total-token spread. Cache-off,
  the round's value is robustness/correctness (33/33 held), not raw suite tokens
  — consistent with F1 measuring *higher* full-suite tokens than F0.
- **F4 (cache ON) vs F3 (same binary):** mean cost **$0.6770 vs $2.2486 =
  −69.9%** at 33/33 correct, converting ~406k fresh input tokens/run into ~342k
  cheap cache reads (fresh input −85.6%). This reproduces F2's cache win
  ($0.6856) on #126. Enable with `DAIMONOS_AGENT_PROMPT_CACHE=true`.

Reports:
[`2026-08-03-post-126-vs-f0.md`](2026-08-03-post-126-vs-f0.md) (F3),
[`2026-08-03-cache-on-126-vs-f3.md`](2026-08-03-cache-on-126-vs-f3.md) (F4).

Baseline caveat: `c3d103a` (F0's commit) can no longer run live against
Anthropic — `thinking.signature` enforcement landed *inside* the round — so
F0's recorded numbers are the pre-round reference; a fresh live baseline on this
scope is not reproducible today. Recorded on branch
`bench/opt-lineage-f3-f4-cache` (commit `c5e49aa`).

## Task 04 targeted lineage

Scope fingerprint: `task04-explore-architecture-v1 / direct Anthropic /
claude-opus-4-8 / thinking=medium / compaction=off`.

| Stage | Parent | Change | Runs | Mean total tokens | Mean cost | Mean wall | Correct | Immediate delta | Cumulative vs T0 |
|---|---|---|---:|---:|---:|---:|---:|---|---|
| T0 | — | Original pre-1193/1194 full-suite baseline, Task 04 slice | 3 | 42,569 | $0.2627 | 37.3s | 3/3 | baseline | baseline |
| T1 | T0 | 1193/1194 implementation, Task 04 slice | 3 | 56,569 | $0.3328 | 38.7s | 3/3 | tokens +32.89%; cost +26.67%; wall +3.57% | same |
| T2 | T1 | Metadata-only context diagnostics, cache disabled | 4 | 42,459 | $0.2600 | 38.5s | 4/4 | tokens -24.94%; cost -21.87%; wall -0.43% | tokens -0.26%; cost -1.02%; wall +3.12% |
| T3 | T2 | Anthropic tool-prefix cache enabled | 4 | 51,995 | $0.1480 | 38.0s | 4/4 | tokens **+22.46%**; cost **-43.09%**; wall -1.30% | tokens **+22.15%**; cost **-43.67%**; wall +1.79% |

T2/T3 report:
[`2026-08-01-native-context-cache.md`](2026-08-01-native-context-cache.md).

T3's token increase is retained explicitly: one candidate run took six model
calls instead of three. Symmetric decomposition attributes +39,817.9 prompt
tokens to call count and -2,306.9 to mean context size. The three matched
three-call warm-cache runs reduced cost approximately 54.7%.

F2 report:
[`2026-08-03-full-suite-prompt-cache.md`](2026-08-03-full-suite-prompt-cache.md).

F2 reduced mean total tokens and cost versus both F1 and F0, but increased
mean wall time by 6–7%. The per-task table in the report remains the source of
truth for local upward blips.

## Index lifecycle lineage

Scope fingerprint: `index-lifecycle-v1 / small=200 / large=3000 /
max_files=500 / max_walk_entries=1000 / 30 interleaved replicates / 20 warm
calls`.

| Stage | Parent | Mode | Correct profile-runs | Small startup | Large unmarked ready | Large marked ready | Max inotify |
|---|---|---|---:|---:|---:|---:|---:|
| I0 | — | legacy | 30/90 | 228.40 ms | no correct result | no correct result | 21 |
| I1 | I0 | eager | 90/90 | 226.49 ms | 0.59 ms | 0.68 ms | 42 |
| I2 | I0 | lazy | 90/90 | 226.07 ms | 8.13 ms | 8.62 ms | 21 |
| I3 | I0 | hybrid (default) | 90/90 | 229.41 ms | 7.78 ms | 0.65 ms | 42 |

Report:
[`2026-08-03-index-lifecycle.md`](2026-08-03-index-lifecycle.md).

I3 keeps startup within ±0.5% of I0 on all fixtures and restores deterministic
filename correctness under partial coverage. The retained upward blip is a
second recursive watcher set on warm projects (21 → 42 watches in this
fixture); follow-up Vikunja task 1210 tracks watcher sharing.

## Read-transform-write targeted lineage

Scope fingerprint: `read-transform-write-03-07-v1 / direct Anthropic /
claude-opus-4-8 / thinking=medium / compaction=off / prompt-cache=off /
fixture=bda5798`.

This is a separate lineage from B0/B1 because task 02 was removed after it was
shown to be at the two-call floor. Comparing R0/R1 to B0/B1 as one aggregate
would mix task scopes.

| Stage | Parent | Change | Runs | Batch adoption | Mean calls | Mean tokens | Mean cost | Mean wall | Correct | Immediate delta | Cumulative vs R0 |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---|---|
| R0 | — | Same-commit base prompt | 8 | 12.5% | 5.625 | 76,282.9 | $0.3976 | 14.875s | 8/8 | baseline | baseline |
| R1 | R0 | Teach read → transform → write inside `execute_script` | 8 | 100% | 3.375 | 46,241.8 | $0.2501 | 14.500s | 8/8 | calls -40.0%; tokens -39.4%; cost -37.1%; wall -2.5%; adoption +87.5pp | same |

Repetitions are task 03 n=3 and task 07 n=5 in each arm. Both binaries were
built from `50b0c86425c6f3f51f3aef54a34866d0f5a5b14c` and differ only in
`prompts/agent_system.md`. Binary SHA-256:

- R0: `59d329f188070536756a75580f377df51592cda5042d8acfe196966ce5446871`
- R1: `2057f84b8507c7504923305b08e1dbcef4f755f64631f8c46ae8d2282551f039`

Retained regressions: task 07 mean output increased **47.9%** (893.0 →
1,320.4 tokens) and mean wall time increased **17.3%** (16.2s → 19.0s).
Task 03 had no upward metric movement. The machine-readable R0/R1 stages record
all 16 raw run directories, the task-set fingerprint, fixture commit, exact
metrics, and deltas.

Report:
[`2026-08-14-1230-phase2-read-transform-write.md`](2026-08-14-1230-phase2-read-transform-write.md).

## SWE-bench OpenRouter cache targeted lineage

Scope fingerprint: `swebench-five-v1 / OpenRouter /
anthropic/claude-opus-4.8 / thinking=provider-default / compaction=off /
official Docker / cold cache / three repetitions`.

Task-set fingerprint:
`3cc9e346383b65ff688e1e7987143acde96ea1fc1097ae59e3e2384a78a83fd5`.
This lineage is separate from the in-house agent suites above. Metrics use the
14 paired instance-repetitions where both cache modes resolved; the candidate's
unresolved Sphinx sample is excluded from both arms.

| Stage | Parent | Change | Paired runs | Tokens | Calls | Cost | Wall | Correct | Immediate delta | Cumulative vs SW0 |
|---|---|---|---:|---:|---:|---:|---:|---:|---|---|
| SW0 | — | OpenRouter prompt cache off | 14 | 1,577,361 | 94 | $8.174165 | 262.242s | 14/14 | baseline | baseline |
| SW1 | SW0 | Explicit latest-message cache breakpoint | 14 | 1,747,557 | 103 | $2.698824 | 322.296s | 14/14 | tokens +10.79%; calls +9.57%; cost **-66.98%**; wall +22.90% | same |

Across all samples, SW0 resolved 15/15 and SW1 resolved 14/15. Retained
candidate regressions:

- `django__django-11815`: mean tokens +8.18%, calls +8.33%, output +6.22%,
  wall +8.56%;
- `django__django-12155`: mean wall +2.19%;
- `django__django-12708`: mean tokens +50.56%, calls +38.89%, output +141.94%,
  wall +104.02%;
- `sphinx-doc__sphinx-8035`: mean tokens +1.06%, calls +5.17%, output +6.53%,
  wall +9.83%, plus one correctness failure.

The separately recorded within-TTL warm run is not part of SW1. Report:
[`2026-09-10-swebench-openrouter-cache-parity.md`](2026-09-10-swebench-openrouter-cache-parity.md).

## SWE-bench full-50 harness lineage

Scope fingerprint: `swebench-mini50-v1 / OpenRouter /
anthropic/claude-opus-4.8 / explicit prompt cache / official Docker /
one repetition`.

Task-set fingerprint:
`6ce05e6b926c91faecdbb4243014ac85dca471dfad30ccdb69453343b75267c3`.
This is separate from SW0/SW1 because the task scope changed from five to all
50 instances. Metrics use the 37 tasks where both harnesses resolved.

| Stage | Parent | Harness | Paired tasks | Tokens | Calls | Cost | Wall | Paired correct | Immediate delta | Cumulative vs SWF0 |
|---|---|---|---:|---:|---:|---:|---:|---:|---|---|
| SWF0 | — | mini-swe-agent 2.4.6 | 37 | 6,016,050 | 514 | $7.858095 | 1,766.831s | 37/37 | baseline | baseline |
| SWF1 | SWF0 | cached Daimonos | 37 | 7,169,536 | 335 | $9.886105 | 1,283.254s | 37/37 | tokens +19.17%; calls -34.82%; cost +25.81%; wall -27.37% | same |

As-run correctness was 45/50 for SWF0 and 41/50 for SWF1. Eight tasks were
Daimonos-only failures, four were mini-only failures, and both failed
`sphinx-doc__sphinx-7748`. The report's complete 50-row table preserves every
per-task cost, token, call, wall-time, and correctness regression.

Report:
[`2026-09-11-swebench-openrouter-full50-r1.md`](2026-09-11-swebench-openrouter-full50-r1.md).

## SWE-bench outlier diagnostic lineage

Scope fingerprint: `swebench-outliers-8638-9229-v1 / OpenRouter /
anthropic/claude-opus-4.8 / explicit prompt cache / official Docker`.

| Stage | Parent | Run | Tokens | Calls | Cost | Wall | Correct | Delta |
|---|---|---|---:|---:|---:|---:|---:|---|
| SWO0 | — | Full-50 source attempts | 10,091,478 | 155 | $9.368654 | 1,022.266s | 2/2 | baseline |
| SWO1 | SWO0 | Traced reruns | 3,821,051 | 77 | $3.549320 | 579.949s | 1/2 | not comparable: correctness regressed |

No aggregate savings are claimed from SWO1. `sphinx-8638` preserved
correctness while cost fell 79.57%; `sphinx-9229` cost fell 51.21% but failed.
The traces show stochastic trajectory length and iterative debugging rather
than a deterministic identical-tool loop.

Report:
[`2026-09-11-swebench-outlier-tool-traces.md`](2026-09-11-swebench-outlier-tool-traces.md).
