#![allow(dead_code)]

use std::collections::HashMap;

use waltr_agent::{
    CHANNEL_EXTERNAL_REQUEST, ExternalRequest, ExternalRequestId, ExternalRequestKind,
    ExternalRequestStatus, ExternalResponse, PendingTimelineEvent, SessionId, TimelineChannel,
    TimelineContent, TimelineEventKind, TimelineStatus,
};
use waltr_client::{
    Configuration, GrpcEffectExecutor, InferenceDefaults, McpServer, McpServerType, Model,
    ModelOptions, ModelType, Provider, ProviderType, ResourceOrigin, ResourceWrite,
};
pub fn server_endpoint() -> String {
    waltr_server::test_support::setup_server().base_url()
}

pub async fn server_endpoint_with_inference_config() -> Result<String, Box<dyn std::error::Error>> {
    let root = waltr_server::test_support::temp_root();
    waltr_server::test_support::persist_test_self_mcp(&root);
    let config_service = waltr_server::config_service::ConfigService::new(
        root,
        waltr_server::test_support::default_catalog_loader(),
    )
    .await?;
    let server = waltr_server::test_support::setup_server_with_services(config_service);
    let executor = GrpcEffectExecutor::for_endpoint(server.base_url());
    let configuration = inference_config();
    let absent_etag = absent_slot_etag(&executor).await?;
    for (name, provider) in configuration.providers {
        let model = provider
            .models
            .keys()
            .next()
            .expect("seeded provider model");
        executor
            .write_resource(ResourceWrite::Replace {
                path: format!("providers/{name}.yaml"),
                source: format!(
                    "apiVersion: waltr.dev/v1alpha1\nkind: Provider\nmetadata:\n  name: {name}\nspec:\n  transport: openaiChatCompletions\n  url: {}\n  models:\n    {model}:\n      name: {model}\n      type: 1\n      options:\n        reasoning_efforts: [low, medium, high]\n",
                    provider.url
                ),
                expected_etag: absent_etag.clone(),
            })
            .await?;
    }
    for (name, mcp_server) in configuration.mcp_servers {
        executor
            .write_resource(ResourceWrite::Replace {
                path: format!("mcp-servers/{name}.yaml"),
                source: format!(
                    "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata:\n  name: {name}\nspec:\n  transport: stdio\n  command: {}\n",
                    mcp_server.command
                ),
                expected_etag: absent_etag.clone(),
            })
            .await?;
    }
    if let Some(defaults) = configuration.inference_defaults {
        let catalog = executor.load_resource_catalog().await?;
        let baseline = catalog
            .resources
            .iter()
            .find(|resource| resource.path == "defaults/default.yaml")
            .ok_or("defaults baseline missing")?;
        executor
            .write_resource(ResourceWrite::Replace {
                path: "defaults/default.yaml".to_owned(),
                source: format!(
                    "apiVersion: waltr.dev/v1alpha1\nkind: Defaults\nmetadata:\n  name: default\nspec:\n  inference:\n    provider: {}\n    model: {}\n",
                    defaults.provider_key, defaults.model_key
                ),
                expected_etag: baseline.persisted_etag.clone(),
            })
            .await?;
    }
    Ok(server.base_url())
}

pub async fn absent_slot_etag(
    executor: &GrpcEffectExecutor,
) -> Result<String, Box<dyn std::error::Error>> {
    executor
        .load_resource_catalog()
        .await?
        .resources
        .into_iter()
        .find(|resource| ResourceOrigin::try_from(resource.origin) == Ok(ResourceOrigin::Bundled))
        .map(|resource| resource.persisted_etag)
        .ok_or_else(|| "absent slot baseline missing".into())
}

