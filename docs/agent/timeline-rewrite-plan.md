# Agent Timeline Rewrite Plan

This plan describes a hard cutover of `crates/waltr-agent/` from separate live events, session journal events, transcript projections, and final-only tool results to one durable timeline model that can support reactive multi-client UIs through the server.

The current agent crate is only consumed by tests, so this plan intentionally avoids compatibility shims. The rewrite should preserve implemented behavior while deleting old event paths, duplicate projections, TODO-only public surfaces, and unwired middleware hooks.

## Goals

- Stream assistant text and thinking as live deltas.
- Emit tool-call previews as soon as the provider identifies a tool call, before tool execution starts.
- Stream generic tool progress as `tool_progress_info` and `tool_progress_error` deltas.
- Persist a durable, ordered, replayable session timeline with stable per-session cursors.
- Keep provider replay separate from UI timeline display events.
- Make the future server API mostly pass through, filter, cap, and fan out timeline events instead of translating many feature-specific agent event types.
- Update tests to validate equivalent behavior through the new model.
- Remove old `AgentEvent`, `AgentTranscriptEvent`, duplicate `SessionEvent` variants, unwired middleware hooks, and TODO-only public modules during the same pass.

## Non-Goals

- Do not build the server watch/slice API in this agent rewrite.
- Do not add backwards compatibility for old agent event names or session event variants.
- Do not make shell or MCP progress provider-replayable by default.
- Do not implement the TODO model catalog or agent OpenTelemetry work as part of this rewrite.
- Do not introduce server, client, TUI, or web dependencies into `waltr-agent`.

## Target Architecture

The agent crate should expose one canonical streamable model:

```text
provider stream/tool execution/session runtime
    -> agent timeline events
    -> session store cursor assignment and persistence
    -> live agent stream
    -> later server fanout, filtering, capping, and history reads
```

The agent timeline is the public display/history contract. Provider replay remains an internal projection built from the timeline plus provider metadata and final tool results.

## Target Public API

Add a new module:

```rust
pub mod timeline;
```

Move the stream boundary to timeline events:

```rust
pub type BoxAgentTimelineStream = Pin<Box<dyn Stream<Item = Result<AgentTimelineEvent>> + Send>>;
```

Remove the old stream boundary:

```rust
// Delete.
pub type BoxAgentEventStream = ...;
pub enum AgentEvent { ... }
```

### Timeline Event Shape

Use a pending event before persistence and a cursor-assigned event after store append:

```rust
pub struct PendingTimelineEvent {
    pub session_id: SessionId,
    pub turn: u64,
    pub item_id: TimelineItemId,
    pub parent_id: Option<TimelineItemId>,
    pub kind: TimelineEventKind,
    pub channel: Option<TimelineChannel>,
    pub content: TimelineContent,
    pub status: Option<TimelineStatus>,
    pub metadata: serde_json::Value,
}

pub struct AgentTimelineEvent {
    pub cursor: TimelineCursor,
    pub session_id: SessionId,
    pub turn: u64,
    pub item_id: TimelineItemId,
    pub parent_id: Option<TimelineItemId>,
    pub kind: TimelineEventKind,
    pub channel: Option<TimelineChannel>,
    pub content: TimelineContent,
    pub status: Option<TimelineStatus>,
    pub metadata: serde_json::Value,
}
```

Use explicit newtypes for identifiers:

```rust
pub struct TimelineCursor(pub u64);
pub struct TimelineItemId(pub String);
pub struct TimelineContentId(pub String);
```

Cursor rules:

- Cursors are monotonically increasing within one session.
- Cursor assignment happens at append time in the `SessionStore`.
- An append of multiple pending events returns the same events with assigned cursors in persisted order.
- Cursor values are stable once returned.
- There is no global cursor across sessions.

Item identity rules:

