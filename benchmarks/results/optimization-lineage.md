# Daimonos optimization benchmark lineage

This is the durable index for optimization benchmarks. Append a stage for
every optimization implementation; never replace an older row. Raw run
directories remain local, while each stage links to a committed report.

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

F0/F1 report:
[`1193-1194-anthropic-medium-comparison.md`](1193-1194-anthropic-medium-comparison.md).

Important F1 per-task upward blips retained in that report:

- Task 04 explore architecture: **+32.89%** tokens
- Task 09 exec git log: **+50.12%** tokens

These remain visible even though other tasks improved.

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
