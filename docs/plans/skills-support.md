# Agent Skills Support

## Goal

Add opt-in Agent Skills support to `waltr-agent` following the progressive disclosure model from `agentskills.io`.

Skills support has three parts:

- A system prompt catalog that lists currently available skills by `name`, `description`, and root/location.
- A dedicated tool that loads full `SKILL.md` content for a selected skill.
- Filesystem integration that reveals additional skill catalogs when filesystem operations encounter new `.agents/skills` roots.

This should be implemented with its own module under filesystem tooling, not mixed into the existing `AGENTS.md` logic.

## Constraints

- Everything is opt-in and disabled by default.
- Root-level skills are scanned automatically when skills support is enabled.
- The root scan checks `<root>/.agents/skills`.
- Filesystem reads or directory walking can reveal additional `.agents/skills` roots under the configured filesystem root.
- No recursive scan under `.agents/skills`.
- A skill is discovered only from an immediate child directory containing `SKILL.md`:

```text
<some-root>/.agents/skills/<skill-dir>/SKILL.md
```

- Full skill content is loaded only through the dedicated skill-loading tool.
- Dynamic filesystem discovery reveals catalogs only; it must not eagerly load full `SKILL.md` content.
- If multiple visible skills share the same `name`, loading that skill requires a `root` parameter.
- Do not use override semantics for duplicate names. Keep all visible duplicates and require explicit disambiguation.
- Resource files are listed but not read eagerly.

## Module Layout

Add a dedicated filesystem skills module:

```text
crates/waltr-agent/src/tools/filesystem/skills.rs
```

If splitting the current single-file filesystem module is too large for this change, use:

```text
crates/waltr-agent/src/tools/filesystem_skills.rs
```

and keep the public API grouped under filesystem configuration. The preferred end state is a filesystem submodule because skills discovery is tied to filesystem roots, traversal, and read output.

Expected responsibilities for the skills module:

- Discover `.agents/skills` roots.
- Scan immediate child skill directories.
- Parse `SKILL.md` frontmatter.
- Validate required skill metadata.
- Build the visible skills catalog.
- Render system prompt catalog fragments.
- Render dynamic `<system-reminder>` catalog updates.
- Resolve skill names plus optional root into a single skill.
- Render loaded skill content and resource listings for the load tool.

## Skill Metadata

Each skill directory must contain exact `SKILL.md`.

`SKILL.md` must contain YAML frontmatter with at least:

```yaml
---
name: example-skill
description: Use when an example task needs skill-specific context.
---
```

Initial parser scope:

- Parse only the top YAML frontmatter block delimited by `---`.
- Require `name` and `description`.
- Ignore unknown frontmatter keys for now.
- Treat malformed frontmatter as a skipped skill rather than failing the whole catalog.
- Keep implementation minimal unless a YAML parser dependency is approved.

Validation:

- `name` is non-empty and should follow the Agent Skills naming convention: lowercase letters, numbers, and hyphens.
- `description` is non-empty.
- Prefer accepting valid-enough skills over strict enforcement initially, but tests should cover missing fields.

Core data shape:

```rust
pub struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub root: PathBuf,
    pub skills_dir: PathBuf,
    pub skill_dir: PathBuf,
    pub skill_path: PathBuf,
}
```

The `root` is the filesystem root that made the skill visible. It is the value the model must pass when disambiguating duplicate skill names.

## Public API

Add a skills configuration API alongside filesystem tooling:

```rust
pub struct SkillsConfig { ... }

impl SkillsConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self;
}
```

Add an agent builder method for static catalog and load-tool wiring:

```rust
pub fn with_skills(mut self, config: SkillsConfig) -> Self
```

Expected behavior:

- Discover `<root>/.agents/skills` immediately.
- Add a system prompt catalog if at least one skill exists.
- Add the dedicated load tool if at least one skill exists.
- Seed per-run tool context with initially visible skills.

Add opt-in filesystem dynamic discovery:

```rust
impl FilesystemConfig {
    pub fn with_skills_context(mut self) -> Self;
}
```

Expected behavior:

- Enables filesystem operations to reveal additional `.agents/skills` roots.
- Does not enable skills support by itself unless `AgentBuilder::with_skills(...)` is also used.
- Uses the same configured filesystem root for containment and discovery bounds.

## System Prompt Catalog

When skills are visible, append a catalog fragment to the system prompt.

Catalog requirements:

- Include a short explanation that skills provide task-specific instructions.
- Tell the model to call the skill loading tool before using a skill.
- List each available skill with `name`, `description`, and `root`.
- If a skill name is ambiguous, explicitly say `root` is required when loading it.

Example rendering:

```text
Available skills:

- example-skill: Use when an example task needs skill-specific context. Root: /repo

Use the load_skill tool to load a skill before following its instructions. If multiple skills share a name, pass the root shown in the catalog.
```

If no skills are visible, render nothing and do not expose the load tool.

## Load Skill Tool

Add a dedicated tool, likely named `load_skill`.

Input schema:

```json
{
  "type": "object",
  "properties": {
    "name": {
      "type": "string",
      "description": "Skill name from the available skills catalog"
    },
    "root": {
      "type": "string",
      "description": "Required when more than one visible skill has this name"
    }
  },
  "required": ["name"]
}
```

