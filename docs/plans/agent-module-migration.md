# Agent Module Migration

## Goal

Move all current `waltr-agent` production code into the module skeleton created by `agent-module-restructure.md`. Import churn is accepted when it improves long-term ownership boundaries. Public API compatibility should still be preserved through `mod.rs` re-exports unless a breaking path is explicitly chosen and documented.

## Current Baseline

- Skeleton modules already exist with TODO docs.
- Live-provider tests are isolated under `crates/waltr-agent/tests/live.rs` and `crates/waltr-agent/tests/live/`.
- Deterministic agent integration tests remain under `crates/waltr-agent/tests/agent.rs`.
- Verification has passed both default and shell-feature targets:
  - `cargo fmt -p waltr-agent -- --check`
  - `cargo test -p waltr-agent`
  - `cargo clippy -p waltr-agent --all-targets`
  - `cargo test -p waltr-agent --features shell`
  - `cargo clippy -p waltr-agent --features shell --all-targets`

## Migration Rules

- Prefer real module ownership over compatibility shims when choosing between the two.
- Preserve root-level `waltr_agent::{...}` re-exports for existing public types during the migration.
- Accept changed internal imports freely.
- Keep each wave behavior-neutral unless explicitly called out.
- Move tests only when the production move makes source-local coverage impossible or noisy.
- Do not combine high-risk modules in one change; each wave should compile, test, and clippy independently.
- Leave unrelated dirty worktree files untouched.

## Rust Module Conflict Strategy

Several targets currently have both a flat file and a desired directory. Rust cannot use `foo.rs` and `foo/mod.rs` as the same module at the same time. Resolve these conflicts by converting the flat file into `mod.rs` in the same wave that moves the code.

Recommended conversion pattern:

1. Move `src/foo.rs` to `src/foo/mod.rs`.
2. Keep public exports from `foo/mod.rs` stable with `pub use`.
3. Move coherent code chunks from `mod.rs` into child files.
4. Replace direct sibling references with `crate::foo::Type` or `super::Type` as appropriate.
5. Run verification before the next conversion.

Modules needing this conversion:

- `src/agent.rs` to `src/agent/mod.rs`
- `src/permission.rs` to `src/permission/mod.rs`
- `src/tools/filesystem.rs` to `src/tools/filesystem/mod.rs`
- `src/tools/shell.rs` to `src/tools/shell/mod.rs`
- `src/provider/anthropic.rs` to `src/provider/anthropic/mod.rs`

Existing directory modules that can be split in place:

- `src/mcp/mod.rs`
- `src/provider/openai/mod.rs`
- `src/provider/openai/chat.rs` after converting to `chat/mod.rs`
- `src/provider/openai/responses.rs` after converting to `responses/mod.rs`
- `src/tools/filesystem_skills.rs` can either remain as a compatibility shim or migrate into `src/tools/skills/mod.rs`

## Stage 1: Agent Core

Convert `src/agent.rs` into `src/agent/mod.rs`, then split the core runtime into focused files.

Target ownership:

| Target file | Move from current `agent.rs` |
| --- | --- |
| `agent/mod.rs` | Module docs, public re-exports, `Agent` type, small glue only. |
| `agent/builder.rs` | `AgentBuilder` fields and builder methods. |
| `agent/tool.rs` | `AgentTool`, handler type aliases, tool call/permission claim plumbing. |
| `agent/context.rs` | `AgentToolContext` and context construction/update helpers. |
| `agent/event.rs` | `AgentEvent` and event stream type alias. |
| `agent/state.rs` | `AgentState`, `AgentTranscriptEvent`, transcript helpers. |
| `agent/run.rs` | `AgentRunBuilder` and per-run override methods. |
| `agent/loop.rs` | `AgentLoopState`, loop phase enum, surfaced-tool filtering, tool execution, error result formatting. |

Execution order:

1. Move file root: `agent.rs` to `agent/mod.rs`.
2. Move passive types first: `event.rs`, `state.rs`.
3. Move `AgentTool` and `AgentToolContext` next.
4. Move `AgentBuilder` and update imports.
5. Move `AgentRunBuilder`.
6. Move loop internals last.
7. Keep `src/lib.rs` re-exporting the same public agent types.

Parallelization:

| Work item | Can delegate? | Notes |
| --- | --- | --- |
| Event/state extraction | Yes | Mostly passive type movement. |
| Tool/context extraction | Yes | Coordinate names used by builder and loop. |
| Builder/run extraction | Partly | One owner should reconcile imports. |
| Loop extraction | No | Depends on all previous moves. |

