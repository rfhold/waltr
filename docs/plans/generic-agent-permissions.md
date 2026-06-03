# Generic Agent Permissions Plan

## Goal

Move filesystem root enforcement out of filesystem-specific tool code and into a generic permission system that can also support future tools, especially shell execution. The first permission gate should preserve current safety: filesystem paths outside the configured root are denied by default unless the caller explicitly allows or approves them.

## Requirements

- Support three permission levels: `allow`, `ask`, and `deny`.
- Support whole-tool permissions, such as blanket deny for write tools.
- Whole-tool denied tools must not be surfaced to the model.
- Support contextual permissions based on tool input, such as filesystem paths outside the configured root.
- Contextual denials must be returned as recoverable tool results so the model can retry.
- Support `allow once` and `allow always` decisions for asked permissions.
- `allow always` must add a concrete approved pattern, not a broad global bypass.
- Default filesystem configuration should register the configured root as an allowed pattern.
- Filesystem tools should describe permission-relevant resources generically so the agent core can handle permissions outside of filesystem code.
- The design should support future shell claims, including multiple commands and multiple working directories in one tool call.

## Core Concepts

### Permission Level

`PermissionLevel` describes the default policy for a tool or unmatched claim.

- `Allow`: execute without prompting.
- `Ask`: request a decision from the configured permission handler.
- `Deny`: deny with a message.

### Permission Decision

`PermissionDecision` is returned by the permission handler for `Ask` cases.

- `AllowOnce`: permit this specific tool call only.
- `AllowAlways(Vec<PermissionPattern>)`: permit this call and store reusable approved patterns for the current run/session.
- `Deny(String)`: deny with a caller-provided message.

Patterns returned by `AllowAlways` must be normalized and concrete. For filesystem use, examples include one file path or one path prefix, such as `/repo/shared/file.txt` or `/repo/shared/**`.

### Permission Operation

Use abstract operations instead of filesystem-specific verbs.

- `Read`
- `Write`
- `Delete`
- `Execute`
- `Network`

Initial filesystem mapping:

- `read`, `glob`, `grep`: `Read`
- `edit`, `write`, `apply_patch`: `Write`
- `delete`: `Delete`

Future shell mapping:

- shell command execution: `Execute`
- inferred file reads/writes/deletes can add extra claims later.

### Permission Resource

Use generic resources so the permission engine is not tied to filesystem tools.

Initial resource variants:

- `FilesystemPath(PathBuf)`
- `FilesystemPathPrefix(PathBuf)`
- `WorkingDirectory(PathBuf)`
- `ShellCommand(String)`

Future variants can cover network hosts, environment variables, credentials, or subprocess capabilities.

### Permission Claim

Each tool call produces zero or more claims before execution.

Fields:

- `operation`: abstract operation.
- `resource`: abstract resource.
- `summary`: human-readable text for ask/deny messages.
- optional `tool_name` or source metadata for diagnostics.

Examples:

- `read { filePath: "status.txt" }` produces `Read` on `FilesystemPath(/root/status.txt)`.
- `read { filePath: "../shared/notes.txt" }` produces `Read` on `FilesystemPath(/shared/notes.txt)`.
- `apply_patch` produces one claim per affected file, using `Write` for add/update and `Delete` for delete.
- future `shell { command: "cd .. && ls && cat foo" }` may produce claims for `WorkingDirectory(/parent)`, `ShellCommand("ls")`, `ShellCommand("cat foo")`, and later inferred `Read` on `/parent/foo`.

### Permission Pattern

Patterns are reusable approvals checked against claims.

Fields:

- operation matcher: exact operation or any operation.
- resource matcher: resource-specific pattern.

Initial resource matchers:

- filesystem path prefix.
- exact filesystem path if needed.
- working directory prefix.
- exact shell command.
- shell command prefix or pattern if needed later.

Default filesystem setup registers the configured root as allowed filesystem path prefixes for the operations enabled by the tool permissions. This preserves current behavior while making it generic.

## Agent Tool Metadata

Extend `AgentTool` with permission metadata separate from the provider-facing `ToolDefinition`.

Metadata should include:

- whole-tool permission level.
- default contextual permission level for unmatched claims.
- claim extraction callback or typed claim extractor.

Whole-tool permission is evaluated before surfacing tools:

- `Allow`: include tool definition.
- `Ask`: include only if an ask handler is configured, otherwise hide or deny by default.
- `Deny`: do not include the tool definition in requests.

Contextual permissions are evaluated after the model calls a surfaced tool and before the handler executes.

## Permission Engine Flow

