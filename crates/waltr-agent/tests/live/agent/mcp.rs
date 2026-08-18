//! MCP agent integration tests.

use std::{net::SocketAddr, sync::Arc};

use waltr_agent::{
    AgentRuntimeProvider, AgentSessionRuntimeFactory, AgentTimelineEvent, CHANNEL_ASSISTANT_TEXT,
    CHANNEL_TOOL_CALL, CHANNEL_TOOL_RESULT, FileCredentialStore, InferenceProviderSettings,
    InferenceSettings, LlmError, LlmStream, McpResolvedConnection, MemorySessionStore, ModelApi,
    ModelRef, RunConflictPolicy, SessionCoordinator, TimelineContent, TimelineEventKind,
    TimelineQuery, TimelineStatus,
};
use mcp::{
    McpToolCall, McpToolDefinition, McpToolResult,
    server::{ServerContext, ServerError, ServerResult},
};
use serde_json::json;
use tokio::{net::TcpListener, sync::oneshot};

use crate::{live_provider, scripted_provider::CapturingProvider, session::AgentTestSession};

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn live_mcp_agent_loop_covers_stdio_and_streamable_http_direct_tools()
-> waltr_agent::Result<()> {
    let Some(live) = live_provider::skip_if_missing_config(live_provider::selected_from_env(
        "WALTR_AGENT_LIVE_PROVIDER",
        "openai_responses_websocket",
    ))?
    else {
        return Ok(());
    };
    run_live_mcp(live.provider, live.model).await
}

async fn run_live_mcp(
    provider: impl LlmStream + 'static,
    model: ModelRef,
) -> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let http_server = HttpMcpTestServer::start().await?;
    let provider = Arc::new(CapturingProvider::new(provider));

    let prompt = "Use the mcp_stdio_test_echo_marker tool exactly once with value stdio-mcp-tool-called-707. Use the mcp_http_test_echo_marker tool exactly once with value http-mcp-tool-called-707. After both tool calls return, report a concise final status containing stdio-mcp-tool-called-707, http-mcp-tool-called-707, and mcp-flow-complete.";

    let sessions = mcp_sessions(
        Arc::clone(&provider),
        model,
        vec![
            stdio_runtime_server(&stdio_command),
            http_runtime_server(&http_server),
        ],
    );
    let session = run_session_prompt(&sessions, prompt).await?;
    crate::session::report_tool_errors(&provider, &session, "mcp live test");
    let context = format!(
        "{}\n{}",
        crate::session::session_context(&provider, &session),
        provider.tool_context()
    );
    let direct_results = tool_result_text(&session);

    assert_tool_was_surfaced(&provider, "mcp_stdio_test_echo_marker");
    assert_tool_was_surfaced(&provider, "mcp_http_test_echo_marker");
    assert_tool_was_not_surfaced(&provider, "search_mcp_tools");
    assert_tool_was_not_surfaced(&provider, "get_mcp_tool_details");
    assert_tool_was_not_surfaced(&provider, "call_mcp_tool");
    require_timeline_tool_execution(provider.name(), &session, &context)?;
    assert_tool_call(&session, "mcp_stdio_test_echo_marker", &context);
    assert_tool_call(&session, "mcp_http_test_echo_marker", &context);
    assert!(
        direct_results.contains("stdio-mcp-tool-called-707"),
        "{context}"
    );
    assert!(
        direct_results.contains("http-mcp-tool-called-707"),
        "{context}"
    );
    assert!(
        session.text.contains("stdio-mcp-tool-called-707"),
        "{context}"
    );
    assert!(
        session.text.contains("http-mcp-tool-called-707"),
        "{context}"
    );
    assert!(session.text.contains("mcp-flow-complete"), "{context}");

    Ok(())
}

fn assert_tool_was_surfaced(provider: &CapturingProvider<impl LlmStream>, name: &str) {
    let tools = provider.all_tool_names();
    assert!(
        tools.iter().any(|tool| tool == name),
        "{}",
        provider.tool_context()
    );
}

fn assert_tool_was_not_surfaced(provider: &CapturingProvider<impl LlmStream>, name: &str) {
    let tools = provider.all_tool_names();
    assert!(
        !tools.iter().any(|tool| tool == name),
        "{}",
        provider.tool_context()
    );
}

