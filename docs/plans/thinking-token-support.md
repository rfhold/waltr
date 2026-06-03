# Thinking Token Support

## Goal

Add provider-level thinking/reasoning support to `waltr-agent`, including live surfacing, transcript storage, and provider-specific replay payloads needed for tool-call continuations.

Thinking is a provider setting, not an `AgentBuilder` option. Providers that support visible or replayable reasoning expose provider-specific configuration APIs. Providers that do not expose reasoning remain unchanged.

## Current State

- `CompletionEvent` only has `Delta`, `ToolCall`, and `Finished`.
- `AgentEvent` and `AgentTranscriptEvent` only store text/tool events.
- `AgentState::last_assistant_text()` concatenates assistant text only.
- The agent loop replays `tool_calls` and `tool_results` between turns, but not assistant thinking/reasoning payloads.
- Anthropic and OpenAI Responses require preserving reasoning payloads for reliable tool-result continuation when thinking/reasoning is enabled.
- `crates/waltr-agent/tests/streaming.rs` and `crates/waltr-agent/tests/tools.rs` duplicate live provider construction already centralized under `tests/agent/live_provider.rs`.

## Provider-Level APIs

### Anthropic Messages

Add provider-specific thinking config:

```rust
pub enum AnthropicThinkingConfig {
    Enabled {
        budget_tokens: u32,
        display: Option<AnthropicThinkingDisplay>,
    },
    Adaptive {
        display: Option<AnthropicThinkingDisplay>,
    },
}

pub enum AnthropicThinkingDisplay {
    Summarized,
    Omitted,
}
```

Wire mapping:

- `Enabled { budget_tokens, display }` serializes to
  `{"type": "enabled", "budget_tokens": N, "display": "..."}`.
- `Adaptive { display }` serializes to `{"type": "adaptive", "display": "..."}`.
- `display` is omitted from the body when `None`.

Add builder API:

```rust
impl AnthropicMessages {
    pub fn with_thinking(self, config: AnthropicThinkingConfig) -> Self;
}
```

Notes:

- Keep `with_max_tokens` separate.
- For `Enabled`, `budget_tokens` must be at least 1024 and strictly less than
  `max_tokens`.
- Model compatibility constrains which mode is valid:
  - Manual `Enabled` is supported on most current models but returns a 400
    error on Claude Opus 4.8 and Opus 4.7, which require `Adaptive`.
  - `Adaptive` is recommended on Opus 4.6 / Sonnet 4.6 and required on the
    newest Opus models; manual mode is deprecated there.
- `display` defaults differ by model (`"summarized"` on Claude 4 / Sonnet 4.6,
  `"omitted"` on Opus 4.8 / 4.7), so set it explicitly when summaries are
  required. With `display: "omitted"`, no `thinking_delta` events stream and only
  a `signature_delta` arrives.
- The live test should pick a model that accepts the configured mode (e.g. a
  Haiku/Sonnet model for `Enabled`).

### OpenAI Responses

Add provider-specific reasoning config:

```rust
pub struct OpenAiReasoningConfig {
    pub effort: OpenAiReasoningEffort,
    pub summary: Option<OpenAiReasoningSummary>,
}

pub enum OpenAiReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
}

pub enum OpenAiReasoningSummary {
    Auto,
}
```

Add builder APIs:

```rust
impl OpenAiResponses {
    pub fn with_reasoning(self, config: OpenAiReasoningConfig) -> Self;
}

impl OpenAiResponsesWebSocket {
    pub fn with_reasoning(self, config: OpenAiReasoningConfig) -> Self;
}
```

### OpenAI Chat Completions

Do not add reasoning support initially.

OpenAI Chat participates in the live thinking test matrix as an explicit no-thinking baseline. It should stream normal final text and produce no thinking events or stored thinking blocks.

## Provider-Neutral Stream Types

Extend `CompletionEvent`:

```rust
pub enum CompletionEvent {
    Delta(String),
    ThinkingDelta(ThinkingDelta),
    ThinkingBlock(ThinkingBlock),
    ToolCall(ToolCall),
    Finished,
}
```

Add provider-neutral thinking types:

```rust
pub struct ThinkingDelta {
    pub text: String,
}

pub struct ThinkingBlock {
    pub kind: ThinkingBlockKind,
    pub text: String,
    pub payload: serde_json::Value,
}

pub enum ThinkingBlockKind {
    Anthropic,
    OpenAiResponses,
}
```

Extend `CompletionRequest`:

```rust
pub struct CompletionRequest<'a> {
    pub prompt: &'a str,
    pub system_prompt: Option<&'a str>,
    pub tools: Vec<ToolDefinition>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_results: Vec<ToolResult>,
    pub thinking_blocks: Vec<ThinkingBlock>,
}
```

`thinking_blocks` are provider replay data. They are not intended to be displayed directly unless surfaced through agent transcript events.

## Agent Surfacing And Storage

Extend `AgentEvent`:

```rust
pub enum AgentEvent {
    ThinkingDelta { turn: usize, delta: String },
    // existing variants...
}
```

Extend `AgentTranscriptEvent`:

