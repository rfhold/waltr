use std::collections::HashMap;

use crate::support::grpc::{config_client, get_config, session_client};
use waltr::grpc::waltr::{
    config::v1::{
        Configuration, McpServer, McpServerMapPatch, McpServerType, Model, ModelType,
        PatchConfigRequest, Provider, ProviderMapPatch, ProviderType, RuntimeDefaults,
    },
    session::v1::{
        CreateSessionRequest, ListSessionsRequest, RuntimeConfigMatch, SessionRuntimeConfig,
        SessionRuntimeConfigRequest, UpdateSessionRuntimeConfigRequest, WatchSessionsRequest,
        session_service_client::SessionServiceClient, watch_sessions_response,
    },
};
use waltr_server::{
    config::ConfigStore as _,
    config_service::ConfigService,
    test_support::{setup_server_with_services, temp_root},
};
use tonic::transport::Channel;

#[tokio::test]
async fn create_session_uses_runtime_defaults() -> Result<(), Box<dyn std::error::Error>> {
    let config_service = ConfigService::new(temp_root()).expect("config service should start");
    config_service
        .store()
        .save(config_with_defaults())
        .await
        .expect("config should save");
    let server = setup_server_with_services(config_service);
    wait_for_config_handle().await;
    let mut client = session_client(server).await?;

    let session = client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");
    assert_anthropic_runtime(
        session
            .runtime_config
            .expect("runtime config should be set"),
    );

    Ok(())
}

#[tokio::test]
async fn create_session_without_provider_default_keeps_runtime_unset()
-> Result<(), Box<dyn std::error::Error>> {
    let config_service = ConfigService::new(temp_root()).expect("config service should start");
    let mut config = config_with_defaults();
    config.runtime_defaults = Some(RuntimeDefaults {
        provider_key: String::new(),
        model_key: String::new(),
        mcp_default_enabled: Some(true),
        enabled_mcp_server_keys: Vec::new(),
    });
    config_service
        .store()
        .save(config)
        .await
        .expect("config should save");
    let server = setup_server_with_services(config_service);
    wait_for_config_handle().await;
    let mut client = session_client(server).await?;

    let session = client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");
    assert!(session.runtime_config.is_none());

    Ok(())
}

