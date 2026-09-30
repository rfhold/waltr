# Filesystem Tools

Filesystem tools give an agent policy-controlled access to workspace files. `waltr-agent` owns provider-facing schemas and permission evaluation; `waltr-agent-sandbox` owns filesystem execution through the required private worker. Tools are opt-in through `AgentBuilder`; a default agent has no filesystem tools. See [the private workspace boundary](../architecture/agent.md#private-workspace-boundary).

## Entry Points

| API | Purpose |
| --- | --- |
| `AgentBuilder::with_filesystem_tools` | Enables filesystem tools for an agent. |
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

The configured root is used for default permission policy construction and provider-facing root hints; the connected worker reports the canonical root used by session runtime construction. Root hints are not a substitute for permission evaluation in the agent loop.

When filesystem tools are enabled and no explicit permission policy is supplied, the builder merges a default filesystem policy for the configured root. After the whole-tool gate, the worker validates the final tool arguments and returns claims and a single-use ticket. The agent evaluates claims after pre-tool middleware and before dispatch, recording the durable dispatch fence first. An uncertain dispatch is suspended rather than retried automatically.

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
- LSP diagnostics, formatting, and symbol tools are placeholder modules, not implemented filesystem tools.
- The server installs these tools from agent definitions, but there is no separate public worker API or OS isolation.

## Tests

See [../testing.md](../testing.md) for canonical test commands.

Focused coverage:

| Path | Covers |
| --- | --- |
| [../../crates/waltr-agent/src/tools/filesystem/mod.rs](../../crates/waltr-agent/src/tools/filesystem/mod.rs) | Schema and profile selection coverage. |
| [../../crates/waltr-agent-sandbox/src/tools.rs](../../crates/waltr-agent-sandbox/src/tools.rs) | Worker tool execution, bounded search, and snapshot coverage and implementation. |
| [../../crates/waltr-agent/src/agent/mod.rs](../../crates/waltr-agent/src/agent/mod.rs) | Agent builder opt-in behavior. |
| [../../crates/waltr-agent/tests/agent/loop.rs](../../crates/waltr-agent/tests/agent/loop.rs) | Context middleware and filesystem-backed loop behavior. |
| [../../crates/waltr-agent/tests/agent/session.rs](../../crates/waltr-agent/tests/agent/session.rs) | Request-scoped metadata and durable session behavior. |
| [../../crates/waltr-agent/tests/live/agent/filesystem.rs](../../crates/waltr-agent/tests/live/agent/filesystem.rs) | Ignored live-provider filesystem coverage. |

## References

| Path | Purpose |
| --- | --- |
| [../../crates/waltr-agent/src/tools/filesystem/config.rs](../../crates/waltr-agent/src/tools/filesystem/config.rs) | Filesystem tool configuration. |
| [../../crates/waltr-agent/src/tools/filesystem/profile.rs](../../crates/waltr-agent/src/tools/filesystem/profile.rs) | Tool profile selection. |
| [../../crates/waltr-agent-sandbox/src/patch.rs](../../crates/waltr-agent-sandbox/src/patch.rs) | Simplified patch parser and application. |
| [../../crates/waltr-agent/src/middleware/file_templates.rs](../../crates/waltr-agent/src/middleware/file_templates.rs) | Worker-backed file autocomplete and template reads. |
| [../../crates/waltr-agent/src/tools/sandbox.rs](../../crates/waltr-agent/src/tools/sandbox.rs) | Agent-side worker claims and approved tool execution. |
| [../../crates/waltr-agent/src/middleware/tool_metadata.rs](../../crates/waltr-agent/src/middleware/tool_metadata.rs) | Request-scoped root hints. |
