# OpenRouter SWE-bench full-50 comparison, repetition 1

## Result

On all 50 mini-suite instances:

- cached Daimonos resolved **41/50 (82%)**, using 23,605,300 tokens, 715 model
  calls, $26.927765, and 3,532.219 seconds of agent wall time;
- mini-swe-agent resolved **45/50 (90%)**, using 17,769,188 tokens, 1,030
  model calls, $20.306602, and 4,418.314 seconds of agent wall time.

Those selected final-attempt totals include failed patches and are descriptive,
not an efficiency or operational-spend claim. On the 50 scoreable final
trajectories, Daimonos cost 32.61% more and used 32.84% more raw tokens, while
making 30.58% fewer calls and taking 20.06% less agent wall time. The killed
mini attempt described below is additional operational cost and is not included
in that percentage.

The primary cost comparison uses the 37 instances where both patches resolved:

- Daimonos: 7,169,536 tokens, 335 calls, **$9.886105**, 1,283.254s;
- mini-swe-agent: 6,016,050 tokens, 514 calls, **$7.858095**, 1,766.831s.

On that correctness-paired scope, Daimonos cost **25.81% more** and used 19.17%
more raw tokens, but made 34.82% fewer calls and was 27.37% faster. No failed
sample contributes to those percentages.

The paired result is not produced by the two largest as-run Daimonos outliers,
because mini failed both of those tasks and they are outside the 37-task set.
Daimonos cost more on 31/37 paired tasks; the median per-task cost ratio was
1.68x (median paired difference $0.057053). These medians are descriptive, not
a substitute for another repetition.

This reverses the five-instance result, where cached Daimonos was within 4.01%
of mini. The five-task slice was not representative of the full-suite cost
distribution.

## Scope and identity

- Dataset: `MariusHobbhahn/swe-bench-verified-mini`, all 50 test instances.
- Enriched task-set SHA-256:
  `6ce05e6b926c91faecdbb4243014ac85dca471dfad30ccdb69453343b75267c3`.
- Evaluator: SWE-bench 5.0.2 official Docker images.
- Provider/model: OpenRouter `anthropic/claude-opus-4.8`.
- Cost source: sum of OpenRouter's per-generation `usage.cost` for both arms.
- Caching: explicit ephemeral latest-message breakpoints for both arms.
- Compaction: off for Daimonos.
- Workers: one agent worker per arm; two evaluator workers.
- Repetitions: one. This is a full-scope baseline, not a stable aggregate.

The arms were resumed in segments after monitored stalls and budget checks.
Segmentation does not duplicate scoreable task metrics: every selected final
trajectory contains exactly one complete agent attempt for that task.

Daimonos:

- source commit: `780b272807b0df77cac796ae803dc677692b3698`;
- musl binary SHA-256:
  `0c8843a01605111777737518e17161e6e65526802ad7a9d017dc37ecf017c316`.

mini-swe-agent:

- version 2.4.6;
- CPython 3.12.14;
- installed distribution `RECORD` SHA-256:
  `a6edef76fde19d093ddefd01f67de68f0f18fe8cd217615b5f7483b375bf12ed`;
