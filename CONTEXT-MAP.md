# Context map

Date: 2026-10-28

| Context | Glossary | Decisions |
|---------|----------|-----------|
| Pinned git reads | [Git reads](docs/contexts/git-read/CONTEXT.md) | [ADR-017](docs/adr/017-git-read-tool.md), [Proposed ADR-018](docs/adr/018-strict-git-pinning.md), [Proposed ADR-019](docs/adr/019-ignore-repository-filters.md) |
| Session durability | [Glossary](docs/contexts/session-durability/CONTEXT.md) | [ADR-014](docs/adr/014-sqlite-session-memory.md), [ADR-015](docs/adr/015-session-protocol-v3.md), [ADR-016](docs/adr/016-cancelled-turn-partial-commit.md), [ADR-020](docs/adr/020-durable-turn-checkpoints.md) |

This map covers git reads and session durability. Other contexts are not mapped here yet.

_Anchors:_ src/plugins/git_read.rs::GitReadPlugin