Acceptance criteria:

- `agent/mod.rs` is no longer a 1,700-line runtime file.
- Agent public API remains available from `waltr_agent::{Agent, AgentBuilder, AgentTool, ...}`.
- Default and shell-feature verification pass.

## Stage 2: Permission System

Convert `src/permission.rs` into `src/permission/mod.rs`, then move the model and engine pieces into child modules.

Target ownership:

| Target file | Move from current `permission.rs` |
| --- | --- |
| `permission/mod.rs` | Module docs and public re-exports. |
| `permission/level.rs` | `PermissionLevel`, `PermissionEffect`, conflict ranking. |
| `permission/claim.rs` | `PermissionOperation`, `PermissionResource`, `PermissionClaim`. |
| `permission/request.rs` | `PermissionCheckContext`, `PermissionRequest`. |
| `permission/handler.rs` | `PermissionDecision`, `PermissionHandler`, `SharedPermissionHandler`. |
| `permission/selector.rs` | `PermissionToolSelector` and selector matching. |
| `permission/rule.rs` | Tool, class, directory, and command rule structs/builders. |
| `permission/pattern.rs` | Path/command/resource reusable patterns. |
| `permission/policy.rs` | `PermissionPolicy`, policy construction, merge, fallback/defaults, rule evaluation entry points. |
| `permission/engine.rs` | `PermissionEngine`, reusable allow state, check execution, deny/error formatting. |

Execution order:

1. Move passive enums and structs: level, claim, request, handler.
2. Move selectors, rules, and patterns together or in a tightly coordinated sequence.
3. Move policy once rule/pattern imports are stable.
4. Move engine last.
5. Keep `src/lib.rs` re-exporting all existing permission types.

Parallelization:

| Work item | Can delegate? | Notes |
| --- | --- | --- |
| Level/claim/request/handler | Yes | Low-risk type movement. |
| Selector/rule/pattern | Partly | Keep matching behavior under one owner. |
| Policy | No | Central dependency surface. |
| Engine | No | Runtime behavior and reusable permissions. |

Acceptance criteria:

- Permission tests still cover selector specificity, conflict ranking, default filesystem policy, denial messages, and allow-always reuse.
- Permission public re-exports remain stable.

## Stage 3: Filesystem Tool

Convert `src/tools/filesystem.rs` into `src/tools/filesystem/mod.rs`, then split config, schema, operations, path handling, formatting, patching, permissions, and dynamic context.

Target ownership:

| Target file | Move from current `tools/filesystem.rs` |
| --- | --- |
| `tools/filesystem/mod.rs` | Public re-exports, `FilesystemConfig::into_agent_tools`, dispatch glue. |
| `tools/filesystem/config.rs` | `FilesystemConfig` and builder methods. |
| `tools/filesystem/profile.rs` | `FilesystemToolProfile`, `ResolvedFilesystemToolProfile`, tool set resolution. |
| `tools/filesystem/schema.rs` | `FilesystemToolKind`, tool names/descriptions/schemas, `ToolDefinition` construction. |
| `tools/filesystem/ops.rs` | Read/glob/grep/edit/write/delete execution APIs. |
| `tools/filesystem/path.rs` | Scoped path resolution, model path parsing, normalization, root escape errors. |
| `tools/filesystem/formatting.rs` | File/directory/grep/read output formatting, truncation helpers. |
| `tools/filesystem/patch.rs` | Apply-patch parser, operation model, application logic. |
| `tools/filesystem/permissions.rs` | Permission claim construction for all filesystem tools. |
| `tools/filesystem/context.rs` | AGENTS.md read reminders and skills context reminders. |
| `tools/filesystem/lsp_hints.rs` | Stays TODO-only until LSP integration exists. |

Execution order:

1. Move profile and schema first; these are mostly data/metadata.
2. Move path and formatting helpers.
3. Move patch parser/application as one unit.
4. Move permission claim generation after path/patch helpers are available.
5. Move operations and dynamic context last.
6. Keep `waltr_agent::tools::filesystem::{FilesystemConfig, ...}` stable through re-exports.

Parallelization:

| Work item | Can delegate? | Notes |
| --- | --- | --- |
| Profile/schema | Yes | Good first slice. |
| Path/formatting | Yes | Mostly pure helpers. |
| Patch | Yes | Self-contained once path helper API is clear. |
| Permissions | Partly | Depends on schema, path, and patch. |
| Ops/context/glue | No | Final integration point. |

Acceptance criteria:

- Filesystem unit tests remain equivalent.
- Agent integration tests continue to pass.
- `--features shell` still compiles, because shell may share permission/path expectations.

## Stage 4: Shell Tool

Convert `src/tools/shell.rs` into `src/tools/shell/mod.rs` under `#[cfg(feature = "shell")]`, then split parser, execution, permissions, path handling, formatting, and config.

Target ownership:

| Target file | Move from current `tools/shell.rs` |
| --- | --- |
| `tools/shell/mod.rs` | Public re-exports, `ShellConfig::into_agent_tool`, dispatch glue. |
| `tools/shell/config.rs` | `ShellConfig`, defaults, builder methods, input schema wiring. |
| `tools/shell/parser.rs` | Tree-sitter parser, command/token extraction, redirection/cwd detection. |
| `tools/shell/execute.rs` | Process spawning, timeout handling, stdout/stderr/status capture. |
| `tools/shell/permissions.rs` | Permission claim inference and reusable command patterns. |
| `tools/shell/path.rs` | Workdir/path resolution, home/model path handling, dynamic path detection. |
| `tools/shell/formatting.rs` | Shell result formatting and output truncation. |

Execution order:

1. Convert flat file to `tools/shell/mod.rs` without behavior changes.
2. Move parser and parser tests first.
3. Move path helpers used by parser/permissions.
4. Move execution and formatting.
5. Move permission inference.
6. Keep feature gates on the module declarations and tests.

Parallelization:

| Work item | Can delegate? | Notes |
| --- | --- | --- |
| Parser | Yes | Self-contained but feature-gated. |
| Execute/formatting | Yes | Coordinate output type ownership. |
| Path | Yes | Shared helper dependency. |
| Permissions | Partly | Depends on parser and path. |

Acceptance criteria:

- `cargo test -p waltr-agent --features shell` passes.
- `cargo clippy -p waltr-agent --features shell --all-targets` passes.
- Default feature build still excludes shell-only code.

## Stage 5: Skills Tool Migration

Migrate `src/tools/filesystem_skills.rs` into `src/tools/skills/` while keeping existing public paths as needed.

Target ownership:

| Target file | Move from current `filesystem_skills.rs` |
| --- | --- |
| `tools/skills/mod.rs` | Public re-exports and `load_skill` tool glue. |
| `tools/skills/config.rs` | `SkillsConfig`. |
| `tools/skills/definition.rs` | `SkillDefinition`, frontmatter parsing and validation model. |
| `tools/skills/discovery.rs` | Root/directory discovery and skill loading from disk. |
| `tools/skills/render.rs` | Catalog/reminder/rendered skill output. |
| `tools/skills/load_tool.rs` | Provider-facing `load_skill` input schema and execution. |
| `tools/skills/resources.rs` | Resource file collection and rendering. |

Compatibility options:

- Preferred: update internal imports to `tools::skills::*` and keep `tools::filesystem_skills` as a deprecated compatibility shim that re-exports from `tools::skills`.
- Acceptable: remove `filesystem_skills` if the project is ready for the public path change and document it.

Acceptance criteria:

- Existing skills unit tests pass.
- Agent skill prompt/load tests pass.
- `waltr_agent::{SkillDefinition, SkillsConfig}` root re-exports stay stable.

## Stage 6: MCP

Split `src/mcp/mod.rs` in place, since it already owns a directory module.

Target ownership:

| Target file | Move from current `mcp/mod.rs` |
| --- | --- |
| `mcp/mod.rs` | Public re-exports and minimal module glue. |
| `mcp/config.rs` | `McpConfig`, `McpServerConfig`, `McpTransportConfig`. |
| `mcp/runtime.rs` | `McpRuntime` and direct agent tool orchestration. |
| `mcp/server.rs` | `McpServer`, lifecycle, initialization, list/call tool cache. |
| `mcp/tool.rs` | MCP tool parsing, `AgentTool` wrapping, result formatting. |
| `mcp/protocol.rs` | JSON-RPC request/response/notification structs and protocol errors. |
| `mcp/name.rs` | Direct tool name formatting and sanitizer. |
| `mcp/transport/mod.rs` | Transport trait/enum glue. |
| `mcp/transport/stdio.rs` | Stdio process transport. |
| `mcp/transport/http.rs` | Streamable HTTP transport. |
| `mcp/transport/sse.rs` | SSE response parsing. |

