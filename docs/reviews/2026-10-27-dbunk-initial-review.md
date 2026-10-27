# Initial review of dbunk feature requests

Date: 2026-10-27
Tracking: Vikunja #1575, project #183.
Source: `daimonos-feature-requests-for-dbunk.md`, supplied by the user.
State: Awaiting the dbunk PM's answers. No production implementation started.

This follows Parts 1 and 4 of the source document. Defaults and existing contracts remain unchanged.
New crate dependencies require separate human confirmation before addition.

## FR-01 — QUESTION

- **Q-FR-01-1:** Do ordinary evidence-tool turns continue without consuming corrective retries?
  The phrase "ends a turn any other way" could otherwise count every normal read call as a failed ending.
- Full draft 2020-12 validation will need a suitable validator. I will request approval before adding a crate.

## FR-02 — QUESTION

- **Q-FR-02-1:** Must explicitly allowlisted composites still check every inner call against the same allowlist?
  The alternative wording could permit an explicitly listed script to invoke unlisted tools.
- **Q-FR-02-2:** Must the caller explicitly include the FR-01 final-answer tool in the allowlist?
  Automatically adding it would violate the exact-list requirement.
- Existing agent restrictions must remain active. The new cross-runtime policy must not weaken them.

## FR-03 — QUESTION

- **Q-FR-03-1:** May confined mode refuse tools whose filesystem access it cannot constrain, even when separately allowlisted?
  This includes arbitrary commands, script code, and opaque plugins. Validating declared path fields cannot constrain their internal file access.
  Otherwise this requirement needs an OS sandbox boundary, not just path normalization.

## FR-04 — QUESTION

- **Q-FR-04-1:** Is the fixed header set an allowed-name set, or must every listed header appear on the wire?
  This determines whether the strict transport must use HTTP/1.1 rather than HTTP/2 framing and header rules.
- **Q-FR-04-2:** Must FR-04 require FR-01 explicitly, or may its probe omit the final-answer tool when FR-01 is off?
- Intended implementation is an opt-in transport profile. Existing provider serialization and defaults remain unchanged.

## FR-05 — CLEAR

Caller-supplied MCP workers, an explicit credential-free child environment, remote-only tool execution, and no local fallback.
This includes routing the final-answer tool through the worker when both features are enabled.

## FR-06 — REJECTED-REGRESSION

The merged `git_read` deliberately supports refs, tags, short SHAs, relative refs, and ranges.
Removing those forms violates the overriding no-regression rule and ADR-017.

- **CP-FR-06-1:** Add a default-off strict-pinning policy. In that policy, history calls require full 40-character hashes.
  Reject refs, ranges, implicit HEAD, and operations that cannot provide a pinned view. Preserve the existing default behavior.
- **CP-FR-06-2:** Retain existing operation names and result shapes. Offer requested aliases and commit bodies through an additive adapter or explicit options.
  Existing names include `show_commit`, `show_file`, and `list_tree`. The current log result has no commit body.

## FR-07 — CLEAR

Add a default-off workspace-write prohibition, relocate internal state through FR-08, and refuse operations that require workspace writes.
Delivery depends on the FR-08 answer and the confinement boundary from FR-03.

## FR-08 — QUESTION

- **Q-FR-08-1:** Does "everything Daimonos writes" mean internal state only, with intentional tool writes governed separately by FR-07?
  A literal interpretation would also relocate or refuse caller-requested `write_file` destinations whenever only the state option is enabled.
- Explicit internal-state destinations outside the chosen state root will be rejected, not silently honored or relocated.
  This includes the FR-04 audit path when FR-08 is enabled.

## FR-09 — CLEAR

Add an explicit-config-only mode before automatic discovery, including environment files, instructions, plugin registrations, and ignore files.
Report ignored paths without applying their contents.

## FR-10 — CLEAR

Add a configurable image without replacing the existing image's default invocation contract.
Test the requested restricted runtime after FR-05 and FR-08 are available.

## FR-11 — CLEAR

Add default-off, in-memory truncation with actionable paging guidance and no spill file or saved-file pointer.
Keep current spill and offload defaults unchanged.

## FR-12 — CLEAR