1. Agent builds the tool list for a turn.
2. Tools denied at the whole-tool level are filtered out before sending the request to the provider.
3. Model emits a tool call.
4. Agent finds the matching `AgentTool`.
5. Tool extracts permission claims from the raw arguments.
6. Permission engine matches each claim against approved patterns.
7. Claims matched by existing patterns are allowed.
8. Unmatched claims use the tool/contextual permission level.
9. `Allow` permits execution.
10. `Deny` returns a recoverable tool error with the deny message.
11. `Ask` calls the permission handler.
12. `AllowOnce` permits this execution only.
13. `AllowAlways` stores returned patterns and permits this execution.
14. `Deny(message)` returns a recoverable tool error with the message.
15. If any claim is denied, the tool handler must not execute.
16. If all claims are allowed, execute the tool handler.

Denied tool results should include useful diagnostics:

- permission denial message.
- tool name.
- original arguments.
- relevant resolved resources.

## Filesystem Migration

Filesystem tools should keep path normalization and resolution helpers, but those helpers should classify paths instead of enforcing policy directly.

Path handling should produce normalized absolute paths wherever possible.

For existing paths:

- Resolve relative paths against the configured root.
- Canonicalize existing absolute paths.
- Produce a `FilesystemPath` claim.

For writable paths that may not exist:

- Resolve relative paths against the configured root.
- Canonicalize the nearest existing parent.
- Preserve the final target path.
- Produce a `FilesystemPath` claim.

For `apply_patch`:

- Parse the patch first.
- Extract all file operations before mutating files.
- Produce one claim per file operation.
- Only execute the patch after all claims are allowed.

The filesystem backend may still reject invalid paths or failed IO, but root escape should no longer be filesystem-specific policy. It should become an unmatched permission claim against allowed patterns.

## Default Filesystem Policy

When filesystem tools are enabled:

- Register the configured root as an allowed filesystem path prefix.
- Use `Ask` or `Deny` for unmatched filesystem claims depending on caller config.
- The initial safe default should be unmatched external filesystem access denied.

For interactive permission use:

- Set unmatched external filesystem access to `Ask`.
- A user approving `../shared` always should add an approved pattern for the normalized absolute target, such as `/repo/shared/**`.
- The approval should be scoped to operation where possible, for example read-only approval for `Read` claims.

## Whole-Tool Permissions

Whole-tool permissions are separate from filesystem profiles.

Filesystem profile decides which mutation tools are model-appropriate:

- exact edit profile.
- whole-file write profile.
- structured patch profile.

Whole-tool permissions decide whether those tools are exposed:

- blanket deny writes hides `edit`, `write`, `delete`, and `apply_patch`.
- blanket allow reads exposes `read`, `glob`, and `grep`.
- future shell blanket deny hides shell entirely.

This separation lets a caller configure read-only agents without changing provider/model profile logic.

## Ask Handler Shape

Start with a callback-based ask handler. This is the smallest API change and fits the current streaming model.

The handler receives a permission request containing:

- tool name.
- tool call id.
- original arguments.
- claims requiring a decision.
- human-readable summary.

The handler returns a `PermissionDecision`.

If no handler is configured and a claim requires `Ask`, deny by default with a clear message.

A future interactive UI can be built on top of this. If needed later, the agent stream can add a `PermissionRequest` event and pause/resume API, but that should not be part of the first implementation.

## Test Strategy

Avoid multiplying live LLM calls. Permission behavior is runtime behavior and should be covered mostly with deterministic tests.

### Test Layout

Keep `crates/waltr-agent/tests/agent.rs` as the integration test entrypoint and add:

- `crates/waltr-agent/tests/agent/permissions.rs`

Existing files remain:

- `tests/agent/loop.rs`
- `tests/agent/filesystem.rs`
- `tests/agent/session.rs`

### Deterministic Permission Test

Add one strong deterministic test using a scripted fake provider:

`filesystem_permissions_cover_default_allow_ask_allow_always_and_deny`

Temp directory setup:

- root directory:
  - `allowed.txt`
  - `nested/inside.txt`
- external sibling directory:
  - `external.txt`
  - `nested/external-nested.txt`
  - `nested/another.txt`
  - `denied.txt`

Scripted model flow:

1. Read `allowed.txt`.
2. Read `../outside/external.txt`.
3. Read `../outside/nested/external-nested.txt`.
4. Read `../outside/nested/another.txt`.
5. Read a denied external path.
6. Emit final text after observing tool results.

Permission handler script:

- In-root read should not call the handler.
- First external read returns `AllowOnce`.
- Second external nested read returns `AllowAlways` for the normalized nested directory prefix.
- Third external nested read should match the stored pattern and not call the handler.
- Denied external path returns `Deny("external access denied by test")`.

Assertions:

- default root allow works without handler invocation.
- ask with `AllowOnce` executes successfully.
- ask with `AllowAlways` stores and reuses a pattern.
- ask with `Deny(message)` returns a recoverable tool result containing the message.
- agent loop continues after denial.

