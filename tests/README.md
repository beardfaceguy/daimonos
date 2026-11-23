# Run the test suites

The full suites invoke installed CLI programs. They do not replace missing programs with stubs.
Rust uses the repository's pinned toolchain in `rust-toolchain.toml`.

## Prerequisites

Install Git, curl, Node.js, npm, ShellCheck, Python, and Python virtual-environment support.
Git must support `--no-lazy-fetch`. This is required for the git_read tests.

Ubuntu and Debian:

```bash
sudo apt-get update
sudo apt-get install --yes git curl nodejs npm shellcheck python3-venv
```

macOS with Homebrew:

```bash
brew install git curl node shellcheck python
```

Create a Python environment from the committed dependency list:

```bash
python3 -m venv .venv
.venv/bin/python -m pip install -r tests/requirements.txt
export PATH="$PWD/.venv/bin:$PATH"
```

## Verify the environment

```bash
git --no-lazy-fetch --version
node --version
npm --version
shellcheck --version
curl --version
python3 -m pytest --version
```

Record these versions with test results. Package versions depend on the host package repositories.
The CI test jobs explicitly install npm and ShellCheck before running the suites.
The other CLI programs are part of the configured CI environment.

## Run

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
python3 -m pytest tests/ -v
```

`tests/test_request_control.py` covers the controlled model transport (WP07 / FR-04) through the CLI; it uses only local fake endpoints and never a credentialed endpoint. It includes startup validation (AC-04.7, including the refusal of a sampling parameter smuggled via `extra_body_fields` and of a `messages` profile that omits the mandatory `max_tokens`, plus acceptance of a `chat` profile that omits it), the default-off opt-out regression, process-level recording of body/headers/retry/redirect/tool-choice on both wires, a fail-closed refusal of `--model-profile` combined with `--interactive`, and an ambient-proxy-ignored check against a local counting proxy. The transport-level exact body/header/timeout/proxy/forced-choice assertions (AC-04.1–04.5) run under `cargo test providers::controlled_transport_tests`.

Run from the repository root. The Python fixture builds the binary and starts a fresh MCP session per test.
The git_read tests use temporary repositories and require no remote objects or provider API key.
The `tests/test_read_only_workspace.py` suite (FR-07, `--workspace-read-only`) includes a test that
bind-mounts an evidence workspace read-only via `unshare -Urnm`; it is skipped when Linux user and
mount namespaces are unavailable, like the git_read read-only-mount test. It also drives
`daimonos agent --print` with a local fake provider to prove `execute_script` is refused
(`tool_policy_unsupported`) before any Starlark runs in agent mode, and the same refusal through
the `daimonos acp` JSON-RPC frontend (both drive the shared `run_inner` loop), and that an
`DAIMONOS_OPENROUTER_TRACE_DIR` inside the workspace (INVENTORIES W11) is refused at startup; both
use local fake endpoints only (no live credentials). The suite also drives an interactive
read-only `daimonos agent` over a PTY to prove the bootstrapped session daemon inherits
`--workspace-read-only` (part of the daemon launch identity, so daemon reuse cannot bypass the
gate), checks that each `batch` child and a `tool_pipeline` carry their named refusal code with no
file created, that the `~`-prefixed trace dir is resolved like the writer, and that startup
refusals use the exit-2 `run_configuration_invalid`/`reason` shape.

The `tests/test_run_audit.py` suite (FR-04, `--audit-path`) drives `daimonos
agent --print` with a local fake OpenAI-style provider to prove the opt-in run
audit: a versioned `run_start`…`run_end` JSONL lifecycle, `tool_call`/
`tool_refusal` records with SHA-256 argument/result hashes and durations,
`model_request` records, the fail-closed exit-23 contract on an audit open
failure (no model request runs), the exit-2 refusal when the path is outside an
explicit state root, and the opt-out regression (no file when the option is
off). It includes the AC-04.8 scan asserting the exact credential value appears
nowhere in the complete audit record. All endpoints are local fakes; no live
credentials are used.

`tests/test_tool_contract.py` (FR-13) drives the real `daimonos tool-contract`
command and a live MCP server with no provider API key: it checks that the
deterministic dump is byte-stable, equals `tools/list` for the same workspace,
config, and schema mode, and that the contract version reported over MCP
initialization equals the dump's version.

`tests/test_path_annotations.py` (FR-14) drives the same real `daimonos
tool-contract` dump and asserts AC-14.1: every Daimonos-owned filesystem path,
glob, or walk argument — including the elements of path arrays — carries the
`x-daimonos-path` annotation, and no non-path, opaque-command/script, or
reservation-glob argument does. `get_tool_schema` over a real MCP server is
asserted to carry the marks for every dump tool and the three on-demand tools
(`diff_files`/`tool_pipeline`/`lint_repair`) — the only caller surface for the
marks of the tools whose initial-tier `tools/list` schema is stripped (until
WP17). The marks are caller-facing only: a process test drives `agent --print`
against a local fake provider endpoint on both wires (OpenAI Chat Completions
and Anthropic Messages) and asserts the recorded model request never contains
`x-daimonos-path` (FR-15 preserves the model-facing schema). No provider API key
or live endpoint is required — the fake endpoint is local.

`tests/test_configuration_isolation.py` drives a real `daimonos` process to assert
explicit-only configuration discovery (`--ignore-config-files`, FR-09): a hostile
workspace corpus configures nothing, each ignored source is reported by path, and
an explicit `--config` is still honored. It also asserts ignore-file suppression
across every reachable walker — content `search`, `search mode="files"`, their
confined (`--confine-paths`) equivalents, and `snapshot` create/restore — plus the
exit-2 `config_invalid` failure for an invalid explicit `--config`. Process-level
proofs confirm a hostile workspace `agent-instructions.md` never reaches the
model request (observed on the recorded chat-completions request captured by a
local fake provider) and that an ignored `[tools.*]` registration is absent
from `tools/list` and undispatchable. Like the other suites it needs the CLI
programs on `PATH` (the plugin tests spawn `pytest`); export the virtualenv's
`bin` directory as shown above before running.

Temporary programs under `target/test-tools` can support local checks on a restricted host.
They are not a required bootstrap path. Use the documented package installation on a clean host.
If a local run uses temporary programs, report their versions and the PATH prefix explicitly.

## tests/test_structured_output.py (WP05, FR-01)

Structured final output acceptance tests (ADR-024). AC-01.1–01.7 plus the turn-limit, zero/exhausted-retry, evidence-turn, and success-on-last-request cases run against a local scripted model endpoint on BOTH wires — Chat Completions (`openrouter`) and Messages (`anthropic`). No live API or real credentials. Run: `python3 -m pytest tests/test_structured_output.py -v`.

- `test_tool_allowlist.py` (FR-02/WP06): drives a real `daimonos` process proving `--tool-allowlist` is the effective set at every listing and dispatch boundary — AC-02.1..02.6, the audit `tool_refusal`, nested Starlark/batch cannot bypass it, and the default-off regression. Reuses the fake-provider agent harness from `test_run_audit.py`. The r2 review-fix tests add: schema lookup (`get_tool_schema`) and signature dump (`list_tool_signatures`) are filtered to the allowlist, the MCP server instructions hide unlisted signatures, the effective report is the intersection with the agent-env `allowed_commands` legacy restriction, `--final-output` omitting the final-answer tool fails startup (`tool_allowlist_missing_final_output`), a workspace `daimonos.toml` can neither widen nor disable the list, and a config-registered `[tools.*]` tool may be allowlisted.

## tests/test_capability_probe.py (WP09, FR-04)

Same-tool capability-probe acceptance tests (ADR-027, requirements 14–17). They drive a real `daimonos agent --print` process against local fake Messages endpoints (the pattern from `test_request_control.py`), never a credentialed endpoint. Coverage: the `--capability-probe-payload` caller interface and its startup refusals (requires `--final-output` and `--model-profile`; rejected on `--dry-run` and the non-agent frontends); local payload validation before any request is sent (invalid/missing-field/non-JSON → exit 2, zero requests); the one unretried probe offering the frozen catalog — a pass proceeds to exit 0, while text-only, wrong-tool, two-calls, invalid-arguments, a 503, and a 3xx redirect each give the `model_unsupported` exit 21 with exactly one request and no further work (AC-04.1–04.7, zero-retry, local-reference, redirect-not-followed); the probe outcome recorded as a single `probe_result` audit event with no credential value (AC-04.6/04.8); and the probe request counting against the `max_model_requests` ceiling (R5, exit 24). Run: `python3 -m pytest tests/test_capability_probe.py -v`.
## tests/test_worker_isolation.py (WP10, FR-05)

Credential-free worker execution acceptance tests (ADR-028). Drive a real
`daimonos agent --worker-config` process against a local scripted model endpoint
and a RECORDING, credential-free MCP-stdio worker (pure stdlib). The worker dumps
its own process environment and records every served tool call, so isolation and
routing are observed from the outside — no live API, no real credentials. Covers
AC-05.1 (no credential in worker env; the credential is a real var of the
model-side process), AC-05.3 (every call served by a worker, recorded under the
`worker` audit route, none in-process), AC-05.4 (a killed worker aborts with the
distinct `worker_unavailable` exit 22 and runs no later tool), AC-05.5 (the
`--tool-allowlist` is the sole admission gate for worker tools), the WP04
read-only pass-through (worker tools are the caller's sandbox, not path-checked),
the WP08/ADR-026 audited-worker-death case, the default-off regression, and the
fail-closed control-file validation (exit 2). The no-network container leg
(AC-05.2) and the WP09 capability-probe leg are integration merge items — the
pure-stdlib worker makes no outbound connection. Run:
`python3 -m pytest tests/test_worker_isolation.py -v`.

- `test_worker_isolation.py` (WP10 r2): adds credential-disclosure refusal (R1), the no-network namespace leg via `unshare -Urn` (AC-05.2, skipped if the host cannot create a user+net namespace), post-discovery allowlist validation (R3), the `--final-output`+`--worker-config` final-only case (R4), process-table reaping incl. a stalled worker + descendant (R6), typed lost-worker terminal (R7), and `command`-with-`=` refusal (R8).
- `test_worker_isolation.py` (WP10 r5): the AC-05.2 network-isolation leg requires a host that can create an unprivileged user and network namespace (`unshare -Urn`) and is skipped on hosts that cannot; because the plan's final gate treats a skipped mandatory acceptance check as a blocker, this skip is NOT a pass and the gate must run on a userns-capable host.

## tests/test_full_schemas.py (WP17, FR-15)

Self-contained full-schema serving/export acceptance tests (ADR-030). Drive the
real `daimonos` binary: the `tool-contract` dump and the MCP `tools/list` server
with the opt-in full-schema mode on (`--full-tool-schemas`). AC-15.1 (every
schema in the dump validates against the Draft 2020-12 metaschema — checked
authoritatively by the binary's own validator, a zero exit being the pass — and
contains no `$ref` outside itself), the owned-path marks appearing in the dump
for every tool once schemas are full, the opt-out regression (default dump keeps
the stripped placeholders), and frontend coverage (MCP server, agent frontend,
help text; the session-daemon launch identity is covered by Rust unit tests in
`src/session_bootstrap.rs`). No live API or real credentials. Run:
`python3 -m pytest tests/test_full_schemas.py -v`.

The r2 supplied-schema cases drive `daimonos agent --final-output` against a
local scripted model endpoint on both wires (Chat Completions and Messages) and
record every request body: in full-schema mode a supplied final-answer schema
with a recursive local reference is refused before any request (exit 20,
`schema_unusable`), while without the mode the same schema is accepted (exit 0),
and a non-recursive `$defs`/`$ref` (or `$anchor`) schema is accepted and offered
to the model deep-equal/unchanged. The metaschema and self-containment checks
rely on the product's own Draft 2020-12 validator (`src/schema_validation.rs` /
`src/schema_contract.rs`); no third-party `jsonschema` dependency is added or
required.

The r3 cases extend the supplied-schema coverage to every reference form the
recursion detector follows: both-wire process tests refuse a supplied
final-answer schema that is recursive through a plain-name `$anchor` and one
recursive through its own `$id` (exit 20, `schema_unusable`, zero requests),
with the default-mode accept pins alongside. The matching Draft 2020-12
URI/anchor resolution and the fail-closed (D41) refusal of an unresolvable
in-document reference are proven at the `src/schema_contract.rs` unit seam
(decoded JSON-pointer, `$anchor`, `$dynamicRef`/`$dynamicAnchor`, and `$id`-named
`$ref`), still with no third-party dependency.

The r4 cases pin the typed-traversal fix (integrator decision D46): a supplied
final-answer schema whose `const` *data* merely looks like a `$ref` is NOT
recursive — it is accepted in full-schema mode and offered to the model
unchanged — while a schema that IS recursive through an embedded `$id` is
refused (exit 20, `schema_unusable`, zero requests) even when a decoy `$id`
sits in a `default` data value, with the default-mode accept pins alongside.
The keyword classification (reference vs data vs schema-position vs unknown)
and the duplicate-`$id`/anchor refusal are proven at the
`src/schema_contract.rs` unit seam, including a table that routes a cycle
through every 2020-12 schema keyword.

### WP18 cooperative tool budgets (`tests/test_tool_budgets.py`)

Process-level acceptance tests for FR-19 (ADR-031): the three default-off
budget options drive a real `daimonos` MCP process (and a real `agent --print`
process for the audit/`--output json`/worker cases). Timing tests (AC-19.1) use
a deadline of 1000–1500 ms and assert an upper bound of several seconds while
keeping the data small, so a slower CI runner still stops promptly without
flaking; each bound and its reason are stated in the test docstring. The
`exec` and `git_read` reap proofs follow the method of
`tests/test_worker_isolation.py::test_r6_stalled_worker_is_force_reaped`
(scanning `/proc` for a unique marker) to show no child or grandchild outlives
the stop; the `git_read` proof installs a `$GIT_DIR`-gated fake `git` shim so
repo discovery still uses the real git while an operation call hangs. These
tests need a Unix host with `/proc` and a real `git`; a skip is not a pass.

`tests/test_tool_budgets.py` (WP18, FR-19): process-level cooperative tool
budget tests — scanned-byte metering for content search, native deadline checks
(read_file/ls/both search modes), plugin deadline+reap (curl, git), the
`tool_budget_unenforceable` refusals (git plugin, index search, pipeline), the
AC-19.1 large-tree timeout bound, and the `limit_hit` audit event (by
kind+fields). AC-19.1 writes 10000 small files and stays under ~10 s.
