use crate::support::grpc::session_client;
use waltr::grpc::waltr::{
    config::v1::ProviderType,
    session::v1::{
        CreateSessionRequest, GetTimelineRequest, SubmitUserMessageRequest, TimelineDirection,
        TimelineQuery, timeline_content, watch_timeline_response,
    },
};
use waltr_server::{
    config_service::ConfigService,
    test_support::{
        default_catalog_loader, resource_state_with_overrides, setup_server_with_services,
    },
};

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
    assert_provider_submit_user_message(provider).await
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY"]
async fn anthropic_messages_submits_user_message() -> Result<(), Box<dyn std::error::Error>> {
    let Some(provider) = skip_if_missing(anthropic_messages_provider()) else {
        return Ok(());
    };
    assert_provider_submit_user_message(provider).await
}

#[tokio::test]
#[ignore = "requires WALTR_AGENT_LIVE_GATEWAY=1"]
async fn gateway_openai_chat_completions_submits_user_message()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(provider) = skip_if_missing(gateway_openai_chat_completions_provider()) else {
        return Ok(());
    };
    assert_provider_submit_user_message(provider).await
}

#[tokio::test]
#[ignore = "requires WALTR_AGENT_LIVE_GATEWAY=1"]
async fn gateway_anthropic_messages_submits_user_message() -> Result<(), Box<dyn std::error::Error>>
{
    let Some(provider) = skip_if_missing(gateway_anthropic_messages_provider()) else {
        return Ok(());
    };
    assert_provider_submit_user_message(provider).await
}

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY"]
async fn openai_chat_completions_watch_timeline_reconnects_with_cursor()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(provider) = skip_if_missing(openai_chat_completions_provider()) else {
        return Ok(());
    };
    assert_provider_watch_timeline_reconnects_with_cursor(provider).await
}

async fn assert_provider_submit_user_message(
    provider: LiveServerProvider,
) -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_live_server(provider).await;
    let mut client = session_client(server).await?;

    let session = client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");
    client
        .submit_user_message(SubmitUserMessageRequest {
            session_id: session.id.clone(),
            text: "Reply with one short sentence.".to_owned(),
        })
        .await?;

    let text = wait_for_assistant_text(&mut client, &session.id).await?;
    assert!(
        !text.trim().is_empty(),
        "assistant timeline text should not be empty"
    );

    Ok(())
}

async fn assert_provider_watch_timeline_reconnects_with_cursor(
    provider: LiveServerProvider,
) -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_live_server(provider).await;
    let mut client = session_client(server).await?;

    let session = client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");
    client
        .submit_user_message(SubmitUserMessageRequest {
            session_id: session.id.clone(),
            text: "Reply with a short sentence containing the word reconnect.".to_owned(),
        })
        .await?;

    let event = wait_for_assistant_event(&mut client, &session.id).await?;
    let after_cursor = event.cursor.saturating_sub(1);
    let mut watch = client
        .watch_timeline(waltr::grpc::waltr::session::v1::WatchTimelineRequest {
            session_id: session.id,
            after_cursor: Some(after_cursor),
            channels: vec!["assistant_text".to_owned()],
            initial_limit: None,
        })
        .await?
        .into_inner();

    let update = tokio::time::timeout(std::time::Duration::from_secs(5), watch.message())
        .await??
        .expect("watch should replay assistant event after cursor");
    let Some(watch_timeline_response::Update::Event(replayed)) = update.update else {
        panic!("timeline event should be present");
    };
    assert!(replayed.cursor > after_cursor);
    assert!(!assistant_text(&replayed).is_empty());

    Ok(())
}

