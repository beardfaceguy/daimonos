# Spike #1129 — Replacing Starlark `execute_script` with real Python

**Date:** 2026-08-11
**Status:** Complete — recommendation is **NO-GO** on replacing Starlark
**Tracks:** Vikunja #1129 (project 183). Blocks/related: #1230 (batch adoption)
**Branch:** `perf/1230-batch-adoption` (worktree `daimonos-batch-adoption`)
**Scope:** Investigation only. No production change to the script runtime.

---

## 0. Executive summary

The spike asked whether replacing the Starlark `execute_script` sandbox with
real Python would fix `execute_script` batch adoption for #1230.

**It would not, and the premise contains a defect.** The #1230 pilot evidence
that motivated this spike was collected through a harness bug that stripped the
Starlark builtin signatures out of the model's context during exactly the
generations under test. The most-cited "Python would fix this" failure —
hallucinating `create_snapshot()` instead of `snapshot(action=...)` — is a
direct consequence of that bug, and Python does not fix it.

Three findings drive the recommendation:

1. **Language friction is not the binding constraint on adoption.** B1 recorded
   9/9 correctness with explicitly **zero** `execute_script` result errors and
   still **0/9** adoption. A metric that language errors are not currently
   affecting cannot be moved by changing the language.
2. **The cited syntax failures are already fixed in Starlark**, in uncommitted
   work on this very branch. Top-level control flow is repaired by
   `wrap_top_level_control_flow` (`src/script.rs:399`); multi-line string
   literals by `normalize_string_literals` (`src/script.rs:275`).
3. **The remaining failure is a tool-namespace bug, not a language deficiency**,
   and it is now fixed and covered by a regression test (§2).

External CPython is nonetheless the correct architecture *if* the language ever
becomes binding, and §3–§4 specify it. But building it now would spend multiple
weeks of security-critical engineering to relieve a constraint that is not
binding, while the actual blocker — getting the model to choose a batched script
at all — remains untouched.

**Recommendation: NO-GO now. Re-evaluate only when the §7 gate trips.**

---

## 1. Evidence base

### 1.1 What #1230 actually measured

From `benchmarks/results/2026-08-10-batch-adoption-targeted.md`:

| Arm | Correct | Batch adoption | Script errors |
|---|---:|---:|---|
| B0 baseline | 9/9 | **0/9** | `execute_script` used twice, ≤240 B, never the batched strategy |
| B1 first-generation restriction | 9/9 | **0/9** | "No `execute_script` result errors occurred" |

Both arms are correctness-clean. B1 is *explicitly* error-free. The intervention
changed token and cost figures (−22.5% tokens, −21.5% cost) but the report
correctly declines to attribute those to adoption, since adoption never moved.

The conclusion the data supports: **the model declines to batch for reasons
unrelated to Starlark's syntax.** It used Starlark successfully, without errors,
for small scripts — then continued with separate operations.

### 1.2 Where the Starlark errors came from

Errors appeared only under the *stronger forced-script pilots*, which is the
condition §2 shows was corrupted. This matters for the causal claim: the spike
brief treats "stronger forcing → more Starlark errors" as evidence that Starlark
is fragile under load. §2 shows a large share of that fragility was injected by
the harness, not by the language.

---

## 2. Root cause: the restricted toolset destroyed the signature block

This is the spike's central finding. It is source-backed and now test-covered.

### 2.1 The mechanism

Three facts compose into the bug:

1. `tool_facade::active_schemas` builds the agent's tool catalog and **appends
   the exact Starlark builtin signatures** to `execute_script`'s description
   (`src/tool_facade.rs:37-40`):

   ```rust
   if t.name == crate::agent::EXECUTE_SCRIPT_TOOL {
       description.push_str("\n\nExact Starlark tool signatures:\n");
       description.push_str(&crate::script::tool_signatures());
   }
   ```

