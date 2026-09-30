# AGENTS.md

`AGENTS.md` support lets a host add repository instructions to agent prompts without baking those instructions into provider or UI code. Agent prompt middleware owns durable state and optional filesystem-read context; instruction file I/O goes through the private workspace worker. See [agent architecture](../architecture/agent.md#private-workspace-boundary).

## Entry Points

| API | Purpose |
| --- | --- |
| `AGENTS_INSTRUCTION_FILE` | Default instruction filename, `AGENTS.md`. |
| `AgentsPromptConfig` | Host-provided static instruction roots and optional filename override. |
| `AgentsInstruction` | Loaded instruction content and source path. |
| `AgentBuilder::with_agents_system_prompt` | Enables static instruction prompt contribution. |
| `FilesystemConfig::with_agents_read_context` | Enables dynamic reminders after filesystem reads. |

The agent crate exposes these APIs through `waltr_agent`, but does not automatically discover repository instructions unless a host opts in.

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
| [../../crates/waltr-agent/src/prompt.rs](../../crates/waltr-agent/src/prompt.rs) | Prompt and instruction configuration types. |
| [../../crates/waltr-agent/tests/agent/loop.rs](../../crates/waltr-agent/tests/agent/loop.rs) | Deterministic prompt and read-context behavior. |
| [../../crates/waltr-agent/tests/agent/session.rs](../../crates/waltr-agent/tests/agent/session.rs) | Durable middleware state and resume behavior. |
| [../../crates/waltr-agent/tests/live/agent/context.rs](../../crates/waltr-agent/tests/live/agent/context.rs) | Ignored live-provider context coverage. |

## References

| Path | Purpose |
| --- | --- |
| [../../crates/waltr-agent-sandbox/src/tools.rs](../../crates/waltr-agent-sandbox/src/tools.rs) | Worker workspace instruction reads and path resolution. |
| [../../crates/waltr-agent/src/middleware/agents.rs](../../crates/waltr-agent/src/middleware/agents.rs) | Static instruction middleware and filesystem read-context reminders. |
| [../../crates/waltr-agent/src/agent/builder.rs](../../crates/waltr-agent/src/agent/builder.rs) | Builder wiring and middleware ordering. |
| [../../crates/waltr-agent/src/tools/filesystem/config.rs](../../crates/waltr-agent/src/tools/filesystem/config.rs) | Filesystem read-context configuration. |
