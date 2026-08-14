# ADR 013: repowise is the KGL substrate; graphify is a manual fallback

_Date: 2026-08-13_

_Supersedes: [ADR 012](012-graphify-as-kgl-substrate.md)_

_Anchors: `src/kgl/substrate_repowise.rs::RepowiseSubstrate`, `src/kgl/autoindex.rs::detect`, `.git-hooks/_index-sync`_

## Status

Accepted.

## Context

ADR 012 kept graphify installed after #168 removed it as this repo's doc tool,
because `graphify-out/graph.json` had a second consumer: KGL's substrate. That
was explicitly an interim state, and 012 named its own end state as "a
repowise-backed substrate and no graphify at all".

#1291 built that substrate. `RepowiseSubstrate` reads `.repowise/wiki.db` and
produces a materially better graph than the one it replaces:

| | nodes | edges | calls |
|---|---|---|---|
| graphify | 6,080 | 15,216 | — |
| repowise | 16,070 | 33,703 | 13,152 |

plus precise start/end lines, qualified names, visibility and signatures.

Two things learned while building it changed the intended end state.

**The new substrate has the more fragile input contract.** `RepowiseSubstrate`
reads seven column names — `node_id`, `node_type`, `kind`, `name`, `file_path`,
`start_line`, `end_line` — directly out of repowise's private SQLite index.
That is not a published API and carries no compatibility promise. repowise
ships quickly; within a single day we found four fixes present in its git tree
and absent from the released wheel. A schema rename breaks KGL's indexing.

graphify's `graph.json`, by contrast, is networkx node-link — a documented
interchange format. The substrate producing the *better* graph depends on the
*weaker* contract, which is a reason to keep a second one rather than delete it.

**Auto-refreshing graphify is a recurring cost with no remaining benefit.**
Once KGL stopped reading `graph.json`, the hook was refreshing an artifact
nothing consumed: ~5s on every commit, 16 MB on disk, and a shrink guard that
refuses to rebuild whenever a commit deletes a public symbol — silently, into a
log file, until someone runs `graphify update . --force` by hand. That last one
fired the same day it was introduced.

## Decision

**repowise is the KGL substrate.** `detect` prefers it; `substrate:"repowise"`
is an explicit request path. Both keep the non-destructive guard: an index that
is missing, empty, or holds no parsed symbols must not be indexed, because
populate's prune reads zero nodes as "everything was deleted".

**graphify remains a supported substrate, and is no longer auto-refreshed.**
`substrate_graphify.rs`, the `detect` fallback, and the `substrate:"graphify"`
request path all stay. `.graphifyignore` stays. What goes is the graphify half
of `.git-hooks/_index-sync`.

Recovering the fallback is one command:

```sh
graphify update .      # then: kgl_query index, or substrate:"graphify"
```

This is a deliberate reversal of 012's stated end state. 012 assumed the
repowise substrate would make graphify redundant. It made graphify *unused*,
which is not the same thing: an unused fallback against an undocumented
upstream schema is worth keeping, and only its automation was worth removing.

## Consequences

- One index refreshes automatically. Per-commit hook cost drops to repowise
  alone, and the shrink-guard stall is gone.
- `graphify-out/` goes stale from now on. That is intended — it is a fallback,
  not a live index, and `graphify update .` is how you wake it.
- KGL's graph roughly triples in size and gains call edges, so `blast_radius`
  and `writers_of` return substantially more.
- The `Substrate` trait now carries three backends (repowise, graphify, x07),
  which is what it was for.

## Alternatives considered

- **Delete graphify entirely**, per 012's stated end state. Rejected: it leaves
  KGL with a single substrate whose input is another tool's private schema, and
  no recovery path short of writing new code.
- **Keep refreshing graphify in the hook.** Rejected: it pays a per-commit cost
  and a recurring manual unstick for an artifact nothing reads. A fallback does
  not need to be warm, only reachable.