2. The same function filters to `Full | Terse | AgentOnly`
   (`src/tool_facade.rs:28-33`). `list_tool_signatures` is
   `ToolTier::OnDemand` (`src/tools.rs:376-382`), so it is **absent from the
   agent catalog entirely**. In agent mode the appended block is the *only*
   signature source that exists.

3. `script_first_toolset` then **assigned over** that description:

   ```rust
   tool.description = batch_first_description.to_string();   // pre-fix
   ```

   destroying the signature block — while the replacement `batch_first` text
   instructed the model to *"call the separately available `list_tool_signatures`
   tool first; do not guess"* (`prompts/tool_descriptions.toml`), naming a tool
   that is not in the catalog and cannot be called.

### 2.2 Net effect during the forced-script generations

The model saw exactly **one** tool — `execute_script` — with:

- no builtin signatures,
- an instruction to consult a lookup tool that does not exist,
- and no other tool available to make progress.

Hallucinating `create_snapshot()` is the expected outcome. The real signature is
`snapshot(action: str, *, id: str = None, tag: str = None)`
(`src/script.rs:1260-1269`, `src/script.rs:1537`). There is no `create_snapshot`
builtin; the complete surface is `tool`, `read_file`, `write_file`, `edit_file`,
`search`, `exec`, `ls`, `snapshot`, `git`, `gh`, `cargo`, `pytest`,
`session_stats`, `docker`, `discord`.

### 2.3 Proof

A regression test was written red-first and now passes
(`src/agent_cmd.rs::script_first_toolset_preserves_starlark_signatures`). Its
first assertion — that `list_tool_signatures` is absent from the agent catalog —
passed on unmodified code, confirming fact (2) empirically. Its second assertion
failed on unmodified code with *"restricted execute_script description dropped
the Starlark signatures"*, confirming fact (3).

### 2.4 Fix applied

`src/agent_cmd.rs` now re-appends the signature block after the batch-first
guidance, and `prompts/tool_descriptions.toml` no longer directs the model to an
unreachable tool. See §9 for the exact diff surface.

### 2.5 Why the `list_tool_signatures` pilot did not rescue it

#1230's 2026-08-11 comment records a pilot that *exposed* `list_tool_signatures`
during the restricted window. Task 03 regressed to 10 calls / 3 script errors /
$0.5927. That is consistent with — not contrary to — the diagnosis:

- Exposing the tool restores signature *availability* but costs a **round-trip**
  to use, in an experiment whose entire purpose is reducing round-trips.
- It does not restore the **inline** block, which `script_first_toolset` was
  still overwriting. The model had to choose between guessing and spending a
  call.

The fix in §2.4 is strictly better on both axes: signatures are present *and*
free, costing zero extra generations. The pilot tested a more expensive remedy
for a defect that had a free one.

### 2.6 Residual gap: return shapes

Captured failure 3 in #1230 — *"larger scripts continue to hit return-shape/
signature mismatches"* — is **not** fixed by §2.4. `tool_signatures()` declares
every builtin as `-> dict` (`src/script.rs:1531-1541`) without naming the keys,
so a model that knows how to *call* `search` still has to guess what it returns.

This is a cheap, targeted improvement to the Starlark path and should be tried
before any language change is reconsidered: document the response key set
alongside each signature. It is a change to one generated string, and it
addresses a failure mode that switching to Python would **not** fix — a Python
script guessing wrong dict keys fails exactly the same way.

**Consequence for #1129:** every forced-script pilot that produced Starlark
errors ran under a corrupted context and must be **re-run before its evidence is
used to justify a language change**. That re-run is #1230 work, not #1129 work.

---

## 3. Option comparison

Four candidates, judged against: sandbox strength, correctness impact,
engineering cost, and packaging burden.

### 3.1 Retained / improved Starlark — *recommended*

