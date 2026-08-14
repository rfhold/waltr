# Filesystem Tools

Filesystem tools give an agent controlled access to workspace files. They are implemented in `crates/waltr-agent/` and are opt-in through `AgentBuilder`; a default agent has no filesystem tools.

## Entry Points

| API | Purpose |
| --- | --- |
| `AgentBuilder::with_filesystem_tools` | Enables filesystem tools for an agent. |
| `FilesystemConfig` | Configures the root path, tool profile, and optional context integrations. |
| `FilesystemToolProfile` | Chooses the write/edit tool shape exposed to the provider. |
| `FilesystemTools` | Direct helper API used by tests and tool handlers. |

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
| `glob` | Recursively matches file paths using simple `*` and `**` patterns and returns sorted absolute paths. |
| `grep` | Performs literal substring search over UTF-8 files, with an optional include glob. |
| `edit` | Replaces an exact string, requiring one match unless replace-all is requested. |
| `write` | Writes an entire file and creates parent directories as needed. |
| `delete` | Deletes files. |
| `apply_patch` | Applies the project-specific patch envelope with add, update, and delete sections. |

The patch format is intentionally simpler than general unified diff semantics. It uses a `*** Begin Patch` envelope and file-oriented sections rather than arbitrary diff context.

## Permissions And Root Handling

The configured root is used for direct filesystem helpers, default permission policy construction, and provider-facing root hints. It is not a substitute for permission evaluation in the agent loop.

When filesystem tools are enabled and no explicit permission policy is supplied, the builder merges a default filesystem policy for the configured root. During tool execution, middleware turns requested filesystem operations into permission claims before the tool handler runs.

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

## Boundaries

- Filesystem tools are not enabled by default.
- `grep` is literal substring search, not regex search.
- Binary, image, and PDF-specific read handling is not implemented.
- `.gitignore` filtering is not implemented in the agent filesystem walker.
- LSP diagnostics, formatting, and symbol tools are placeholder modules, not implemented filesystem tools.
- The filesystem tools are agent-library features; there is no separate server or UI configuration surface for them yet.

## Tests

See [../testing.md](../testing.md) for canonical test commands.

Focused coverage:

| Path | Covers |
| --- | --- |
| [../../crates/waltr-agent/src/tools/filesystem/mod.rs](../../crates/waltr-agent/src/tools/filesystem/mod.rs) | Unit coverage for filesystem tool behavior. |
| [../../crates/waltr-agent/src/agent/mod.rs](../../crates/waltr-agent/src/agent/mod.rs) | Agent builder opt-in behavior. |
| [../../crates/waltr-agent/tests/agent/loop.rs](../../crates/waltr-agent/tests/agent/loop.rs) | Context middleware and filesystem-backed loop behavior. |
| [../../crates/waltr-agent/tests/agent/session.rs](../../crates/waltr-agent/tests/agent/session.rs) | Request-scoped metadata and durable session behavior. |
| [../../crates/waltr-agent/tests/live/agent/filesystem.rs](../../crates/waltr-agent/tests/live/agent/filesystem.rs) | Ignored live-provider filesystem coverage. |

## References

| Path | Purpose |
| --- | --- |
| [../../crates/waltr-agent/src/tools/filesystem/config.rs](../../crates/waltr-agent/src/tools/filesystem/config.rs) | Filesystem tool configuration. |
| [../../crates/waltr-agent/src/tools/filesystem/profile.rs](../../crates/waltr-agent/src/tools/filesystem/profile.rs) | Tool profile selection. |
| [../../crates/waltr-agent/src/tools/filesystem/ops.rs](../../crates/waltr-agent/src/tools/filesystem/ops.rs) | Filesystem operation implementation. |
| [../../crates/waltr-agent/src/tools/filesystem/patch.rs](../../crates/waltr-agent/src/tools/filesystem/patch.rs) | Simplified patch parser and application. |
| [../../crates/waltr-agent/src/tools/filesystem/permissions.rs](../../crates/waltr-agent/src/tools/filesystem/permissions.rs) | Filesystem permission claims and defaults. |
| [../../crates/waltr-agent/src/middleware/tool_metadata.rs](../../crates/waltr-agent/src/middleware/tool_metadata.rs) | Request-scoped root hints. |
