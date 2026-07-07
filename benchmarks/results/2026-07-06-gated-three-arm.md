# Does daimonos cut token costs? An honest three-arm measurement

*2026-07-06. Claude Code headless (CLI pinned 2.1.143, see below), model `opus`,
n=4 per (task, arm), correctness-gated, contamination-canaried. ~$4.20 of API spend
plus a ~$1.45 follow-up validation run.*

This is the first run of the rebuilt benchmark harness (vikunja project #178) and
it retires our old headline. The methodology is adapted from ponytail's corrected
agentic benchmark (DietrichGebert/ponytail, `benchmarks/results/2026-06-18-agentic.md`),
whose central lesson — *control for the prompt before crediting the mechanism* —
turned out to apply to us directly.

## The claim being tested

Earlier runs (April 2026) reported daimonos saving ~27% of tokens and cost versus a
baseline agent. Those runs had two flaws the new harness removes:

1. **Prompt asymmetry.** The daimonos arm carried an appended system prompt with
   terse-output directives; the baseline had none. Any savings from the *prompt*
   were credited to the *tools*.
2. **No isolation guarantee.** The baseline ran without `--strict-mcp-config`, so a
   user-scope daimonos registration could have silently loaded into it, and nothing
   verified task outputs were actually correct.

## Setup

- **Engine:** headless `claude -p --output-format stream-json`, real API `usage`
  metrics (tokens, cache reads/writes, cost) — not character estimates.
- **Arms:**
  - `baseline` — Claude Code built-in tools, no daimonos, no extra prompt.
  - `baseline-terse` — built-in tools plus the *same* terse-style directive the
    daimonos arm carries (minus the tool-routing sentences). This is the control
    that isolates the prompt effect.
  - `daimonos` — daimonos MCP tools plus its standard deployment prompt.
- **Isolation:** every arm runs `--strict-mcp-config`; baseline arms pass no
  `--mcp-config`, guaranteeing zero MCP servers. A canary marks any non-daimonos
  run that makes an `mcp__daimonos__` call as contaminated and invalid. Zero
  contaminated runs occurred.
- **Correctness gate:** each task carries machine-checkable criteria (response
  regexes and workspace shell checks, e.g. `cargo test` after a rename task). Runs
  that fail checks are excluded from metric stats. One daimonos run was excluded —
  an API stream timeout, not a wrong answer.
- **Repetition:** n=4 per (task, arm); the analyzer reports means with min–max
  spread so deltas read against run-to-run noise.
- **Tasks:** the 11 tool-op tasks in `benchmarks/tasks/` (read, search, rename,
  explore, tests, git, snapshot [daimonos-only], and four exec-routing tasks)
  against the small Rust fixture app in `benchmarks/workspace/`.
- **CLI pin:** claude CLI ≥2.1.162 has a regression where `-p` mode never binds
  stdio MCP tools (filed upstream: anthropics/claude-code#74926). All arms ran on
  2.1.143, where MCP binds correctly. Same CLI across arms keeps comparisons
  internally valid.

## Results

Aggregate over the 10 tasks shared by all arms (mean per suite-run, correctness-gated):

| Metric | baseline | baseline-terse | daimonos |
|---|--:|--:|--:|
| Output tokens | 7,257 | 4,773 | 5,186 |
| Cache-read tokens | 72,354 | 57,934 | 158,245 |
| Tool calls | 20 | 18 | 21 |
| API turns (4 suites) | 122 | 113 | 144 |
| Cost (USD) | 0.3866 | 0.2856 | 0.3447 |
| Wall (ms) | 137,250 | 105,500 | 126,833 |

The two numbers that matter:

- **daimonos vs baseline (deployed effect): −28.5% output tokens, −10.8% cost,
  −7.6% wall.** Daimonos as shipped beats a vanilla agent.
- **daimonos vs baseline-terse (tools-only effect): +8.7% output tokens, +20.7%
  cost, +20.2% wall.** Once the prompt is controlled for, the tool machinery
  *costs* money on this suite.

**The old ~27% claim is retired.** Most of it was the terse prompt. The honest
statement is the two-delta form above.

### Where the tools win and lose (per-task, means over n=4)

| Task | baseline | baseline-terse | daimonos |
|---|--:|--:|--:|
| Read & understand (out tok) | 384 | 311 | 352 |
| **Search usages (out tok)** | **1,051** | **440** | **228** |
| Search usages (cost) | $0.049 | $0.030 | **$0.015** |
| Rename across file (cost) | $0.074 | $0.034 | $0.045 |
| Explore architecture (out tok) | 2,254 | 1,520 | 1,708 |
| Run tests (out tok) | 547 | 408 | 380 |
| Git status (out tok) | 217 | 168 | 296 |
| Exec cargo test (out tok) | 521 | 324 | 334 |
| Exec git log (out tok) | 440 | 300 | 393 |
| Build + lint (out tok) | 524 | 293 | 341 |
| Multi-command (out tok) | 623 | 460 | 487 |

Search is the standout: daimonos halves cost *even against the terse control* —
the trigram index is a structural advantage no prompt can imitate. Test/exec
tasks are washes. Explore and git-status lose slightly (extra tool-activation
round-trips). Savings are non-uniform, exactly as ponytail found: report per-task,
not just aggregate.

## Why the tools-only delta is negative: the prefix tax

The cache-read row explains the cost gap. Per API call (cache reads ÷ turns):

| arm | cache-read tok/call |
|---|--:|
| baseline | 2,372 |
| baseline-terse | 2,051 |
| daimonos | **5,124** |

Daimonos adds **~3,073 tokens to every API call's prefix**. A static capture of
its `initialize` + `tools/list` accounts for ~2,164 tokens of content —
instructions ~595, tool names+descriptions ~872, inputSchemas ~697 (the terse
schema tier already strips 15/24 schemas) — with the remainder being MCP plumbing
(`mcp__daimonos__` name prefixes, tool-block wrapping) and estimate slack.

Two compounding effects: the bigger prefix (~87% of the extra reads) and **+27%
more API turns** (144 vs 113 — tool activation/discovery round-trips, each
re-reading the whole prefix).

The cost math closes: extra cache reads ≈ 126.5k/suite × $0.50/M ≈ **$0.063/suite
predicted; $0.059/suite measured gap**. The entire tools-only cost regression is
prefix overhead — the output compaction itself was fine, just outweighed on a
suite of small tasks with little verbose output to compact.

### First fix, validated same day

Context-gating the two heaviest tools (`kgl_query`/`kgl_assert`, 1.6k chars,
niche) on `.kgl` store existence — commit `425fd93` — cut the measured prefix to
**4,818 tok/call (−342)** in an n=4 re-run, closing ~11% of the gap with zero
behavior change (identical turn count, 44/44 tasks correct). Remaining levers are
tracked in vikunja #936 (prefix diet) and project #181 (verbosity dial).

## Limitations — read before quoting

- **This suite structurally favors the prompt-only arm.** Small tasks, tiny
  fixture workspace, short sessions: little verbose output for daimonos to
  compact, while the prefix tax rides every call. Real long sessions shift the
  math — verbose tool results accumulate in context and get re-read every call,
  so compaction compounds exactly like the prefix does. A feature-task tier
  (vikunja #927) is the planned test of that regime.
- n=4, single model (`opus`), one fixture repo.
- CLI pinned to 2.1.143 due to the upstream `-p` MCP regression; all arms
  identical, so internal validity holds, but absolute numbers may shift on
  fixed CLIs.
- Correctness checks are pattern/filesystem-based; they were calibrated against
  real responses and caught two of their own bugs (stale fixture ground truth;
  a table-format false negative that biased against the terse arm) — treat the
  gate as good, not perfect.

## Reproduce

```bash
# full three-arm gated run (~$4–6, ~1.5h; smoke-gates the MCP wiring first)
CLAUDE_BIN=~/.local/share/claude/versions/2.1.143 \
  ./benchmarks/run-all-arms.sh gated

# analyze any tag
python3 benchmarks/analyze-results.py benchmarks/results/ gated
```

Raw data for this writeup: `benchmarks/results/*-gated-r*` and `*-gated2-r*`
(local, gitignored). Harness provenance: vikunja project #178, tasks #925–#930;
commits `f02284d`, `2117c09`, `c6a208b`, `e510ab9`, `66999c5`, `425fd93`.