**How it is sandboxed today.** `starlark-rust` 0.13 evaluates in-process with no
host access by construction: the only capabilities are the builtins daimonos
registers in `build_globals` (`src/script.rs:508`). There is no filesystem,
network, environment, or subprocess surface except through those bindings, which
route via `dispatch_request` (`src/script.rs:629`) into the same `ops::dispatch`
path — so tool validation, analytics, tracing, and approvals apply uniformly.

**Known weaknesses (documented in-tree at `src/script.rs:141-150`):**

- *Pure-CPU runaway scripts cannot be cancelled.* starlark 0.13's `before_stmt`
  hook is `pub(crate)`, so a compute loop with no tool calls runs until process
  exit, leaking one OS thread. Bounded — not fixed — by
  `process.max_script_threads` (`src/config.rs:147-153`) via the
  `SCRIPT_PERMITS` semaphore (`src/script.rs:50`).
- *Cancellation is only checked between tool dispatches* (`with_ctx`,
  `src/script.rs:591`), so a script blocked inside a slow `exec` overruns its
  timeout.
- *Dialect friction* — no `import`, no top-level control flow, no `try`.

**Why it still wins.** The two friction sources that actually appeared in
captured failures are already repaired on this branch, and the third was the §2
harness bug. Remaining work is incremental and cheap:

| Improvement | Cost | Addresses |
|---|---|---|
| §2 signature fix | done | hallucinated builtins |
| `wrap_top_level_control_flow` | done (uncommitted) | top-level `for`/`if` |
| `normalize_string_literals` | already shipped | multi-line strings |
| Document return-shape keys (§2.6) | ~0.5 d | return-shape mismatches |
| Upgrade to starlark 0.14 | ~1–2 d (blocked, see below) | enables real CPU cancellation |

Every one of these is cheaper than a week of the Python option, and the first
three are already done.

The 0.14 upgrade is separately motivated: `Cargo.toml:26-34` documents that the
crate is pinned at 0.13 by a `Visitor::visit_field` trait-bound problem in
`starlark_map`, with a note that the bound stays "until we move to starlark
0.14". Landing that upgrade would close the single worst containment gap
(uncancellable CPU) without any new process model.

### 3.2 External CPython process — *strongest alternative, not warranted now*

A `python3` child process, contained by OS primitives, talking to the Rust
parent over a framed RPC socket. Rust keeps every responsibility it has today;
Python gets no direct capability at all.

**Strengths.** Real Python semantics (imports from a curated stdlib subset,
exceptions, comprehensions, f-strings) — eliminating dialect friction entirely.
Process-level containment is genuinely enforceable: kill, resource-limit, and
reap are all OS operations, so the uncancellable-CPU class that Starlark cannot
solve in-process disappears.

**Costs.** This is the expensive option. It needs a broker protocol, a
containment layer, a supervision/reaping layer, a security test suite, and a
packaging story — all security-critical, all new. §4 is its threat model and §5
its PoC shape.

**Hard portability finding (measured on this host, kernel 7.0.0-29-generic):**

| Probe | Result |
|---|---|
| `unshare -Ur` (raw unprivileged userns) | **FAILS** — `write failed /proc/self/uid_map: Operation not permitted` |
| `/proc/sys/kernel/apparmor_restrict_unprivileged_userns` | `1` |
| `bwrap --unshare-all` running python3 | **works** |
| network reachable inside bwrap | **no** (`OSError`) |
| `~/.ssh` readable inside bwrap | **no** (`FileNotFoundError`) |
| cgroup v2 delegated controllers (user slice) | `cpu memory pids` |
| Landlock | present in kernel |

The consequence is architecturally significant: **daimonos cannot hand-roll its
own namespace sandbox in Rust on a stock modern Ubuntu host.** AppArmor blocks
unprivileged `CLONE_NEWUSER`; only binaries with a permitting profile —
`bubblewrap` here — can create the namespaces. So external CPython acquires a
hard runtime dependency on an external setuid/profiled helper. That collides
with the AGENTS.md rule that external binaries belong in the plugin system and
with daimonos's single-static-binary distribution model (Dockerfile, MCPB
bundle, Buildroot image). Delegated cgroup v2 controllers *are* available, so
resource limits are enforceable without root — but containment is not.

