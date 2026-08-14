#![allow(clippy::default_trait_access)]

use waltr_agent::{InferenceProviderSettings, InferenceSettings, ModelApi};
use waltr_client::{
    ClientCommand, ClientRuntime, ClientSessionTemplateKind, ClientSessionTemplateSearchTarget,
};

use crate::support::snapshots::{dispatch_stream_command, next_snapshot_until, no_sleep};

#[tokio::test]
async fn client_runtime_returns_session_template_search_results()
-> Result<(), Box<dyn std::error::Error>> {
    let sessions = waltr_agent::SessionCoordinator::new(waltr_agent::MemorySessionStore::new());
    let session_id = sessions
        .create_with_inference_settings(Some(test_inference_settings()), None, Default::default())
        .await?
        .id()
        .as_str()
        .to_owned();
    let server = waltr_server::test_support::setup_server_with_seeded_sessions(sessions);

    let mut stream = ClientRuntime::for_endpoint(server.base_url()).into_state_stream(no_sleep);
    next_snapshot_until(&mut stream, |snapshot| snapshot.status.ready).await;

    let started = dispatch_stream_command(
        &mut stream,
        ClientCommand::SearchSessionTemplates {
            request_id: "tool-search-1".to_owned(),
            session_id: session_id.clone(),
            query: "rea".to_owned(),
            limit: 10,
            kinds: vec![ClientSessionTemplateKind::Tool],
            target: ClientSessionTemplateSearchTarget::Resources,
            cursor: None,
        },
    )
    .await?;
    let started_search = started
        .sessions
        .template_searches
        .get(&session_id)
        .expect("template search should be exposed immediately");
    assert_eq!(started_search.request_id, "tool-search-1");
    assert_eq!(started_search.status, "loading");

    let completed = next_snapshot_until(&mut stream, |snapshot| {
        snapshot
            .sessions
            .template_searches
            .get(&session_id)
            .is_some_and(|search| search.request_id == "tool-search-1" && search.status == "ready")
    })
    .await;
    let search = &completed.sessions.template_searches[&session_id];
    assert!(search.error.is_none());
    assert!(search.options.iter().any(|option| {
        option.kind == "tool"
            && option.label == "read"
            && option.template == "@tool(read, {\"filePath\":\"\"})"
    }));

    Ok(())
}

fn test_inference_settings() -> InferenceSettings {
    InferenceSettings {
        provider: InferenceProviderSettings {
            key: "test".to_owned(),
            api: ModelApi::OpenAiResponses,
            url: String::new(),
            model_id: "test-model".to_owned(),
            model_cost: None,
            headers: Default::default(),
            oauth: None,
            reasoning_effort: None,
        },
    }
}
