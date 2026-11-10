# AGENTS.md

The server adds repository instructions through concrete host middleware. The private workspace worker performs instruction file I/O.
See [the private workspace boundary](../architecture/agent.md#private-workspace-boundary).

## Entry Points

| API | Purpose |
| --- | --- |
| `AGENTS_INSTRUCTION_FILE` | Default instruction filename, `AGENTS.md`. |
| `AgentsPromptConfig` | Host-provided static instruction roots and optional filename override. |
| `AgentsInstruction` | Loaded instruction content and source path. |
| `HostRuntimeBuilder::with_agents_system_prompt` | Enables static instruction prompt contribution. |
| `FilesystemConfig::with_agents_read_context` | Enables dynamic reminders after filesystem reads. |

The server exposes these APIs through `waltr_server::agent_host`; instruction discovery requires explicit host configuration.

## Static Prompt Contribution

Static instruction loading is configured with `AgentsPromptConfig`. For each configured root, the middleware reads the configured instruction filename at that root and renders it into the system prompt as:

```text
Instructions from: <path>
<trimmed content>
```

Loaded instruction content is stored in middleware state so durable sessions replay the same prompt context on later turns and resumes. This avoids silently changing an in-progress session when an instruction file changes on disk.

Static loading is root-based and host-configured. It does not walk the repository tree looking for every `AGENTS.md` file.

## Filesystem Read Context

Filesystem read context is separate from static prompt contribution. When `FilesystemConfig::with_agents_read_context` is enabled, successful `read` tool calls can reveal nearby instruction files.

After a file read, the middleware walks from the read file's parent directory up to the configured root. Any not-yet-loaded `AGENTS.md` files found along that path are appended to the tool result as a `<system-reminder>` so the provider sees the relevant local instructions before continuing.

Read-context behavior is intentionally narrow:

- It only runs after successful `read` tool calls.
- It stays inside the configured root.
- It only loads regular instruction files.
- It skips dynamic injection when the user directly reads the instruction file itself.
- It records loaded instruction state to avoid duplicate reminders.

## Middleware Order

`AGENTS.md` middleware is assembled after the base system prompt and configured middleware, then before skills middleware. This keeps base prompt behavior deterministic while allowing skills to see the final instruction context that precedes them.

## Boundaries

- `AGENTS.md` behavior is library-builder and server agent-definition configuration, not a separate TUI, web, or public worker API.
- Static prompt content is captured for durable replay; there is no file watcher or automatic reload for already captured content.
- Dynamic context depends on the filesystem tool being enabled and configured with read context.
- The agent does not execute instructions from `AGENTS.md`; it contributes them as prompt context.

## Tests

See [../testing.md](../testing.md) for canonical test commands.

Focused coverage:

| Path | Covers |
| --- | --- |
| [../../crates/waltr-server/src/agent_host/prompt.rs](../../crates/waltr-server/src/agent_host/prompt.rs) | Prompt and instruction configuration types. |
| [../../crates/waltr-server/tests/agent_host/loop.rs](../../crates/waltr-server/tests/agent_host/loop.rs) | Prompt and read-context cases. |
| [../../crates/waltr-server/tests/agent_host/session.rs](../../crates/waltr-server/tests/agent_host/session.rs) | Durable host context cases. |
| [../../crates/waltr-server/tests/agent_host/live/context.rs](../../crates/waltr-server/tests/agent_host/live/context.rs) | Live-provider context cases. |

## References

| Path | Purpose |
| --- | --- |
| [../../crates/waltr-agent-sandbox/src/tools.rs](../../crates/waltr-agent-sandbox/src/tools.rs) | Worker workspace instruction reads and path resolution. |
| [../../crates/waltr-server/src/agent_host/middleware/agents.rs](../../crates/waltr-server/src/agent_host/middleware/agents.rs) | Instruction context and reminders. |
| [../../crates/waltr-server/src/agent_host/builder.rs](../../crates/waltr-server/src/agent_host/builder.rs) | Host assembly order. |
| [../../crates/waltr-server/src/agent_host/tools/filesystem/config.rs](../../crates/waltr-server/src/agent_host/tools/filesystem/config.rs) | Read-context configuration. |