- One assistant text item per assistant answer turn.
- One thinking item per provider thinking block or stream segment that should render independently.
- One tool-call item per provider tool call.
- Tool progress and tool result events use `parent_id` pointing to the tool-call item.
- External requests use their request id as, or as part of, the item id.

### Timeline Kinds

Keep lifecycle kinds generic and stable:

```rust
pub enum TimelineEventKind {
    ItemStarted,
    ContentDelta,
    ItemUpdated,
    ItemFinished,
    ExternalRequestOpened,
    ExternalRequestResolved,
    SessionStatusChanged,
}
```

Do not add feature-specific kinds such as `ShellStdoutDelta` or `McpProgress`. Feature-specific behavior belongs in `channel` and `metadata`.

### Timeline Channels

Use documented constants for known channels:

```rust
pub const CHANNEL_ASSISTANT_TEXT: &str = "assistant_text";
pub const CHANNEL_THINKING: &str = "thinking";
pub const CHANNEL_TOOL_CALL: &str = "tool_call";
pub const CHANNEL_TOOL_PROGRESS_INFO: &str = "tool_progress_info";
pub const CHANNEL_TOOL_PROGRESS_ERROR: &str = "tool_progress_error";
pub const CHANNEL_TOOL_RESULT: &str = "tool_result";
pub const CHANNEL_EXTERNAL_REQUEST: &str = "external_request";
```

Prefer a string-backed `TimelineChannel` newtype over a closed enum so future server wire formats can tolerate unknown channels.

### Timeline Content

Represent common payloads without making the lifecycle kinds feature-specific:

```rust
pub enum TimelineContent {
    Empty,
    TextDelta { delta: String },
    TextSnapshot { text: String },
    ToolCall { call: ToolCall },
    ToolResult { result: ToolResult },
    ExternalRequest { request: ExternalRequestDraft },
    ContentRef { content_id: TimelineContentId, bytes: u64 },
    Json { value: serde_json::Value },
}
```

Large content handling can start simple:

- Persist full content in the session store.
- Include inline content in the event until a configured threshold is crossed.
- Use `ContentRef` for larger values.
- Let the future server decide per-client caps and ranges.

The first implementation may set a high threshold and always inline content, but the API should be shaped for content references.

## Target Session Model

Replace the current mix of `SessionEvent`, `AgentTranscriptEvent`, `AgentState`, and duplicated snapshot data with a timeline-first model.

### Durable Journal

Use one internal journal enum if non-display records are still needed:

```rust
pub enum SessionJournalEvent {
    Timeline(AgentTimelineEvent),
    ProviderRequestRecorded(RequestRecord),
    MiddlewareStateUpdated(MiddlewareStateUpdate),
}
```

This should replace the existing broad `SessionEvent` enum. Do not keep duplicate legacy variants like `TextDelta`, `ProviderTextDelta`, `ToolCall`, and `ToolCallRecorded`.

Provider request records and middleware state updates are internal session records. They should not leak into the UI timeline unless there is a deliberate status or metadata event for UI display.

### Snapshot Projection

Keep `SessionSnapshot` as a derived projection, not persisted source of truth.

The snapshot should contain:

- `id`
- `revision`
- `turn`
- `status`
- `pending_work`
- `external_requests`
- `middleware_state`
- provider replay turns
- optional recent timeline summary if useful for tests or future server bootstrap

Remove `AgentState` and `AgentTranscriptEvent`. Any transcript-like view should be derived from timeline events.

### Provider Replay

Provider replay should be rebuilt from timeline events and internal provider records:

- Assistant text deltas form assistant text content for the turn.
- Thinking deltas and thinking block metadata form provider-compatible thinking items where supported.
- Tool calls and final tool results form replayable tool interactions.
- Tool progress is UI/history-only and is not replayed to the provider by default.
- Provider metadata remains stored for continuation optimization, but replay correctness must not depend on it.

## Target Session Store

Change `SessionStore` from whole-record append/load semantics to cursor/range semantics.

Proposed trait shape:

```rust
#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn create(&self, session_id: SessionId) -> Result<SessionRecord>;
    async fn load(&self, session_id: SessionId) -> Result<SessionRecord>;
    async fn append_timeline(
        &self,
        session_id: SessionId,
        expected_revision: Revision,
        events: Vec<PendingTimelineEvent>,
    ) -> Result<Vec<AgentTimelineEvent>>;
    async fn append_journal(
        &self,
        session_id: SessionId,
        expected_revision: Revision,
        events: Vec<SessionJournalEvent>,
    ) -> Result<Revision>;
    async fn timeline_after(
        &self,
        session_id: SessionId,
        cursor: Option<TimelineCursor>,
        limit: usize,
    ) -> Result<Vec<AgentTimelineEvent>>;
    async fn timeline_before(
        &self,
        session_id: SessionId,
        cursor: Option<TimelineCursor>,
        limit: usize,
    ) -> Result<Vec<AgentTimelineEvent>>;
    async fn load_content(
        &self,
        session_id: SessionId,
        content_id: TimelineContentId,
        range: Option<ContentRange>,
    ) -> Result<TimelineContentChunk>;
}
```

Implementation notes:

- Backends can initially implement range methods by loading the full record and slicing in memory.
- Memory storage should assign cursors directly from the session record.
- Filesystem/OpenDAL/S3/Scylla/PostgreSQL storage can keep JSON whole-record storage at first if the trait is cursor-shaped.
- Do not design the trait around server subscriptions. The server can add fanout on top of append/range reads.

If `append_timeline` and `append_journal` feel redundant during implementation, keep one append method that accepts a batch of journal records, but ensure it returns cursor-assigned timeline events for live emission.

## Target Tool API

Replace final-string-only tools with context-based execution.

Current shape to remove:

```rust
Fn(&ToolCall) -> Future<Result<String>>
```

Target shape:

```rust
pub struct ToolExecutionContext {
    pub session_id: SessionId,
    pub turn: u64,
    pub call: ToolCall,
    pub item_id: TimelineItemId,
    pub progress: ToolProgressSink,
}

pub struct ToolProgressSink { ... }

impl ToolProgressSink {
    pub async fn info(&self, delta: impl Into<String>) -> Result<()>;
    pub async fn error(&self, delta: impl Into<String>) -> Result<()>;
    pub async fn event(&self, channel: TimelineChannel, content: TimelineContent) -> Result<()>;
}

pub trait AgentToolHandler: Send + Sync {
    async fn call(&self, context: ToolExecutionContext) -> Result<ToolResult>;
}
```

Progress sink behavior:

- `info` emits `ContentDelta` on `tool_progress_info`.
- `error` emits `ContentDelta` on `tool_progress_error`.
- Progress events are persisted before being returned to live stream consumers.
- Progress events have `parent_id` set to the tool-call item id.
- Progress events do not become provider replay items.

Tool result behavior:

- Final result is still the provider-facing result.
- The final result emits an `ItemFinished` or `ContentDelta` plus `ItemFinished` on `tool_result`.
- The result is included in provider replay.

## Runtime Flow After Rewrite

Provider streaming:

1. Build request through middleware.
2. Record provider request in the internal journal.
3. Stream provider events.
4. For thinking/text deltas, append timeline deltas and return them immediately.
5. For tool calls, append and return the tool-call preview immediately.
6. Execute the tool after preview emission.
7. Stream tool progress through the progress sink.
8. Append final tool result.
9. Finish the turn; future loads derive the snapshot projection from the journal.

Permission suspension:

1. Tool call preview is emitted when the provider requests the tool.
2. Permission claims are collected before execution.
3. `Deny` emits a final tool result error and does not execute.
4. `Ask` emits an external request timeline event and suspends.
5. Resume records the external response and continues according to runtime semantics.

Question suspension:

1. Question tool call preview is emitted.
2. Middleware converts the question into an external request.
3. Timeline records the external request.
4. Resume records the answer as the tool result path already does today.

