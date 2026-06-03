# AGENTS.md Support

## Goal

Add opt-in `AGENTS.md` instruction support to `waltr-agent`.

This has two parts:

- A system prompt helper that can load `AGENTS.md` content into the agent system context without requiring filesystem tools.
- Optional filesystem-tool integration that dynamically appends relevant nested `AGENTS.md` instructions when the `read` tool reads a file.

Dynamic loading is intentionally limited to `read` only. Other filesystem tools (`glob`, `grep`, `edit`, `write`, `delete`, and `apply_patch`) should not trigger `AGENTS.md` discovery in this version.

## Prior Art

opencode implements instruction loading in two paths:

- Static system prompt fragments are rendered as `Instructions from: <path>\n<content>`.
- Dynamic instruction discovery is wired to the read tool and appends newly loaded instructions to read output inside a `<system-reminder>` block.

Waltr should follow the same broad model, but only support `AGENTS.md` initially.

## Public API

### System Prompt Helper

Add a prompt helper in `crates/waltr-agent/src/prompt.rs` for rendering instruction files:

```rust
pub struct AgentsPromptConfig { ... }
```

Expected behavior:

- Default instruction filename is `AGENTS.md`.
- Load from a configured root or explicit path.
- Render each loaded file as:

```text
Instructions from: /absolute/path/to/AGENTS.md
<contents>
```

Add an agent builder method:

```rust
pub fn with_agents_system_prompt(mut self, config: AgentsPromptConfig) -> Self
```

This should append rendered instruction content to `SystemPromptBuilder`, like existing base/date/filesystem system prompt helpers.

### Filesystem Read Integration

Add opt-in configuration to filesystem tools:

```rust
impl FilesystemConfig {
    pub fn with_agents_read_context(mut self) -> Self
}
```

When enabled, only the `read` tool dynamically discovers and appends relevant `AGENTS.md` files.

## Static Loading Behavior

Static loading should not require filesystem tools.

Initial scope:

- Load `AGENTS.md` from the configured root when present.
- Canonicalize paths when possible.
- Include the source path in the rendered system prompt.
- If the file does not exist, render nothing.

Do not add support for `CLAUDE.md`, `CONTEXT.md`, remote URLs, or glob-configured instruction files in this change.

## Dynamic Read Behavior

When `FilesystemConfig::with_agents_read_context()` is enabled and the `read` tool reads a file:

1. Resolve and scope-check the target file using existing filesystem path helpers.
2. If the target path itself is `AGENTS.md`, do not append that file as dynamic context.
3. Start from the target file's parent directory.
4. Walk upward toward the filesystem tool root.
5. At each directory, check for `AGENTS.md`.
6. Skip any `AGENTS.md` already loaded into the static system prompt.
7. Skip any `AGENTS.md` already dynamically loaded earlier in the same agent run.
8. Append newly loaded instructions to the read output in a system reminder block:

```xml
<system-reminder>
Instructions from: /absolute/path/to/AGENTS.md
<contents>
</system-reminder>
```

Directory reads should not trigger dynamic instruction loading.

## State Tracking

Track dynamically loaded instruction paths in the agent loop, not in provider-specific code.

One minimal approach:

- Extend filesystem read tool execution so it can report loaded instruction paths alongside string content.
- Extend `AgentTool` or tool execution internals to return structured output from tools that need metadata.
- Store loaded `AGENTS.md` paths in `AgentLoopState` or a small shared context passed to tool handlers.

The implementation should avoid provider-specific metadata because Waltr currently stores tool results as plain text in `ToolResult`.

## Prompt Ordering

System prompt fragments should preserve existing ordering behavior:

1. Base system prompt, when enabled.
2. Date context, when enabled.
3. Filesystem system prompt, when enabled.
4. Static `AGENTS.md` prompt helper, based on builder call order.
5. User-provided system prompt fragments, based on builder/run call order.

Dynamic read-loaded instructions are not provider system messages. They are appended to the read tool result content as system reminders and therefore appear in the tool-result continuation turn.

## Tests

Add deterministic unit tests for:

- Static `AGENTS.md` helper renders `Instructions from: <path>` plus contents.
- Missing static `AGENTS.md` renders no prompt fragment.
- `AgentBuilder::with_agents_system_prompt(...)` includes static instructions in the provider request system prompt.
- `FilesystemConfig` does not load dynamic instructions unless explicitly opted in.
- Reading a file under a nested directory appends the nearest applicable `AGENTS.md` reminder.
- Reading a directory does not append dynamic instructions.
- Reading `AGENTS.md` directly does not append itself as a reminder.
- A dynamically loaded `AGENTS.md` is not appended again later in the same agent run.
- A statically loaded root `AGENTS.md` is not appended again dynamically.

## Non-Goals

- Do not wire dynamic loading to `glob`, `grep`, `edit`, `write`, `delete`, or `apply_patch`.
- Do not support `CLAUDE.md` or `CONTEXT.md`.
- Do not support remote instruction URLs.
- Do not add project-wide automatic loading unless the builder/helper is explicitly enabled.
- Do not change provider request formats for dynamic instructions.
