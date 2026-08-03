# Hybrid lazy indexing lifecycle benchmark

## Question

Does merged lazy filename/path indexing (`cc6ecb3`) improve correctness on
cold and capped workspaces without regressing startup latency or steady-state
search performance versus the pre-feature implementation (`53339f9`)?

## Method

- Deterministic server benchmark; no LLM or API calls.
- Release binaries built from isolated worktrees.
- 30 replicates per arm/profile, with arm order rotated each replicate to
  prevent thermal/load drift from favoring a fixed arm.
- 20 warm filename searches after each successful first search.
- Fixtures:
  - `small`: 200 files, complete under the configured limits.
  - `large-unmarked`: 3,000 files, no project marker.
  - `large-marked`: 3,000 files plus `Cargo.toml`.
- Limits: `max_files = 500`, `max_walk_entries = 1000`.
- Large fixtures query `.rs`, so correctness means returning at least one
  filename inside partial coverage. The small fixture queries an exact target.
- Startup measures process launch until the Unix socket exists.
- Resource values are the maximum sampled after startup, readiness, and warm
  searches. They are not sub-millisecond continuous peak samples.

Raw artifact:
[`2026-08-03-index-lifecycle-r4.json`](2026-08-03-index-lifecycle-r4.json).

## Results

### Small fixture

| Arm | Correct | Startup median | First search | Ready | Warm median | RSS median | Inotify max |
|---|---:|---:|---:|---:|---:|---:|---:|
| Legacy baseline | 30/30 | 228.40 ms | 0.46 ms | 0.86 ms | 0.017 ms | 22,170 KiB | 21 |
| Candidate eager | 30/30 | 226.49 ms | 0.50 ms | 0.51 ms | 0.024 ms | 22,450 KiB | 42 |
| Candidate lazy | 30/30 | 226.07 ms | 2.67 ms | 2.68 ms | 0.025 ms | 22,552 KiB | 21 |
| Candidate hybrid | 30/30 | 229.41 ms | 0.44 ms | 0.45 ms | 0.026 ms | 22,480 KiB | 42 |

Hybrid versus baseline:

- startup **+0.44%**;
- readiness **-47.8%**;
- RSS **+310 KiB (+1.40%)**;
- warm search adds about **9 µs** per call.

### Large unmarked fixture

| Arm | Correct | Startup median | First/ready search | Warm median | RSS median | Coverage | Inotify max |
|---|---:|---:|---:|---:|---:|---|---:|
| Legacy baseline | 0/30 | 226.32 ms | no correct result | — | 21,698 KiB | legacy empty | 0 |
| Candidate eager | 30/30 | 222.05 ms | 0.59 ms | 0.114 ms | 23,198 KiB | partial | 42 |
| Candidate lazy | 30/30 | 221.37 ms | 8.13 ms | 0.117 ms | 23,120 KiB | partial | 21 |
| Candidate hybrid | 30/30 | 227.36 ms | 7.78 ms | 0.116 ms | 23,184 KiB | partial | 21 |

Hybrid keeps startup effectively flat (**+0.46%**) and converts a deterministic
legacy failure into **30/30 correct** bounded searches. The first cold search
costs 7.78 ms; subsequent searches cost 0.116 ms median. RSS increases by
1,486 KiB (6.85%) after materializing the partial path index.

### Large marked fixture

| Arm | Correct | Startup median | First/ready search | Warm median | RSS median | Coverage | Inotify max |
|---|---:|---:|---:|---:|---:|---|---:|
| Legacy baseline | 0/30 | 244.52 ms | no correct result | — | 22,362 KiB | legacy | 21 |
| Candidate eager | 30/30 | 245.39 ms | 0.68 ms | 0.124 ms | 23,142 KiB | partial | 42 |
| Candidate lazy | 30/30 | 239.13 ms | 8.62 ms | 0.116 ms | 23,190 KiB | partial | 21 |
| Candidate hybrid | 30/30 | 244.03 ms | 0.65 ms | 0.123 ms | 23,196 KiB | partial | 42 |

Hybrid startup is flat (**-0.20%**) and warm-starts the marked project:
first correct filename results arrive in 0.65 ms. RSS increases by 834 KiB
(3.73%).

## Conclusion

The hybrid default passes the performance/correctness gate:

- no material startup regression across the three fixtures;
- 30/30 correctness in every candidate mode/profile;
- bounded cold-search cost below 9 ms for 3,000-file fixtures;
- sub-0.13 ms median warm searches;
- explicit `partial` coverage whenever traversal limits are reached.

The principal upward blip is watcher usage. Hybrid/eager warm projects use one
additional recursive watcher set: 42 versus 21 inotify watches in these
21-directory fixtures. This scales with directory count and should be addressed
by sharing filesystem invalidation between the path index and pipeline cache
before treating watcher footprint as fully optimized.

The RSS increase (0.3–1.5 MiB in these fixtures) is expected because the
candidate now retains a correct path index where the legacy implementation
returned no filename results. It should still be tracked in larger fixtures.