### 3.3 PyO3 / embedded CPython — *rejected*

Ruled out by the brief, and the reasoning holds independently. CPython embedded
in-process shares the daimonos address space, file descriptors, environment, and
signal handlers. `ctypes` alone is a complete escape: it reaches `libc` and
therefore every syscall the daimonos process can make. Removing dangerous
modules from an in-process interpreter is a blocklist, and blocklists on CPython
have a long history of bypasses via `__subclasses__` traversal, `gc` object
graph walking, and codec/import machinery. It also imports the GIL into a Tokio
runtime. **Not a sandbox. Rejected.**

### 3.4 RustPython — *rejected*

A pure-Rust Python interpreter would preserve the single-binary model and give
Python-ish syntax without a subprocess. It fails on three counts:

- **Not a security boundary by design.** It exposes `os`, `subprocess`, and
  filesystem modules; disabling them is again a blocklist over a large, moving
  surface, and RustPython does not advertise sandbox guarantees.
- **Same uncancellable-CPU problem** as Starlark — it is in-process, so a
  compute loop is no more interruptible.
- **Incomplete and slow.** Partial stdlib and Python-level semantics gaps mean
  the "real Python" benefit is not actually delivered; scripts that work in
  CPython may fail here, reintroducing dialect friction under a different name.

It is strictly dominated: worse isolation than external CPython, no better
cancellation than Starlark, and a much larger dependency than either.

### 3.5 Summary

| | Starlark (improved) | External CPython | PyO3 | RustPython |
|---|---|---|---|---|
| Sandbox strength | Good (capability-only) | Strong (OS-enforced) | **None** | Weak |
| CPU-runaway cancellation | No (fixable via 0.14) | Yes | No | No |
| Real Python semantics | No | Yes | Yes | Partial |
| Fixes hallucinated builtins | n/a — §2 did | **No** | No | No |
| New external dependency | none | **bubblewrap + python3** | libpython | none |
| Single-binary packaging | preserved | **broken** | broken | preserved |
| Engineering cost | ~1–2 d | **~3–5 wk** | n/a | ~2–3 wk |

---

## 4. Threat model — external CPython

Recorded so the option is specified, not so it is built. Assets: workspace
contents, host filesystem, credentials (`ANTHROPIC_API_KEY`, `~/.ssh`, `gh`
tokens), the daimonos process itself, and host availability.

| # | Surface | Threat | Required control |
|---|---|---|---|
| T1 | Filesystem | Script reads `~/.ssh`, `agent.env`, writes outside workspace | Mount namespace: workspace bind only; everything else absent, not merely unreadable. Landlock as defence-in-depth |
| T2 | Environment | `os.environ` harvests provider API keys | Child spawned with an explicitly constructed empty env — never inherited |
| T3 | Network | Exfiltration, SSRF, unmetered LLM calls | Network namespace with loopback only. All LLM access via ADR-008 broker calls, budget-enforced in Rust |
| T4 | Subprocesses | `os.system`, `subprocess` bypass approvals/analytics | No shell in the mount namespace; seccomp denies `execve`/`execveat`/`fork`/`clone` beyond what the interpreter needs |
| T5 | Credentials | Token theft via any of T1–T4 | Union of T1–T4; the child holds no secret material at any point |
| T6 | Cancellation | Script ignores timeout; blocks on a slow broker call | Parent-owned deadline; `SIGKILL` to the **process group**, never `SIGTERM` alone |
| T7 | CPU | Infinite loop | cgroup v2 `cpu.max` + wall-clock deadline. *This is the class Starlark cannot solve in-process* |
| T8 | Memory | Allocation bomb | cgroup v2 `memory.max`; OOM-kill contained to the child's cgroup |
| T9 | PIDs | Fork bomb | cgroup v2 `pids.max` + PID namespace |
| T10 | Output | Multi-GB stdout floods the parent | Bounded framed reads; hard byte cap; kill on breach |
| T11 | Descendants | Orphans survive the script | PID namespace so child PID 1 death reaps the tree; process-group kill as backstop |
| T12 | Cleanup | Leaked FDs, tmpfiles, cgroups across runs | `CLOEXEC` on every parent FD; per-run tmpfs; cgroup removed on exit; **mandatory `waitpid` reap** |
| T13 | RPC | Script forges tool calls or escalates | RPC is *request-only*; Rust re-validates every call against the same `ops::dispatch` path, applies approvals, and can deny. Frame the RPC on a dedicated FD, never stdout |
| T14 | Confused deputy | Script drives Rust into out-of-scope operations | Broker enforces the same policy as a direct tool call — no privileged "script mode" |

