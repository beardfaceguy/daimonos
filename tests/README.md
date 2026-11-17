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

Run from the repository root. The Python fixture builds the binary and starts a fresh MCP session per test.
The git_read tests use temporary repositories and require no remote objects or provider API key.
The `tests/test_read_only_workspace.py` suite (FR-07, `--workspace-read-only`) includes a test that
bind-mounts an evidence workspace read-only via `unshare -Urnm`; it is skipped when Linux user and
mount namespaces are unavailable, like the git_read read-only-mount test. It also drives
`daimonos agent --print` with a local fake provider to prove `execute_script` is refused
(`tool_policy_unsupported`) before any Starlark runs in agent mode, and that an
`DAIMONOS_OPENROUTER_TRACE_DIR` inside the workspace (INVENTORIES W11) is refused at startup; both
use local fake endpoints only (no live credentials). The suite also drives an interactive
read-only `daimonos agent` over a PTY to prove the bootstrapped session daemon inherits
`--workspace-read-only` (part of the daemon launch identity, so daemon reuse cannot bypass the
gate), checks that each `batch` child and a `tool_pipeline` carry their named refusal code with no
file created, that the `~`-prefixed trace dir is resolved like the writer, and that startup
refusals use the exit-2 `run_configuration_invalid`/`reason` shape.

Temporary programs under `target/test-tools` can support local checks on a restricted host.
They are not a required bootstrap path. Use the documented package installation on a clean host.
If a local run uses temporary programs, report their versions and the PATH prefix explicitly.
