# ADR-025: Controlled model requests use an isolated opt-in transport profile

- **Status:** Accepted
- **Date:** 2026-10-28
- **Deciders:** Repository maintainer, by explicit user approval on 2026-10-29 UTC.
- **Consulted:** None recorded.
- **Informed:** None recorded.
- **Spec:** Local dbunk requests and PM amendments, sealed in the [package plan](../handoffs/2026-10-28-dbunk-remaining-work.md).
- **Packages:** WP07
- **Dependencies:** Accepted ADR-001. Independent of Accepted ADR-024 and ADR-027.
- **Supersedes:** None
- **Superseded by:** None

## Context

Current provider adapters have distinct wire formats, timeout handling, retry behavior, and default client construction.
The default HTTP builder can fall back to an unconstrained client after a builder failure.
dbunk requires requests whose configuration and network behavior are known before review starts.
How can that contract coexist with ordinary provider behavior?

## Decision drivers

- No inference of wire, endpoint, credential source, or model.
- No ambient headers, proxies, retries, redirects, or client telemetry.
- Preserve provider-local wire ownership and legacy defaults.
- Make transport usable without structured output or a capability probe.

## Decision

We will add a separate opt-in request profile rather than change default adapters globally.
Resolve provider label, model, base URL, wire, and credential-variable name from one caller-owned source.
All five are required. Reading the credential value from that named variable does not permit mixing configuration fields from other sources.
Reject incomplete profiles before requests. Never infer wire from a label, URL, or model.

Use a profile-scoped HTTP client with explicit caller timeout, no redirects, and no ambient proxy or default headers.
A client-construction error fails the profile. Do not reuse the legacy fallback client.
Only an explicit proxy is allowed. No automatic HTTP retry occurs.
Caller-configured retries are visible, audited when enabled, and subject to the model-request ceiling.

Wire adapters serialize the required fixed top-level field sets from FR-04.
Chat uses Chat Completions, including OpenRouter's compatible endpoint. Messages uses stable Anthropic Messages paths.
Do not repurpose the native OpenAI Responses adapter.
Set automatic tool choice explicitly whenever tools are offered.
Do not add sampling parameters, forced choices, strict schema flags, cache hints, or extra body fields by default.
Explicit extensions cannot override the mandatory sampling/forced-choice prohibitions.

Send the required credential, Content-Type, fixed User-Agent, and Messages version headers.
Permit only the specified standard HTTP transport headers beyond that required subset.
Suppress ambient organization/project/custom headers, tracing propagation, and model-client telemetry export.
No model catalog discovery supplies hidden defaults.
Outside this profile, current adapters retain their behavior.

## Alternatives

- **Change all adapters' defaults:** breaks unrelated users and endpoint-specific support.
- **Infer compatibility from URL/model:** violates explicit wire ownership.
- **Use the ordinary client and remove headers afterward:** ambient middleware can already observe or transmit secrets.
- **Require structured output to enable transport:** couples independent PM requirements.

## Consequences

Profile-specific construction and serialization need recording-endpoint tests.
Some vendor conveniences are intentionally unavailable in this mode.
Provider-neutral core options must not contain vendor wire branches.
The profile controls Daimonos requests, not a caller's upstream retry policy.

## Confirmation

Run future `tests/test_request_control.py` and existing provider tests.
Assert exact fixed bodies and allowed/required header sets on both wires.
Install hostile proxy/header environments and a tracing provider. Observe that no forbidden value travels.
Return 503 and redirects. Count actual attempts and verify that no other host receives a request.
Enable transport without structured output, then disable it and rerun legacy regressions.

## Anchors

_Anchors:_ src/providers/mod.rs::ProviderTimeouts::client, src/providers/mod.rs::LlmProvider, src/providers/anthropic.rs::AnthropicProvider, src/providers/openrouter.rs::OpenRouterProvider

## Revisit when

A new supported wire, required vendor header, or explicitly requested extension changes the fixed contract.

## Open questions

- The user accepted this decision on 2026-10-29 UTC. Remaining interface questions do not imply implementation approval.
- Option names and explicit-extension validation need the shared interface review. The required five fields and wire choices are not open product questions.