fn test_stdio_server_command() -> String {
    if let Some(command) = option_env!("CARGO_BIN_EXE_waltr-agent-test-mcp-server") {
        return command.to_owned();
    }
    let mut path = std::env::current_exe().expect("current test executable path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("waltr-agent-test-mcp-server");
    path.display().to_string()
}

fn stdio_runtime_server(command: &str) -> McpResolvedConnection {
    McpResolvedConnection {
        key: "stdio_test".to_owned(),
        kind: waltr_agent::McpConnectionKind::Stdio,
        command: command.to_owned(),
        url: String::new(),
    }
}

fn http_runtime_server(server: &HttpMcpTestServer) -> McpResolvedConnection {
    McpResolvedConnection {
        key: "http_test".to_owned(),
        kind: waltr_agent::McpConnectionKind::StreamableHttp,
        command: String::new(),
        url: format!("http://{}/mcp", server.address),
    }
}

fn mcp_sessions(
    provider: Arc<impl LlmStream + 'static>,
    model: ModelRef,
    connections: Vec<McpResolvedConnection>,
) -> SessionCoordinator<MemorySessionStore> {
    let root = tempfile::tempdir().expect("tempdir").keep();
    let credentials = FileCredentialStore::new(root.join("credentials.json"));
    let factory = AgentSessionRuntimeFactory::new(
        root,
        credentials,
        crate::agent_definitions::mcp_agent(true),
    )
    .with_runtime_provider(move |_| Ok(AgentRuntimeProvider::new(model.clone(), provider.clone())))
    .with_mcp_connection_resolver(move |_| {
        let connections = connections.clone();
        Box::pin(async move { Ok(connections) })
    });
    SessionCoordinator::new(MemorySessionStore::new()).with_runtime_factory(factory)
}

async fn run_session_prompt(
    sessions: &SessionCoordinator<MemorySessionStore>,
    prompt: &str,
) -> waltr_agent::Result<AgentTestSession> {
    let session_id = sessions.create().await?.id().clone();
    sessions
        .set_inference_settings(&session_id, Some(test_inference_settings()))
        .await?;
    let submission = sessions
        .submit_prompt(session_id.clone(), prompt, RunConflictPolicy::ReplaceActive)
        .await?;
    submission.handle.wait().await?;
    let events = sessions
        .timeline(&session_id, TimelineQuery::forward(None, 500))
        .await?;
    Ok(test_session_from_timeline(events))
}

fn test_inference_settings() -> InferenceSettings {
    InferenceSettings {
        provider: InferenceProviderSettings {
            key: "test".to_owned(),
            api: ModelApi::OpenAiResponses,
            url: String::new(),
            model_id: "live-model".to_owned(),
            model_cost: None,
            headers: std::collections::BTreeMap::default(),
            oauth: None,
            reasoning_effort: None,
        },
    }
}

fn test_session_from_timeline(timeline: Vec<AgentTimelineEvent>) -> AgentTestSession {
    let mut session = AgentTestSession::default();
    let mut saw_tool_result = false;
    for event in timeline {
        match &event.content {
            TimelineContent::TextDelta { delta }
                if crate::session::channel_is(&event, CHANNEL_ASSISTANT_TEXT) =>
            {
                if saw_tool_result && !delta.is_empty() {
                    session.saw_text_after_tool_result = true;
                }
                session.text.push_str(delta);
            }
            TimelineContent::ToolCall { call }
                if crate::session::channel_is(&event, CHANNEL_TOOL_CALL) =>
            {
                session.tool_calls.push(call.clone());
            }
            TimelineContent::ToolResult { result }
                if crate::session::channel_is(&event, CHANNEL_TOOL_RESULT) =>
            {
                session.tool_results.push(result.clone());
                saw_tool_result = true;
                if result.content.starts_with("Tool error:") {
                    session.tool_errors.push(result.clone());
                }
            }
            TimelineContent::ExternalRequest { request } => {
                session.external_requests.push(request.clone());
            }
            TimelineContent::Empty
                if event.kind == TimelineEventKind::SessionStatusChanged
                    && event.status == Some(TimelineStatus::Finished) =>
            {
                session.finished = true;
            }
            TimelineContent::Empty
            | TimelineContent::TextDelta { .. }
            | TimelineContent::TextSnapshot { .. }
            | TimelineContent::ContentRef { .. }
            | TimelineContent::Json { .. }
            | TimelineContent::ToolCall { .. }
            | TimelineContent::ToolResult { .. } => {}
        }
        session.timeline.push(event);
    }
    session
}

#[derive(Debug)]
struct HttpMcpTestServer {
    address: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
}

impl HttpMcpTestServer {
    async fn start() -> waltr_agent::Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let app = EchoMarkerServer.streamable_http_router();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    shutdown_receiver.await.ok();
                })
                .await
                .ok();
        });
        Ok(Self {
            address,
            shutdown: Some(shutdown_sender),
        })
    }
}