### Focused Deterministic Tests

Add small focused tests only when they isolate a distinct rule better than adding another branch to the strong test.

Recommended focused tests:

- `blanket_denied_tools_are_not_surfaced`
  - deny filesystem write operations.
  - assert mutation tools are absent from `agent.tools()`.
  - assert read tools remain surfaced.
- `allow_always_pattern_is_reused`
  - test the permission engine directly without a provider.
  - claims under approved prefix pass.
  - claims outside prefix still require ask or deny.
- `multi_claim_denial_blocks_execution`
  - use a generic test tool with multiple extracted claims.
  - one claim allowed and one denied.
  - assert handler does not execute.
  - assert tool result contains denied claim/message.

### Filesystem Unit Tests

Keep filesystem unit tests focused on parsing, path normalization, and operation behavior.

Add cases there only for filesystem-specific claim extraction:

- `apply_patch` extracts all affected paths.
- `../` paths normalize to absolute external paths for claims.
- multiple patch operations produce multiple claims.

Do not test permission decisions in filesystem unit tests.

### Live Permission Test

Add exactly one ignored live permission test:

`live_permissions_agent_loop`

Default provider:

- `openai_responses_websocket`

Provider selector env:

- `WALTR_AGENT_LIVE_PROVIDER`

Supported values:

- `openai_responses_websocket`
- `openai_responses`
- `openai_chat_completions`
- `anthropic_messages`

Required API keys:

- OpenAI variants use `OPENAI_API_KEY`.
- Anthropic uses `ANTHROPIC_API_KEY`.

The live test should reuse the same temp root/external sibling shape as the deterministic test.

Live permission behavior:

- root allowed by default.
- external access starts as `Ask`.
- first external path returns `AllowOnce`.
- second external nested directory returns `AllowAlways`.
- third path under the nested directory should reuse the approved pattern.
- denied external path returns `Deny("external access denied by test")`.

Prompt should instruct the model to:

- read an in-root file.
- read one external file.
- read two files under an external nested directory.
- attempt a denied external file.
- report concise final status including the denial message.

Assertions:

- in-root content appears.
- allow-once external content appears.
- allow-always nested content appears.
- deny message appears.
- handler call count proves the `AllowAlways` pattern was reused.

Run examples:

```sh
cargo test -p waltr-agent --test agent live_permissions_agent_loop -- --ignored --nocapture
WALTR_AGENT_LIVE_PROVIDER=openai_responses cargo test -p waltr-agent --test agent live_permissions_agent_loop -- --ignored --nocapture
WALTR_AGENT_LIVE_PROVIDER=anthropic_messages cargo test -p waltr-agent --test agent live_permissions_agent_loop -- --ignored --nocapture
```

Invalid `WALTR_AGENT_LIVE_PROVIDER` values should fail fast with a clear message listing supported values.

## Implementation Phases

### Phase 1: Permission Types And Engine

- Add generic permission types.
- Add pattern matching for filesystem path prefixes.
- Add permission engine state with default allowed patterns.
- Add direct engine tests for pattern matching and `AllowAlways` reuse.

### Phase 2: Agent Tool Integration

- Add permission metadata to `AgentTool`.
- Add whole-tool filtering before provider requests.
- Add contextual claim checking before handler execution.
- Add ask handler support to `AgentBuilder` or `AgentRunBuilder`.
- Return denied permissions as recoverable tool results.

### Phase 3: Filesystem Claims

- Add filesystem claim extraction for all filesystem tool kinds.
- Replace hard-coded root escape policy with permission claims.
- Keep path normalization and IO validation in filesystem code.
- Ensure `apply_patch` checks every affected path before applying any mutation.

### Phase 4: Permission Tests

- Add deterministic `tests/agent/permissions.rs` coverage.
- Add focused permission engine and whole-tool surfacing tests.
- Add filesystem claim extraction unit tests.

### Phase 5: Live Test

- Add one ignored provider-selectable live permission test.
- Default to OpenAI Responses WebSocket.
- Keep all provider-specific selection behind `WALTR_AGENT_LIVE_PROVIDER`.

## Open Questions

- Should whole-tool `Ask` hide tools when no permission handler exists, or surface them and deny at execution time? The safer first behavior is to hide them unless a handler is configured.
- Should approved `AllowAlways` patterns be run-scoped only at first? Recommended: yes. Persistence should be explicit and owned by the caller.
- Should default filesystem patterns allow all operations under root, or should read/write/delete defaults be split? Recommended: split by operation internally, while preserving current behavior for enabled tools.
- How expressive should shell command patterns be initially? Recommended: exact and prefix patterns first; avoid full shell parsing policy in the first filesystem-focused implementation.
