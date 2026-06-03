# Agent Module Restructure

## Goal

Restructure `waltr-agent` so the current core stays stable while future agent features have clear module homes. The first implementation pass should create the target module skeleton, add TODOs that describe ownership, isolate critical-path tests, and preserve public API compatibility through `mod.rs` re-exports.

## Drivers

- `src/agent.rs` currently mixes the public agent API, builder, tool abstraction, run loop, transcript state, context handling, and a large embedded test suite.
- `src/tools/filesystem.rs` currently mixes config, profiles, schemas, operations, path scoping, formatting, patch parsing, permission claims, AGENTS/skills context, and tests.
- `src/permission.rs` currently mixes the full policy model, selectors, patterns, claims, requests, handler API, reusable decisions, engine, and tests.
- `src/tools/shell.rs` currently mixes config, schema, command parsing, execution, permission inference, output formatting, path handling, and tests.
- `src/mcp/mod.rs` currently mixes public config, runtime, server lifecycle, transports, JSON-RPC, SSE parsing, tool wrapping, naming, and result formatting.
- Provider modules currently mix request encoding, streaming transport, parser logic, thinking/tool handling, provider config, and tests.

## Constraints

- Keep existing public paths stable where practical by re-exporting through `mod.rs` files.
- Accept temporary compatibility leakage through `mod.rs` if it moves the repo toward the desired long-term shape.
- Avoid behavior changes during the mechanical restructure.
- Keep critical-path tests intact, especially ignored live-provider coverage.
- Move tests only when that improves isolation without weakening assertions.
- Prefer small mechanical slices that can be formatted, tested, and reviewed independently.

## Target Layout

```text
src/
  agent/
    mod.rs
    builder.rs
    context.rs
    event.rs
    loop.rs
    run.rs
    state.rs
    tool.rs
    session/
      mod.rs
      compression.rs
      storage/
        mod.rs
        filesystem.rs
        opendal.rs
        s3.rs
        scylla.rs
      title.rs
    subagent/
      mod.rs
      async.rs
      message.rs
      supervisor.rs
      sync.rs
  middleware/
    mod.rs
    chain.rs
    context.rs
    cost.rs
    otel.rs
    secrets.rs
  models/
    mod.rs
    catalog.rs
    models_dev.rs
    pricing.rs
    usage.rs
  tools/
    mod.rs
    question.rs
    task.rs
    filesystem/
      mod.rs
      config.rs
      context.rs
      formatting.rs
      lsp_hints.rs
      ops.rs
      patch.rs
      path.rs
      permissions.rs
      profile.rs
      schema.rs
    shell/
      mod.rs
      config.rs
      execute.rs
      formatting.rs
      parser.rs
      path.rs
      permissions.rs
    lsp/
      mod.rs
      config.rs
      diagnostics.rs
      formatting.rs
      tools.rs
    skills/
      mod.rs
      config.rs
      definition.rs
      discovery.rs
      load_tool.rs
      render.rs
      resources.rs
  permission/
    mod.rs
    claim.rs
    engine.rs
    handler.rs
    level.rs
    pattern.rs
    policy.rs
    request.rs
    rule.rs
    selector.rs
  mcp/
    mod.rs
    config.rs
    name.rs
    protocol.rs
    runtime.rs
    server.rs
    tool.rs
    transport/
      mod.rs
      http.rs
      sse.rs
      stdio.rs
  provider/
    mod.rs
    anthropic/
      mod.rs
      request.rs
      parser.rs
      stream.rs
      thinking.rs
    openai/
      mod.rs
      shared.rs
      reasoning.rs
      chat/
        mod.rs
        request.rs
        parser.rs
      responses/
        mod.rs
        request.rs
        parser.rs
        http.rs
        websocket.rs
```

## Skeleton TODO Pattern

Every new placeholder module should contain a terse module-level TODO that defines ownership. Do not introduce speculative public APIs during the skeleton pass.

```rust
//! TODO: Move filesystem patch parsing/application here.
//!
//! This module should own the stripped-down apply_patch parser, patch operation
//! model, permission claim helpers for patches, and filesystem application logic.
```

## Stage 0: Baseline And Guardrails

