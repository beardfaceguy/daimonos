# Tool contract changelog

This file tracks changes to the **tool contract** — the set of tool names,
descriptions, argument schemas, and result shapes daimonos serves and exports.

The tool contract carries its own semantic version
(`src/tool_contract.rs::CONTRACT_VERSION`), deliberately separate from the Cargo
package (binary) version and from any source-commit identity. See
[ADR-029](adr/029-effective-tool-contract.md) and FR-13.

Versioning rules:

- **Major** — a breaking change to any tool: a renamed tool, a renamed
  argument, a changed argument type or meaning, a removed tool, or a changed
  result shape.
- **Minor** — an additive, backward-compatible change: a new tool, a new
  optional argument, or a newly populated schema annotation.
- **Patch** — a correction that does not change behavior: a description fix or a
  documentation clarification.

Every change that bumps `CONTRACT_VERSION` must add an entry here, and every
entry here must correspond to a version bump. The deterministic dump
(`daimonos tool-contract`) and the MCP initialization metadata
(`_meta["io.daimonos/tool-contract-version"]`) both report this version.

## [1.0.0] - 2026-11-17

Initial tool-contract version (WP15, FR-13).

- Established the effective tool contract as a function of the effective serving
  inputs (workspace context checks, exposure tier, schema mode, and
  confinement), reusing the live serving pipeline rather than a second
  handwritten list. The deterministic JSON dump therefore equals what the MCP
  server serves for the same inputs.
- Added `daimonos tool-contract`: a session-free JSON dump of
  `contract_version`, `effective_options`, and the effective `tools` (each with
  `name`, `description`, `input_schema`, and registry-derived `tier`/`route`
  metadata).
- Reported the contract version over MCP initialization in
  `_meta["io.daimonos/tool-contract-version"]`, distinct from
  `serverInfo.version` (the binary version).
