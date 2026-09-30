# Skills

Agent skills are reusable prompt/resource packages discovered from `.agents/skills/` directories and exposed to the provider through a visible catalog plus a synthetic `load_skill` tool. Agent middleware owns catalog state and rendering; the private worker owns workspace discovery, reads, and resource listing. Skills are not enabled by default. See [the private workspace boundary](../architecture/agent.md#private-workspace-boundary).

## Entry Points

| API | Purpose |
| --- | --- |
| `SkillsConfig` | Configures visible skill catalogs. |
| `SkillDefinition` | Parsed skill metadata and file locations. |
| `AgentBuilder::with_skills` | Enables skills middleware and the `load_skill` tool. |
| `FilesystemConfig::with_skills_context` | Enables dynamic skill reveal from filesystem reads/searches. |
| `SandboxHandle` | Worker-backed catalog, skill reads, and path discovery used by skills middleware. |

Constants:

| Constant | Value |
| --- | --- |
| `SKILLS_DIRECTORY` | `.agents/skills` |
| `SKILL_FILE` | `SKILL.md` |

## Discovery

Skill discovery scans immediate child directories under a skills directory. Each skill must contain `SKILL.md`.

The frontmatter parser currently requires simple `name` and `description` fields. Skill names are lowercase ASCII letters, digits, and hyphens with additional hyphen placement rules.

The worker discovers eligible skill directories and reads frontmatter; agent middleware records root, metadata, and paths for later `load_skill` use. Discovery does not execute files or load arbitrary resource contents.

## Prompt Catalog

When skills are enabled, middleware adds an available-skills catalog to the system prompt. The catalog tells the provider which skills are currently visible and instructs it to call `load_skill` before using a skill.

Visible catalogs come from:

- Roots configured through `SkillsConfig`.
- Dynamic reveal from filesystem context when `with_skills_context` is enabled.

The catalog is request context. Loaded skill content is returned as a tool result rather than being injected into future system prompts.

## `load_skill`

Skills middleware exposes a synthetic tool named `load_skill`.

Input shape:

| Field | Purpose |
| --- | --- |
| `name` | Skill name to load. |
| `root` | Optional root disambiguator when multiple visible catalogs contain the same name. |

The tool only resolves visible catalog entries. Worker reads return `SKILL.md` content and list resource paths under standard resource directories such as `scripts`, `references`, and `assets`; agent middleware formats and retains loaded content in durable middleware state.

Resource contents are not loaded automatically, and scripts are not executed by the skills system.

## Dynamic Reveal

When filesystem skills context is enabled, successful `read`, `glob`, or `grep` results can reveal nearby skill catalogs. The middleware discovers ancestor and tree-adjacent `.agents/skills` directories, then adds one-time reminders for newly visible skills.

Dynamic reveal depends on filesystem tools and is separate from static skills configured through `SkillsConfig`.

## Boundaries

- Skills are opt-in through `AgentBuilder::with_skills`.
- Dynamic reveal is separately opt-in through filesystem configuration.
- `load_skill` cannot load arbitrary paths; it can only load visible discovered skills.
- Skill resource paths are listed, not read automatically.
- Skill scripts are not executed by the skills implementation.
- Server agent definitions can enable skills; there is no separate public worker route, TUI, web, or CLI skills API.

## Tests

See [../testing.md](../testing.md) for canonical test commands.

Focused coverage:

| Path | Covers |
| --- | --- |
| [../../crates/waltr-agent-sandbox/src/tools.rs](../../crates/waltr-agent-sandbox/src/tools.rs) | Worker skill discovery and file I/O implementation and coverage. |
| [../../crates/waltr-agent/tests/agent/loop.rs](../../crates/waltr-agent/tests/agent/loop.rs) | Deterministic skills middleware and dynamic reveal behavior. |
| [../../crates/waltr-agent/tests/agent/session.rs](../../crates/waltr-agent/tests/agent/session.rs) | Durable loaded-skill and middleware state behavior. |
| [../../crates/waltr-agent/tests/live/agent/context.rs](../../crates/waltr-agent/tests/live/agent/context.rs) | Ignored live-provider context coverage. |
| [../../crates/waltr-agent/tests/live/agent/coding_agent.rs](../../crates/waltr-agent/tests/live/agent/coding_agent.rs) | Ignored live-provider coding-agent skill coverage. |

## References

| Path | Purpose |
| --- | --- |
| [../../crates/waltr-agent/src/tools/skills/config.rs](../../crates/waltr-agent/src/tools/skills/config.rs) | Skills configuration. |
| [../../crates/waltr-agent/src/tools/skills/definition.rs](../../crates/waltr-agent/src/tools/skills/definition.rs) | Skill frontmatter and name validation. |
| [../../crates/waltr-agent/src/tools/skills/render.rs](../../crates/waltr-agent/src/tools/skills/render.rs) | Visible skill catalog and reminder rendering. |
| [../../crates/waltr-agent/src/middleware/sandbox_skills.rs](../../crates/waltr-agent/src/middleware/sandbox_skills.rs) | Worker-backed skills middleware, `load_skill`, and dynamic reveal. |