Execution order:

1. Move config/name/tool-formatting first.
2. Move protocol and SSE helpers.
3. Move stdio and HTTP transports.
4. Move server lifecycle.
5. Move runtime orchestration last.

Parallelization:

| Work item | Can delegate? | Notes |
| --- | --- | --- |
| Config/name/tool | Yes | Low-risk API movement. |
| Protocol/SSE | Yes | Keep protocol structs private unless needed. |
| Stdio transport | Yes | Independent from HTTP transport. |
| HTTP transport | Yes | Independent from stdio transport. |
| Server/runtime | Partly | Do after protocol and transports. |

Acceptance criteria:

- Deterministic MCP integration test passes.
- Live MCP test remains ignored and discoverable under `tests/live.rs`.

## Stage 7: OpenAI Provider

Split OpenAI shared, Chat, and Responses code. This stage intentionally accepts import churn because provider internals need clearer boundaries for models.dev, usage, cost tracking, and middleware.

Target ownership:

| Target file | Move from current OpenAI files |
| --- | --- |
| `provider/openai/mod.rs` | Public re-exports, shared constants only if tiny. |
| `provider/openai/shared.rs` | HTTP SSE streaming, API error helper, shared tool schema conversion where not API-specific. |
| `provider/openai/reasoning.rs` | `OpenAiReasoningConfig`, effort, summary, request JSON helper. |
| `provider/openai/chat/mod.rs` | `OpenAiChatCompletions` public type and `LlmStream` impl. |
| `provider/openai/chat/request.rs` | Chat request body/message/tool encoding. |
| `provider/openai/chat/parser.rs` | Chat streaming parser and partial tool-call handling if Chat-specific. |
| `provider/openai/responses/mod.rs` | `OpenAiResponses` public type and `LlmStream` impl. |
| `provider/openai/responses/request.rs` | Responses body/input/tool-result encoding. |
| `provider/openai/responses/parser.rs` | `ResponsesParser`, event parsing, thinking/tool call extraction. |
| `provider/openai/responses/http.rs` | Responses HTTP provider transport glue if kept separate from public type. |
| `provider/openai/responses/websocket.rs` | `OpenAiResponsesWebSocket` public type and websocket event loop. |

Execution order:

1. Move reasoning config first.
2. Move shared HTTP/SSE helper and API error helper.
3. Convert `chat.rs` to `chat/mod.rs`, then move request/parser pieces.
4. Convert `responses.rs` to `responses/mod.rs`, then move request/parser pieces.
5. Move websocket provider under `responses/websocket.rs` and re-export `OpenAiResponsesWebSocket` from `provider/openai/mod.rs`.
6. Keep provider public re-exports from `provider/mod.rs` stable.

Parallelization:

| Work item | Can delegate? | Notes |
| --- | --- | --- |
| Reasoning | Yes | Low-risk passive types. |
| Chat split | Yes | Can proceed after shared helper ownership is clear. |
| Responses parser/request | Partly | Coordinate with websocket reuse. |
| Websocket move | Partly | Depends on Responses parser/request exports. |

Acceptance criteria:

- Provider unit tests pass.
- Live provider tests remain ignored and compile in `tests/live.rs`.
- Public constructors and root provider re-exports remain stable.

## Stage 8: Anthropic Provider

Convert `src/provider/anthropic.rs` into `src/provider/anthropic/mod.rs`, then split request encoding, stream transport, parser, and thinking support.

Target ownership:

| Target file | Move from current `anthropic.rs` |
| --- | --- |
| `provider/anthropic/mod.rs` | `AnthropicMessages` public type, `LlmStream` impl, public re-exports. |
| `provider/anthropic/request.rs` | Messages request body, tools, system prompt, thinking config request encoding. |
| `provider/anthropic/parser.rs` | SSE event parsing and Anthropic event to `CompletionEvent` mapping. |
| `provider/anthropic/stream.rs` | HTTP request execution and byte/SSE stream framing. |
| `provider/anthropic/thinking.rs` | `AnthropicThinkingConfig`, `AnthropicThinkingDisplay`, thinking block helpers. |

Important naming note:

- The child module `stream` currently collides with `futures_util::stream` if both are imported as `stream`. During this move, alias the futures import as `futures_stream` or stop importing it into `mod.rs`.