```rust
pub enum AgentTranscriptEvent {
    Thinking { turn: usize, text: String },
    // existing variants...
}
```

Extend `AgentState`:

```rust
pub struct AgentState {
    pub events: Vec<AgentTranscriptEvent>,
    pub thinking_blocks: Vec<ThinkingBlock>,
}
```

Agent loop behavior:

- Surface `CompletionEvent::ThinkingDelta` as `AgentEvent::ThinkingDelta`.
- Store thinking deltas in `AgentTranscriptEvent::Thinking`.
- Store complete `CompletionEvent::ThinkingBlock` values in `AgentState::thinking_blocks` and the loop replay state.
- Set `request.thinking_blocks` on subsequent provider requests.
- Keep `AgentState::last_assistant_text()` unchanged so thinking never pollutes normal final answer text.

## Anthropic Implementation

Request body when thinking is configured:

```json
{
  "thinking": {
    "type": "enabled",
    "budget_tokens": 10000,
    "display": "summarized"
  }
}
```

Streaming parser behavior:

- Track active thinking content blocks by content block index.
- Emit `CompletionEvent::ThinkingDelta` for `thinking_delta`.
- Capture `signature_delta` for the active thinking block.
- Emit `CompletionEvent::ThinkingBlock` at `content_block_stop` for completed thinking blocks.

Tool continuation behavior:

- When sending tool results after a tool call, include stored Anthropic thinking blocks in the assistant message content before the `tool_use` block.
- Preserve the provider payload and signature exactly enough for Anthropic continuation requirements.
- Do not synthesize or mutate thinking payloads.

## OpenAI Responses Implementation

Request body when reasoning is configured:

```json
{
  "store": false,
  "reasoning": {
    "effort": "low",
    "summary": "auto"
  },
  "include": ["reasoning.encrypted_content"]
}
```

Streaming parser behavior:

- Handle reasoning output item events such as `response.output_item.added` and `response.output_item.done`.
- Handle reasoning summary delta/done events if present in streamed payloads.
- Surface available reasoning summary text as `CompletionEvent::ThinkingDelta`.
- Store complete reasoning output items, including `encrypted_content`, as `ThinkingBlock` values.

Replay behavior:

- In `responses_input()`, include stored OpenAI reasoning items before function call output items when continuing after tool calls.
- Keep `store: false` unchanged.
- Include `reasoning.encrypted_content` only when reasoning is configured.

## Shared Live Provider Test Helper Cleanup

Move shared live provider support out of the agent-only test module:

- From: `crates/waltr-agent/tests/agent/live_provider.rs`
- To: `crates/waltr-agent/tests/support/live_provider.rs`

Import it from integration tests with:

```rust
#[path = "support/live_provider.rs"]
mod live_provider;
```

Update these tests to use the shared helper:

- `crates/waltr-agent/tests/agent.rs`
- `crates/waltr-agent/tests/streaming.rs`
- `crates/waltr-agent/tests/tools.rs`
- `crates/waltr-agent/tests/thinking.rs`

Cleanup requirements:

- Remove duplicated `ANTHROPIC_TEST_MODEL`, `OPENAI_TEST_MODEL`, and `required_env` helpers from non-agent tests.
- Keep environment variable usage unchanged:
  - `ANTHROPIC_API_KEY`
  - `OPENAI_API_KEY`
- Preserve existing ignored live test behavior and names where practical.

## New Root Integration Test

Add:

- `crates/waltr-agent/tests/thinking.rs`

This is a crate-root integration test file. A workspace-root `tests/` directory is not useful here because the workspace root has no package.

The test should fan out to all 4 providers:

- Anthropic Messages with thinking enabled.
- OpenAI Responses with reasoning enabled.
- OpenAI Responses WebSocket with reasoning enabled.
- OpenAI Chat Completions as the no-thinking baseline.

Mark live tests `#[ignore]`, matching existing live provider tests.

Expected behavior:

- Thinking-capable providers surface at least one thinking event or store at least one thinking block.
- Final answer text remains separate and contains the requested simple answer.
- Chat Completions streams the final answer and emits/stores no thinking.

## Deterministic Tests

Add non-live tests for:

- `CompletionRequest::new` initializes empty `thinking_blocks`.
- Agent stores thinking deltas in the transcript.
- Agent surfaces `AgentEvent::ThinkingDelta`.
- Agent excludes thinking from `last_assistant_text()`.
- Agent replays stored thinking blocks on tool-result continuation.
- Anthropic request body includes thinking config.
- Anthropic parser emits thinking delta and completed block with signature.
- Anthropic continuation includes thinking block before tool result.
- OpenAI Responses body includes `reasoning`, `include`, and `store: false`.
- OpenAI Responses parser stores reasoning output items.
- OpenAI Responses input replays reasoning items before function outputs.

## Verification

Run:

```sh
cargo fmt -p waltr-agent
cargo test -p waltr-agent
cargo test -p waltr-agent --features shell
cargo clippy -p waltr-agent --all-targets
cargo clippy -p waltr-agent --all-targets --features shell
```

Optional ignored live tests:

```sh
cargo test -p waltr-agent --test thinking -- --ignored
```