impl Drop for HttpMcpTestServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

fn tool_result_text(session: &AgentTestSession) -> String {
    session
        .timeline
        .iter()
        .filter_map(|event| {
            if crate::session::channel_is(event, CHANNEL_TOOL_RESULT)
                && let TimelineContent::ToolResult { result } = &event.content
            {
                Some(result.content.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_tool_call(session: &AgentTestSession, name: &str, context: &str) {
    assert!(
        session.timeline.iter().any(|event| {
            crate::session::channel_is(event, CHANNEL_TOOL_CALL)
                && matches!(&event.content, TimelineContent::ToolCall { call } if call.name == name)
        }),
        "{context}"
    );
    assert!(
        session.timeline.iter().any(|event| {
            crate::session::channel_is(event, CHANNEL_TOOL_RESULT)
                && matches!(&event.content, TimelineContent::ToolResult { result } if result.name == name)
        }),
        "{context}"
    );
}

fn require_timeline_tool_execution(
    provider: &'static str,
    session: &AgentTestSession,
    context: &str,
) -> waltr_agent::Result<()> {
    let has_tool_call = session.timeline.iter().any(|event| {
        crate::session::channel_is(event, CHANNEL_TOOL_CALL)
            && matches!(&event.content, TimelineContent::ToolCall { .. })
    });
    let has_tool_result = session.timeline.iter().any(|event| {
        crate::session::channel_is(event, CHANNEL_TOOL_RESULT)
            && matches!(&event.content, TimelineContent::ToolResult { .. })
    });
    let has_tool_call_and_result = has_tool_call && has_tool_result;
    if has_tool_call_and_result {
        return Ok(());
    }

    Err(LlmError::Protocol {
        provider,
        message: format!(
            "live MCP prompt did not execute a tool; provider received MCP tools but chose not to call them\n{context}"
        ),
    })
}

#[derive(Debug, serde::Deserialize)]
struct EchoMarkerRequest {
    value: String,
}

#[derive(Debug, Clone)]
struct EchoMarkerServer;

#[mcp::mcp_server(name = "waltr-live-test-mcp-server", version = env!("CARGO_PKG_VERSION"))]
impl EchoMarkerServer {
    #[tool(name = "echo_marker", definition = echo_marker_tool_definition())]
    fn call_echo_marker(call: McpToolCall, _context: ServerContext) -> ServerResult<McpToolResult> {
        call_echo_marker(call)
    }
}

fn echo_marker_tool_definition() -> McpToolDefinition {
    McpToolDefinition {
        name: "echo_marker".to_owned(),
        title: None,
        description: Some("Echo a deterministic marker.".to_owned()),
        icons: Vec::new(),
        input_schema: json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"]
        }),
        output_schema: None,
        annotations: None,
        meta: None,
    }
}

fn call_echo_marker(call: McpToolCall) -> ServerResult<McpToolResult> {
    if call.name != "echo_marker" {
        return Err(ServerError::invalid_params("unknown test tool"));
    }
    match serde_json::from_value::<EchoMarkerRequest>(call.arguments) {
        Ok(EchoMarkerRequest { value }) => Ok(McpToolResult::text(value)),
        Err(error) => Err(ServerError::invalid_params(error.to_string())),
    }
}