Non-negotiable: **any sandbox escape, leaked process, correctness failure, or
unbounded resource path is a no-go.**

### 4.1 Trust boundary

```
┌─ daimonos (Rust) ── trusted ──────────────────────────────┐
│ validation · approvals · session · analytics · tracing    │
│ KGL · LLM budgets · deadline · reap                        │
│                        ▲                                   │
│                 framed RPC (dedicated FD)                  │
└────────────────────────┼───────────────────────────────────┘
┌────────────────────────▼── untrusted ─────────────────────┐
│ python3: no fs · no env · no net · no exec · no creds     │
│ cgroup(cpu,memory,pids) · PID+mount+net+IPC+UTS ns        │
└────────────────────────────────────────────────────────────┘
```

Rust retains **all** responsibility listed in the PoC constraints. Python is a
pure expression engine that can only ask.

---

## 5. PoC — deferred, and why

The brief authorised a PoC "only if warranted". **It is not warranted now**, on
three grounds:

1. **It targets a non-binding constraint.** B1 showed 0 script errors and 0
   adoption. Language is not what is blocking #1230.
2. **Its motivating evidence is invalid.** The forced-script error rate that
   would justify it was produced under the §2 bug and must be re-measured first.
3. **It cannot fix the headline failure.** `create_snapshot()` is a
   tool-namespace error. A Python script calling `create_snapshot()` fails
   identically — with a `NameError` instead of a Starlark eval error. §2 fixed
   it; Python would not have.

Building it anyway would mean 3–5 weeks of security-critical work, a broken
single-binary packaging story, and a new `bubblewrap` dependency — to relieve a
constraint that is not binding.

**What was built instead** is the containment feasibility probe (§3.2 table),
which is the genuine gate: it is cheap, needs no model or API spend, and its
result already materially changed the design conclusion by proving raw
unprivileged namespaces are unavailable on a stock host. Had that probe passed
trivially, the PoC estimate would be lower and the packaging objection weaker.
It did not.

### 5.1 Shape, if the §7 gate ever trips

Build order is deliberate — **security tests before any model or API benchmark**:

1. **Containment harness** — spawn `python3` under bwrap + cgroup v2, empty env,
   workspace bind, netns, `pids.max`/`memory.max`/`cpu.max`, deadline, group
   kill, mandatory reap. No RPC yet.
2. **Security test suite** — one red-first test per T1–T14 (§6). Every one must
   pass before step 3.
3. **Framed RPC broker** — length-prefixed JSON on a dedicated FD, distinct from
   script stdout. Request-only; Rust re-validates through `ops::dispatch`.
4. **Builtin shim** — a Python module exposing the §2 signature set, generated
   from the same `tool_signatures()` source so the two dialects cannot drift.
5. **Only then** the §7 benchmark.

Steps 1–2 alone are ~1.5–2 weeks and are the honest go/no-go point.