Parallelization:

| Work item | Can delegate? | Notes |
| --- | --- | --- |
| Thinking config | Yes | Passive types and helpers. |
| Request encoding | Yes | Mostly request JSON construction. |
| Parser | Yes | Self-contained event mapping. |
| Stream transport | Partly | Coordinate with parser return types. |

Acceptance criteria:

- Anthropic unit tests pass.
- Live Anthropic tests remain ignored and compile.

## Stage 9: Models And Middleware Preparation

After current code is moved, wire the new empty top-level modules for future implementation without adding broad abstractions prematurely.

Models target:

| Target file | Initial ownership |
| --- | --- |
| `models/mod.rs` | Public module docs and re-exports. |
| `models/catalog.rs` | Static/dynamic model catalog model. |
| `models/models_dev.rs` | models.dev loading/parsing. |
| `models/pricing.rs` | Pricing metadata. |
| `models/usage.rs` | Usage and cost accounting structs. |

Middleware target:

| Target file | Initial ownership |
| --- | --- |
| `middleware/mod.rs` | Public middleware docs and re-exports. |
| `middleware/chain.rs` | Middleware chain traits and ordering. |
| `middleware/context.rs` | Provider/tool/session context objects passed through middleware. |
| `middleware/cost.rs` | Cost tracking middleware once usage data exists. |
| `middleware/otel.rs` | OpenTelemetry middleware. |
| `middleware/secrets.rs` | Secret placeholder/redaction middleware. |

Rule:

- Do not implement middleware traits until the agent loop, provider request path, tool execution, and session boundaries have been split. The trait shape should be based on real call sites, not guessed abstractions.

## Stage 10: Session And Subagent Preparation

Move only code that exists today. Keep session/subagent files TODO-only until there is real runtime state or storage logic to own.

Future ownership:

| Target file | Future ownership |
| --- | --- |
| `agent/session/mod.rs` | Session public model and re-exports. |
| `agent/session/compression.rs` | Transcript compression policy and execution. |
| `agent/session/title.rs` | Auto-title generation. |
| `agent/session/storage/mod.rs` | Storage traits and capability model. |
| `agent/session/storage/filesystem.rs` | Filesystem backend. |
| `agent/session/storage/opendal.rs` | Feature-gated `OpenDAL` backend. |
| `agent/session/storage/s3.rs` | Feature-gated S3 backend if not covered by `OpenDAL`. |
| `agent/session/storage/scylla.rs` | Feature-gated Scylla backend. |
| `agent/subagent/sync.rs` | Synchronous subagent execution. |
| `agent/subagent/async.rs` | Asynchronous subagent execution. |
| `agent/subagent/message.rs` | Parent/child messaging protocol. |
| `agent/subagent/supervisor.rs` | Supervisor session orchestration. |

## Verification Matrix

Run after each completed wave:

```sh
cargo fmt -p waltr-agent -- --check
cargo test -p waltr-agent
cargo clippy -p waltr-agent --all-targets
```

Run after any wave touching shell, permission, tools, tests, or shared public exports:

```sh
cargo test -p waltr-agent --features shell
cargo clippy -p waltr-agent --features shell --all-targets
```

Optional live validation remains explicit:

```sh
cargo test -p waltr-agent --test live <test_name> -- --ignored
```

Use the required live-provider environment variables for live tests instead of enabling all ignored tests indiscriminately.

## Recommended Review Groups

| Group | Contents | Risk |
| --- | --- | --- |
| Agent core migration | `agent.rs` to `agent/*` | Medium |
| Permission migration | `permission.rs` to `permission/*` | Medium |
| Filesystem migration | `tools/filesystem.rs` to `tools/filesystem/*` | High |
| Shell migration | `tools/shell.rs` to `tools/shell/*` | Medium |
| Skills migration | `filesystem_skills.rs` to `tools/skills/*` | Medium |
| MCP migration | `mcp/mod.rs` to `mcp/*` | Medium |
| OpenAI migration | OpenAI provider split | Medium |
| Anthropic migration | Anthropic provider split | Medium |
| Middleware/models/session foundations | New abstractions after migration | High |

## Stop Conditions

Stop and reassess if any wave requires behavior changes outside import/module movement, exposes a real pre-existing bug, or forces a public API break. Fix discovered bugs when practical, but do not hide behavior changes inside a mechanical migration commit.