async fn setup_live_server(
    provider: LiveServerProvider,
) -> &'static waltr_server::test_support::TestServer {
    let transport = match provider.provider_type {
        ProviderType::AnthropicMessages => "anthropicMessages",
        ProviderType::OpenaiChatCompletions => "openaiChatCompletions",
        _ => panic!("unsupported live provider transport"),
    };
    let provider_source = format!(
        "apiVersion: waltr.dev/v1alpha1\nkind: Provider\nmetadata:\n  name: {}\nspec:\n  transport: {transport}\n  url: {}\n  headers:\n    api_key: {:?}\n  models:\n    {:?}:\n      name: {:?}\n      type: 1\n",
        provider.key, provider.url, provider.api_key, provider.model_key, provider.model_key
    );
    let defaults_source = format!(
        "apiVersion: waltr.dev/v1alpha1\nkind: Defaults\nmetadata:\n  name: default\nspec:\n  inference:\n    provider: {}\n    model: {}\n",
        provider.key, provider.model_key
    );
    let resources = resource_state_with_overrides(&[
        (
            format!("providers/{}.yaml", provider.key).as_str(),
            provider_source.as_str(),
        ),
        ("defaults/default.yaml", defaults_source.as_str()),
    ]);
    let config_service = ConfigService::from_state(resources, default_catalog_loader());
    let server = setup_server_with_services(config_service);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    server
}

struct LiveServerProvider {
    key: &'static str,
    provider_type: ProviderType,
    url: &'static str,
    api_key: String,
    model_key: &'static str,
}

fn openai_chat_completions_provider() -> Result<LiveServerProvider, MissingLiveConfig> {
    Ok(LiveServerProvider {
        key: "openai",
        provider_type: ProviderType::OpenaiChatCompletions,
        url: "https://api.openai.com/v1/chat/completions",
        api_key: required_env("OPENAI_API_KEY")?,
        model_key: OPENAI_TEST_MODEL,
    })
}

fn anthropic_messages_provider() -> Result<LiveServerProvider, MissingLiveConfig> {
    Ok(LiveServerProvider {
        key: "anthropic",
        provider_type: ProviderType::AnthropicMessages,
        url: "https://api.anthropic.com/v1/messages",
        api_key: required_env("ANTHROPIC_API_KEY")?,
        model_key: ANTHROPIC_TEST_MODEL,
    })
}

fn gateway_openai_chat_completions_provider() -> Result<LiveServerProvider, MissingLiveConfig> {
    require_gateway()?;
    Ok(LiveServerProvider {
        key: "gateway-openai",
        provider_type: ProviderType::OpenaiChatCompletions,
        url: GATEWAY_BASE_URL,
        api_key: String::new(),
        model_key: GATEWAY_OPENAI_TEST_MODEL,
    })
}

fn gateway_anthropic_messages_provider() -> Result<LiveServerProvider, MissingLiveConfig> {
    require_gateway()?;
    Ok(LiveServerProvider {
        key: "gateway-anthropic",
        provider_type: ProviderType::AnthropicMessages,
        url: GATEWAY_BASE_URL,
        api_key: String::new(),
        model_key: GATEWAY_ANTHROPIC_TEST_MODEL,
    })
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

async fn wait_for_assistant_text(
    client: &mut waltr::grpc::waltr::session::v1::session_service_client::SessionServiceClient<
        tonic::transport::Channel,
    >,
    session_id: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    Ok(assistant_text(&wait_for_assistant_event(client, session_id).await?).to_owned())
}

async fn wait_for_assistant_event(
    client: &mut waltr::grpc::waltr::session::v1::session_service_client::SessionServiceClient<
        tonic::transport::Channel,
    >,
    session_id: &str,
) -> Result<waltr::grpc::waltr::session::v1::TimelineEvent, Box<dyn std::error::Error>> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_mins(1);
    while tokio::time::Instant::now() < deadline {
        let events = client
            .get_timeline(GetTimelineRequest {
                session_id: session_id.to_owned(),
                query: Some(TimelineQuery {
                    direction: TimelineDirection::Forward as i32,
                    cursor: None,
                    limit: 100,
                    channels: vec!["assistant_text".to_owned()],
                }),
            })
            .await?
            .into_inner()
            .events;
        if let Some(event) = events
            .into_iter()
            .find(|event| !assistant_text(event).trim().is_empty())
        {
            return Ok(event);
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    Err("assistant timeline text was not produced before timeout".into())
}

fn assistant_text(event: &waltr::grpc::waltr::session::v1::TimelineEvent) -> &str {
    let Some(content) = event
        .content
        .as_ref()
        .and_then(|content| content.content.as_ref())
    else {
        return "";
    };
    match content {
        timeline_content::Content::TextSnapshot(snapshot) => snapshot.text.as_str(),
        timeline_content::Content::TextDelta(delta) => delta.delta.as_str(),
        _ => "",
    }
}
