use waltr::grpc::waltr::session::v1::{
    SearchSessionTemplatesRequest, SessionTemplateKind, SessionTemplateSearchTarget,
    search_session_templates_response,
};
use waltr_agent::{ModelApi, SessionProviderRuntimeSettings, SessionRuntimeSettings};
use waltr_client::{DefaultGrpcTransport, WaltrTransport};

#[tokio::test]
async fn transport_returns_session_template_search_results()
-> Result<(), Box<dyn std::error::Error>> {
    let (server, sessions) = waltr_server::test_support::setup_server_with_sessions();
    let session_id = sessions
        .create_with_runtime_settings(Some(test_runtime_settings()), Default::default())
        .await?
        .id()
        .as_str()
        .to_owned();

    let response = DefaultGrpcTransport::new(server.base_url())
        .search_session_templates(SearchSessionTemplatesRequest {
            request_id: "tool".to_owned(),
            session_id,
            query: "rea".to_owned(),
            limit: 10,
            kinds: vec![SessionTemplateKind::Tool as i32],
            target: SessionTemplateSearchTarget::Resources as i32,
            cursor: None,
        })
        .await?;
    assert_eq!(response.request_id, "tool");
    let Some(search_session_templates_response::Result::Options(options)) = response.result else {
        panic!("expected options response");
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