Resolution behavior:

- If no visible skill has `name`, return a clear tool error.
- If exactly one visible skill has `name`, load it.
- If multiple visible skills have `name` and `root` is missing, return a clear tool error listing valid roots.
- If `root` is provided, it must match one visible skill with that name.
- Loading the same `skill_path` more than once should return a short already-loaded message or the same content consistently; prefer tracking loaded paths to avoid repeated token-heavy output.

Output format:

```xml
<skill_content name="example-skill">
Skill directory: /repo/.agents/skills/example-skill

...full SKILL.md content...

<skill_resources>
- references/guide.md
- scripts/helper.sh
- assets/example.txt
</skill_resources>
</skill_content>
```

Resource listing behavior:

- List files under immediate optional resource directories: `scripts/`, `references/`, and `assets/`.
- Do not read resource file contents.
- Keep paths relative to the skill directory.
- Skip missing resource directories.

## Dynamic Filesystem Discovery

When `FilesystemConfig::with_skills_context()` is enabled, filesystem operations should check for `.agents/skills` roots that become relevant through filesystem access.

Discovery triggers:

- `read` of a file checks the file parent and ancestors up to the configured root.
- `read` of a directory checks that directory and ancestors up to the configured root.
- Directory walking operations should check directories they traverse for direct `.agents/skills` children.
- `glob` and `grep` should reveal discovered skills when their traversal encounters new `.agents/skills` roots, if practical with the existing implementation.

Discovery bounds:

- Never reveal skills outside the configured filesystem root.
- Canonicalize paths when possible.
- Track revealed `.agents/skills` roots per run to avoid duplicate reminders.
- Scan each discovered `.agents/skills` root one level deep only.

Dynamic reminder format:

```xml
<system-reminder>
New skills are available from /repo/subdir:

- subdir-skill: Use when working in the subdir package. Root: /repo/subdir

Use the load_skill tool to load a skill before following its instructions. If multiple skills share a name, pass the root shown in the catalog.
</system-reminder>
```

Dynamic reveal should also update the per-run visible skills index so `load_skill` can load newly revealed skills.

## Agent Context

Extend `AgentToolContext` with skills state:

```rust
pub struct AgentToolContext {
    pub agents_system_paths: HashSet<PathBuf>,
    pub agents_loaded_paths: HashSet<PathBuf>,
    pub visible_skills: Vec<SkillDefinition>,
    pub skills_revealed_roots: HashSet<PathBuf>,
    pub skills_loaded_paths: HashSet<PathBuf>,
}
```

The exact shape can vary, but it must support:

- Initial visible skills from `AgentBuilder::with_skills(...)`.
- Dynamic insertion of newly revealed skills during filesystem operations.
- Duplicate-name resolution across all currently visible skills.
- Loaded skill deduplication.

## Interaction With AGENTS.md

Keep AGENTS.md support separate from skills support.

- `AGENTS.md` static prompt loading stays in `prompt.rs`.
- `AGENTS.md` dynamic loading remains read-tool-only unless separately changed.
- Skills discovery and skill loading live in the dedicated filesystem skills module.
- The two features can both append `<system-reminder>` blocks to filesystem output when both are enabled.

When both reminders are needed, append them as separate blocks in deterministic order. Prefer AGENTS.md instructions first, then skills catalog updates.

## Tests

Add deterministic tests for static skills support:

- Root `<root>/.agents/skills` is scanned automatically when skills are enabled.
- No skills means no catalog and no load tool exposure.
- System prompt lists visible skills with descriptions and roots.
- Malformed or missing frontmatter skips the bad skill without failing all skills.

Add deterministic tests for `load_skill`:

- Unique skill name loads full `SKILL.md` content.
- Resource directories are listed but not read.
- Unknown skill returns a clear error.
- Duplicate names require `root`.
- Duplicate names with matching `root` load the intended skill.
- Repeated load of the same skill is deduplicated.

Add deterministic tests for dynamic filesystem discovery:

- Dynamic discovery is disabled by default.
- Dynamic discovery is opt-in via `FilesystemConfig::with_skills_context()`.
- Reading a file under a subtree reveals that subtree's `.agents/skills` catalog.
- Reading a directory can reveal that directory's `.agents/skills` catalog.
- Directory walking can reveal encountered `.agents/skills` roots.
- Newly revealed skills become loadable in the same run.
- Revealing an overlapping skill name causes `load_skill` to require `root`.
- A discovered `.agents/skills` root is revealed only once per run.
- Scanning under `.agents/skills` is not recursive.

## Verification

Run:

```sh
cargo fmt -p waltr-agent
cargo test -p waltr-agent skills
cargo test -p waltr-agent --lib
cargo test -p waltr-agent
cargo clippy -p waltr-agent --all-targets
```

If filesystem changes affect shell feature builds, also run:

```sh
cargo test -p waltr-agent --features shell
cargo clippy -p waltr-agent --all-targets --features shell
```

## Non-Goals

- Recursive skill scanning under `.agents/skills`.
- Loading remote skills.
- Reading skill resources eagerly.
- Enforcing all Agent Skills optional metadata fields.
- Implementing `allowed-tools` behavior.
- Persisting loaded skill state across agent runs.