pub fn provider_resource_write(name: &str, expected_etag: String) -> ResourceWrite {
    ResourceWrite::Replace {
        path: format!("providers/{name}.yaml"),
        source: format!(
            "apiVersion: waltr.dev/v1alpha1\nkind: Provider\nmetadata:\n  name: {name}\nspec:\n  transport: anthropicMessages\n  url: https://{name}.example.test\n"
        ),
        expected_etag,
    }
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
                waltr_agent::PendingTimelineItemId::New,
                "hello",
            ),
            waltr_agent::PendingTimelineEvent::assistant_text_snapshot(
                session_id.clone(),
                1,
                waltr_agent::PendingTimelineItemId::New,
                "hi there",
            ),
        ];
        sessions.append_timeline(&session_id, events).await?;
    }

    let server = waltr_server::test_support::setup_server_with_seeded_sessions(sessions);
    Ok((server.base_url(), session_id.as_str().to_owned()))
}

pub async fn server_endpoint_with_rollback_timeline()
-> Result<(String, String), Box<dyn std::error::Error>> {
    let sessions = waltr_agent::SessionCoordinator::new(waltr_agent::MemorySessionStore::new());
    let session = sessions.create().await?;
    let session_id = session.id().clone();
    let events = vec![
        waltr_agent::PendingTimelineEvent::user_text_snapshot(
            session_id.clone(),
            1,
            waltr_agent::PendingTimelineItemId::New,
            "first prompt",
        ),
        waltr_agent::PendingTimelineEvent::user_text_snapshot(
            session_id.clone(),
            2,
            waltr_agent::PendingTimelineItemId::New,
            "second prompt",
        ),
        waltr_agent::PendingTimelineEvent::assistant_text_snapshot(
            session_id.clone(),
            2,
            waltr_agent::PendingTimelineItemId::New,
            "second answer",
        ),
    ];
    sessions.append_timeline(&session_id, events).await?;

    let server = waltr_server::test_support::setup_server_with_seeded_sessions(sessions);
    Ok((server.base_url(), session_id.as_str().to_owned()))
}

pub async fn server_endpoint_with_resolved_external_request()
-> Result<(String, String), Box<dyn std::error::Error>> {
    let sessions = waltr_agent::SessionCoordinator::new(waltr_agent::MemorySessionStore::new());
    let session = sessions.create().await?;
    let session_id = session.id().clone();
    sessions
        .append_timeline(
            &session_id,
            vec![
                external_request_event(&session_id, ExternalRequestStatus::Pending),
                external_request_event(
                    &session_id,
                    ExternalRequestStatus::Responded(ExternalResponse::new(
                        serde_json::json!({ "answers": [{ "header": "Choice" }] }),
                    )),
                ),
            ],
        )
        .await?;

    let server = waltr_server::test_support::setup_server_with_seeded_sessions(sessions);
    Ok((server.base_url(), session_id.as_str().to_owned()))
}

fn inference_config() -> Configuration {
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
        inference_defaults: Some(InferenceDefaults {
            provider_key: "openai".to_owned(),
            model_key: "gpt".to_owned(),
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

fn external_request_event(
    session_id: &SessionId,
    status: ExternalRequestStatus,
) -> PendingTimelineEvent {
    let kind = match status {
        ExternalRequestStatus::Pending => TimelineEventKind::ExternalRequestOpened,
        ExternalRequestStatus::Responded(_) | ExternalRequestStatus::Canceled(_) => {
            TimelineEventKind::ExternalRequestResolved
        }
    };
    let timeline_status = TimelineStatus::from(&status);
    let mut event = PendingTimelineEvent::new(
        session_id.clone(),
        1,
        waltr_agent::PendingTimelineItemId::New,
        kind,
        Some(TimelineChannel::from(CHANNEL_EXTERNAL_REQUEST)),
        TimelineContent::ExternalRequest {
            request: ExternalRequest {
                id: ExternalRequestId::new("question-1"),
                kind: ExternalRequestKind::Question,
                tool_call_id: Some("call-question".to_owned()),
                prompt: "Choose a path".to_owned(),
                payload: serde_json::json!({
                    "questions": [{
                        "header": "Choice",
                        "question": "Continue?",
                        "multiple": false,
                        "options": [{ "label": "Yes", "description": "Continue" }]
                    }]
                }),
                status,
            },
        },
    );
    event.status = Some(timeline_status);
    event
}
