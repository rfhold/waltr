# System Prompt Construction Plan

## Goal

Add provider-neutral system prompt construction to `waltr-agent` so callers can explicitly opt into a reusable base prompt and optional context fragments.

The test harness should use a system prompt that tells live models they are running in a harness and must follow instructions exactly. Tool-specific prompt additions should be available but not automatic.

## Requirements

- Expose a reusable base system prompt from `waltr-agent`.
- Do not implicitly attach the base prompt to every request.
- Let callers explicitly opt into the base prompt.
- Let callers explicitly opt into filesystem prompt additions.
- Let callers explicitly opt into a filesystem snapshot.
- Let callers explicitly opt into shell prompt additions.
- Let callers explicitly opt into date context.
- Live agent tests should use a harness system prompt.
- Provider implementations should receive system prompts through `CompletionRequest`, not by concatenating text into the user prompt.

## Non-Goals

- Do not add a YAML or serde config layer in this pass.
- Do not make filesystem or shell tools automatically add system prompt text.
- Do not include filesystem snapshots by default.
- Do not make current date nondeterministic in tests.
- Do not move permission policy text into the system prompt in this pass.

## Public API Shape

Add a new prompt module and re-export the stable pieces:

```rust
pub mod prompt;

pub use prompt::{BASE_SYSTEM_PROMPT, SystemPromptBuilder};
```

Add provider-neutral system prompt support:

```rust
pub struct CompletionRequest<'a> {
    pub prompt: &'a str,
    pub system_prompt: Option<&'a str>,
    pub tools: Vec<ToolDefinition>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_results: Vec<ToolResult>,
}

impl<'a, P> CompletionRequestBuilder<'a, P>
where
    P: LlmStream + ?Sized,
{
    pub fn with_system_prompt(mut self, system_prompt: &'a str) -> Self;
}
```

Add explicit agent-level opt-ins:

```rust
impl AgentBuilder {
    pub fn with_base_system_prompt(self) -> Self;
    pub fn with_system_prompt(self, prompt: impl Into<String>) -> Self;
    pub fn with_system_date(self, date: impl Into<String>) -> Self;
    pub fn with_filesystem_system_prompt(self, config: FilesystemPromptConfig) -> Self;

    #[cfg(feature = "shell")]
    pub fn with_shell_system_prompt(self, config: ShellPromptConfig) -> Self;
}
```

Add run-level overrides or additions:

```rust
impl<'a, P> AgentRunBuilder<'a, P>
where
    P: LlmStream + ?Sized,
{
    pub fn with_system_prompt(self, prompt: impl Into<String>) -> Self;
    pub fn with_system_prompt_fragment(self, prompt: impl Into<String>) -> Self;
}
```

Run-level behavior should be simple:

- `with_system_prompt(...)` replaces the agent-assembled system prompt for that run.
- `with_system_prompt_fragment(...)` appends a fragment to the agent-assembled system prompt for that run.

## Prompt Builder

Use one small builder to keep ordering deterministic:

```rust
pub struct SystemPromptBuilder {
    fragments: Vec<String>,
}

impl SystemPromptBuilder {
    pub fn new() -> Self;
    pub fn with_base_prompt(self) -> Self;
    pub fn with_fragment(self, fragment: impl Into<String>) -> Self;
    pub fn with_date(self, date: impl Into<String>) -> Self;
    pub fn build(self) -> Option<String>;
}
```

Fragments should be joined with blank lines. Empty or whitespace-only fragments should be ignored.

Recommended order for agent-assembled prompts:

1. Base prompt.
2. Date.
3. Filesystem prompt.
4. Filesystem snapshot.
5. Shell prompt.
6. Custom caller fragments.
7. Test harness fragment.

The implementation does not need to encode semantic fragment types if call order is already deterministic in `AgentBuilder`; however, tests should assert the observed order.

## Base Prompt

Initial base prompt should stay short and provider-neutral:

```text
You are Waltr Agent, a careful coding and operations assistant. Follow the user's instructions, use available tools when they are helpful, and report results clearly. Treat tool errors as recoverable information and continue when possible.
```

The base prompt is exported for callers that want to inspect, reuse, or compose it manually.

## Date Prompt

Date should be explicit and deterministic:

```rust
AgentBuilder::with_system_date("2026-06-02")
```

Prompt fragment:

```text
Current date: 2026-06-02.
```

Avoid a current-date helper in the first pass unless it can be implemented without adding dependencies. Tests should not use wall-clock time.

## Filesystem Prompt

Filesystem prompt text should be explicit and separate from filesystem tool registration.

Tool registration remains:

```rust
AgentBuilder::with_filesystem_tools(FilesystemConfig::new(root))
```

Prompt registration is separate:

```rust
AgentBuilder::with_filesystem_system_prompt(
    FilesystemPromptConfig::new(root)
)
```

Suggested config:

```rust
pub struct FilesystemPromptConfig {
    pub root: PathBuf,
    pub include_snapshot: bool,
    pub max_snapshot_entries: usize,
    pub max_snapshot_depth: usize,
    pub max_snapshot_bytes: usize,
}
```

Suggested defaults:

- `include_snapshot`: `false`.
- `max_snapshot_entries`: `200`.
- `max_snapshot_depth`: `4`.
- `max_snapshot_bytes`: `16 * 1024`.

Builder methods:

```rust
impl FilesystemPromptConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self;
    pub fn with_snapshot(self) -> Self;
    pub fn with_max_snapshot_entries(self, entries: usize) -> Self;
    pub fn with_max_snapshot_depth(self, depth: usize) -> Self;
    pub fn with_max_snapshot_bytes(self, bytes: usize) -> Self;
}
```

Basic filesystem fragment:

```text
Filesystem tools are available when exposed in the tool list. Paths are scoped to this filesystem root: /path/to/root. Prefer relative paths under the root unless the user gives an absolute path inside the root.
```

Snapshot fragment:

```text
Current filesystem snapshot:
- Cargo.toml
- crates/
- crates/waltr-agent/
- crates/waltr-agent/src/
- crates/waltr-agent/src/lib.rs
```

Snapshot rules:

- Sort entries lexicographically for deterministic output.
- Use paths relative to the configured root.
- Add trailing `/` for directories.
- Do not include `.`.
- Do not follow symlinks outside the root.
- Stop once entry, depth, or byte limits are reached.
- Include a truncation marker if output is limited.

Filesystem tool descriptions can keep their existing concise root guidance. The system prompt adds broader behavior guidance only when explicitly requested.

## Shell Prompt

Shell prompt text should be explicit and separate from shell tool registration.

Tool registration remains:

```rust
AgentBuilder::with_shell_tool(ShellConfig::new(root))
```

Prompt registration is separate:

```rust
AgentBuilder::with_shell_system_prompt(ShellPromptConfig::new(root))
```

Suggested config:

```rust
pub struct ShellPromptConfig {
    pub root: PathBuf,
    pub default_workdir: PathBuf,
    pub os: String,
    pub shell_program: String,
    pub shell_invocation: String,
}
```

Suggested defaults:

- `os`: `std::env::consts::OS`.
- `shell_program`: `/bin/bash`.
- `shell_invocation`: `/bin/bash -lc`.
- `default_workdir`: root.

Prompt fragment:

```text
The shell tool executes commands on linux using /bin/bash -lc. The default working directory is /path/to/root. Relative workdirs and path-like command arguments resolve under root /path/to/root. Use Bash/POSIX shell syntax.
```

This text should be behind the existing `shell` Cargo feature if it depends on `ShellConfig` or shell-only exports.

## Test Harness Prompt

Add a shared test-only prompt constant:

```rust
const TEST_HARNESS_SYSTEM_PROMPT: &str = "...";
```

Suggested text:

```text
You are running inside an automated test harness. Follow the user instructions exactly. Use tools when the user explicitly asks you to use them. When asked to report exact values, include those values verbatim. Keep final answers concise and do not add extra commentary unless requested.
```

`AgentTestSession::run` should append this as a run-level system prompt fragment so live tests do not need to repeat harness instructions in user prompts.

Tests that exercise low-level provider streaming without `AgentTestSession` can opt in directly through `CompletionRequestBuilder::with_system_prompt(...)` only when needed.

## Provider Mapping

Providers should preserve system/user separation.

Anthropic Messages:

```json
{
  "system": "...",
  "messages": [
    { "role": "user", "content": "..." }
  ]
}
```

OpenAI Chat Completions:

```json
{
  "messages": [
    { "role": "system", "content": "..." },
    { "role": "user", "content": "..." }
  ]
}
```

OpenAI Responses HTTP and WebSocket:

```json
{
  "input": [
    {
      "type": "message",
      "role": "system",
      "content": [{ "type": "input_text", "text": "..." }]
    },
    {
      "type": "message",
      "role": "user",
      "content": [{ "type": "input_text", "text": "..." }]
    }
  ]
}
```

If the Responses API rejects `system`, use `developer` instead, but keep that provider-specific mapping contained in the OpenAI provider module.

## Implementation Steps

1. Add `src/prompt.rs` with `BASE_SYSTEM_PROMPT`, `SystemPromptBuilder`, and unit tests for fragment joining.
2. Re-export prompt APIs from `src/lib.rs`.
3. Add `system_prompt` to `CompletionRequest` and `CompletionRequestBuilder`.
4. Map `system_prompt` in Anthropic, OpenAI Chat Completions, OpenAI Responses HTTP, and OpenAI Responses WebSocket.
5. Add request-construction tests for each provider helper where possible.
6. Add agent-level system prompt storage and explicit builder methods.
7. Add run-level system prompt replacement and fragment append methods.
8. Add `FilesystemPromptConfig` and snapshot generation tests.
9. Add feature-gated `ShellPromptConfig` and shell fragment tests.
10. Update `AgentTestSession::run` to append `TEST_HARNESS_SYSTEM_PROMPT`.
11. Simplify live test user prompts where harness wording is now redundant.
12. Run default and shell-feature test/clippy verification.

## Verification

Run:

```sh
cargo fmt -p waltr-agent
cargo test -p waltr-agent
cargo test -p waltr-agent --features shell
cargo clippy -p waltr-agent --all-targets
cargo clippy -p waltr-agent --all-targets --features shell
```

Optional live checks after deterministic tests pass:

```sh
cargo test -p waltr-agent --features shell --test agent permissions::live_permissions_agent_loop -- --ignored --exact
```

## Open Decisions

- Whether OpenAI Responses should use `system` or `developer` for system prompt input after live/API validation.
- Whether to add a convenience `with_current_date()` helper later.
- Whether filesystem snapshots should skip common large directories such as `.git`, `target`, and `node_modules`, or remain a literal bounded snapshot.
