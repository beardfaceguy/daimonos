# Handoff: Implement Agent Skills in Daimonos

Repositories reviewed:

- Zed: `/home/beardface/zed`
- Daimonos: `/home/beardface/work/daimonos`

## Executive summary

Zed implements skills as **discoverable, selectively loaded instruction packages**, following this layout:

```text
<skills-root>/<skill-name>/SKILL.md
```

It searches:

```text
/home/beardface/.agents/skills/<skill-name>/SKILL.md
<worktree>/.agents/skills/<skill-name>/SKILL.md
```

The important architecture is:

1. At startup or refresh, load only each skill's frontmatter metadata.
2. Put eligible names and descriptions in the system prompt as a catalog.
3. When the model decides a skill applies, it calls a `skill` tool.
4. The tool reads the body from disk on demand and inserts it into the conversation.
5. Users may bypass model selection with `/skill-name`.
6. Skills with `disable-model-invocation: true` are excluded from the model catalog but remain manually invocable.

This avoids putting every skill body in every request.

## Skill file format

```markdown
---
name: vik
description: Search for and load a Vikunja project.
disable-model-invocation: true
---

# Load a Vikunja Project

Detailed instructions go here.
```

Metadata:

- `name`: required
- `description`: required
- `disable-model-invocation`: optional, default false

Name validation:

- Nonempty
- At most 64 bytes
- ASCII lowercase letters, digits, and hyphens
- Cannot begin or end with a hyphen

Description behavior:

- Must not be blank
- Recommended maximum of 1024 bytes
- Creation/import enforces the maximum strictly
- Discovery loads longer descriptions with a warning

Whole skill files are capped at 100 KB.

# Zed architecture

## 1. Core skill model and parser

Central implementation:

```text
/home/beardface/zed/crates/agent_skills/agent_skills.rs
```

Responsibilities:

- `Skill`, `SkillSummary`, `SkillMetadata`, and `SkillSource`
- YAML frontmatter parsing
- Name and description validation and slugification
- One-level directory discovery
- Frontmatter/body separation
- On-demand body reading
- Source precedence and overrides
- Built-in skills
- Share-link encoding/decoding
- `.agents/skills` path classification
- Unit tests

Package and workspace manifests:

```text
/home/beardface/zed/crates/agent_skills/Cargo.toml
/home/beardface/zed/Cargo.toml
```

`load_skills_from_directory` scans exactly one level:

```text
<root>/<immediate-child>/SKILL.md
```

Thus `.agents/skills/foo/SKILL.md` is valid, but `.agents/skills/group/foo/SKILL.md` is not discovered. Results are path-sorted for deterministic conflict resolution.

During discovery, Zed retains metadata and paths but discards the body. `read_skill_body` reparses it only on activation, keeping memory proportional to total frontmatter rather than total file size.

## 2. Sources and precedence

`SkillSource` supports:

1. Built-in
2. Global
3. Project-local

Global skills live at `~/.agents/skills`; project skills live at `<worktree>/.agents/skills`.

Project-local skills override same-named global skills in the **model-facing catalog**. Zed nevertheless retains the complete list for UI completion and explicit source selection.

Recommended Daimonos precedence:

```text
project-local > global > built-in
```

Resolve ties deterministically and expose ambiguity rather than relying on filesystem order.

## 3. Discovery and project-context assembly

Primary orchestration:

```text
/home/beardface/zed/crates/agent/src/agent.rs
```

It handles global scans and watching, global/project loading, worktree trust, errors and warnings, source overrides, catalog budgets, full UI/manual lists, catalog summaries, and slash invocation.

The main assembly function is `build_project_context`.

### Trust boundary

Zed does not load project-local skills from untrusted worktrees. Repository skills may contain hostile descriptions or bodies. Daimonos should similarly treat global skills as user-owned while treating workspace-local skills as repository-controlled input. If it lacks a trust system, local loading should be explicit, configurable, or clearly governed by policy.

## 4. Global directory watching

The relevant methods in `agent.rs` are:

- `ensure_skills_scan_started`
- `run_skills_scan`
- `run_skills_watch`

Scanning is lazy and idempotent. If `~/.agents/skills` does not exist, state returns to idle so later interactions retry. If it exists, Zed watches it and refreshes project contexts after changes.

