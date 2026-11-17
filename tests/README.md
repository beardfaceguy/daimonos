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

Temporary programs under `target/test-tools` can support local checks on a restricted host.
They are not a required bootstrap path. Use the documented package installation on a clean host.
If a local run uses temporary programs, report their versions and the PATH prefix explicitly.