This stage is mostly serial because it establishes the reference point for all later parallel work.

Tasks:

- Capture `git status` and note unrelated dirty files.
- Run `cargo fmt -p waltr-agent --check` if available, or `cargo fmt -p waltr-agent` before edits.
- Run `cargo test -p waltr-agent` to confirm deterministic baseline.
- Keep ignored live tests unchanged unless explicitly running them through the live-provider environment.
- Record any pre-existing failures before restructuring begins.

Parallel delegation:

| Work item | Can delegate? | Notes |
| --- | --- | --- |
| Baseline status and deterministic tests | No | One owner should establish the authoritative baseline. |
| Inventory current ignored live tests | Yes | Can be done in parallel with baseline test execution if it only reads files. |
| Identify source-local `#[cfg(test)]` blocks | Yes | Read-only inventory can be delegated. |

Acceptance criteria:

- Deterministic baseline is known.
- Live-provider tests that represent critical-path coverage are listed by file and test name.
- No code movement has happened yet.

## Stage 1: Create Skeleton Modules And TODOs

This stage can be highly parallel because files are mostly empty placeholders with module TODOs.

Tasks:

- Create the `agent/`, `middleware/`, `models/`, `tools/*/`, `permission/`, `mcp/`, and provider submodule skeletons.
- Add module declarations only where needed to compile or where they do not conflict with existing flat files.
- Prefer compatibility shims when an existing flat file name conflicts with a target directory name.
- Keep `lib.rs` exports stable.
- Avoid moving production logic in this stage unless required to make Rust module layout compile.

Parallel delegation:

| Group | Files | Dependencies | Can run in parallel? |
| --- | --- | --- | --- |
| Agent skeleton | `src/agent/*`, `src/agent/session/*`, `src/agent/subagent/*` | Needs a decision on replacing `src/agent.rs` with `src/agent/mod.rs` or deferring via shim | Yes, but one owner should handle the `agent.rs` conflict. |
| Tools skeleton | `src/tools/filesystem/*`, `src/tools/shell/*`, `src/tools/lsp/*`, `src/tools/skills/*`, `src/tools/task.rs` | Needs a decision on flat file conflicts for `filesystem.rs`, `shell.rs`, `filesystem_skills.rs` | Yes, with one owner per tool family. |
| Permission skeleton | `src/permission/*` | Needs a decision on replacing `src/permission.rs` with `src/permission/mod.rs` or deferring | Yes, but only one owner should touch permission module declarations. |
| MCP skeleton | `src/mcp/*`, `src/mcp/transport/*` | Existing `src/mcp/mod.rs` can declare new child modules immediately | Yes. |
| Provider skeleton | `src/provider/anthropic/*`, `src/provider/openai/*` | Conflicts with existing `anthropic.rs` and `openai/mod.rs` need one owner | Partly. OpenAI children are safer than Anthropic directory conversion. |
| Middleware and models skeleton | `src/middleware/*`, `src/models/*` | New top-level modules need `lib.rs` declarations only when intended public surface exists | Yes. |

Acceptance criteria:

- New skeleton files exist with TODOs that describe ownership.
- `cargo fmt -p waltr-agent` succeeds.
- `cargo test -p waltr-agent` still succeeds.
- Public API exports are unchanged unless explicitly documented.

## Stage 2: Isolate Critical Tests

This stage can be parallelized by source area, but each source file should have one owner to avoid conflicting edits.

Tasks:

- Move large source-local tests out of overloaded production modules where practical.
- Keep deterministic critical-path tests under `crates/waltr-agent/tests/` by area.
- Keep ignored live-provider tests explicit and easy to discover.
- Preserve current assertions rather than rewriting test intent during moves.
- Put shared helpers under `tests/support/` instead of duplicating across files.

Suggested test organization:

```text
tests/
  agent/
    context.rs
    loop.rs
    permissions.rs
    question.rs
    session.rs
  tools/
    filesystem.rs
    shell.rs
    skills.rs
  mcp/
    runtime.rs
    transport.rs
  provider/
    anthropic.rs
    openai_chat.rs
    openai_responses.rs
  live/
    agent.rs
    mcp.rs
    question.rs
  support/
    live_provider.rs
    temp.rs
```

