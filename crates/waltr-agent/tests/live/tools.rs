//! Live provider tool-call integration tests.

use futures_util::StreamExt;
use waltr_agent::{CompletionEvent, LlmError, LlmStream, ToolCall, ToolDefinition};
use serde_json::json;

use crate::live_provider;

const ECHO_VALUE: &str = "waltr-tool-echo-7391";

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn anthropic_messages_streams_echo_tool_call() -> waltr_agent::Result<()> {
    let Some(live) = live_provider::skip_if_missing_config(live_provider::anthropic_messages())?
    else {
        return Ok(());
    };
    assert_provider_calls_echo_tool(live.provider).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_chat_completions_streams_echo_tool_call() -> waltr_agent::Result<()> {
    let Some(live) =
        live_provider::skip_if_missing_config(live_provider::openai_chat_completions())?
    else {
        return Ok(());
    };
    assert_provider_calls_echo_tool(live.provider).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_streams_echo_tool_call() -> waltr_agent::Result<()> {
    let Some(live) = live_provider::skip_if_missing_config(live_provider::openai_responses())?
    else {
        return Ok(());
    };
    assert_provider_calls_echo_tool(live.provider).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_websocket_streams_echo_tool_call() -> waltr_agent::Result<()> {
    let Some(live) =
        live_provider::skip_if_missing_config(live_provider::openai_responses_websocket())?
    else {
        return Ok(());
    };
    assert_provider_calls_echo_tool(live.provider).await
}

async fn assert_provider_calls_echo_tool(provider: impl LlmStream) -> waltr_agent::Result<()> {
    let prompt = format!(
        "Call the test_echo tool with value exactly \"{ECHO_VALUE}\". Do not answer in text."
    );
    let mut stream = provider
        .prompt(&prompt)
        .with_tool(echo_tool_definition())
        .stream()
        .await?;

    let mut tool_call = None;
    while let Some(event) = stream.next().await {
        if let CompletionEvent::ToolCall(call) = event? {
            tool_call = Some(call);
            break;
        }
    }

    let tool_call = tool_call.ok_or_else(|| LlmError::Protocol {
        provider: provider.name(),
        message: "provider emitted no tool call".to_owned(),
    })?;

    assert_echo_tool_call(provider.name(), &tool_call);
    assert_eq!(execute_echo_tool(&tool_call)?, ECHO_VALUE);

    Ok(())
}

fn echo_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        "test_echo",
        "Echoes the provided value back to the caller.",
        json!({
            "type": "object",
            "properties": {
                "value": { "type": "string" }
            },
            "required": ["value"],
            "additionalProperties": false
        }),
    )
}

fn assert_echo_tool_call(provider: &str, tool_call: &ToolCall) {
    assert_eq!(tool_call.name, "test_echo", "{provider}");
    assert_eq!(tool_call.arguments["value"], ECHO_VALUE, "{provider}");
}

fn execute_echo_tool(tool_call: &ToolCall) -> waltr_agent::Result<String> {
    tool_call.arguments["value"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| LlmError::Protocol {
            provider: "test_echo",
            message: "test_echo value must be a string".to_owned(),
        })
}
