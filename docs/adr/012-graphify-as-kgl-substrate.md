# ADR 012: graphify retained as a KGL substrate, not as a doc tool

_Date: 2026-08-13_

_Anchors: `src/kgl/substrate_graphify.rs::GraphifySubstrate`, `src/kgl/autoindex.rs::detect`, `.git-hooks/_index-sync`_

## Status

**Superseded by [ADR 013](013-repowise-as-kgl-substrate.md)** (2026-08-13).

The repowise substrate this ADR anticipated now exists, so graphify is no
longer what KGL indexes. 013 departs from the end state named below: graphify
is *retained* as a manual fallback rather than removed, because the new
substrate depends on repowise's private index schema and wants a second
source that does not. Only graphify's automatic refresh was removed.

## Context

graphify was removed in #168 and replaced with repowise for documentation,
search, and git signals. The removal deleted `graphify-out/`, and its
pre-commit hook — which had been silently skipping for over a week because it
gated on an untracked, machine-specific file absent from every worktree.

That removal was scoped to the wiki/search use case. It missed a second
consumer: `graphify-out/graph.json` is the substrate `GraphifySubstrate` reads
to build the KGL graph, and KGL is user-facing through the `kgl_query` and
`kgl_assert` MCP tools. Three live call sites depend on it —
`kgl/query.rs`, `kgl/autoindex.rs`, `kgl/store.rs`.

Deleting `graphify-out/` therefore disabled KGL re-indexing. It failed safely:
`query.rs` guards on `graphify_has_code_nodes` and refuses non-destructively
rather than indexing empty and letting prune wipe the existing graph and its
agent-declared metadata. But `.kgl/kgl.db` was frozen at its last build, with
no alternative substrate present (no `*.x07.json` exists in this repo), and
KGL — unlike repowise — does not report staleness.

The failure was invisible to CI: the crate compiles, tests pass, and the
degradation is a runtime path.

## Decision

Keep graphify installed **solely as a data producer for KGL**, and keep
`graphify-out/graph.json` regenerated. graphify no longer owns documentation,
search, wiki generation, or any committed artifact:

| Concern | Owner |
|---|---|
| docs, search, git signals, code health | repowise (`.repowise/`) |
| KGL substrate (`graph.json` only) | graphify (`graphify-out/`) |

Both output directories are gitignored. Nothing graphify produces is tracked —
that was the defect in the old pre-commit hook, which `git add`-ed a 6 MB
`graph.json` into every commit and made routine `git checkout` fail on a file
no human edits.

Regeneration moves into `.git-hooks/_index-sync`, called from post-commit,
post-merge and post-checkout. The full chain is:

```
source --(graphify update)--> graph.json --(KGL autoindex)--> kgl.db
```

KGL already owns the second arrow (startup autoindex plus a file watcher, gated
by `DAIMONOS_KGL_AUTOINDEX`, off by default). This ADR assigns the first arrow
to the hook. Measured cost is ~5s, backgrounded, and it cannot fail a git
operation.

`.graphifyignore` stays: it is load-bearing for substrate generation, not a
leftover.

## Consequences

- KGL keeps working, and its graph tracks the code again.
- graphify remains a real dependency. Anyone without it installed gets a stale
  substrate rather than an error — the hook self-skips when `graphify` is not
  on PATH.
- Two index directories exist where the #168 goal was one. This is a deliberate
  interim state, not the target.

## Intended end state

Replace `GraphifySubstrate` with a repowise-backed substrate. repowise's index
already carries the shape `substrate_graphify.rs` consumes — nodes plus
`calls`/`depends_on` edges from AST-level structure — so a `substrate_repowise.rs`
would restore KGL indexing and remove the last graphify dependency, collapsing
back to one index. Tracked separately; this ADR should be superseded when that
lands.

## Alternatives considered

- **Accept the loss.** KGL frozen at its last graph, drifting from the code with
  every commit. Rejected: `kgl_query`/`kgl_assert` are exposed MCP tools, and a
  silently-stale graph is the exact failure this repo has now hit twice.
- **Write the repowise substrate immediately.** Correct end state, but it is
  real work and it blocks KGL in the meantime. Deferred rather than declined.
