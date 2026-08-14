use std::collections::HashMap;

use crate::support::grpc::{config_client, get_config, session_client};
use waltr::grpc::waltr::{
    config::v1::{
        Configuration, InferenceDefaults, Model, ModelOptions, ModelType, PatchConfigRequest,
        Provider, ProviderMapPatch, ProviderType,
    },
    session::v1::{
        CreateSessionRequest, InferenceSettings, InferenceSettingsMatch, InferenceSettingsRequest,
        ListSessionsRequest, UpdateSessionInferenceSettingsRequest, WatchSessionsRequest,
        session_service_client::SessionServiceClient, watch_sessions_response,
    },
};
use waltr_server::{
    config::ConfigStore as _,
    config_service::ConfigService,
    test_support::{default_config_builder, setup_server_with_services, temp_root},
};
use tonic::transport::Channel;

#[tokio::test]
async fn create_session_persists_inference_defaults() -> Result<(), Box<dyn std::error::Error>> {
    let config_service = ConfigService::new(temp_root(), default_config_builder())?;
    config_service.store().save(test_config()).await?;
    let server = setup_server_with_services(config_service);
    let mut client = session_client(server).await?;

    let session = client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session");
    assert_anthropic_settings(session.inference_settings.expect("inference settings"));

    let listed = client
        .list_sessions(ListSessionsRequest::default())
        .await?
        .into_inner()
        .sessions;
    assert_anthropic_settings(
        listed[0]
            .inference_settings
            .clone()
            .expect("persisted settings"),
    );
    Ok(())
}

