# Three-way SWE-bench comparison — 5 instances, in-container, 2026-09-02

All three harnesses drove the same model (Claude Opus 4.8) inside the official
SWE-bench Docker image for each instance (real test environment, runnable tests).

Runs:
- daimonos: `20260902-154510-swebench-docker-3way` (musl binary, `run_agent.py --docker`)
- mini-swe-agent: `mini-3way` (`mini-extra swebench`, tokens extracted from traj via `extract_mini.py`)
- cursor-agent: `20260902-154642-swebench-cursor-docker-3way` (`run_cursor.py --docker`, auth.json mounted)

## Resolution (official swebench harness, SWE-bench/SWE-bench_Verified)

All three: **5/5 resolved**, including sphinx-doc__sphinx-8035.

## Tokens / calls / wall

| instance | daimonos tok (calls) | mini tok (calls) | cursor tok (calls) |
|---|---:|---:|---:|
| django__django-11815 | 54,755 (4) | 38,305 (9) | 113,242 |
| django__django-12155 | 53,773 (4) | 33,095 (9) | 348,425 |
| django__django-12708 | 88,765 (6) | 196,492 (22) | 451,349 |
| sphinx-doc__sphinx-8035 | 172,729 (10) | 342,108 (30) | 930,495 |
| sphinx-doc__sphinx-9367 | 66,812 (5) | 18,611 (6) | 230,223 |
| **TOTAL tokens** | **436,834** | **628,611** | **2,073,734** |
| **TOTAL wall (s)** | **84** | **247** | **279** |

## Cost (OpenRouter key usage deltas)

- daimonos: $1.53 for the 5-instance batch
- mini-swe-agent: $1.66
- cursor-agent: billed via Cursor backend (not on this key). Note: most of
  cursor's token volume is cache-read (billed at a fraction of input price),
  so its raw token total overstates relative cost.

## Caveats

- n=5; run-to-run variance is real (daimonos used 503k tokens on sphinx-8035
  in an earlier identical-config run vs 173k here).
- daimonos's own `cost_usd` field reports $0.00 on OpenRouter (adapter bug,
  still open) — cost above comes from key usage deltas.
- Prior host-mode results (including the original sphinx-8035 daimonos failure)
  are superseded: that failure was an environment artifact (no runnable tests
  on bare host), not harness-induced model degradation.
