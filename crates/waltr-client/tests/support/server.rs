#![allow(dead_code)]

use std::collections::HashMap;

use waltr_client::{
    Configuration, McpServer, McpServerType, Model, ModelOptions, ModelType, Provider,
    ProviderType, RuntimeDefaults,
};
use waltr_server::config::ConfigStore as _;

pub fn server_endpoint() -> String {
    waltr_server::test_support::setup_server().base_url()
}

pub async fn server_endpoint_with_runtime_config() -> Result<String, Box<dyn std::error::Error>> {
    let config_service =
        waltr_server::config_service::ConfigService::new(waltr_server::test_support::temp_root())?;
    config_service.store().save(runtime_config()).await?;
    Ok(waltr_server::test_support::setup_server_with_services(config_service).base_url())
}

pub async fn server_endpoint_with_seeded_timeline(
    seed_text: bool,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    let sessions = waltr_agent::SessionCoordinator::new(waltr_agent::MemorySessionStore::new());
    let session = sessions.create().await?;
    let session_id = session.id().clone();
    if seed_text {
        let events = vec![
            waltr_agent::PendingTimelineEvent::user_text_snapshot(
                session_id.clone(),
                1,
                "user-1",
                "hello",
            ),
            waltr_agent::PendingTimelineEvent::assistant_text_snapshot(
                session_id.clone(),
                1,
                "assistant-1",
                "hi there",
            ),
        ];
        sessions.append_timeline(&session_id, events).await?;
    }

    let server = waltr_server::test_support::setup_server_with_seeded_sessions(sessions);
    Ok((server.base_url(), session_id.as_str().to_owned()))
}

fn runtime_config() -> Configuration {
    Configuration {
        providers: HashMap::from([("openai".to_owned(), provider("openai", "gpt"))]),
        mcp_servers: HashMap::from([(
            "git".to_owned(),
            McpServer {
                name: "git".to_owned(),
                r#type: McpServerType::Stdio as i32,
                command: "git-mcp".to_owned(),
                ..McpServer::default()
            },
        )]),
        runtime_defaults: Some(RuntimeDefaults {
            provider_key: "openai".to_owned(),
            model_key: "gpt".to_owned(),
            mcp_default_enabled: Some(false),
            enabled_mcp_server_keys: vec!["git".to_owned()],
            reasoning_effort: None,
        }),
        ..Configuration::default()
    }
}

fn provider(name: &str, model_key: &str) -> Provider {
    Provider {
        r#type: ProviderType::OpenaiChatCompletions as i32,
        name: name.to_owned(),
        url: format!("https://{name}.example.test"),
        models: HashMap::from([(
            model_key.to_owned(),
            Model {
                name: model_key.to_owned(),
                r#type: ModelType::Llm as i32,
                options: Some(ModelOptions {
                    reasoning_efforts: vec![
                        "low".to_owned(),
                        "medium".to_owned(),
                        "high".to_owned(),
                    ],
                    ..ModelOptions::default()
                }),
                ..Model::default()
            },
        )]),
        ..Provider::default()
    }
}