On Linux, Zed watches the root and each immediate skill directory because inotify is not recursive. It deliberately avoids recursive watching, matching one-level discovery and avoiding huge watch trees.

For an initial Daimonos implementation, rescanning at session startup is enough; watching can follow.

## 5. System-prompt catalog

Catalog context:

```text
/home/beardface/zed/crates/prompt_store/src/prompts.rs
```

Template:

```text
/home/beardface/zed/crates/agent/src/templates/system_prompt.hbs
```

Zed renders a compact catalog:

```xml
<available_skills>
  <skill>
    <name>...</name>
    <description>...</description>
    <location>...</location>
  </skill>
</available_skills>
```

The model is told to match a request to a description, call the `skill` tool, follow the returned instructions, and resolve support files relative to the skill directory. Bodies do not enter the system prompt.

Zed also limits total catalog-description size in `agent.rs`. Skills that do not fit are omitted with visible issues instead of disappearing silently.

Daimonos should deduplicate by precedence, exclude manually-only skills, sort deterministically, enforce a configurable character/token budget, and warn about omitted entries.

## 6. Model-driven activation

Tool implementation:

```text
/home/beardface/zed/crates/agent/src/tools/skill_tool.rs
/home/beardface/zed/crates/agent/src/tools.rs
```

The tool resolves a skill against the catalog, reads its body on demand, and returns a structured `<skill_content>` envelope. It integrates with Zed's permission system.

Recommended Daimonos tool:

```text
skill(name: string) -> skill body envelope
```

It should:

1. Resolve only against session-discovered eligible skills.
2. Reject arbitrary filesystem paths.
3. Reopen the exact stored `SKILL.md`.
4. Reparse and validate it.
5. Return its body with identifying metadata.
6. Resolve support-file paths relative to the skill directory.
7. Record activation in conversation/tool history.

Do not accept an arbitrary model-supplied location as a skill.

## 7. Manual slash activation

Slash parsing and dispatch are in:

```text
/home/beardface/zed/crates/agent/src/agent.rs
```

`send_skill_invocation` handles a command such as:

```text
/vik foo
```

Zed resolves `vik`, reads its body, wraps it in the same envelope as the tool, strips `/vik`, preserves `foo` and attachments, and sends the envelope followed by the remaining request.

Same-named sources can be qualified:

```text
/:skill-name
/<worktree>:skill-name
```

Empty scope denotes global. Unqualified names use source precedence. MCP prompts win over skills on unqualified name collisions.

A Daimonos MVP can start with exact unqualified lookup and deterministic precedence, then add scoped invocation later.

## 8. `disable-model-invocation`

A skill with:

```yaml
disable-model-invocation: true
```

is omitted from `<available_skills>` but remains available through explicit slash invocation. This is appropriate for side-effectful workflows, user-selected macros, migrated commands, and procedures that should not activate autonomously.

Daimonos should preserve this distinction.

## 9. Completion, mentions, and conversation state

Related Zed files:

```text
/home/beardface/zed/crates/acp_thread/src/mention.rs
/home/beardface/zed/crates/agent_ui/src/completion_provider.rs
/home/beardface/zed/crates/agent_ui/src/message_editor.rs
/home/beardface/zed/crates/agent_ui/src/mention_set.rs
/home/beardface/zed/crates/agent_ui/src/ui/mention_crease.rs
/home/beardface/zed/crates/agent_ui/src/conversation_view.rs
/home/beardface/zed/crates/agent_ui/src/conversation_view/thread_view.rs
/home/beardface/zed/crates/agent/src/thread.rs
```

Completion displays names, descriptions, sources, paths, and warnings. Duplicate names remain separately visible. Skill content has a distinct conversation representation so replay, compaction, and serialization retain the activated instructions.

A terminal-oriented Daimonos version can initially parse leading `/skill-name`, list skills in help, and report ambiguity/errors in plain text.

## 10. Permission and filesystem safety

Relevant files:

