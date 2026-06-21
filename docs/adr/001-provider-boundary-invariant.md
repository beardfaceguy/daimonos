# ADR-001: Core must be model/provider-agnostic — no provider code in core, ever

**Date:** 2026-06-20
**Status:** Accepted — non-negotiable

## Decision

No model/provider-specific code in core, ever.

## Three-layer architecture

- **`core`** — defines the `LlmProvider` trait + neutral types (`Message`, `ToolSchema`,
  `LlmResponse`, `Usage`) + the agent loop. Core knows nothing about Anthropic, OpenAI,
  `/v1/messages`, `cache_control`, `x-api-key`, model-ID strings, or adaptive-thinking quirks.
- **`providers/anthropic`** — the ONLY place Anthropic-specific code lives (module in v1, own
  crate in phase A). Future providers (`providers/openai`, `providers/openrouter`) drop in behind
  the trait with zero core changes.
- **`providers/*`** — same pattern for any future provider.

## Design rule: "Core expresses intent, provider translates to mechanism"

This keeps provider-specific features out of core:

- **Caching:** core marks *which prompt parts are stable* (provider-neutral "cacheable prefix
  boundary"); the Anthropic provider turns that into a `cache_control` breakpoint, OpenAI-style
  providers ignore it (they have automatic prefix caching), others no-op. Core never names
  `cache_control`.
- **Usage telemetry:** neutral `Usage` uses provider-neutral field names (`cache_read`,
  `cache_write`, `input`, `output`, nested `cost` struct). Each provider maps its own response
  fields in — no Anthropic field name (`cache_read_input_tokens`, etc.) crosses the trait
  boundary.

## Neutral `Usage` shape (validated against OpenClaw)

```rust
pub struct Usage {
    pub cache_read: u64,
    pub cache_write: u64,
    pub input: u64,
    pub output: u64,
    pub cost: Cost,
}

pub struct Cost {
    pub input_usd: f64,
    pub output_usd: f64,
    pub cache_read_usd: f64,
    pub cache_write_usd: f64,
    pub total_usd: f64,
}
```

Cost is **provider-computed and passed through** — core never calculates it.

## Why

The whole point of the agentic CLI is owning the LLM loop to unlock prompt caching, real cost
accounting, and in-process direct tool dispatch — but that must not couple the system to any one
vendor.

Note: there is no official Anthropic Rust SDK, so the Anthropic provider is hand-rolled over
`reqwest` against raw HTTP. That hand-rolled code is provider-local, never core.

## Lazy tool-schema exposure caveat

The tool set must be decided **once per session**, not mutated mid-conversation. Changing the
tool set invalidates the entire prompt-cache prefix (ADR relates to task #879).

## Build order

Phase B (minimal extraction: `src/tool_facade.rs` + `src/providers/`) first, then phase A (full
crate split) once the loop is proven.

**Task order:** #872 (tool facade) → #875 (LlmProvider trait + Anthropic) → #873 (agent loop) →
#876 (CLI subcommand) → optimization tasks (#877, #878, #879, #880).
