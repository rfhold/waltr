use crate::support::grpc::{resource_client, session_client};
use waltr::grpc::waltr::{
    resource::v1::{InspectResourceRequest, ReplaceResourceRequest},
    session::v1::{
        CreateSessionRequest, InferenceSettingsMatch, InferenceSettingsRequest,
        ListSessionsRequest, UpdateSessionInferenceSettingsRequest, WatchSessionsRequest,
        watch_sessions_response,
    },
};
use waltr_server::test_support::setup_server;

#[tokio::test]
async fn create_session_persists_resource_defaults() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = session_client(server).await?;
    let session = client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session");
    let provider = session.inference_settings.unwrap().provider.unwrap();
    assert_eq!(provider.key, "codex");
    assert_eq!(provider.model_id, "codex/gpt-5.6");
    assert_eq!(provider.reasoning_effort.as_deref(), Some("high"));
    let listed = client
        .list_sessions(ListSessionsRequest::default())
        .await?
        .into_inner()
        .sessions;
    assert_eq!(
        listed[0]
            .inference_settings
            .as_ref()
            .unwrap()
            .provider
            .as_ref()
            .unwrap()
            .key,
        "codex"
    );
    Ok(())
}

#[tokio::test]
async fn explicit_settings_update_persists_and_publishes() -> Result<(), Box<dyn std::error::Error>>
{
    let server = setup_server();
    let mut mutation_client = session_client(server).await?;
    let mut watch_client = session_client(server).await?;
    let created = mutation_client
        .create_session(CreateSessionRequest {
            inference_settings: Some(InferenceSettingsRequest {
                provider_key: "openai".to_owned(),
                model_id: "gpt-5.6".to_owned(),
                reasoning_effort: Some("medium".to_owned()),
            }),
            ..CreateSessionRequest::default()
        })
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
            id: created.id,
            inference_settings: Some(InferenceSettingsRequest {
                provider_key: "openai".to_owned(),
                model_id: "gpt-5.6".to_owned(),
                reasoning_effort: Some("high".to_owned()),
            }),
        })
        .await?;
    let watched = match watch.message().await?.unwrap().event.unwrap() {
        watch_sessions_response::Event::Session(session) => session,
        other => panic!("expected session update, got {other:?}"),
    };
    assert_eq!(
        watched
            .inference_settings
            .unwrap()
            .provider
            .unwrap()
            .reasoning_effort
            .as_deref(),
        Some("high")
    );
    Ok(())
}

#[tokio::test]
async fn resource_change_reports_provider_drift() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut sessions = session_client(server).await?;
    let created = sessions
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .unwrap();
    let mut resources = resource_client(server).await?;
    let inspection = resources
        .inspect_resource(InspectResourceRequest {
            path: Some("providers/codex.yaml".to_owned()),
        })
        .await?
        .into_inner()
        .resources
        .remove(0);
    resources
        .replace_resource(ReplaceResourceRequest {
            path: "providers/codex.yaml".to_owned(),
            source: "apiVersion: waltr.dev/v1alpha1\nkind: Provider\nmetadata:\n  name: codex\nspec:\n  transport: openaiResponses\n  url: https://changed.example.test/responses\n  catalog:\n    provider: openai\n    include: [gpt-5.*]\n    keyPrefix: codex/\n".to_owned(),
            expected_etag: inspection.persisted_etag,
        })
        .await?;
    for _ in 0..20 {
        let settings = sessions
            .list_sessions(ListSessionsRequest::default())
            .await?
            .into_inner()
            .sessions
            .into_iter()
            .find(|session| session.id == created.id)
            .unwrap()
            .inference_settings
            .unwrap();
        if settings.provider.as_ref().unwrap().r#match == InferenceSettingsMatch::Changed as i32 {
            assert_eq!(
                settings.provider.unwrap().url,
                "https://chatgpt.com/backend-api/codex/responses"
            );
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Err("inference settings did not report resource drift".into())
}
