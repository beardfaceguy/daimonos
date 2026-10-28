# Pinned git reads

Date: 2026-10-28

- **Workspace:** The fixed root that contains repositories. The session cwd does not change this boundary.
- **Pinned commit:** A full commit SHA returned with a read result. It makes a citation independent of moving refs.
- **Strict pinning:** The optional policy that also requires explicit SHA-1 commit identifiers as inputs. It rejects mutable names and tag objects.
- **Promisor:** A partial-clone remote that holds objects absent locally. `git_read` never asks it for objects.
- **Alternate store:** Another git object directory. Chained stores must stay inside the workspace.
- **Patch preview:** Bounded patch text. A truncated preview is not a complete patch.
- **Pointer file:** Git metadata that selects a git directory or common directory. It must stay inside the workspace.
- **Bare repository:** A git database without a working tree. History reads are valid, but worktree status is not.
- **Filter-free status:** Status without repository-configured external filter transformations. A filtered file can appear modified even when ordinary Git reports clean.

[ADR-017](../../adr/017-git-read-tool.md) records why the tool is separate from `git`.
[Configuration](../../configuration.md#read-only-git-git_read) names its limits.

_Anchors:_ src/plugins/git_read.rs::GitReadPlugin, src/plugins/git_read/runner.rs::ReadRepo, src/config.rs::GitReadConfig
