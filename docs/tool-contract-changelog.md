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

## [1.1.0] - 2026-11-17

Owned path annotations (WP16, FR-14). Additive, backward-compatible.

- Added the standard schema annotation `"x-daimonos-path": true` to every
  Daimonos-owned argument whose value is a filesystem path, a glob, or a
  directory to walk, including the elements of path arrays (the array `items`
  node is marked). This is a newly populated schema annotation, so it is a minor
  change under the versioning rules above.
- The marker means the same thing in every tool and is documented in
  `docs/configuration.md`. A reader can enumerate the path slots of any tool
  from its schema without a hand-written per-tool list (FR-14).
- Annotations are applied in the shared serving pipeline, so the deterministic
  dump and the live `tools/list` carry identical marks for the same inputs.
- No annotation is added to non-path arguments, to opaque command/script/argv
  strings, to agent-mail reservation glob patterns (never resolved against the
  filesystem), or to caller-provided worker / final-answer schemas.

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