Parallel delegation:

| Group | Source files | Target tests | Dependencies | Can run in parallel? |
| --- | --- | --- | --- | --- |
| Agent tests | `src/agent.rs` | `tests/agent/*.rs`, `tests/live/agent.rs` | Needs access to private internals or public-only rewrites | Yes, but only one owner should edit `src/agent.rs`. |
| Filesystem tests | `src/tools/filesystem.rs` | `tests/tools/filesystem.rs` | May need helper exposure or test-only utilities | Yes. |
| Shell tests | `src/tools/shell.rs` | `tests/tools/shell.rs` | Feature-gated shell tests must preserve cfg behavior | Yes. |
| Permission tests | `src/permission.rs` | `tests/agent/permissions.rs` or `tests/permission/*.rs` | Private helper coverage may need public behavior assertions | Yes. |
| MCP tests | `src/mcp/mod.rs`, existing `tests/agent/mcp.rs` | `tests/mcp/*.rs`, `tests/live/mcp.rs` | Avoid changing live-provider semantics | Yes. |
| Provider tests | provider modules | `tests/provider/*.rs` | Private parser tests may stay source-local until parser modules split | Yes. |

Acceptance criteria:

- Critical-path live tests remain ignored by default and are easy to run intentionally.
- Deterministic test coverage remains at least equivalent.
- `cargo test -p waltr-agent` succeeds.
- Test movement does not require behavior changes.

## Stage 3: Mechanical Production Code Moves

This stage should happen in waves. Each wave should compile and pass deterministic tests before the next wave starts.

Wave 3A: Agent Core

- Move `AgentTool` and `AgentToolContext` into `agent/tool.rs` and `agent/context.rs`.
- Move `AgentEvent`, `AgentState`, and `AgentTranscriptEvent` into `agent/event.rs` and `agent/state.rs`.
- Move `AgentBuilder` into `agent/builder.rs`.
- Move `AgentRunBuilder` into `agent/run.rs`.
- Move `AgentLoopState` and loop-only helpers into `agent/loop.rs`.
- Keep `agent/mod.rs` as the stable public export surface.

Parallel delegation:

| Group | Can delegate? | Notes |
| --- | --- | --- |
| Tool/context extraction | Yes | Low conflict if it happens before loop extraction. |
| Event/state extraction | Yes | Low conflict and mostly type movement. |
| Builder/run extraction | Partly | Builder and run builder share imports and should be coordinated. |
| Loop extraction | No | High dependency on moved types; do after the other agent slices. |

Wave 3B: Filesystem Tool

- Move config and profile types into `tools/filesystem/config.rs` and `tools/filesystem/profile.rs`.
- Move tool definitions and JSON schemas into `tools/filesystem/schema.rs`.
- Move file operations into `tools/filesystem/ops.rs`.
- Move path scoping and model path parsing into `tools/filesystem/path.rs`.
- Move output formatting into `tools/filesystem/formatting.rs`.
- Move patch parsing/application into `tools/filesystem/patch.rs`.
- Move permission claim construction into `tools/filesystem/permissions.rs`.
- Move AGENTS/skills dynamic context helpers into `tools/filesystem/context.rs`.

Parallel delegation:

| Group | Can delegate? | Notes |
| --- | --- | --- |
| Config/profile/schema | Yes | Good first filesystem slice. |
| Path/formatting helpers | Yes | Mostly pure helpers. |
| Patch module | Yes | Can be owned independently after path helper boundaries are clear. |
| Permission claims | Partly | Depends on path and patch helpers. |
| Operations/context | Partly | Depends on config, path, formatting, and skills context. |

Wave 3C: Permission System

- Move permission levels and effects into `permission/level.rs`.
- Move claims and resources into `permission/claim.rs`.
- Move permission requests and check context into `permission/request.rs`.
- Move handler types into `permission/handler.rs`.
- Move selectors, rules, and patterns into `permission/selector.rs`, `permission/rule.rs`, and `permission/pattern.rs`.
- Move policy assembly and merge logic into `permission/policy.rs`.
- Move check execution and reusable allow handling into `permission/engine.rs`.

Parallel delegation:

| Group | Can delegate? | Notes |
| --- | --- | --- |
| Level/claim/request/handler movement | Yes | Mostly type movement. |
| Selector/rule/pattern movement | Yes | Keep one owner for matching semantics. |
| Policy movement | Partly | Depends on rule and pattern modules. |
| Engine movement | No | Do last after all model types are in place. |

Wave 3D: Shell Tool

- Move shell config and schema wiring into `tools/shell/config.rs`.
- Move tree-sitter bash parsing into `tools/shell/parser.rs`.
- Move command execution into `tools/shell/execute.rs`.
- Move permission inference into `tools/shell/permissions.rs`.
- Move path handling into `tools/shell/path.rs`.
- Move output formatting into `tools/shell/formatting.rs`.

Parallel delegation:

| Group | Can delegate? | Notes |
| --- | --- | --- |
| Parser extraction | Yes | Self-contained but feature-gated. |
| Execute/formatting extraction | Yes | Coordinate output type ownership. |
| Path extraction | Yes | Shared dependency for parser and permissions. |
| Permission inference extraction | Partly | Depends on parser and path modules. |

Wave 3E: MCP

- Move public config into `mcp/config.rs`.
- Move tool wrapping and result formatting into `mcp/tool.rs`.
- Move server lifecycle into `mcp/server.rs`.
- Move runtime orchestration into `mcp/runtime.rs`.
- Move JSON-RPC structs/helpers into `mcp/protocol.rs`.
- Move direct tool naming into `mcp/name.rs`.
- Move stdio, HTTP, and SSE transport logic into `mcp/transport/*`.

Parallel delegation:

| Group | Can delegate? | Notes |
| --- | --- | --- |
| Config/name/tool formatting | Yes | Low conflict. |
| Protocol/SSE extraction | Yes | Protocol types should move before transports. |
| Stdio transport | Yes | Separate from HTTP transport. |
| HTTP transport | Yes | Separate from stdio transport. |
| Server/runtime extraction | Partly | Do after protocol and transports are in place. |

Wave 3F: Providers

- Split request builders from streaming parsers.
- Keep provider public constructors stable.
- Move OpenAI shared Responses logic into `provider/openai/shared.rs` and `provider/openai/responses/*`.
- Move OpenAI reasoning config into `provider/openai/reasoning.rs`.
- Move Chat request/parser logic into `provider/openai/chat/*`.
- Move Anthropic request/parser/stream/thinking logic into `provider/anthropic/*`.

Parallel delegation:

| Group | Can delegate? | Notes |
| --- | --- | --- |
| OpenAI Chat split | Yes | Isolated from Responses except shared tool encoding. |
| OpenAI Responses split | Partly | Coordinate shared parser and websocket reuse. |
| Anthropic split | Yes | Independent from OpenAI. |
| Shared provider abstractions | No | Avoid speculative abstractions until splits are complete. |

Acceptance criteria for every wave:

- `cargo fmt -p waltr-agent` succeeds.
- `cargo test -p waltr-agent` succeeds.
- `cargo clippy -p waltr-agent --all-targets` succeeds before merging a completed wave.
- Public API changes are documented if any path cannot be preserved.

## Stage 4: Future Feature Homes

These are not implementation tasks for the initial restructure. The skeleton should make them straightforward later.

