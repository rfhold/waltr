# Question Tool Support

## Goal

Add an opt-in library-only question tool to `waltr-agent` so an agent can ask the host application to collect structured user input during an agent loop.

The tool should be rich enough to mirror the opencode question tool shape exposed to agents today:

- multiple questions in one call
- concise headers
- selectable options with labels and descriptions
- single-select and multi-select questions
- optional custom free-form answers, enabled by default

## Scope

- Add a `tools::question` module to `waltr-agent`.
- Keep the tool disabled unless explicitly configured.
- Expose the tool through `AgentBuilder` and run-level custom tool plumbing like the other built-in tools.
- Accept a host-provided callback that performs the actual user interaction.
- Return structured JSON answers to the model as a normal `ToolResult` string.
- Add deterministic agent-loop coverage that does not depend on live provider behavior.
- Add one ignored live-provider integration test that validates a real model can call the question tool and use the returned answers.

## Non-Goals

- Do not implement TUI, modal, terminal, web, or native UI rendering in this change.
- Do not make the question tool available by default.
- Do not add a Cargo feature; no additional dependencies are required for the library-only implementation.
- Do not add permission prompts to this tool. The question tool itself is the user-interaction mechanism, not a permission gate.
- Do not support provider-specific tool schema extensions unless required by a failing provider test.

## Public API Shape

Add `crates/waltr-agent/src/tools/question.rs` and export it from `src/tools/mod.rs`:

```rust
pub mod question;
```

Re-export the public module from `src/lib.rs` through the existing `tools` export path. Direct top-level type re-exports are optional; keep the first implementation minimal unless call sites become noisy.

Initial public types:

- `QuestionConfig`
- `QuestionInput`
- `QuestionPrompt`
- `QuestionOption`
- `QuestionAnswer`
- `QuestionOutput`

Builder shape:

```rust
use waltr_agent::{Agent, tools::question::QuestionConfig};

let agent = Agent::builder(model)
    .with_question_tool(QuestionConfig::new(|input| {
        // Ask the user through the host application and return answers.
        Ok(answer_questions(input))
    }))
    .build();
```

Add an async constructor for UI integrations that need to await user interaction:

```rust
let agent = Agent::builder(model)
    .with_question_tool(QuestionConfig::new_async(|input| async move {
        Ok(answer_questions_with_ui(input).await)
    }))
    .build();
```

Add an agent builder method:

```rust
impl AgentBuilder {
    pub fn with_question_tool(mut self, config: QuestionConfig) -> Self {
        self.tools.push(config.into_agent_tool());
        self
    }
}
```

The method stays synchronous because the callback is only invoked when the model calls the tool.

## Tool Definition

Use a stable provider-facing tool name:

```text
question
```

Description should clearly tell the model when to use it:

```text
Ask the user one or more structured questions and return their answers. Use this when progress requires user preference, clarification, or a decision.
```

Input schema:

```json
{
  "type": "object",
  "additionalProperties": false,
  "required": ["questions"],
  "properties": {
    "questions": {
      "type": "array",
      "minItems": 1,
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["question", "header", "options", "multiple"],
        "properties": {
          "question": {
            "type": "string",
            "description": "The complete question to ask the user."
          },
          "header": {
            "type": "string",
            "description": "A very short label for the question, ideally 30 characters or fewer."
          },
          "options": {
            "type": "array",
            "items": {
              "type": "object",
              "additionalProperties": false,
              "required": ["label", "description"],
              "properties": {
                "label": {
                  "type": "string",
                  "description": "Short display text for the option."
                },
                "description": {
                  "type": "string",
                  "description": "A concise explanation of what this choice means."
                }
              }
            }
          },
          "multiple": {
            "type": "boolean",
            "description": "Whether the user may select multiple options."
          },
          "custom": {
            "type": "boolean",
            "description": "Whether the host should allow a custom free-form answer. Defaults to true when omitted."
          }
        }
      }
    }
  }
}
```

Rust input types should mirror the schema and use `#[serde(rename_all = "camelCase")]` only if field names require it. `custom` should deserialize as `Option<bool>` or a helper that defaults to enabled.

## Callback Contract

The host callback receives the parsed `QuestionInput` and returns `QuestionOutput`.

Suggested output shape:

```rust
pub struct QuestionOutput {
    pub answers: Vec<QuestionAnswer>,
}

pub struct QuestionAnswer {
    pub header: String,
    pub selections: Vec<String>,
    pub custom: Option<String>,
}
```

Semantics:

- `answers` should contain one answer per requested question, in the same order.
- `header` identifies the answered prompt and should normally match the input prompt header.
- `selections` contains selected option labels.
- `custom` contains the free-form response when the user supplied one.
- For a single-select question, the host should return zero or one selected labels, plus optional custom text if allowed.
- For a multi-select question, the host may return any number of selected labels, plus optional custom text if allowed.

The tool should serialize `QuestionOutput` as compact JSON for the model. If the callback returns an error, return the normal agent-loop tool error string rather than failing the whole run.

## Validation

Keep validation minimal and provider-neutral:

- Require at least one question.
- Preserve question order.
- Treat omitted `custom` as `true`.
- Reject malformed JSON through existing `serde_json` error handling.
- Do not reject empty option lists if `custom` is enabled.
- Reject questions that have no options and `custom == false`, because there is no valid way for the user to answer.

Do not over-validate labels or descriptions in the first pass. The schema descriptions should guide the model, and host applications can enforce presentation-specific constraints.

## Agent Integration

Follow existing built-in tool patterns:

- `QuestionConfig::into_agent_tool()` builds one `AgentTool`.
- The tool uses `AgentTool::new_async(...)` so async host callbacks work without blocking.
- The handler deserializes `ToolCall.arguments` into `QuestionInput`.
- The handler invokes the configured callback.
- The handler serializes `QuestionOutput` to a JSON string.

No provider adapter changes should be necessary because OpenAI Responses, OpenAI Chat Completions, and Anthropic already pass `ToolDefinition.input_schema` through provider-neutral tool conversion.

## Tests

Add `crates/waltr-agent/tests/agent/question.rs` and wire it from `crates/waltr-agent/tests/agent.rs`:

```rust
#[path = "agent/question.rs"]
mod question;
```

### Deterministic Agent-Loop Test

Add one deterministic test that mirrors the MCP direct-tool test style and does not require provider credentials:

```rust
#[tokio::test]
async fn question_agent_loop_surfaces_tool_and_returns_structured_answers() -> waltr_agent::Result<()> {
    // ...
}
```

The deterministic test should:

- Build an agent with `.with_question_tool(config)`.
- Use a scripted provider that captures `CompletionRequest.tools`.
- Assert `question` is surfaced to the model.
- Assert the surfaced tool schema includes `questions`, `question`, `header`, `options`, `label`, `description`, `multiple`, and `custom`.
- Script the provider to call `question` with two prompts:
  - a single-select question with custom enabled by omission
  - a multi-select question with `custom: false`
- Use a callback that records the parsed `QuestionInput`.
- Return structured answers containing markers such as `selected-plan-101`, `selected-test-202`, and `custom-note-303`.
- Assert the callback saw the expected prompt headers, options, `multiple` flags, and default-custom behavior.
- Assert the transcript contains a `ToolCall` for `question`.
- Assert the `ToolResult` JSON contains the answer markers.
- Assert final model text contains `question-flow-complete`.

Add a small deterministic error-path test:

```rust
#[tokio::test]
async fn question_tool_rejects_unanswerable_prompt_without_options_or_custom() -> waltr_agent::Result<()> {
    // ...
}
```

The error-path test should:

- Build an agent with `.with_question_tool(config)`.
- Script the provider to call `question` with one prompt that has `options: []` and `custom: false`.
- Assert the callback is not invoked.
- Assert the tool result contains `Tool error:` and a clear validation message.

### Ignored Live-Provider Test

Add one ignored live-provider test matching the style of MCP, permissions, and context tests:

```rust
#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn live_question_agent_loop_asks_and_uses_structured_answers() -> waltr_agent::Result<()> {
    // ...
}
```

The live test should:

- Select the live provider with `live_provider::selected_from_env("WALTR_AGENT_LIVE_PROVIDER", "openai_responses_websocket")`.
- Build an agent with `.with_question_tool(config)`.
- Use a callback that does not require actual interactive UI; it should deterministically answer any received prompts.
- Capture the provider-facing tool definitions sent to the model and include them in failure diagnostics.
- Prompt the live provider to ask the user which implementation path to choose and which tests to include.
- Have the callback return answers containing exact markers:
  - `question-live-choice-101`
  - `question-live-test-202`
  - `question-live-custom-303`
- Assert the transcript contains a `question` tool call and tool result.
- Assert the callback received at least one structured question from the model.
- Assert the final answer contains all markers and `question-live-flow-complete`.
- Call `crate::session::report_tool_errors(&provider, &session, "question live test")` before assertions for better diagnostics.

This validates live model behavior without requiring a real TUI prompt. The callback is still the same library API a future TUI integration would use.

### Schema Unit Coverage

If the deterministic integration test becomes too broad, add a focused unit test in `question.rs` for schema details. Prefer keeping schema assertions in the integration test first, because it proves the actual provider-facing `ToolDefinition` is surfaced by the agent builder.

## Verification

Run:

```sh
cargo fmt -p waltr-agent
cargo test -p waltr-agent --test agent question_agent_loop_surfaces_tool_and_returns_structured_answers
cargo test -p waltr-agent --test agent question_tool_rejects_unanswerable_prompt_without_options_or_custom
cargo test -p waltr-agent --test agent live_question_agent_loop_asks_and_uses_structured_answers
cargo test -p waltr-agent --test agent live_question_agent_loop_asks_and_uses_structured_answers -- --ignored
cargo test -p waltr-agent
cargo clippy -p waltr-agent --all-targets
```

The ignored live test is documented in verification but should not be required for normal local or CI runs unless live-provider credentials or `WALTR_AGENT_LIVE_GATEWAY=1` are configured.