## Middleware Simplification

Keep middleware internal unless there is a concrete public extension requirement.

Keep only hooks that are wired in the loop during this pass:

- `build_system_prompt`
- `list_tools`
- `edit_tool_metadata`
- `before_provider_request`
- `before_tool_call`
- `after_tool_call`
- `on_suspend`

Remove or do not expose hooks unless they are wired in the same pass:

- `on_provider_event`
- `on_resume`

Remove TODO-only public middleware modules until they are implemented:

- `cost`
- `otel`
- `pricing`
- `secrets`
- `usage`

Keep existing implemented middleware behavior:

- system prompt capture and overrides
- `AGENTS.md` prompt/read context
- skills catalog, `load_skill`, and dynamic reveal
- permission-filtered tool visibility
- root hints
- filesystem and shell permission claims
- question suspension

## Public Surface Cleanup

Because tests are the only current consumer, make `src/lib.rs` export only intentional APIs.

Keep public:

- `Agent`
- `AgentBuilder`
- run/session entry points required by tests and future hosts
- timeline types
- model identity/provider-neutral stream types needed to construct providers and tests
- provider adapter constructors if still used directly
- tool config types for filesystem, shell, skills, question, and MCP
- permission policy types
- session store trait and concrete stores that are intended deployment options

Hide or remove:

- public middleware internals
- TODO-only `models` catalog modules
- TODO-only agent OTel/cost/pricing/usage/secrets modules
- old `AgentEvent`
- old `AgentTranscriptEvent`
- duplicate old `SessionEvent` variants
- dead-code-only builder methods for custom middleware chains unless tests need them after rewrite

## Built-In Tool Migration

### Filesystem

Filesystem tools can mostly return final results only.

Required changes:

- Update handlers to accept `ToolExecutionContext`.
- Return `ToolResult` instead of `String`.
- Preserve current permission claims and root handling.
- Preserve AGENTS.md and skills dynamic context behavior.

Optional in the first pass:

- Emit a short `tool_progress_info` event for large writes, deletes, or patch application.
- Do not add progress where it would add noise.

### Shell

Shell should become the primary progress proof point.

Required changes:

- Replace blocking final output collection with stdout/stderr streaming.
- Emit stdout chunks as `tool_progress_info`.
- Emit stderr chunks as `tool_progress_error`.
- Preserve timeout behavior.
- Preserve max final output behavior.
- Keep nonzero exit as a final tool result, not a Rust error.
- Preserve parser-derived permission claims.

The final shell tool result should still contain a bounded final summary compatible with provider replay.

### MCP

MCP can start with final results only if the current transport cannot expose meaningful progress yet.

Required changes:

- Update tool handlers to accept `ToolExecutionContext`.
- Return `ToolResult`.
- Preserve direct `mcp_<server>_<tool>` tool exposure.
- Preserve stdio and streamable HTTP behavior.

If MCP transport progress or notifications are exposed during the rewrite:

- Map informational protocol progress/logs to `tool_progress_info`.
- Map protocol errors/diagnostics to `tool_progress_error`.
- Do not add MCP-specific public timeline event kinds.

### Skills

Required changes:

- Update synthetic `load_skill` tool to return `ToolResult`.
- Preserve catalog prompt behavior.
- Preserve visible-skill checks and ambiguous-skill errors.
- Preserve resource path listing without loading resource contents.

### Question

Required changes:

- Keep question as an external request flow.
- Emit the tool-call preview before suspension.
- Emit the external request timeline item.
- Preserve durable resume behavior.

## File-Level Work Plan

### Add

- `crates/waltr-agent/src/timeline.rs`
- Optional: `crates/waltr-agent/src/agent/projection.rs` for snapshot/provider replay projection from timeline events.
- Optional: `crates/waltr-agent/src/agent/content.rs` for content references and range reads if it grows beyond `timeline.rs`.

### Rewrite

