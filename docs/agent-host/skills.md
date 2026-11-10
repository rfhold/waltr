# Skills

This document covers workspace skills and the shared `load_skill` interface. Remote MCP Skills use separate opt-in, origin labels, and manifest-bound consent; see [MCP Skills](mcp.md#mcp-skills).

Skills supply prompt/resource packages from `.agents/skills/` through a visible catalog and `load_skill`.
Server middleware owns catalog state and presentation; the private worker owns workspace discovery and reads. Skills require opt-in.
See [the private workspace boundary](../architecture/agent.md#private-workspace-boundary).

## Entry Points

| API | Purpose |
| --- | --- |
| `SkillsConfig` | Configures visible skill catalogs. |
| `SkillDefinition` | Parsed skill metadata and file locations. |
| `HostRuntimeBuilder::with_skills` | Enables Skills and `load_skill`. |
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

One `SkillsMiddleware` publishes the synthetic `load_skill` tool and routes calls to the local workspace or remote MCP adapter. Its schema is the same whether local skills, MCP Skills, or both are enabled. Every call requires an explicit `source`; a disabled source is rejected rather than inferred or substituted.

Input shape:

| Field | Purpose |
| --- | --- |
| `source` | Required selector: `local` or `mcp`. |
| `name` | Required for `source: "local"`: visible skill name to load. |
| `root` | Optional for `source: "local"`: root disambiguator when multiple visible catalogs contain the same name. |
| `server` | Required for `source: "mcp"`: configured MCP server key. |
| `uri` | Required for `source: "mcp"`: absolute skill URI ending in `/SKILL.md`. |

```json
{"source":"local","name":"inspect","root":"/workspace"}
```

```json
{"source":"mcp","server":"waltr","uri":"skill://resource-administration/SKILL.md"}
```

Local selectors cannot include `server` or `uri`; MCP selectors cannot include `name` or `root`. Unknown fields are rejected.

For `source: "local"`, the tool only resolves visible catalog entries. Worker reads return `SKILL.md` content and list resource paths under standard resource directories such as `scripts`, `references`, and `assets`; agent middleware formats and retains loaded content in durable middleware state.

For `source: "mcp"`, the tool fetches the exact remote manifest and requires explicit manifest-bound activation approval before revealing verified `SKILL.md`. Direct remote URIs need not appear in discovery. Supporting files use `read_mcp_resource` with `server` and the exact file `uri`, not another loader selector; this reads data and does not activate skills.

Resource contents are not loaded automatically, and scripts are not executed by the skills system.

## Dynamic Reveal

When filesystem skills context is enabled, successful `read`, `glob`, or `grep` results can reveal nearby skill catalogs. The middleware discovers ancestor and tree-adjacent `.agents/skills` directories, then adds one-time reminders for newly visible skills.

Dynamic reveal depends on filesystem tools and is separate from static skills configured through `SkillsConfig`.

## Boundaries

- Skills require `HostRuntimeBuilder::with_skills` or a normal server definition.
- Dynamic reveal is separately opt-in through filesystem configuration.
- `load_skill` with `source: "local"` cannot load arbitrary paths; it can only load visible discovered skills.
- Skill resource paths are listed, not read automatically.
- Skill scripts are not executed by the skills implementation.
- Server agent definitions can enable skills; there is no separate public worker route, TUI, web, or CLI skills API.

## Tests

See [../testing.md](../testing.md) for canonical test commands.

Focused coverage:

| Path | Covers |
| --- | --- |
| [../../crates/waltr-agent-sandbox/src/tools.rs](../../crates/waltr-agent-sandbox/src/tools.rs) | Worker skill discovery and file I/O implementation and coverage. |
| [../../crates/waltr-server/tests/agent_host/loop.rs](../../crates/waltr-server/tests/agent_host/loop.rs) | Skills and dynamic reveal cases. |
| [../../crates/waltr-server/tests/agent_host/session.rs](../../crates/waltr-server/tests/agent_host/session.rs) | Durable host context cases. |
| [../../crates/waltr-server/tests/agent_host/live/context.rs](../../crates/waltr-server/tests/agent_host/live/context.rs) | Live-provider context cases. |
| [../../crates/waltr-server/tests/agent_host/live/coding_agent.rs](../../crates/waltr-server/tests/agent_host/live/coding_agent.rs) | Live-provider Skills cases. |

## References

| Path | Purpose |
| --- | --- |
| [../../crates/waltr-server/src/agent_host/tools/skills/config.rs](../../crates/waltr-server/src/agent_host/tools/skills/config.rs) | Skills configuration. |
| [../../crates/waltr-server/src/agent_host/tools/skills/definition.rs](../../crates/waltr-server/src/agent_host/tools/skills/definition.rs) | Frontmatter and name validation. |
| [../../crates/waltr-server/src/agent_host/tools/skills/render.rs](../../crates/waltr-server/src/agent_host/tools/skills/render.rs) | Catalog and reminders. |
| [../../crates/waltr-server/src/agent_host/middleware/skills.rs](../../crates/waltr-server/src/agent_host/middleware/skills.rs) | Shared loader and source routes. |
| [../../crates/waltr-server/src/agent_host/middleware/sandbox_skills.rs](../../crates/waltr-server/src/agent_host/middleware/sandbox_skills.rs) | Local discovery and reveal. |