#[tokio::test]
async fn create_session_manual_runtime_config_wins_over_defaults()
-> Result<(), Box<dyn std::error::Error>> {
    let config_service = ConfigService::new(temp_root()).expect("config service should start");
    config_service
        .store()
        .save(config_with_defaults())
        .await
        .expect("config should save");
    let server = setup_server_with_services(config_service);
    wait_for_config_handle().await;
    let mut client = session_client(server).await?;

    let session = client
        .create_session(CreateSessionRequest {
            runtime_config: Some(SessionRuntimeConfigRequest {
                provider_key: "openai".to_owned(),
                model_id: "gpt".to_owned(),
                enabled_mcp_server_keys: Vec::new(),
            }),
        })
        .await?
        .into_inner()
        .session
        .expect("created session should be present");
    let runtime = session
        .runtime_config
        .expect("runtime config should be set");

    let provider = runtime.provider.expect("provider should be set");
    assert_eq!(provider.key, "openai");
    assert_eq!(provider.r#type, ProviderType::Openai as i32);
    assert_eq!(provider.url, "https://openai.example.test");
    assert_eq!(provider.model_id, "gpt");
    assert_eq!(provider.r#match, RuntimeConfigMatch::Configured as i32);
    assert!(runtime.mcp_servers.is_empty());

    Ok(())
}

#[tokio::test]
async fn update_runtime_config_publishes_session_upsert() -> Result<(), Box<dyn std::error::Error>>
{
    let config_service = ConfigService::new(temp_root()).expect("config service should start");
    config_service
        .store()
        .save(config_with_defaults())
        .await
        .expect("config should save");
    let server = setup_server_with_services(config_service);
    wait_for_config_handle().await;
    let mut mutation_client = session_client(server).await?;
    let mut watch_client = session_client(server).await?;
    let created = mutation_client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");
    let mut watch = watch_client
        .watch_sessions(WatchSessionsRequest::default())
        .await?
        .into_inner();
    let _snapshot = tokio::time::timeout(std::time::Duration::from_secs(2), watch.message())
        .await??
        .expect("snapshot should arrive");

    let updated = mutation_client
        .update_session_runtime_config(UpdateSessionRuntimeConfigRequest {
            id: created.id.clone(),
            runtime_config: Some(SessionRuntimeConfigRequest {
                provider_key: "openai".to_owned(),
                model_id: "gpt".to_owned(),
                enabled_mcp_server_keys: Vec::new(),
            }),
        })
        .await?
        .into_inner()
        .session
        .expect("updated session should be present");

    let update = tokio::time::timeout(std::time::Duration::from_secs(2), watch.message())
        .await??
        .expect("watch update should arrive");
    let watched = match update.event.expect("event should be present") {
        watch_sessions_response::Event::Session(session) => session,
        watch_sessions_response::Event::DeletedId(id) => {
            panic!("expected session, got deleted {id}")
        }
        watch_sessions_response::Event::Snapshot(snapshot) => {
            panic!(
                "expected session, got snapshot with {} sessions",
                snapshot.sessions.len()
            )
        }
    };

    assert_eq!(updated.id, created.id);
    assert!(updated.revision > created.revision);
    assert_eq!(watched.id, created.id);
    assert_eq!(watched.revision, updated.revision);
    let runtime = watched
        .runtime_config
        .expect("runtime config should be set");
    let provider = runtime.provider.expect("provider should be set");
    assert_eq!(provider.key, "openai");
    assert_eq!(provider.model_id, "gpt");
    assert_eq!(provider.r#match, RuntimeConfigMatch::Configured as i32);

    Ok(())
}

#[tokio::test]
async fn session_runtime_summary_marks_changed_provider_and_missing_mcp()
-> Result<(), Box<dyn std::error::Error>> {
    let config_service = ConfigService::new(temp_root()).expect("config service should start");
    config_service
        .store()
        .save(config_with_defaults())
        .await
        .expect("config should save");
    let server = setup_server_with_services(config_service);
    wait_for_config_handle().await;
    let mut session_client = session_client(server).await?;
    let mut config_client = config_client(server).await?;

    let created = session_client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");
    assert_anthropic_runtime(
        created
            .runtime_config
            .clone()
            .expect("runtime config should be set"),
    );

    patch_changed_provider_and_removed_mcp(&mut config_client).await?;

    let runtime = wait_for_runtime_match(
        &mut session_client,
        &created.id,
        RuntimeConfigMatch::Changed,
        RuntimeConfigMatch::Missing,
    )
    .await?;
    let provider = runtime.provider.expect("provider should be set");
    assert_eq!(provider.url, "https://anthropic.example.test");
    assert_eq!(provider.r#match, RuntimeConfigMatch::Changed as i32);
    assert_eq!(runtime.mcp_servers[0].key, "filesystem");
    assert_eq!(runtime.mcp_servers[0].command, "run-filesystem");
    assert_eq!(
        runtime.mcp_servers[0].r#match,
        RuntimeConfigMatch::Missing as i32
    );

    Ok(())
}

fn config_with_defaults() -> Configuration {
    Configuration {
        providers: HashMap::from([
            (
                "anthropic".to_owned(),
                provider("anthropic", ProviderType::Anthropic, "sonnet"),
            ),
            (
                "openai".to_owned(),
                provider("openai", ProviderType::Openai, "gpt"),
            ),
        ]),
        mcp_servers: HashMap::from([(
            "filesystem".to_owned(),
            McpServer {
                name: "filesystem".to_owned(),
                r#type: McpServerType::Stdio as i32,
                command: "run-filesystem".to_owned(),
                ..McpServer::default()
            },
        )]),
        runtime_defaults: Some(RuntimeDefaults {
            provider_key: "anthropic".to_owned(),
            model_key: "sonnet".to_owned(),
            mcp_default_enabled: None,
            enabled_mcp_server_keys: Vec::new(),
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
                ..Model::default()
            },
        )]),
        ..Provider::default()
    }
}

fn assert_anthropic_runtime(runtime: SessionRuntimeConfig) {
    let provider = runtime.provider.expect("provider should be set");
    assert_eq!(provider.key, "anthropic");
    assert_eq!(provider.r#type, ProviderType::Anthropic as i32);
    assert_eq!(provider.url, "https://anthropic.example.test");
    assert_eq!(provider.model_id, "sonnet");
    assert_eq!(provider.r#match, RuntimeConfigMatch::Configured as i32);

    assert_eq!(runtime.mcp_servers.len(), 1);
    let server = &runtime.mcp_servers[0];
    assert_eq!(server.key, "filesystem");
    assert_eq!(server.r#type, McpServerType::Stdio as i32);
    assert_eq!(server.command, "run-filesystem");
    assert!(server.url.is_empty());
    assert_eq!(server.r#match, RuntimeConfigMatch::Configured as i32);
}

async fn wait_for_runtime_match(
    client: &mut SessionServiceClient<Channel>,
    session_id: &str,
    provider_match: RuntimeConfigMatch,
    mcp_match: RuntimeConfigMatch,
) -> Result<SessionRuntimeConfig, Box<dyn std::error::Error>> {
    for _ in 0..20 {
        let sessions = client
            .list_sessions(ListSessionsRequest::default())
            .await?
            .into_inner()
            .sessions;
        let session = sessions
            .into_iter()
            .find(|session| session.id == session_id)
            .expect("session should be listed");
        let runtime = session
            .runtime_config
            .expect("runtime config should be set");
        let current_provider_match = runtime
            .provider
            .as_ref()
            .map(|provider| provider.r#match)
            .unwrap_or_default();
        let current_mcp_match = runtime
            .mcp_servers
            .first()
            .map(|server| server.r#match)
            .unwrap_or_default();
        if current_provider_match == provider_match as i32 && current_mcp_match == mcp_match as i32
        {
            return Ok(runtime);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    Err("runtime config did not reach expected match statuses".into())
}

async fn patch_changed_provider_and_removed_mcp(
    client: &mut waltr::grpc::waltr::config::v1::config_service_client::ConfigServiceClient<Channel>,
) -> Result<(), Box<dyn std::error::Error>> {
    for _ in 0..5 {
        let mut changed_config = get_config(client).await?;
        let expected_etag = changed_config.etag.clone();
        let provider = changed_config
            .providers
            .get_mut("anthropic")
            .expect("anthropic provider should exist");
        provider.url.clear();
        provider.url.push_str("https://changed.example.test");
        changed_config.mcp_servers.clear();

        let result = client
            .patch_config(PatchConfigRequest {
                expected_etag,
                providers: Some(ProviderMapPatch {
                    providers: changed_config.providers,
                }),
                mcp_servers: Some(McpServerMapPatch {
                    mcp_servers: changed_config.mcp_servers,
                }),
                runtime_defaults: None,
            })
            .await;
        match result {
            Ok(_) => return Ok(()),
            Err(status) if status.code() == tonic::Code::FailedPrecondition => {}
            Err(status) => return Err(status.into()),
        }
    }

    Err("config patch did not apply after etag retries".into())
}

async fn wait_for_config_handle() {
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
}
