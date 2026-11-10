# Filesystem Tools

The server owns filesystem schemas and permission policy. `waltr-agent-sandbox` executes approved calls through the private worker.
`HostRuntimeBuilder` installs configured tools; a bare generic agent has none. See [the private workspace boundary](../architecture/agent.md#private-workspace-boundary).

## Entry Points

| API | Purpose |
| --- | --- |
| `HostRuntimeBuilder::with_filesystem_tools` | Enables filesystem capabilities. |
| `FilesystemConfig` | Configures the root path, tool profile, and optional context integrations. |
| `FilesystemToolProfile` | Chooses the write/edit tool shape exposed to the provider. |
| `SandboxHandle` | Required worker connection for preparing claims and executing approved tool calls. |

`FilesystemConfig` also controls optional `AGENTS.md` read context and skill reveal context. See [agents_md.md](agents_md.md) and [skills.md](skills.md).

## Tool Profiles

Every profile includes the shared `read`, `glob`, and `grep` discovery tools. Mutation surfaces are mutually exclusive by profile: structured patch models do not also receive standalone edit, write, or delete tools.

| Profile | Mutation tools |
| --- | --- |
| `ExactEdit` | `edit`, `write`, and `delete` |
| `WholeFileWrite` | `write` and `delete` |
| `StructuredPatch` | `apply_patch` only |
| `Auto` | Selects one of the concrete mutation surfaces from the model/provider shape. |

`Auto` currently favors structured patches for newer OpenAI `gpt-*` models except older `gpt-4*` and `oss` model names. Other models default toward exact edits.

## Tools

| Tool | Behavior |
| --- | --- |
| `read` | Reads files with line numbers or lists directories. Defaults to 2,000 lines and truncates very long lines. |
| `glob` | Recursively matches paths using glob patterns and returns sorted absolute paths; results beyond 1,024 (or the output byte cap) are explicitly marked truncated. |
| `grep` | Performs literal substring search over UTF-8 files, with an optional include glob. |
| `edit` | Replaces an exact string, requiring one match unless replace-all is requested. |
| `write` | Writes an entire file and creates parent directories as needed. |
| `delete` | Deletes files. |
| `apply_patch` | Applies the project-specific patch envelope with add, update, and delete sections. |

The patch format uses a `*** Begin Patch` / `*** End Patch` envelope and `*** Add File:`, `*** Update File:`, or `*** Delete File:` sections. Adds require `+`-prefixed lines; updates use `@@` chunks with context, `-` removals, and `+` additions. This is not a general unified diff parser.

## Permissions And Root Handling

The worker reports the canonical workspace root. The server uses it for root hints and policy; hints never substitute for authorization.

The host builder supplies the configured filesystem policy. After whole-tool admission and concrete middleware, the worker returns claims and a single-use ticket.
`CapabilityDispatcher` authorizes the final arguments and claims before Execute. The generic runner records the neutral host fence before it polls dispatch.
Uncertain effects never receive automatic replay. [Server permissions](permissions.md) define live approval behavior.

Practical implications:

- Read, write, edit, delete, and patch operations should be governed by `PermissionPolicy`.
- Root hints are added per provider request as tool metadata; they are not persisted inside tool definitions.
- Hosts that need stricter behavior should provide an explicit policy instead of relying on defaults.

## Context Integrations

Filesystem reads can reveal additional prompt context when explicitly configured:

| Integration | Configuration | Behavior |
| --- | --- | --- |
| `AGENTS.md` | `with_agents_read_context` | Adds newly discovered instruction files as system reminders after successful reads. |
| Skills | `with_skills_context` | Reveals newly discovered skill catalogs from `read`, `glob`, or `grep` results. |

These integrations update middleware state so the same discovered context is not repeatedly appended.

`FilesystemPromptConfig::with_snapshot()` separately opts into a worker-produced, durable prompt snapshot; it is off by default. The worker bounds traversal before replying (defaults: 200 entries, depth 4, 16 KiB; hard caps: 1,024 entries, depth 16, 64 KiB). The snapshot excludes hidden and gitignored paths, does not follow symlinks, and marks truncation when its byte budget permits. File-template autocomplete uses worker-ranked `search_files` with a bounded result set, not a host-side recursive workspace scan. `@file` references read through the worker; see [agent architecture](../architecture/agent.md#private-workspace-boundary).

## Boundaries

- Filesystem tools are not enabled by default.
- `grep` is literal substring search, not regex search.
- Binary, image, and PDF-specific read handling is not implemented.
- Ordinary worker search applies standard ignore filters, with a separate pass for nested hidden `.agents/skills` trees. Snapshot walks exclude hidden and gitignored paths and never follow symlinks.
- Filesystem tools do not support LSP diagnostics, formatting, or symbol operations.
- The server installs these tools from agent definitions, but there is no separate public worker API or OS isolation.

## Tests

See [../testing.md](../testing.md) for canonical test commands.

Focused coverage:

| Path | Covers |
| --- | --- |
| [../../crates/waltr-server/src/agent_host/tools/filesystem/mod.rs](../../crates/waltr-server/src/agent_host/tools/filesystem/mod.rs) | Schema and profile selection. |
| [../../crates/waltr-agent-sandbox/src/tools.rs](../../crates/waltr-agent-sandbox/src/tools.rs) | Worker tool execution, bounded search, and snapshot coverage and implementation. |
| [../../crates/waltr-server/src/agent_host/builder.rs](../../crates/waltr-server/src/agent_host/builder.rs) | Host opt-in assembly. |
| [../../crates/waltr-server/tests/agent_host/loop.rs](../../crates/waltr-server/tests/agent_host/loop.rs) | Context and filesystem cases. |
| [../../crates/waltr-server/tests/agent_host/session.rs](../../crates/waltr-server/tests/agent_host/session.rs) | Request metadata and durable context cases. |
| [../../crates/waltr-server/tests/agent_host/live/filesystem.rs](../../crates/waltr-server/tests/agent_host/live/filesystem.rs) | Live-provider filesystem cases. |

## References

| Path | Purpose |
| --- | --- |
| [../../crates/waltr-server/src/agent_host/tools/filesystem/config.rs](../../crates/waltr-server/src/agent_host/tools/filesystem/config.rs) | Filesystem configuration. |
| [../../crates/waltr-server/src/agent_host/tools/filesystem/profile.rs](../../crates/waltr-server/src/agent_host/tools/filesystem/profile.rs) | Profile selection. |
| [../../crates/waltr-agent-sandbox/src/patch.rs](../../crates/waltr-agent-sandbox/src/patch.rs) | Simplified patch parser and application. |
| [../../crates/waltr-server/src/agent_host/middleware/file_templates.rs](../../crates/waltr-server/src/agent_host/middleware/file_templates.rs) | Worker-backed templates. |
| [../../crates/waltr-server/src/agent_host/tools/sandbox.rs](../../crates/waltr-server/src/agent_host/tools/sandbox.rs) | Worker claim translation. |
| [../../crates/waltr-server/src/agent_host/middleware/tool_metadata.rs](../../crates/waltr-server/src/agent_host/middleware/tool_metadata.rs) | Request-scoped root hints. |
