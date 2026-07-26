//! Ignored live prompt-cache tests across provider wire formats.

use std::fmt::Write as _;

use futures_util::StreamExt;
use waltr_agent::{
    CompletionEvent, CompletionRequest, CompletionTurn, LlmError, LlmStream, PromptTurn,
    TokenUsage, TurnItem,
};

use crate::live_provider;

const FIRST_PROMPT: &str = r"Reply with exactly: cache-alpha";
const SECOND_PROMPT: &str =
    r"The previous assistant response is part of this session. Reply with exactly: cache-beta";

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn anthropic_messages_caches_multi_turn_session() -> waltr_agent::Result<()> {
    let Some(provider) =
        live_provider::skip_if_missing_config(live_provider::anthropic_messages_provider(256))?
    else {
        return Ok(());
    };
    assert_multi_turn_session_is_cached(provider).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_chat_completions_caches_multi_turn_session() -> waltr_agent::Result<()> {
    let Some(provider) =
        live_provider::skip_if_missing_config(live_provider::openai_chat_completions_provider())?
    else {
        return Ok(());
    };
    assert_multi_turn_session_is_cached(provider).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_caches_multi_turn_session() -> waltr_agent::Result<()> {
    let Some(provider) =
        live_provider::skip_if_missing_config(live_provider::openai_responses_provider())?
    else {
        return Ok(());
    };
    assert_multi_turn_session_is_cached(provider).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_websocket_caches_multi_turn_session() -> waltr_agent::Result<()> {
    let Some(provider) = live_provider::skip_if_missing_config(
        live_provider::openai_responses_websocket_provider(),
    )?
    else {
        return Ok(());
    };
    assert_multi_turn_session_is_cached(provider).await
}

async fn assert_multi_turn_session_is_cached(provider: impl LlmStream) -> waltr_agent::Result<()> {
    let label = provider.name();
    let system_prompt = stable_cache_prompt();
    let first_request = first_turn_request(&system_prompt);
    let first = collect_provider_turn(&provider, first_request).await?;
    if first.text.trim().is_empty() {
        return Err(protocol_error(
            label,
            "first turn emitted no assistant text",
        ));
    }

    for attempt in 1..=3 {
        let second_prompt = SECOND_PROMPT.to_owned();
        let second_request = second_turn_request(&system_prompt, &second_prompt, &first.text);
        let second = collect_provider_turn(&provider, second_request).await?;
        if cache_read_tokens(second.usage) > 0 {
            return Ok(());
        }
        eprintln!(
            "{label}: no cache read tokens on multi-turn cache attempt {attempt}; usage={:?}",
            second.usage
        );
    }

    Err(protocol_error(
        label,
        "multi-turn request never reported cache read tokens",
    ))
}

const fn first_turn_request(system_prompt: &str) -> CompletionRequest<'_> {
    let mut request = CompletionRequest::new(FIRST_PROMPT);
    request.system_prompt = Some(system_prompt);
    request
}

fn second_turn_request<'a>(
    system_prompt: &'a str,
    current_prompt: &'a str,
    first_answer: &str,
) -> CompletionRequest<'a> {
    let mut request = CompletionRequest::new(current_prompt);
    request.system_prompt = Some(system_prompt);
    request.prompt_turns = vec![
        PromptTurn {
            turn: 0,
            prompt: FIRST_PROMPT.to_owned(),
        },
        PromptTurn {
            turn: 1,
            prompt: current_prompt.to_owned(),
        },
    ];
    request.turns = vec![CompletionTurn {
        turn: 0,
        metadata: None,
        items: vec![TurnItem::AssistantText(first_answer.to_owned())],
    }];
    request
}

fn stable_cache_prompt() -> String {
    let mut prompt = String::from(
        "You are validating provider prompt caching for a live multi-turn session. \
         Follow the latest user instruction exactly and keep replies short.\n",
    );
    for index in 0..260 {
        writeln!(
            prompt,
            "Stable cache prefix line {index}: deterministic instructions, examples, and policy text remain identical across turns."
        )
        .expect("writing to a string cannot fail");
    }
    prompt
}

#[derive(Default, Debug)]
struct ProviderTurn {
    text: String,
    usage: Option<TokenUsage>,
    finished: bool,
}

async fn collect_provider_turn(
    provider: &impl LlmStream,
    request: CompletionRequest<'_>,
) -> waltr_agent::Result<ProviderTurn> {
    let mut stream = provider.stream_request(request).await?;
    let mut collected = ProviderTurn::default();
    while let Some(event) = stream.next().await {
        match event? {
            CompletionEvent::Delta(delta) => collected.text.push_str(&delta),
            CompletionEvent::ProviderMetadata(metadata) => collected.usage = metadata.usage,
            CompletionEvent::Finished => collected.finished = true,
            CompletionEvent::ThinkingDelta(_)
            | CompletionEvent::ThinkingBlock(_)
            | CompletionEvent::ToolCall(_) => {}
        }
    }
    if !collected.finished {
        return Err(protocol_error(
            provider.name(),
            "provider stream ended without a finished event",
        ));
    }
    Ok(collected)
}

fn cache_read_tokens(usage: Option<TokenUsage>) -> u64 {
    usage
        .and_then(|usage| usage.cache_read_tokens)
        .unwrap_or_default()
}

fn protocol_error(provider: &'static str, message: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        provider,
        message: message.into(),
    }
}