---

## 6. Security tests (gate for step 2)

Each is red-first; each maps to a threat. All are model-free and API-free.

| Test | Asserts |
|---|---|
| `denies_host_file_read` | `open('/etc/shadow')` and `~/.ssh/id_*` fail; workspace file succeeds |
| `denies_workspace_escape` | `../` and absolute paths outside the bind fail |
| `env_is_empty` | `os.environ` contains no `ANTHROPIC_API_KEY`/`GH_TOKEN`; ideally empty |
| `denies_network` | TCP connect, DNS, and UNIX-socket connect all fail |
| `denies_subprocess` | `os.system`, `subprocess.run`, `os.fork` fail |
| `kills_on_cpu_timeout` | `while True: pass` is killed within deadline + grace; **no leaked process** |
| `bounds_memory` | allocation bomb OOM-kills the child only; parent healthy |
| `bounds_pids` | fork bomb hits `pids.max`; host unaffected |
| `bounds_output` | multi-GB stdout truncated at cap, child killed, parent memory flat |
| `reaps_descendants` | grandchildren dead after run; `/proc` scan finds none |
| `reaps_zombies` | no zombie remains; cgroup dir removed |
| `no_fd_leak` | parent FD count stable over 100 runs |
| `rpc_cannot_forge_approval` | denied tool stays denied when requested from script |
| `rpc_isolated_from_stdout` | script writing framed bytes to stdout cannot inject an RPC |
| `cancel_propagates` | client disconnect kills the child promptly |

**Exit criterion:** all pass, plus a 1000-iteration soak with zero leaked
processes, FDs, or cgroups. Any failure is a no-go.

---

## 7. Benchmark design (only after §6 passes)

Compares Python against **best-current Starlark** — the improved baseline
including the §2 fix and `wrap_top_level_control_flow`, not the corrupted
forced-script arm.

**Held identical across arms** (the whole point):

- Provider/model `claude-opus-4-8`; thinking `medium`; compaction off; prompt
  caching off — matching the existing targeted lineage.
- Tasks `02-search-usages`, `03-edit-rename`, `07-snapshot-rollback`.
- Task-set fingerprint
  `1c5984aebc8455d99fc222fcddacc70b9e5cec2124ed4cac8dcad991fdda52c7`;
  fixture commit `600221ccae548a57ac3c4542966431243bd43c6a`.
- Identical system prompt, approvals, safety policy, and tool catalog —
  **including the signature block**, generated from one source for both dialects
  so the only variable is the language.
- Same repetition count (≥3/task; 5 preferred for adoption variance).
- Correctness gate first: a run that fails `check_task.py` contributes no
  efficiency numbers.

**Arms:** A = improved Starlark; B = external CPython. Record via
`ContextComposition` (already extended on this branch,
`src/context_metrics.rs`): `execute_script_calls`,
`execute_script_max_argument_bytes`, `execute_script_result_errors`, plus LLM
calls, tokens, cost, wall time.

**Primary metric:** batch adoption — ≥1 `execute_script` call with ≥700 B
serialized arguments, per the existing lineage definition.

**Decision rule, fixed in advance:**

- Python must show **adoption ≥ 6/9 while Starlark stays ≤ 2/9**, at 9/9
  correctness, to justify the migration.
- Python adoption within ±1 of Starlark ⇒ language is confirmed irrelevant;
  close #1129 permanently.
- Any correctness regression ⇒ immediate no-go regardless of adoption.

Append to `benchmarks/optimization-lineage.json` as a new lineage; do **not**
merge with the targeted lineage — task set and fixture differ.

---

## 8. Recommendation

### Go/no-go: **NO-GO**

Do not replace Starlark. Do not build the external-CPython PoC now.

### Instead, in order

1. **Land the §2 fix** (done here) and **re-run the forced-script pilots.** This
   is #1230's next step and it invalidates/revalidates the entire evidence base.