- `crates/waltr-agent/src/agent/event.rs`: replace old `AgentEvent` with timeline stream type or delete and move stream alias to `timeline.rs`.
- `crates/waltr-agent/src/agent/loop.rs`: emit and persist timeline events; emit tool call before execution; integrate progress sink.
- `crates/waltr-agent/src/agent/tool.rs`: replace final-string handler with context/result/progress handler.
- `crates/waltr-agent/src/agent/session/types.rs`: replace broad session events and transcript state with timeline/journal/snapshot projection types.
- `crates/waltr-agent/src/agent/session/runtime.rs`: resume against the new journal/timeline model.
- `crates/waltr-agent/src/agent/session/storage/mod.rs`: change trait to cursor/range shape.
- `crates/waltr-agent/src/agent/session/storage/*.rs`: update all stores to assign cursors and support range methods.
- `crates/waltr-agent/src/agent/run.rs`: return timeline stream and wire per-run middleware/store overrides into new loop.
- `crates/waltr-agent/src/agent/builder.rs`: simplify middleware setup and remove unused custom middleware methods if not needed.
- `crates/waltr-agent/src/middleware/*.rs`: adjust hook contexts to timeline/tool context and remove unwired hooks.
- `crates/waltr-agent/src/tools/**/*.rs`: update tool handlers to context/result shape.
- `crates/waltr-agent/src/mcp/**/*.rs`: update MCP tool wrapping and optional progress handling.
- `crates/waltr-agent/src/lib.rs`: prune exports and expose the new timeline API.

### Delete Or Hide

- `AgentEvent` enum.
- `AgentTranscriptEvent` enum.
- duplicate/legacy `SessionEvent` variants.
- TODO-only public `models` catalog modules unless they are made crate-private placeholders.
- TODO-only public middleware modules: cost, pricing, usage, secrets, otel.
- unwired middleware hooks unless wired during the rewrite.

## Test Migration Plan

The tests should be updated to assert equivalent behavior, not old type names.

| Current Validation | New Validation |
| --- | --- |
| `AgentEvent::TextDelta` appears while streaming. | Timeline `ContentDelta` with `assistant_text` channel appears while streaming. |
| `AgentEvent::ThinkingDelta` appears while streaming. | Timeline `ContentDelta` with `thinking` channel appears while streaming. |
| `AgentEvent::ToolCall` appears. | Timeline `ItemStarted` or `ToolCall` content with `tool_call` channel appears before any matching result. |
| Tool result appears after execution. | Timeline final `tool_result` event appears with `parent_id` pointing to tool call item. |
| Long-running tools delay old preview. | Add/keep test proving preview arrives before delayed handler completes. |
| Shell final output is returned. | Shell emits generic info/error progress and final bounded provider-facing result. |
| MCP tool final result is returned. | MCP tool call/result uses generic timeline parent/child shape. |
| Permission deny returns tool error. | Deny emits final tool result error without executing tool. |
| Permission ask suspends. | Ask emits external request timeline item and session status suspended. |
| Question tool suspends. | Question emits tool preview plus external request timeline item. |
| `SessionEvent::MiddlewareStateUpdated` exists. | Internal journal records middleware state and replay applies it deterministically. |
| `SessionSnapshot.state.events` contains transcript events. | Snapshot/projection derived from timeline contains equivalent messages/items. |
| Durable stores reload full record. | Durable stores preserve cursor order and range reads return expected timeline windows. |

Test helper changes:

- Replace old event collectors with timeline collectors by channel/kind.
- Add helpers to concatenate text deltas for a channel.
- Add helpers to find events by `item_id` and `parent_id`.
- Add cursor assertions for monotonic ordering.
- Add range-read assertions for `timeline_after` and `timeline_before`.

Primary test files to update:

- `crates/waltr-agent/tests/agent/loop.rs`
- `crates/waltr-agent/tests/agent/session.rs`
- `crates/waltr-agent/tests/agent/permissions.rs`
- `crates/waltr-agent/tests/agent/question.rs`
- `crates/waltr-agent/tests/agent/mcp.rs`
- `crates/waltr-agent/tests/live/agent/*.rs`
- unit tests under `crates/waltr-agent/src/middleware/`
- unit tests under `crates/waltr-agent/src/agent/session/storage/`
- unit tests under `crates/waltr-agent/src/tools/shell/`

## Implementation Sequence

This should be one hard-cutover branch, but use compile checkpoints to keep the work manageable.

### Step 1: Introduce Timeline Types

- Add `timeline.rs`.
- Export timeline types from `lib.rs`.
- Add unit tests for channel constants, cursor ordering helpers, and content reference helpers.
- Do not adapt the loop yet.

Checkpoint:

```sh
cargo test -p waltr-agent --features shell --lib timeline
```

### Step 2: Rewrite Session Types And Store Trait

- Replace `SessionEvent` with `SessionJournalEvent` plus timeline events.
- Remove `AgentTranscriptEvent` and `AgentState` from session snapshots.
- Add cursor fields to records and store state.
- Update memory store first.
- Update filesystem, OpenDAL, S3, Scylla, and PostgreSQL stores with simple whole-record slicing.

Checkpoint:

```sh
cargo test -p waltr-agent --features shell --lib agent::session
```

### Step 3: Rewrite Runtime Projection

- Add projection helpers from journal/timeline to `SessionSnapshot`.
- Rebuild provider replay turns from timeline plus provider records.
- Update resume handling for prompt, external response, tool results, and interrupt.
- Preserve current durable semantics for external requests and pending work.

Checkpoint:

```sh
cargo test -p waltr-agent --features shell --test agent session
```

### Step 4: Rewrite Stream Boundary And Agent Loop

- Change run streams to return `AgentTimelineEvent`.
- Convert provider text/thinking events to timeline deltas.
- Convert thinking blocks and provider metadata to journal/projection records as needed.
- Emit tool-call preview before execution.
- Persist through store append before returning live events.
- Derive snapshots from the journal when runtime code needs projection state.

Checkpoint:

```sh
cargo test -p waltr-agent --features shell --test agent loop
```

### Step 5: Rewrite Tool API And Progress Sink

- Change `AgentTool` handler shape to `ToolExecutionContext -> ToolResult`.
- Add `ToolProgressSink` wired to the loop pending event queue/store append path.
- Update permission and middleware tool contexts.
- Update filesystem, question, skills, MCP, and shell tool wrappers.

Checkpoint:

```sh
cargo test -p waltr-agent --features shell --lib tools
cargo test -p waltr-agent --features shell --test agent
```

### Step 6: Stream Shell Progress

- Replace shell final-only process reading with incremental stdout/stderr reading.
- Emit stdout as `tool_progress_info`.
- Emit stderr as `tool_progress_error`.
- Preserve timeout, max output, and nonzero-exit behavior.
- Add deterministic tests for progress ordering and final result content.

Checkpoint:

```sh
cargo test -p waltr-agent --features shell --lib shell
cargo test -p waltr-agent --features shell --test agent permissions
```

### Step 7: Preserve MCP And Other Tool Behavior

- Update MCP direct tool wrappers to new tool context.
- Keep final-only MCP behavior unless progress is straightforward from existing transport data.
- Ensure MCP tool names and result formatting remain stable.
- Update skills and question tests around timeline events.

Checkpoint:

```sh
cargo test -p waltr-agent --features shell --test agent mcp
cargo test -p waltr-agent --features shell --test agent question
```

### Step 8: Simplify Middleware

- Remove unwired hooks.
- Remove object-safe wrapper methods that only mirror deleted hooks.
- Remove dead-code allowances where they are no longer needed.
- Keep deterministic ordering.
- Keep middleware state journaling where it affects request construction.

Checkpoint:

