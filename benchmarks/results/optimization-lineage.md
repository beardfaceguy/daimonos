# Daimonos optimization benchmark lineage

This is the durable index for optimization benchmarks. Append a stage for
every optimization implementation; never replace an older row. Raw run
directories remain local, while each stage links to a committed report.

> ## ⚠️ Correctness figures for stages F0–F4 are UNVERIFIED
>
> Every F-series stage reports "33/33 correct". For `07-snapshot-rollback` that
> figure **cannot be trusted**: the gate was **vacuous** until 2026-08-04. It
> asserted `! grep -qi toys src/config.rs`, but the *pristine* file doesn't contain
> `toys` either — a run that did nothing was byte-identical to one that succeeded, so
> the check **could not distinguish success from inaction**. It simply did not
> verify the work.
>
> An earlier version of this note claimed an "**invisible ~20% silent-failure rate**
> (task 07 was 80% correct)", inferred from two runs showing 0 ops in
> `~/.daimonos/analytics.db`. **That inference is withdrawn.** In agent mode
> `analytics.db` does not record *direct* (non-scripted) tool calls (Vikunja #136),
> so "0 ops" does **not** establish that those runs did no work. The true historical
> pass/fail rate for task 07 under the old gate is therefore **unknown** — not 80%.
>
> **Token, cost and call-count figures in F0–F4 are unaffected** — only the
> correctness column is in question. The −70% caching result (F4 vs F3) and the
> F3-vs-F0 deltas still stand.
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