```text
/home/beardface/zed/crates/agent/src/tools/skill_tool.rs
/home/beardface/zed/crates/agent/src/tools/tool_permissions.rs
/home/beardface/zed/crates/agent/src/tools/read_file_tool.rs
/home/beardface/zed/crates/agent/src/tools/list_directory_tool.rs
/home/beardface/zed/crates/agent/src/tools/write_file_tool.rs
/home/beardface/zed/crates/agent/src/tools/edit_file_tool.rs
/home/beardface/zed/crates/agent/src/tools/delete_path_tool.rs
/home/beardface/zed/crates/agent/src/tools/move_path_tool.rs
/home/beardface/zed/crates/agent/src/tools/copy_path_tool.rs
/home/beardface/zed/crates/agent/src/tools/create_directory_tool.rs
```

`is_agents_skills_path` in `agent_skills.rs` detects paths under `.agents/skills`. Writes there are handled conservatively because they change future agent behavior.

For Daimonos:

- Skill lookup must not become arbitrary file read.
- Support files should remain inside the skill directory unless ordinary filesystem permissions explicitly allow otherwise.
- Writes to skill directories should require normal write approval.
- Discovery alone must not execute project-local bodies.

## 11. Built-in skills

Zed embeds a built-in skill:

```text
/home/beardface/zed/crates/agent_skills/builtin/create-skill/SKILL.md
```

Built-ins use synthetic paths such as `<built-in>/create-skill/SKILL.md` and static embedded bodies. Daimonos does not need built-ins for an MVP, but this is useful for eventually shipping an official skill-creation workflow.

## 12. Creation, installation, sharing, and migration

Settings and creation UI:

```text
/home/beardface/zed/crates/settings_ui/src/pages/skills_setup.rs
/home/beardface/zed/crates/settings_ui/src/pages/skill_creator.rs
/home/beardface/zed/crates/settings_ui/src/pages.rs
/home/beardface/zed/crates/settings_ui/src/settings_ui.rs
```

Deep-link handling:

```text
/home/beardface/zed/crates/zed/src/zed/open_listener.rs
```

Legacy migration:

```text
/home/beardface/zed/crates/prompt_store/src/rules_to_skills_migration.rs
/home/beardface/zed/crates/prompt_store/src/prompt_store.rs
```

Migrated explicit rules generally become global, manually-only skills.

Daimonos should **not** automatically turn `agent-instructions.md` into a skill. Instructions are always active; skills are selectively activated. Keep both layers separate.

# Suggested Daimonos implementation plan

## Phase 1: parser and discovery

Create:

```text
/home/beardface/work/daimonos/src/skills.rs
```

Suggested model:

```rust
struct SkillMetadata {
    name: String,
    description: String,
    disable_model_invocation: bool,
}

enum SkillSource {
    Global,
    ProjectLocal,
}

struct Skill {
    metadata: SkillMetadata,
    source: SkillSource,
    directory_path: PathBuf,
    skill_file_path: PathBuf,
}
```

Implement global/project directories, one-level discovery, YAML parsing, size limits, validation, deterministic sorting, precedence, and on-demand body reads. Use Zed-compatible paths and format so skills interoperate.

## Phase 2: catalog injection

Modify prompt assembly centered around:

```text
/home/beardface/work/daimonos/src/prompts.rs
```

Keep existing `agent-instructions.md` behavior. Add a compact catalog only when eligible skills exist; exclude manually-only skills and enforce a catalog budget.

## Phase 3: native `skill` tool

Integrate with Daimonos's tool registry/runtime. Accept only a name, resolve it against session state, read the body on demand, return a stable envelope, reject arbitrary paths, and retain the result in history.

## Phase 4: manual invocation

Before ordinary dispatch, recognize:

```text
/<skill-name> [remaining text]
```

Resolve against all discovered skills, including manually-only skills. Inject the body envelope followed by remaining text. Report ambiguity rather than guessing.

## Phase 5: quality of life

Later additions:

- Rescanning/watchers
- `/skills` listing
- Source-qualified invocation
- Malformed-skill warnings
- Built-in `create-skill`
- Import/share support

# Recommended acceptance tests

