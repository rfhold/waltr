use crate::support::grpc::session_client;
use waltr::grpc::waltr::session::v1::{
    SearchSessionTemplatesRequest, SessionTemplateKind, SessionTemplateSearchTarget,
    search_session_templates_response,
};
use waltr_agent::{ModelApi, SessionProviderRuntimeSettings, SessionRuntimeSettings};
use waltr_server::test_support::setup_server_with_sessions;

#[tokio::test]
async fn search_session_templates_returns_tagged_tool_results()
-> Result<(), Box<dyn std::error::Error>> {
    let (server, sessions) = setup_server_with_sessions();
    let session_id = sessions
        .create_with_runtime_settings(Some(test_runtime_settings()), Default::default())
        .await?
        .id()
        .as_str()
        .to_owned();
    let mut client = session_client(server).await?;

    let empty = client
        .search_session_templates(SearchSessionTemplatesRequest {
            request_id: "empty".to_owned(),
            session_id: session_id.clone(),
            query: String::new(),
            limit: 10,
            kinds: vec![SessionTemplateKind::Tool as i32],
            target: SessionTemplateSearchTarget::Resources as i32,
            cursor: None,
        })
        .await?
        .into_inner();
    assert_eq!(empty.request_id, "empty");
    let Some(search_session_templates_response::Result::Options(options)) = empty.result else {
        panic!("expected empty options response");
    };
    assert!(options.options.is_empty());

    let tool = client
        .search_session_templates(SearchSessionTemplatesRequest {
            request_id: "tool".to_owned(),
            session_id,
            query: "rea".to_owned(),
            limit: 10,
            kinds: vec![SessionTemplateKind::Tool as i32],
            target: SessionTemplateSearchTarget::Resources as i32,
            cursor: None,
        })
        .await?
        .into_inner();
    assert_eq!(tool.request_id, "tool");
    let Some(search_session_templates_response::Result::Options(options)) = tool.result else {
        panic!("expected tool options response");
    };
    assert!(options.options.iter().any(|option| {
        option.kind == SessionTemplateKind::Tool as i32
            && option.label == "read"
            && option.template == "@tool(read, {\"filePath\":\"\"})"
    }));

    Ok(())
}

fn test_runtime_settings() -> SessionRuntimeSettings {
    SessionRuntimeSettings {
        provider: SessionProviderRuntimeSettings {
            key: "test".to_owned(),
            api: ModelApi::OpenAiResponses,
            url: String::new(),
            model_id: "test-model".to_owned(),
            model_cost: None,
            headers: Default::default(),
            oauth: None,
            reasoning_effort: None,
        },
        mcp_servers: Vec::new(),
        mcp_trace_propagation: false,
    }
}
