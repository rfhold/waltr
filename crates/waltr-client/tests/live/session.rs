#![allow(clippy::future_not_send)]

use std::collections::HashMap;

use waltr::grpc::waltr::config::v1::{
    Configuration, InferenceDefaults, Model, ModelType, Provider, ProviderType,
};
use waltr_client::{ClientCommand, ClientRuntime, ClientSnapshot, TimelineContentView};
use waltr_server::{
    config_service::ConfigService,
    test_support::{default_catalog_loader, setup_server_with_services, temp_root},
};

use crate::support::snapshots::{dispatch_stream_command, no_sleep};

const OPENAI_TEST_MODEL: &str = "gpt-5-nano";
const ANTHROPIC_TEST_MODEL: &str = "claude-haiku-4-5";
const GATEWAY_ENV: &str = "WALTR_AGENT_LIVE_GATEWAY";
const GATEWAY_BASE_URL: &str = "https://agent-gateway.holdenitdown.net";
const GATEWAY_OPENAI_TEST_MODEL: &str = "openai/gpt-5-nano";
const GATEWAY_ANTHROPIC_TEST_MODEL: &str = "anthropic/claude-haiku-4-5";

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY"]
async fn openai_chat_completions_submits_user_message() -> Result<(), Box<dyn std::error::Error>> {
    let Some(provider) = skip_if_missing(openai_chat_completions_provider()) else {
        return Ok(());
    };
    assert_client_can_submit_user_message(provider).await
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY"]
async fn anthropic_messages_submits_user_message() -> Result<(), Box<dyn std::error::Error>> {
    let Some(provider) = skip_if_missing(anthropic_messages_provider()) else {
        return Ok(());
    };
    assert_client_can_submit_user_message(provider).await
}

#[tokio::test]
#[ignore = "requires WALTR_AGENT_LIVE_GATEWAY=1"]
async fn gateway_openai_chat_completions_submits_user_message()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(provider) = skip_if_missing(gateway_openai_chat_completions_provider()) else {
        return Ok(());
    };
    assert_client_can_submit_user_message(provider).await
}

#[tokio::test]
#[ignore = "requires WALTR_AGENT_LIVE_GATEWAY=1"]
async fn gateway_anthropic_messages_submits_user_message() -> Result<(), Box<dyn std::error::Error>>
{
    let Some(provider) = skip_if_missing(gateway_anthropic_messages_provider()) else {
        return Ok(());
    };
    assert_client_can_submit_user_message(provider).await
}

async fn assert_client_can_submit_user_message(
    provider: LiveClientProvider,
) -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_live_server(live_config(provider)).await;
    let mut stream = ClientRuntime::for_endpoint(server.base_url()).into_state_stream(no_sleep);

    wait_for_system_ready(&mut stream).await;
    let session_id = create_live_session(&mut stream).await;
    open_live_timeline_watch(&mut stream, &session_id).await;
    submit_and_assert_timeline_text(
        &mut stream,
        &session_id,
        "Reply with one short sentence.",
        |text| !text.trim().is_empty(),
    )
    .await;
    submit_and_assert_timeline_text(
        &mut stream,
        &session_id,
        "Reply with another short sentence.",
        |text| text.contains("Reply with another short sentence."),
    )
    .await;

    Ok(())
}

async fn create_live_session(stream: &mut waltr_client::ClientStateStream) -> String {
    dispatch_stream_command(
        stream,
        ClientCommand::CreateSession {
            agent_name: "coding".to_owned(),
        },
    )
    .await
    .expect("session create should dispatch");
    let snapshot = next_live_snapshot_until(stream, |snapshot| {
        snapshot.sessions.create.status == "ready" && !snapshot.sessions.current.is_empty()
    })
    .await;
    snapshot.sessions.current[0].id.clone()
}

async fn open_live_timeline_watch(stream: &mut waltr_client::ClientStateStream, session_id: &str) {
    dispatch_stream_command(
        stream,
        ClientCommand::RegisterSessionTimelineWatchInterest {
            session_id: session_id.to_owned(),
            after_cursor: Some(0),
            channels: vec!["user_text".to_owned(), "assistant_text".to_owned()],
            initial_limit: None,
        },
    )
    .await
    .expect("timeline watch interest should dispatch");
    next_live_snapshot_until(stream, |snapshot| {
        snapshot
            .sessions
            .timelines
            .get(session_id)
            .is_some_and(|timeline| timeline.watch.status == "opened")
    })
    .await;
}

async fn submit_and_assert_timeline_text(
    stream: &mut waltr_client::ClientStateStream,
    session_id: &str,
    text: &str,
    matches: impl Fn(&str) -> bool,
) {
    dispatch_stream_command(
        stream,
        ClientCommand::SubmitUserMessage {
            session_id: session_id.to_owned(),
            text: text.to_owned(),
        },
    )
    .await
    .expect("user message submit should dispatch");

    let snapshot = next_live_snapshot_until(stream, |snapshot| {
        snapshot
            .sessions
            .timelines
            .get(session_id)
            .is_some_and(|timeline| {
                timeline
                    .events
                    .iter()
                    .any(|event| matches(timeline_text(&event.content)))
            })
    })
    .await;
    assert_eq!(snapshot.sessions.submit_user_message.status, "ready");
}