2. **Attack adoption directly** — it is a model-behaviour problem. The §2 bug
   made the restricted-toolset arm strictly worse than baseline, so the
   intervention has never had a fair test.
3. **Document builtin return shapes** (§2.6) — the one captured failure mode
   that §2.4 does not fix, and one Python would not fix either.
4. **Upgrade to starlark 0.14** when the `starlark_map` bound allows
   (`Cargo.toml:26-34`), closing the uncancellable-CPU gap.
5. **Re-open #1129 only if the §7 gate trips.**

### Estimates

| Work | Estimate |
|---|---|
| §2 fix + regression test | done (~2 h) |
| Re-run forced-script pilots | ~0.5 d + API cost |
| starlark 0.14 upgrade | 1–2 d, blocked on upstream bound |
| External CPython PoC steps 1–2 (containment + security tests) | **1.5–2 wk** |
| Steps 3–5 (broker, shim, benchmark) | **1.5–3 wk** |
| Production hardening, cross-platform, packaging | **+3–4 wk** |

### Packaging impact (if ever built)

Material, and a genuine argument against:

- **Breaks the single static binary.** Adds runtime `python3` **and**
  `bubblewrap` — §3.2 shows bwrap is not optional on a stock Ubuntu host.
- **Dockerfile** grows a Python runtime and a setuid/profiled helper.
- **Buildroot image** (`distro/`) must package CPython + bwrap — a large
  footprint increase for an agent-focused OS image.
- **MCPB bundle / Glama listing** can no longer assume a self-contained binary.
- **Linux-first by construction.** macOS needs an entirely separate
  implementation (`sandbox_init` is deprecated); Windows has no equivalent. Both
  would fall back to Starlark, so **Starlark must be retained regardless** —
  meaning Python is a second runtime to maintain, not a replacement.

That last point is decisive on its own: this was never "replace Starlark", only
"add a second, Linux-only script runtime beside it".

### Relationship to #1230

#1129 is a **dependency that resolves to "no change"**, plus one bug fix that
#1230 needs.

- #1230 should **not** wait on a Python runtime.
- #1230 **is** blocked on re-running its pilots with the §2 fix.
- The blocked-by relation should be **cleared** once #1129 closes; the outcome
  is "language is not the lever, here is the bug that made it look like it was".

---

## 9. Changes made on this branch

| File | Change |
|---|---|
| `src/agent_cmd.rs` | Re-append `tool_signatures()` after batch-first guidance; new regression test `script_first_toolset_preserves_starlark_signatures`; updated `script_first_toolset_exposes_only_execute_script` assertions |
| `prompts/tool_descriptions.toml` | `batch_first` no longer directs the model to the unreachable `list_tool_signatures`; points at the inline signature block |
| `docs/research/1129-...md` | This document |

Not touched: `src/script.rs`, `src/agent.rs`, `src/mcp.rs`, `src/config.rs`,
`src/context_metrics.rs`, benchmark harness, and every other piece of the
existing uncommitted #1230 work.

## 10. References

- `benchmarks/results/2026-08-10-batch-adoption-targeted.md` — B0/B1 data
- ADR-007 `docs/adr/007-context-offload-handles.md` — in-sandbox consumption
- ADR-008 `docs/adr/008-programmatic-llm-subcalls.md` — sub-call budgets; any
  Python runtime must honour D3 caps identically
- `src/script.rs:117-150` — threading, cancellation, documented leak classes
- `src/tool_facade.rs:22-48` — catalog construction and signature injection
- `src/tools.rs:376-382` — `list_tool_signatures` tier
- `Cargo.toml:26-34` — starlark 0.13 pin rationale
- `~/work/agentic_scripts/linux-sandbox-feasibility-probe/` — reproduces the
  §3.2 containment table on any host; verdict on this host was
  `viable-with-external-helper`
