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
