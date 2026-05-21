# Contributing

Thanks for your interest in contributing to Daimonos.

## Development setup

1. Clone the repository.
2. Install prerequisites listed in `docs/install.md`.
3. Build locally:

```bash
cargo build
```

## Before opening a pull request

- Run Rust tests:

```bash
cargo test
```

- Run MCP end-to-end tests:

```bash
python3 -m pytest tests/ -v
```

- Ensure new functionality includes tests in the same change.
- Keep configuration defaults in `daimonos.default.toml` when adding tunables.

## Pull request guidelines

- Keep changes focused and reviewable.
- Explain the problem and why the change is needed.
- Include a short test plan in the PR description.
- Update docs when behavior, configuration, or workflows change.

## Project context for coding agents

If you are using an AI coding agent, read `AGENTS.md` before making code
changes.