async fn wait_for_system_ready(stream: &mut waltr_client::ClientStateStream) {
    next_live_snapshot_until(stream, |snapshot| {
        snapshot.status.ready && snapshot.status.watch.status == "received"
    })
    .await;
}

async fn next_live_snapshot_until(
    stream: &mut waltr_client::ClientStateStream,
    mut matches: impl FnMut(&ClientSnapshot) -> bool,
) -> ClientSnapshot {
    let mut last_snapshot = None;
    for _ in 0..64 {
        let snapshot = tokio::time::timeout(std::time::Duration::from_secs(30), stream.next())
            .await
            .expect("shared state stream should make live-test progress");
        if matches(&snapshot) {
            return snapshot;
        }
        last_snapshot = Some(snapshot);
    }

    panic!(
        "shared state stream did not reach expected live-test snapshot; last snapshot: {last_snapshot:?}"
    )
}

async fn setup_live_server(
    config: Configuration,
) -> &'static waltr_server::test_support::TestServer {
    drop(config);
    let root = temp_root();
    waltr_server::test_support::persist_test_self_mcp(&root);
    let config_service = ConfigService::new(root, default_catalog_loader())
        .await
        .expect("config service should start");
    let server = setup_server_with_services(config_service);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    server
}

fn live_config(provider: LiveClientProvider) -> Configuration {
    let provider_key = provider.key.to_owned();
    let model_key = provider.model_key.to_owned();

    Configuration {
        providers: HashMap::from([(
            provider_key.clone(),
            provider_config(
                provider.provider_type,
                provider.key,
                provider.url,
                provider.api_key,
                provider.model_key,
            ),
        )]),
        inference_defaults: Some(InferenceDefaults {
            provider_key,
            model_key,
            reasoning_effort: None,
        }),
        ..Configuration::default()
    }
}

struct LiveClientProvider {
    key: &'static str,
    provider_type: ProviderType,
    url: &'static str,
    api_key: String,
    model_key: &'static str,
}

fn openai_chat_completions_provider() -> Result<LiveClientProvider, MissingLiveConfig> {
    Ok(LiveClientProvider {
        key: "openai",
        provider_type: ProviderType::OpenaiChatCompletions,
        url: "https://api.openai.com/v1/chat/completions",
        api_key: required_env("OPENAI_API_KEY")?,
        model_key: OPENAI_TEST_MODEL,
    })
}

fn anthropic_messages_provider() -> Result<LiveClientProvider, MissingLiveConfig> {
    Ok(LiveClientProvider {
        key: "anthropic",
        provider_type: ProviderType::AnthropicMessages,
        url: "https://api.anthropic.com/v1/messages",
        api_key: required_env("ANTHROPIC_API_KEY")?,
        model_key: ANTHROPIC_TEST_MODEL,
    })
}

fn gateway_openai_chat_completions_provider() -> Result<LiveClientProvider, MissingLiveConfig> {
    require_gateway()?;
    Ok(LiveClientProvider {
        key: "gateway-openai",
        provider_type: ProviderType::OpenaiChatCompletions,
        url: GATEWAY_BASE_URL,
        api_key: String::new(),
        model_key: GATEWAY_OPENAI_TEST_MODEL,
    })
}

fn gateway_anthropic_messages_provider() -> Result<LiveClientProvider, MissingLiveConfig> {
    require_gateway()?;
    Ok(LiveClientProvider {
        key: "gateway-anthropic",
        provider_type: ProviderType::AnthropicMessages,
        url: GATEWAY_BASE_URL,
        api_key: String::new(),
        model_key: GATEWAY_ANTHROPIC_TEST_MODEL,
    })
}

fn provider_config(
    provider_type: ProviderType,
    name: &str,
    url: &str,
    header_value: String,
    model_key: &str,
) -> Provider {
    Provider {
        r#type: provider_type as i32,
        name: name.to_owned(),
        url: url.to_owned(),
        headers: HashMap::from([("api_key".to_owned(), header_value)]),
        models: HashMap::from([(
            model_key.to_owned(),
            Model {
                name: model_key.to_owned(),
                r#type: ModelType::Llm as i32,
                ..Model::default()
            },
        )]),
        ..Provider::default()
    }
}

#[derive(Debug)]
struct MissingLiveConfig {
    name: &'static str,
}

fn required_env(name: &'static str) -> Result<String, MissingLiveConfig> {
    std::env::var(name).map_err(|_| MissingLiveConfig { name })
}

fn require_gateway() -> Result<(), MissingLiveConfig> {
    match std::env::var(GATEWAY_ENV).as_deref() {
        Ok("1" | "true" | "TRUE" | "yes" | "YES") => Ok(()),
        _ => Err(MissingLiveConfig { name: GATEWAY_ENV }),
    }
}

fn skip_if_missing<T>(result: Result<T, MissingLiveConfig>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            eprintln!(
                "skipping live test: missing {}; set {} or use the matching live provider setup",
                error.name, error.name
            );
            None
        }
    }
}

impl std::fmt::Display for MissingLiveConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "missing live config {}", self.name)
    }
}

impl std::error::Error for MissingLiveConfig {}

fn timeline_text(content: &TimelineContentView) -> &str {
    match content {
        TimelineContentView::TextSnapshot { text } => text,
        TimelineContentView::TextDelta { delta } => delta,
        _ => "",
    }
}