```sh
cargo test -p waltr-agent --features shell --lib middleware
cargo test -p waltr-agent --features shell --test agent loop
```

### Step 9: Prune Public API And Old Paths

- Delete old event/transcript types.
- Remove duplicate session variants.
- Hide or remove TODO-only public modules.
- Update docs to describe the timeline model.
- Search for old names and remove all references.

Required searches:

```sh
rg "AgentEvent|AgentTranscriptEvent|ProviderTextDelta|TextDelta|ToolCallRecorded|ToolResultRecorded|on_provider_event|on_resume" crates/waltr-agent docs
```

Checkpoint:

```sh
cargo test -p waltr-agent --features shell --lib
cargo test -p waltr-agent --features shell --test agent
```

### Step 10: Live And Container-Backed Verification

Run deterministic tests first. Run ignored live tests only when credentials and containers are available.

Deterministic:

```sh
cargo test -p waltr-agent --features shell --lib
cargo test -p waltr-agent --features shell --test agent
```

Live ignored:

```sh
cargo test -p waltr-agent --features shell --test live -- --ignored
```

If live tests are skipped, state the missing credentials or services in the final work summary.

## Acceptance Criteria

- `AgentRunBuilder::stream()` yields `AgentTimelineEvent`, not `AgentEvent`.
- Tool-call timeline events are emitted before tool execution completes.
- Shell emits generic `tool_progress_info` and `tool_progress_error` deltas.
- MCP and other tools use the same generic tool-call/result parent-child shape.
- Session timeline events have stable monotonic cursors.
- Session store supports timeline range reads even if implemented by whole-record slicing initially.
- `AgentTranscriptEvent` is deleted.
- Duplicate old `SessionEvent` provider/tool variants are deleted.
- Unwired middleware hooks are deleted or wired.
- TODO-only model catalog and agent OTel/cost/pricing/usage/secrets modules are not public API.
- Existing implemented behavior remains covered by updated tests.
- Agent crate still has no dependency on server, shared client, TUI, or web crates.

## Risks And Mitigations

| Risk | Mitigation |
| --- | --- |
| Timeline model becomes too UI-specific. | Keep lifecycle kinds generic and use channels/metadata for render hints. |
| Provider replay accidentally includes tool progress. | Add tests proving progress events are excluded from provider replay. |
| Cursor assignment complicates stores. | Start with whole-record storage and in-memory slicing behind cursor-shaped trait methods. |
| Shell streaming changes output ordering. | Preserve final result formatting and add stdout/stderr progress ordering tests. |
| Middleware simplification breaks AGENTS/skills prompt capture. | Keep deterministic prompt/middleware ordering tests and durable resume tests. |
| Removing public modules breaks tests heavily. | Update tests to use the new public API; only expose internals through `pub(crate)` test helpers if needed. |

## Documentation Updates After Code Rewrite

Update these docs in the same implementation change:

- [../architecture/agent.md](../architecture/agent.md): replace old event/session descriptions with timeline architecture.
- [middleware.md](middleware.md): remove deleted hooks and public middleware implications.
- [storage.md](storage.md): document cursor/range timeline storage.
- [shell.md](shell.md): document generic progress info/error deltas.
- [mcp.md](mcp.md): document direct MCP tools on the generic timeline model.
- [permissions.md](permissions.md): document permission suspension as timeline external request events.

Leave [model-catalog.md](model-catalog.md) and [otel.md](otel.md) as stubs unless those implementations are added separately.

## Future Server API Fit

This rewrite should leave the server able to add APIs like:

```text
WatchSession(session_id, cursor?, tail_messages?, max_content_bytes?, sections?, follow?)
GetSessionSlice(session_id, before?, after?, limit?, max_content_bytes?, sections?)
GetSessionContent(session_id, content_id, range?)
```

The server should own multi-client fanout, per-client capping, scrollback windows, backpressure, and full-content retrieval. The agent should own stable timeline production, persistence, and range-readable history.
