# SWE-bench per-instance guard calibration

## Decision

Keep the provisional **$2 cost / 300-second wall policy in report-only mode**.
Do not enforce it yet.

Across the full-50 run, two outlier reruns, and four near-threshold calibration
attempts, five observations would have triggered:

- two correct full-run patches (`sphinx-8638`, `sphinx-9229`);
- three incorrect rerun/calibration patches (`django-12273`, `sphinx-9229`,
  `sphinx-9461`).

The policy identifies expensive/long trajectories, but it is not a reliable
quality discriminator. Enforcing it would knowingly discard successful
patches. Report-only telemetry provides the evidence needed for later policy
changes without changing benchmark execution.

All operational values now come from
`benchmarks/swebench/benchmark.toml`; no timeout, cost, wall, retry, reserve, or
experiment-cap value is embedded as a Python default.

## Existing distribution

Cached Daimonos full-50 per-instance distribution:

| Metric | Median | P90 | P95 | P99 | Maximum |
|---|---:|---:|---:|---:|---:|
| Cost | $0.189 | $1.222 | $1.789 | $4.706 | $5.766 |
| Agent wall | 17.016s | 201.247s | 291.600s | 512.308s | 569.893s |
| Model calls | 6 | 34.5 | 50.5 | 77.6 | 84 |

The configured report thresholds round the observed P95 values upward. On the
original full-50 run they flag exactly `sphinx-8638` and `sphinx-9229`; both
patches resolved.

## Calibration runs

The four selected tasks represented original costs near P90/P95. Report-only
mode did not alter execution.

| Instance | Tokens | Calls | Cost | Wall | Trigger | Resolved |
|---|---:|---:|---:|---:|---|:---:|
| `django__django-11885` | 504,213 | 19 | $0.659034 | 83.174s | no | yes |
| `django__django-12273` | 1,488,809 | 43 | $1.594355 | 301.550s | wall | no |
| `django__django-12406` | 1,151,396 | 41 | $1.151213 | 209.664s | no | no |
| `sphinx-doc__sphinx-9461` | 2,357,163 | 61 | $2.194859 | 285.315s | cost | no |

Observed scoreable calibration cost was $5.599461. `sphinx-9461` also had a
pre-reboot interrupted attempt costing $0.870665; it is retained as operational
overhead and excluded from the table.

The tracked experiment planning spend is now $64.384317 against the configured
$70 cap, leaving $5.615683.

## Replay results

Full-50 replay:

- 50 summaries scanned;
- cost triggers: `sphinx-8638`, `sphinx-9229`;
- wall triggers: `sphinx-8638`, `sphinx-9229`;
- both were correct.

Trace reruns:

- `sphinx-8638`: $0.735956, 139.370s, resolved, no trigger;
- `sphinx-9229`: $2.813364, 440.579s, unresolved, both thresholds trigger.

Calibration:

- `django-12273`: unresolved, wall threshold triggers;
- `sphinx-9461`: unresolved, cost threshold triggers;
- `django-11885`: resolved, no trigger;
- `django-12406`: unresolved, no trigger.

## Configuration and crash behavior

`benchmark.toml` owns:

- per-attempt timeout and termination grace;
- OpenRouter settlement retry count and accepted delay;
- report-only cost/wall thresholds;
- total experiment cost cap and planning spend to date.

The runner checks remaining configured budget before each new instance and
refuses to start when it is below the configured per-instance reserve. It emits
`experiment-budget-summary.json` from `finally` and recovers cost from
completed summaries plus an unaccounted instance-local token log after handled
interruptions.

An OS/process crash cannot execute `finally`. The interrupted `sphinx-9461`
token log was therefore reconciled manually before updating the tracked spend.
The seven-hour elapsed time included laptop downtime; model activity had
stopped at 25 calls and $0.870665.

## Artifacts

- `swebench/results/20260912-005528-swebench-docker-guard-calibration-p95-r2`
- `swebench/results/20260912-083454-swebench-docker-guard-calibration-9461-r2`
- `daimonos-anthropic__claude-opus-4.8.20260912-005528-swebench-docker-guard-calibration-p95-r2-crash-partial.json`
- `daimonos-anthropic__claude-opus-4.8.20260912-083454-swebench-docker-guard-calibration-9461-r2.json`

## Next step

Leave `guard.mode = "off"` for ordinary runs and use
`--guard-mode report` for calibration runs. An enforcement mode should be added
only after more repetitions establish an acceptable explicit tradeoff between
cost containment and correct patches lost.
