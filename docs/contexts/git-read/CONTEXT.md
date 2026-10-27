# Pinned git reads

Date: 2026-10-27

- **Workspace:** The fixed root that contains repositories. The session cwd does not change this boundary.
- **Pinned commit:** A full commit SHA returned with a read result. It makes a citation independent of moving refs.
- **Promisor:** A partial-clone remote that holds objects absent locally. `git_read` never asks it for objects.
- **Alternate store:** Another git object directory. Chained stores must stay inside the workspace.
- **Patch preview:** Bounded patch text. A truncated preview is not a complete patch.
- **Pointer file:** Git metadata that selects a git directory or common directory. It must stay inside the workspace.
- **Bare repository:** A git database without a working tree. History reads are valid, but worktree status is not.

[ADR-017](../../adr/017-git-read-tool.md) records why the tool is separate from `git`.
[Configuration](../../configuration.md#read-only-git-git_read) names its limits.

_Anchors:_ src/plugins/git_read.rs::GitReadPlugin, src/plugins/git_read/runner.rs::ReadRepo, src/config.rs::GitReadConfig
