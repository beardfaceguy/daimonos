# Android protocol v2 fixtures

Canonical JSON captured from `session_protocol` for non-Rust clients.

Android tests must deserialize these files without rewriting field names or
inventing frontend-only session semantics. Rust tests pin each fixture to the
serde wire model so incompatible changes require a protocol-version decision.