#[tokio::test]
async fn explicit_create_settings_override_defaults() -> Result<(), Box<dyn std::error::Error>> {
    let config_service = ConfigService::new(temp_root(), default_config_builder())?;
    config_service.store().save(test_config()).await?;
    let server = setup_server_with_services(config_service);
    let mut client = session_client(server).await?;

    let session = client
        .create_session(CreateSessionRequest {
            inference_settings: Some(openai_request("medium")),
            ..CreateSessionRequest::default()
        })
        .await?
        .into_inner()
        .session
        .expect("created session");
    let provider = session
        .inference_settings
        .expect("inference settings")
        .provider
        .expect("provider");
    assert_eq!(provider.key, "openai");
    assert_eq!(provider.model_id, "gpt");
    assert_eq!(provider.reasoning_effort.as_deref(), Some("medium"));
    assert_eq!(provider.r#match, InferenceSettingsMatch::Configured as i32);
    Ok(())
}

#[tokio::test]
async fn update_inference_settings_persists_and_publishes() -> Result<(), Box<dyn std::error::Error>>
{
    let config_service = ConfigService::new(temp_root(), default_config_builder())?;
    config_service.store().save(test_config()).await?;
    let server = setup_server_with_services(config_service);
    let mut mutation_client = session_client(server).await?;
    let mut watch_client = session_client(server).await?;
    let created = mutation_client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session");
    let mut watch = watch_client
        .watch_sessions(WatchSessionsRequest::default())
        .await?
        .into_inner();
    watch.message().await?.expect("initial snapshot");

    mutation_client
        .update_session_inference_settings(UpdateSessionInferenceSettingsRequest {
            id: created.id.clone(),
            inference_settings: Some(openai_request("high")),
        })
        .await?;

    let update = watch.message().await?.expect("watch update");
    let watched = match update.event.expect("event") {
        watch_sessions_response::Event::Session(session) => session,
        other => panic!("expected session upsert, got {other:?}"),
    };
    let provider = watched
        .inference_settings
        .expect("inference settings")
        .provider
        .expect("provider");
    assert_eq!(provider.key, "openai");
    assert_eq!(provider.reasoning_effort.as_deref(), Some("high"));
    Ok(())
}

#[tokio::test]
async fn inference_summary_reports_provider_drift_only() -> Result<(), Box<dyn std::error::Error>> {
    let config_service = ConfigService::new(temp_root(), default_config_builder())?;
    config_service.store().save(test_config()).await?;
    let server = setup_server_with_services(config_service);
    let mut sessions = session_client(server).await?;
    let mut configs = config_client(server).await?;
    let created = sessions
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session");

    patch_anthropic_url(&mut configs).await?;
    let settings =
        wait_for_match(&mut sessions, &created.id, InferenceSettingsMatch::Changed).await?;
    let provider = settings.provider.expect("provider");
    assert_eq!(provider.url, "https://anthropic.example.test");
    assert_eq!(provider.r#match, InferenceSettingsMatch::Changed as i32);
    Ok(())
}

fn test_config() -> Configuration {
    Configuration {
        providers: HashMap::from([
            (
                "anthropic".to_owned(),
                provider("anthropic", ProviderType::AnthropicMessages, "sonnet"),
            ),
            (
                "openai".to_owned(),
                provider("openai", ProviderType::OpenaiChatCompletions, "gpt"),
            ),
        ]),
        inference_defaults: Some(InferenceDefaults {
            provider_key: "anthropic".to_owned(),
            model_key: "sonnet".to_owned(),
            reasoning_effort: None,
        }),
        ..Configuration::default()
    }
}

fn provider(name: &str, provider_type: ProviderType, model_key: &str) -> Provider {
    Provider {
        r#type: provider_type as i32,
        name: name.to_owned(),
        url: format!("https://{name}.example.test"),
        models: HashMap::from([(
            model_key.to_owned(),
            Model {
                name: model_key.to_owned(),
                r#type: ModelType::Llm as i32,
                options: Some(ModelOptions {
                    reasoning_efforts: vec!["medium".to_owned(), "high".to_owned()],
                    ..ModelOptions::default()
                }),
                ..Model::default()
            },
        )]),
        ..Provider::default()
    }
}

fn openai_request(effort: &str) -> InferenceSettingsRequest {
    InferenceSettingsRequest {
        provider_key: "openai".to_owned(),
        model_id: "gpt".to_owned(),
        reasoning_effort: Some(effort.to_owned()),
    }
}

fn assert_anthropic_settings(settings: InferenceSettings) {
    let provider = settings.provider.expect("provider");
    assert_eq!(provider.key, "anthropic");
    assert_eq!(provider.r#type, ProviderType::AnthropicMessages as i32);
    assert_eq!(provider.url, "https://anthropic.example.test");
    assert_eq!(provider.model_id, "sonnet");
    assert_eq!(provider.r#match, InferenceSettingsMatch::Configured as i32);
}

async fn wait_for_match(
    client: &mut SessionServiceClient<Channel>,
    session_id: &str,
    expected: InferenceSettingsMatch,
) -> Result<InferenceSettings, Box<dyn std::error::Error>> {
    for _ in 0..20 {
        let session = client
            .list_sessions(ListSessionsRequest::default())
            .await?
            .into_inner()
            .sessions
            .into_iter()
            .find(|session| session.id == session_id)
            .expect("listed session");
        let settings = session.inference_settings.expect("inference settings");
        if settings.provider.as_ref().map(|provider| provider.r#match) == Some(expected as i32) {
            return Ok(settings);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Err("inference settings did not reach expected match".into())
}

async fn patch_anthropic_url(
    client: &mut waltr::grpc::waltr::config::v1::config_service_client::ConfigServiceClient<Channel>,
) -> Result<(), Box<dyn std::error::Error>> {
    for _ in 0..5 {
        let mut config = get_config(client).await?;
        let expected_etag = config.etag.clone();
        "https://changed.example.test"
            .clone_into(&mut config.providers.get_mut("anthropic").expect("provider").url);
        match client
            .patch_config(PatchConfigRequest {
                expected_etag,
                providers: Some(ProviderMapPatch {
                    providers: config.providers,
                }),
                mcp_servers: None,
                inference_defaults: None,
            })
            .await
        {
            Ok(_) => return Ok(()),
            Err(status) if status.code() == tonic::Code::FailedPrecondition => {}
            Err(status) => return Err(status.into()),
        }
    }
    Err("config patch did not apply after etag retries".into())
}