- explicit `model.set_cache_control=default_end`;
- `model.cost_tracking=ignore_errors` (ranking still uses OpenRouter
  `usage.cost`, not LiteLLM's estimate).

## Token and cache composition

| Harness | Fresh input | Cache write | Cache read | Output | Total |
|---|---:|---:|---:|---:|---:|
| Daimonos | 1,430 | 1,651,361 | 21,722,984 | 229,525 | 23,605,300 |
| mini-swe-agent | 2,060 | 757,276 | 16,721,754 | 288,098 | 17,769,188 |

Both harnesses achieved high cache-read volume. Daimonos's higher cost follows
its 32.84% larger raw-token total and more than twice mini's cache-write tokens,
not an accounting-source mismatch.

## Correctness differences

Daimonos-only failures (mini passed):

- `django__django-11885`
- `django__django-12193`
- `django__django-12273`
- `sphinx-doc__sphinx-10435`
- `sphinx-doc__sphinx-11510`
- `sphinx-doc__sphinx-8056`
- `sphinx-doc__sphinx-8265`
- `sphinx-doc__sphinx-8548`

mini-only failures (Daimonos passed):

- `django__django-12325`
- `django__django-12406`
- `sphinx-doc__sphinx-8638`
- `sphinx-doc__sphinx-9229`

Both failed `sphinx-doc__sphinx-7748`. The mutually resolved set therefore has
37 instances.

## Cost concentration

Largest scoreable Daimonos costs:

- `sphinx-doc__sphinx-9229`: $5.765896;
- `sphinx-doc__sphinx-8638`: $3.602758;
- `sphinx-doc__sphinx-9461`: $1.819816;
- `django__django-11885`: $1.751531;
- `django__django-12273`: $1.261626.

Largest scoreable mini costs:

- `sphinx-doc__sphinx-8548`: $2.958248;
- `django__django-12273`: $1.978650;
- `sphinx-doc__sphinx-9461`: $1.860135;
- `django__django-11885`: $1.652075;
- `django__django-12325`: $1.308967.

Two Daimonos outliers (`sphinx-9229` and `sphinx-8638`) account for $9.368654,
34.79% of its scoreable full-run cost. This variance is why another full
repetition would be required before making a stable population claim.

## Operational spend and interruptions

Scoreable cost was $47.234367 combined. Two manually interrupted Daimonos
attempts consumed another $4.130504, making known attributable spend
**$51.364871**.

The original mini `sphinx-doc__sphinx-8548` attempt stalled before a trajectory
was written and was terminated. Its cost cannot be recovered from local
artifacts; the replacement cost $2.958248 and resolved. mini checks its
$3-per-task threshold before the next request, so a final response can exceed
that value—it is not a provable hard ceiling. The run was monitored with a $3
reserve (`$47.234367 + $4.130504 + $3.00 = $54.364871`), but exact operational
spend remains greater than $51.364871 and unknown from the retained artifacts.

No OpenRouter settlement retry was needed. Every scoreable patch completed
evaluation without infrastructure, ambiguity, container-leak, or evaluator
errors.

## Per-instance results

| Instance | D | mini | D cost | mini cost | D tokens | mini tokens | D calls | mini calls | D wall | mini wall |
|---|:---:|:---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `django__django-11790` | pass | pass | $0.167368 | $0.105583 | 98,470 | 43,230 | 7 | 10 | 20.488s | 30.059s |
| `django__django-11815` | pass | pass | $0.118994 | $0.058105 | 54,731 | 20,801 | 4 | 6 | 8.602s | 13.289s |
| `django__django-11848` | pass | pass | $0.131669 | $0.076793 | 67,906 | 32,442 | 5 | 9 | 12.091s | 23.871s |
| `django__django-11880` | pass | pass | $0.107205 | $0.037364 | 40,445 | 11,890 | 3 | 4 | 7.519s | 7.543s |
| `django__django-11885` | fail | pass | $1.751531 | $1.652075 | 1,561,189 | 1,781,374 | 45 | 64 | 278.854s | 287.326s |
| `django__django-11951` | pass | pass | $0.121966 | $0.059762 | 55,121 | 25,106 | 4 | 7 | 9.616s | 14.500s |
| `django__django-11964` | pass | pass | $0.134473 | $0.070502 | 59,213 | 26,945 | 4 | 6 | 11.100s | 17.029s |
| `django__django-11999` | pass | pass | $0.119821 | $0.093981 | 53,898 | 32,955 | 4 | 8 | 10.932s | 24.980s |
| `django__django-12039` | pass | pass | $0.135984 | $0.097557 | 57,604 | 34,461 | 4 | 7 | 12.061s | 23.226s |
| `django__django-12050` | pass | pass | $0.114947 | $0.071747 | 53,454 | 22,087 | 4 | 6 | 8.997s | 16.679s |
| `django__django-12143` | pass | pass | $0.106810 | $0.036874 | 40,661 | 11,939 | 3 | 4 | 7.182s | 6.892s |
| `django__django-12155` | pass | pass | $0.115368 | $0.066113 | 53,759 | 28,335 | 4 | 8 | 8.540s | 19.928s |
| `django__django-12193` | fail | pass | $0.146684 | $0.089650 | 63,093 | 32,654 | 4 | 8 | 11.812s | 23.680s |
| `django__django-12209` | pass | pass | $0.475424 | $0.394504 | 364,856 | 268,454 | 17 | 23 | 87.513s | 100.306s |
| `django__django-12262` | pass | pass | $0.128152 | $0.071809 | 55,978 | 27,543 | 4 | 7 | 11.778s | 18.492s |
| `django__django-12273` | fail | pass | $1.261626 | $1.978650 | 994,605 | 1,718,035 | 34 | 64 | 290.515s | 477.074s |
| `django__django-12276` | pass | pass | $0.131940 | $0.078698 | 57,551 | 30,001 | 4 | 7 | 10.858s | 17.492s |
| `django__django-12304` | pass | pass | $0.357395 | $0.072992 | 275,131 | 27,484 | 15 | 7 | 57.048s | 19.374s |
| `django__django-12308` | pass | pass | $0.220836 | $0.141519 | 156,222 | 72,319 | 10 | 13 | 28.734s | 38.051s |
| `django__django-12325` | pass | fail | $0.198339 | $1.308967 | 138,951 | 1,239,901 | 9 | 65 | 22.943s | 351.550s |
| `django__django-12406` | pass | fail | $1.217392 | $0.664950 | 1,278,884 | 490,163 | 39 | 36 | 192.624s | 170.450s |
| `django__django-12708` | pass | pass | $0.158395 | $0.240557 | 89,070 | 146,251 | 6 | 18 | 15.394s | 62.998s |
| `django__django-12713` | pass | pass | $0.139963 | $0.110331 | 56,645 | 37,377 | 4 | 8 | 14.927s | 25.512s |
| `django__django-12774` | pass | pass | $0.130058 | $0.088529 | 57,151 | 42,688 | 4 | 10 | 12.000s | 24.182s |
| `django__django-9296` | pass | pass | $0.117001 | $0.053178 | 53,681 | 19,762 | 4 | 6 | 9.239s | 12.309s |
| `sphinx-doc__sphinx-10323` | pass | pass | $0.196194 | $0.144493 | 103,232 | 79,724 | 6 | 10 | 22.574s | 28.507s |
| `sphinx-doc__sphinx-10435` | fail | pass | $0.185973 | $0.312953 | 100,124 | 195,680 | 6 | 21 | 20.569s | 78.160s |
| `sphinx-doc__sphinx-10466` | pass | pass | $0.128888 | $0.071835 | 45,896 | 26,307 | 3 | 5 | 9.618s | 12.672s |
| `sphinx-doc__sphinx-10673` | pass | pass | $0.571141 | $0.896698 | 448,874 | 924,075 | 21 | 45 | 85.724s | 182.876s |
| `sphinx-doc__sphinx-11510` | fail | pass | $0.568590 | $0.319063 | 424,581 | 209,268 | 18 | 19 | 103.377s | 90.291s |
| `sphinx-doc__sphinx-7590` | pass | pass | $0.637085 | $0.601081 | 566,698 | 454,765 | 26 | 33 | 114.653s | 145.000s |
| `sphinx-doc__sphinx-7748` | fail | fail | $0.934620 | $0.722722 | 586,924 | 627,071 | 21 | 39 | 107.858s | 155.930s |
| `sphinx-doc__sphinx-7757` | pass | pass | $0.281246 | $0.396541 | 191,889 | 191,889 | 11 | 22 | 41.491s | 107.783s |
| `sphinx-doc__sphinx-7985` | pass | pass | $0.284224 | $0.445610 | 204,143 | 364,049 | 10 | 28 | 35.608s | 99.488s |
| `sphinx-doc__sphinx-8035` | pass | pass | $0.681760 | $0.486712 | 637,725 | 420,999 | 28 | 35 | 96.258s | 107.916s |
| `sphinx-doc__sphinx-8056` | fail | pass | $0.304961 | $0.360942 | 204,299 | 225,044 | 11 | 22 | 44.198s | 85.794s |
| `sphinx-doc__sphinx-8265` | fail | pass | $0.192816 | $0.266206 | 115,364 | 178,152 | 7 | 18 | 16.313s | 58.382s |
| `sphinx-doc__sphinx-8269` | pass | pass | $0.289440 | $0.061539 | 92,246 | 23,713 | 4 | 6 | 10.367s | 12.472s |
| `sphinx-doc__sphinx-8475` | pass | pass | $0.125019 | $0.061284 | 55,669 | 25,587 | 4 | 7 | 10.464s | 13.698s |
| `sphinx-doc__sphinx-8548` | fail | pass | $0.910474 | $2.958248 | 876,272 | 3,526,138 | 31 | 88 | 137.636s | 467.853s |
| `sphinx-doc__sphinx-8551` | pass | pass | $0.306599 | $0.354425 | 222,257 | 267,982 | 12 | 26 | 45.785s | 87.814s |
| `sphinx-doc__sphinx-8638` | pass | fail | $3.602758 | $0.725927 | 4,551,795 | 560,904 | 71 | 32 | 452.373s | 167.570s |
| `sphinx-doc__sphinx-8721` | pass | pass | $0.141723 | $0.127928 | 49,138 | 72,635 | 3 | 10 | 9.312s | 25.504s |
| `sphinx-doc__sphinx-9229` | pass | fail | $5.765896 | $1.088153 | 5,539,683 | 968,754 | 84 | 40 | 569.893s | 237.423s |
| `sphinx-doc__sphinx-9230` | pass | pass | $0.548218 | $0.097324 | 449,187 | 51,730 | 18 | 11 | 92.397s | 27.203s |
| `sphinx-doc__sphinx-9281` | pass | pass | $0.150106 | $0.064239 | 86,156 | 26,558 | 6 | 7 | 15.240s | 15.590s |
| `sphinx-doc__sphinx-9320` | pass | pass | $0.231354 | $0.054146 | 132,344 | 17,376 | 6 | 5 | 17.718s | 11.965s |
| `sphinx-doc__sphinx-9367` | pass | pass | $0.126155 | $0.052339 | 56,842 | 18,442 | 4 | 6 | 9.765s | 12.437s |
| `sphinx-doc__sphinx-9461` | pass | pass | $1.819816 | $1.860135 | 1,966,070 | 2,067,108 | 55 | 78 | 292.487s | 345.740s |
| `sphinx-doc__sphinx-9698` | pass | pass | $0.133368 | $0.055265 | 59,623 | 21,041 | 4 | 6 | 9.174s | 13.454s |

The identical 191,889-token totals for `sphinx-doc__sphinx-7757` are a verified
coincidence, not a transcription error. Daimonos reported 22 fresh, 20,790
write, 168,397 read, and 2,680 output tokens; mini reported 44 fresh, 15,472
write, 167,743 read, and 8,630 output tokens.

## Raw artifacts

Daimonos source segments:

- `swebench/results/20260910-175326-swebench-docker-openrouter-cache-full50-r1`
- `swebench/results/20260910-193519-swebench-docker-openrouter-cache-full50-r1-resume30`
- `swebench/results/20260911-090228-swebench-docker-openrouter-cache-full50-r1-final7`
- normalized merge:
  `swebench/results/20260911-swebench-docker-openrouter-cache-full50-r1-merged`

mini source segments:

- `swebench/results/mini-openrouter-full50-r1`
- `swebench/results/mini-openrouter-full50-r1-resume-8548`
- `swebench/results/mini-openrouter-full50-r1-resume10`
- normalized merge: `swebench/results/mini-openrouter-full50-r1-merged`

Evaluator reports:

- `daimonos-anthropic__claude-opus-4.8.20260911-swebench-docker-openrouter-cache-full50-r1-merged.json`
- `openrouter__anthropic__claude-opus-4.8.mini-openrouter-full50-r1-merged.json`

## Paired-aggregate reproduction

Run from `benchmarks/swebench/`; this is the exact aggregation used for the
37-task figures:

```python
import json
from pathlib import Path

def summaries(path):
    rows = {}
    for file in Path(path).glob("*.json"):
        try:
            row = json.loads(file.read_text())
        except (OSError, ValueError):
            continue
        if row.get("task_id"):
            rows[row["task_id"]] = row
    return rows

d = summaries("results/20260911-swebench-docker-openrouter-cache-full50-r1-merged")
m = summaries("results/mini-openrouter-full50-r1-merged")
d_report = json.load(open(
    "daimonos-anthropic__claude-opus-4.8."
    "20260911-swebench-docker-openrouter-cache-full50-r1-merged.json"
))
m_report = json.load(open(
    "openrouter__anthropic__claude-opus-4.8."
    "mini-openrouter-full50-r1-merged.json"
))
paired = set(d_report["resolved_ids"]) & set(m_report["resolved_ids"])
fields = ("total_tokens", "llm_calls", "cost_usd", "wall_ms")
for name, rows in (("Daimonos", d), ("mini", m)):
    print(name, len(paired), {
        field: sum(rows[task][field] for task in paired)
        for field in fields
    })
```

Expected totals:

```text
Daimonos 37 {'total_tokens': 7169536, 'llm_calls': 335, 'cost_usd': 9.886105, 'wall_ms': 1283254}
mini 37 {'total_tokens': 6016050, 'llm_calls': 514, 'cost_usd': 7.85809475, 'wall_ms': 1766831}
```

## Next decision

Do not extrapolate the five-instance parity result. The full scope shows mini
has both lower cost and higher resolve rate in this repetition, while Daimonos
retains call-count and latency advantages.

Before paying for another full repetition, inspect the two dominant Daimonos
loops and retain structured tool traces. That work is already tracked by
Vikunja #1464. A second full repetition should follow only after those
diagnostics produce a bounded fix or show the outliers are irreducibly
stochastic.
