# Android protocol v3 fixtures

Canonical JSON captured from `session_protocol` for non-Rust clients.

Android tests must deserialize these files without rewriting field names or
inventing frontend-only session semantics. Rust tests pin each fixture to the
serde wire model so incompatible changes require a protocol-version decision.
Version 3 carries the ordered timeline, `HistoryWindow`, mandatory active tool
state, and privacy-safe durability status transitions.

`remote_auth.json` freezes the challenge, pairing, local-approval result, and
device-proof authentication envelopes used before the session protocol.
Remote authentication signs
`daimonos-remote-v2\0<challenge>\0<ticket>` with the paired Ed25519 device key.
Tickets are opaque, daemon-memory-only, and must not appear in URLs. Pairing
claims are single-use and expire after five minutes.