1. Discovers `~/.agents/skills/foo/SKILL.md`.
2. Discovers `<workspace>/.agents/skills/foo/SKILL.md`.
3. Does not recursively discover nested groups.
4. Project-local `foo` overrides global `foo` in the model catalog.
5. Both sources remain manually addressable if scoping is supported.
6. Invalid names are rejected.
7. Missing or malformed frontmatter is reported.
8. Oversized files are rejected before context loading.
9. Skill bodies are absent from the startup system prompt.
10. `skill("foo")` loads the body on demand.
11. Arbitrary paths cannot be supplied to the tool.
12. `disable-model-invocation: true` excludes a skill from the catalog.
13. That skill still works with `/foo`.
14. `/foo argument` preserves `argument` after the envelope.
15. Relative support files resolve from the skill directory.
16. Catalog size is bounded and omissions are reported.
17. Existing `agent-instructions.md` remains always-on and unchanged.
18. Workspace skills are not silently trusted if a trust mechanism exists.

# Full related-file list

## Core Zed implementation

```text
/home/beardface/zed/Cargo.toml
/home/beardface/zed/crates/agent_skills/Cargo.toml
/home/beardface/zed/crates/agent_skills/agent_skills.rs
/home/beardface/zed/crates/agent_skills/README.md
/home/beardface/zed/crates/agent_skills/builtin/create-skill/SKILL.md
```

## Agent discovery, activation, and catalog

```text
/home/beardface/zed/crates/agent/src/agent.rs
/home/beardface/zed/crates/agent/src/tools.rs
/home/beardface/zed/crates/agent/src/tools/skill_tool.rs
/home/beardface/zed/crates/agent/src/thread.rs
/home/beardface/zed/crates/agent/src/templates/system_prompt.hbs
/home/beardface/zed/crates/prompt_store/src/prompts.rs
```

## Permissions and filesystem operations

```text
/home/beardface/zed/crates/agent/src/tools/tool_permissions.rs
/home/beardface/zed/crates/agent/src/tools/read_file_tool.rs
/home/beardface/zed/crates/agent/src/tools/list_directory_tool.rs
/home/beardface/zed/crates/agent/src/tools/write_file_tool.rs
/home/beardface/zed/crates/agent/src/tools/edit_file_tool.rs
/home/beardface/zed/crates/agent/src/tools/delete_path_tool.rs
/home/beardface/zed/crates/agent/src/tools/move_path_tool.rs
/home/beardface/zed/crates/agent/src/tools/copy_path_tool.rs
/home/beardface/zed/crates/agent/src/tools/create_directory_tool.rs
```

## Slash completion, mentions, and UI

```text
/home/beardface/zed/crates/acp_thread/src/mention.rs
/home/beardface/zed/crates/agent_ui/src/completion_provider.rs
/home/beardface/zed/crates/agent_ui/src/message_editor.rs
/home/beardface/zed/crates/agent_ui/src/mention_set.rs
/home/beardface/zed/crates/agent_ui/src/ui/mention_crease.rs
/home/beardface/zed/crates/agent_ui/src/conversation_view.rs
/home/beardface/zed/crates/agent_ui/src/conversation_view/thread_view.rs
/home/beardface/zed/crates/agent_ui/src/agent_ui.rs
```

## Settings, creation, installation, and sharing

```text
/home/beardface/zed/crates/settings_ui/src/pages/skills_setup.rs
/home/beardface/zed/crates/settings_ui/src/pages/skill_creator.rs
/home/beardface/zed/crates/settings_ui/src/pages.rs
/home/beardface/zed/crates/settings_ui/src/settings_ui.rs
/home/beardface/zed/crates/zed/src/zed/open_listener.rs
```

## Migration

```text
/home/beardface/zed/crates/prompt_store/src/rules_to_skills_migration.rs
/home/beardface/zed/crates/prompt_store/src/prompt_store.rs
/home/beardface/zed/crates/auto_update_ui/src/auto_update_ui.rs
```

## Existing Daimonos files likely to change

```text
/home/beardface/work/daimonos/src/prompts.rs
/home/beardface/work/daimonos/src/main.rs
/home/beardface/work/daimonos/src/config.rs
/home/beardface/work/daimonos/daimonos.default.toml
/home/beardface/work/daimonos/INSTALL.md
/home/beardface/work/daimonos/README.md
```

Before implementation, inspect Daimonos's tool registration and conversation dispatch to decide whether `skills.rs` should own activation or only parsing/discovery. The key design constraint is to keep **always-on instructions** and **on-demand skills** as separate context layers.