Add independent, default-off no-follow enforcement for path components and recursive walks.
Return link target text without reading the target's content or metadata.

## FR-13 — QUESTION

- **Q-FR-13-1:** Should the dump contain the effective tools for a supplied workspace, config, and policy, or the complete built-in catalog?
  Current MCP tools vary with installed programs, configuration, and exposure tiers.
  A complete catalog cannot always equal the effective served list.
- I propose a separate tool-contract version. Existing binary-version output would remain unchanged.

## FR-14 — QUESTION

- **Q-FR-14-1:** Must annotations cover only Daimonos-owned schemas, or also caller-provided MCP servers and opaque command/script inputs?
  Their path semantics cannot be inferred reliably from arbitrary strings or argv.
  If external schemas are included, what declaration must their owner supply, and must missing declarations fail startup?

## FR-15 — COUNTER-PROPOSAL

- **CP-FR-15-1:** Preserve existing terse and lazy defaults. Use an opt-in full-schema serving/export mode for the dbunk contract.
  Require self-contained 2020-12 schemas and stable definition names in that mode. Refuse unsupported schemas rather than rewriting them silently.
- **Q-FR-15-1:** Which endpoint/model combinations define "common model endpoints" for compatibility testing?
  No implementation can guarantee acceptance by every present and future compatible endpoint.
- **Q-FR-15-2:** If a valid caller schema contains a model-incompatible construct, should startup return `schema_unusable`, or should the probe return `model_unsupported`?
  FR-01 requires preserving the supplied schema unchanged.

## FR-16 — NEEDS-HUMAN

- **Q-FR-16-1:** Which public registry and namespace are authorized, and which CI publishing identity should be used?
  Human approval is needed before configuring publication permissions or registry access. Do not send secret values in this exchange.

## FR-17 — CLEAR

Hash full stored bytes from the same read used to produce content, including git blobs, without changing default result shapes.
If an enabled FR-19 budget prevents a full digest, return a limit error rather than omit the required digest.

## FR-18 — COUNTER-PROPOSAL

- **CP-FR-18-1:** Add a default-off offline-tools profile. Disable analytics collection, telemetry exporters, network discovery, and network-dependent read tools there.
  Preserve existing HTTP, GitHub, Discord, and remote MCP behavior outside that profile.
  Reject tools whose no-network behavior cannot be established. Caller-provided workers use the caller's network isolation under FR-05.
- **Q-FR-18-1:** Does the required read-tool set mean the local evidence tools, rather than every tool that returns information?
  Reading a remote service inherently needs a connection and cannot satisfy AC-18.1.

## FR-19 — COUNTER-PROPOSAL

- **CP-FR-19-1:** Enforce cooperative deadlines and explicit input-scan budgets on native operations, and propagate limits through the worker protocol.
  Refuse tools that cannot enforce the requested input budget. Do not substitute output-byte limits for scan limits.
  External git or arbitrary tool-server work cannot receive an exact input-byte guarantee from an output wrapper alone.
- **Q-FR-19-1:** Is the budget logical input bytes, rather than physical disk I/O or output bytes?
  Also confirm whether directory entries and index scans require separate accounting.

## Source checks

The review opened these source sections, not just search hits:

- `src/main.rs`: environment discovery, configuration loading, logging, and tool-service startup.
- `src/paths.rs`: current HOME-based state and session roots.
- `src/session.rs`: session construction and path resolution.
- `src/ops/file_ops.rs`: full-file reads, paging, and cached-read responses.
- `src/tool_output.rs`: spill-directory defaults and output bounding.
- `src/mcp.rs`: initialization metadata and runtime tool exposure.
- `src/plugins/git_read.rs`: existing operations, revision forms, and log result fields.
- `Dockerfile`: the current fixed MCP entrypoint and workspace.
- ADR-001, ADR-007, ADR-010, ADR-014, and ADR-017: provider, output, policy, state, and git constraints.

## Next step

The human relays these blocks to the dbunk PM. Wait for answers before implementing questioned or counter-proposed requests.
FR-07 is the only CLEAR P0 item, but its state relocation depends on FR-08.
No production code or original request file changed during this review.
