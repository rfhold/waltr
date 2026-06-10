//! Live provider streaming integration tests.

use futures_util::StreamExt;
use waltr_agent::{CompletionEvent, LlmError, LlmStream};

use crate::live_provider;

const TEST_PROMPT: &str = r#"say exactly "hello from waltr agent""#;
const EXPECTED_TEXT: &str = "hello from waltr agent";

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn anthropic_messages_streams_prompt_to_expected_final_text() -> waltr_agent::Result<()> {
    assert_provider_streams_expected_text(live_provider::anthropic_messages()?.provider).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_chat_completions_streams_prompt_to_expected_final_text() -> waltr_agent::Result<()> {
    assert_provider_streams_expected_text(live_provider::openai_chat_completions()?.provider).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_streams_prompt_to_expected_final_text() -> waltr_agent::Result<()> {
    assert_provider_streams_expected_text(live_provider::openai_responses()?.provider).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_websocket_streams_prompt_to_expected_final_text() -> waltr_agent::Result<()>
{
    assert_provider_streams_expected_text(live_provider::openai_responses_websocket()?.provider)
        .await
}

async fn assert_provider_streams_expected_text(provider: impl LlmStream) -> waltr_agent::Result<()> {
    let mut stream = provider.stream_prompt(TEST_PROMPT).await?;
    let mut text = String::new();
    let mut delta_count = 0_u16;
    let mut finished = false;

    while let Some(event) = stream.next().await {
        match event? {
            CompletionEvent::Delta(delta) => {
                delta_count += 1;
                text.push_str(&delta);
            }
            CompletionEvent::ToolCall(tool_call) => {
                return Err(LlmError::Protocol {
                    provider: provider.name(),
                    message: format!("provider emitted unexpected tool call: {}", tool_call.name),
                });
            }
            CompletionEvent::ThinkingDelta(_)
            | CompletionEvent::ThinkingBlock(_)
            | CompletionEvent::ProviderMetadata(_) => {}
            CompletionEvent::Finished => finished = true,
        }
    }

    if delta_count == 0 {
        return Err(LlmError::Protocol {
            provider: provider.name(),
            message: "provider emitted no streaming deltas".to_owned(),
        });
    }

    if !finished {
        return Err(LlmError::Protocol {
            provider: provider.name(),
            message: "provider stream ended without a finished event".to_owned(),
        });
    }

    assert_eq!(text.trim(), EXPECTED_TEXT, "{}", provider.name());

    Ok(())
}
