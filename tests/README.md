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

Temporary programs under `target/test-tools` can support local checks on a restricted host.
They are not a required bootstrap path. Use the documented package installation on a clean host.
If a local run uses temporary programs, report their versions and the PATH prefix explicitly.

## tests/test_structured_output.py (WP05, FR-01)

Structured final output acceptance tests (ADR-024). AC-01.1–01.7 plus the turn-limit, zero/exhausted-retry, evidence-turn, and success-on-last-request cases run against a local scripted model endpoint on BOTH wires — Chat Completions (`openrouter`) and Messages (`anthropic`). No live API or real credentials. Run: `python3 -m pytest tests/test_structured_output.py -v`.

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