| Feature | Target home | Notes |
| --- | --- | --- |
| Filesystem formatting | `tools/filesystem/formatting.rs` | Own model-facing file/directory/grep/patch output formatting. |
| Filesystem LSP hints | `tools/filesystem/lsp_hints.rs` | Bridge filesystem edits and LSP diagnostics/hints. |
| LSP tools | `tools/lsp/tools.rs` | Own provider-facing LSP tool definitions. |
| Full middleware system | `middleware/chain.rs`, `middleware/context.rs` | Intercept requests, events, tool calls, session events, and provider usage. |
| OTEL middleware | `middleware/otel.rs` | Emit traces/spans/metrics without coupling to providers or agent loop internals. |
| Models.dev configuration | `models/models_dev.rs`, `models/catalog.rs` | Resolve provider/model metadata before provider construction. |
| Cost tracking | `models/pricing.rs`, `models/usage.rs`, `middleware/cost.rs` | Track token usage and cost through middleware. |
| Secret placeholder middleware | `middleware/secrets.rs` | Redact or placeholder secrets across prompts, tool results, transcripts, and traces. |
| Session compression | `agent/session/compression.rs` | Own transcript compaction policy and model calls. |
| Session auto title | `agent/session/title.rs` | Own title generation from transcript/session metadata. |
| Session storage interface | `agent/session/storage/mod.rs` | Define persistence traits and capability model. |
| Filesystem storage | `agent/session/storage/filesystem.rs` | Default local persistence backend. |
| OpenDAL storage | `agent/session/storage/opendal.rs` | Feature-gated generic object storage backend. |
| S3 storage | `agent/session/storage/s3.rs` | Feature-gated S3-specific backend if not covered by OpenDAL. |
| Scylla storage | `agent/session/storage/scylla.rs` | Feature-gated database-backed session storage. |
| Synchronous subagents | `agent/subagent/sync.rs` | Blocking nested agent execution. |
| Asynchronous subagents | `agent/subagent/async.rs` | Background subagent execution. |
| Subagent messaging | `agent/subagent/message.rs` | Message protocol between parent, supervisor, and async subagents. |
| Task tracking tools/system | `tools/task.rs` | Provider-facing task state tools backed by session state. |
| Agent supervisor session | `agent/subagent/supervisor.rs` | Owns supervision policy, child sessions, messaging, and task coordination. |

## Stage 5: Middleware And Session Integration

This should wait until the mechanical module split is complete.

Tasks:

- Define middleware trait boundaries around agent runs, provider requests, provider events, tool calls, permission checks, session storage, and transcript updates.
- Add middleware context objects that avoid leaking provider-specific or tool-specific internals.
- Add session storage traits with feature-gated backend modules.
- Add session compression and title generation as opt-in services.
- Add task tracking and subagent supervision after session storage and middleware event boundaries are stable.

Parallel delegation:

| Group | Can delegate? | Notes |
| --- | --- | --- |
| Middleware trait design | No | Central architecture decision. |
| OTEL middleware | Yes | Can follow the middleware trait after it exists. |
| Secret placeholder middleware | Yes | Can follow the middleware trait after it exists. |
| Cost middleware and model usage | Yes | Needs model pricing metadata first. |
| Session storage backends | Yes | Backends can be separate once storage traits are fixed. |
| Session compression/title | Yes | Both consume session state but can be independent services. |
| Subagent/task/supervisor system | Partly | Needs session and middleware boundaries first. |

## Review Strategy

Keep review units small and mostly mechanical.

Recommended PR or commit grouping:

| Group | Contents | Risk |
| --- | --- | --- |
| Skeleton and TODOs | New files, module docs, minimal declarations | Low |
| Test isolation | Move tests and helpers, preserve assertions | Medium |
| Agent split | API/builder/tool/state/run loop movement | Medium |
| Filesystem split | Tool internals movement | High |
| Permission split | Policy/engine movement | Medium |
| Shell split | Parser/execution/permission movement | Medium |
| MCP split | Runtime/protocol/transport movement | Medium |
| Provider split | Request/parser/stream movement | Medium |
| Middleware/session foundations | New abstractions | High |

## Verification Commands

Run these after each completed stage and after each production movement wave.

```sh
cargo fmt -p waltr-agent
cargo test -p waltr-agent
cargo clippy -p waltr-agent --all-targets
```

Ignored live-provider tests should remain opt-in. When intentionally validating live behavior, run the specific ignored tests by name with the required provider environment instead of enabling all ignored tests indiscriminately.

## First Implementation Pass

The recommended first implementation pass is intentionally conservative.

Tasks:

- Create the skeleton module files with TODOs.
- Wire only the module declarations that are safe without conflicting with existing flat files.
- Move or isolate source-local tests only where the movement is straightforward.
- Leave deep provider, middleware, session, and subagent behavior untouched.
- Verify that deterministic tests still pass.

Deliverable:

- A compilable repo with the target directory shape visible.
- TODOs that tell future agents exactly what each file should own.
- No intentional runtime behavior changes.
