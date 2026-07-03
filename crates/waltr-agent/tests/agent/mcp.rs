//! MCP agent integration tests.

use std::{fs, net::SocketAddr, sync::Arc, time::Duration};

use axum::Router;
use futures_util::StreamExt;
use waltr_agent::{
    AgentRuntime, AgentSessionRuntimeFactory, AgentTimelineEvent, CHANNEL_ASSISTANT_TEXT,
    CHANNEL_TOOL_CALL, CHANNEL_TOOL_RESULT, CompletionEvent, FileCredentialStore, LlmStream,
    MemorySessionStore, ModelApi, ModelRef, PendingWorkStatus, ProviderId, SessionCoordinator,
    SessionMcpServerRuntimeKind, SessionMcpServerRuntimeSettings, SessionProviderRuntimeSettings,
    SessionRuntimeSettings, SessionStore, Suspension, TimelineContent, TimelineEventKind,
    TimelineQuery, TimelineStatus, ToolCall,
};
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::json;
use tokio::{net::TcpListener, sync::oneshot};

use crate::scripted_provider::{CapturingProvider, ScriptedProvider};
use crate::session::AgentTestSession;

const OPENAI_TEST_MODEL: &str = "gpt-5-nano";

#[tokio::test]
async fn mcp_agent_loop_discovers_and_calls_rmcp_stdio_and_streamable_http_direct_tools()
-> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let http_server = HttpMcpTestServer::start().await?;
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "scripted_mcp_provider",
        |request| direct_events(request.tool_results().count()),
    )));

    let sessions = mcp_sessions(provider.clone());
    let session = run_session_prompt(
        &sessions,
        "call direct MCP tools",
        vec![
            stdio_runtime_server(&stdio_command),
            http_runtime_server(&http_server),
        ],
    )
    .await?;
    session.require_tool_execution(provider.name())?;
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
    assert!(session.text.contains("mcp-flow-complete"), "{context}");

    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn mcp_cancellation_cancels_tool_work_and_preserves_stdio_runtime() -> waltr_agent::Result<()>
{
    let temp = tempfile::tempdir()?;
    let marker = temp.path().join("mcp-cancel-marker.txt");
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(ScriptedProvider::from_turns(
        "mcp_cancel_provider",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "mcp-cancel-call",
                "mcp_stdio_test_delayed_write_marker",
                json!({
                    "path": marker.display().to_string(),
                    "value": "foo",
                    "delay_ms": 5000_u64,
                }),
            ))],
            vec![CompletionEvent::Finished],
        ],
    ));
    let store = MemorySessionStore::new();
    let sessions = mcp_sessions_with_store(store.clone(), provider);
    let session_id = sessions.create().await?.id().clone();
    sessions
        .set_runtime_settings(
            &session_id,
            Some(test_runtime_settings(vec![stdio_runtime_server(
                &stdio_command,
            )])),
        )
        .await?;
    let submission = sessions
        .submit_prompt(
            session_id.clone(),
            "run delayed MCP write",
            waltr_agent::RunConflictPolicy::ReplaceActive,
        )
        .await?;
    let mut stream = sessions.watch_timeline(session_id.clone());

    loop {
        let update = stream.next().await.expect("tool call event");
        let waltr_agent::SessionTimelineEvent::Event(event) = update.event else {
            continue;
        };
        if crate::session::channel_is(&event, CHANNEL_TOOL_RESULT) {
            panic!("MCP tool finished before cancellation: {event:#?}");
        }
        if crate::session::channel_is(&event, CHANNEL_TOOL_CALL) {
            break;
        }
    }

    tokio::select! {
        event = stream.next() => panic!("MCP tool finished before cancellation: {event:#?}"),
        () = tokio::time::sleep(Duration::from_secs(1)) => {}
    }
    sessions.interrupt(&session_id).await?;
    drop(submission.handle.wait().await);

    let remaining = sessions
        .timeline(&session_id, TimelineQuery::forward(None, 100))
        .await?;

    assert!(
        remaining.iter().any(|event| {
            matches!(
                &event.content,
                TimelineContent::ToolResult { result }
                    if crate::session::channel_is(event, CHANNEL_TOOL_RESULT)
                        && result.content == "interrupted"
            )
        }),
        "canceled run should persist an interrupted MCP tool result: {remaining:#?}"
    );
    assert!(
        !marker.exists() || fs::read_to_string(&marker)?.is_empty(),
        "canceled MCP tool should not write marker before the server observes cancellation"
    );
    let record = store.load(&session_id).await.expect("stored session");
    assert!(record.snapshot().pending_work.iter().any(|work| {
        work.id == "mcp-cancel-call"
            && matches!(
                work.status,
                PendingWorkStatus::Canceled(waltr_agent::CancelReason::SessionInterrupted)
            )
    }));
    assert!(matches!(
        record.snapshot().status,
        waltr_agent::SessionStatus::Suspended(Suspension::Interrupted { .. })
    ));
    tokio::time::sleep(Duration::from_secs(5)).await;

    let recovery_provider = Arc::new(ScriptedProvider::from_turns(
        "mcp_recovery_provider",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "mcp-recovery-call",
                "mcp_stdio_test_echo_marker",
                json!({ "value": "stdio-after-cancel-909" }),
            ))],
            vec![CompletionEvent::Finished],
        ],
    ));
    let recovery_sessions = mcp_sessions(recovery_provider.clone());
    let recovery = run_session_prompt(
        &recovery_sessions,
        "recover MCP stdio",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;
    assert!(
        tool_result_text(&recovery).contains("stdio-after-cancel-909"),
        "{}",
        crate::session::session_context(&recovery_provider, &recovery)
    );

    Ok(())
}

fn stdio_runtime_server(command: &str) -> SessionMcpServerRuntimeSettings {
    SessionMcpServerRuntimeSettings {
        key: "stdio_test".to_owned(),
        kind: SessionMcpServerRuntimeKind::Stdio,
        command: command.to_owned(),
        url: String::new(),
    }
}

fn http_runtime_server(server: &HttpMcpTestServer) -> SessionMcpServerRuntimeSettings {
    SessionMcpServerRuntimeSettings {
        key: "http_test".to_owned(),
        kind: SessionMcpServerRuntimeKind::StreamableHttp,
        command: String::new(),
        url: format!("http://{}/mcp", server.address),
    }
}

fn mcp_sessions(provider: Arc<impl LlmStream + 'static>) -> SessionCoordinator<MemorySessionStore> {
    mcp_sessions_with_store(MemorySessionStore::new(), provider)
}

fn mcp_sessions_with_store(
    store: MemorySessionStore,
    provider: Arc<impl LlmStream + 'static>,
) -> SessionCoordinator<MemorySessionStore> {
    let root = tempfile::tempdir().expect("tempdir").keep();
    let credentials = FileCredentialStore::new(root.join("credentials.json"));
    let factory = AgentSessionRuntimeFactory::new(root, credentials)
        .with_runtime_builder(move |_| Ok(AgentRuntime::builder(test_model(), provider.clone())));
    SessionCoordinator::new(store).with_runtime_factory(factory)
}

async fn run_session_prompt(
    sessions: &SessionCoordinator<MemorySessionStore>,
    prompt: &str,
    mcp_servers: Vec<SessionMcpServerRuntimeSettings>,
) -> waltr_agent::Result<AgentTestSession> {
    let session_id = sessions.create().await?.id().clone();
    sessions
        .set_runtime_settings(&session_id, Some(test_runtime_settings(mcp_servers)))
        .await?;
    let submission = sessions
        .submit_prompt(
            session_id.clone(),
            prompt,
            waltr_agent::RunConflictPolicy::ReplaceActive,
        )
        .await?;
    submission.handle.wait().await?;
    let events = sessions
        .timeline(&session_id, TimelineQuery::forward(None, 500))
        .await?;
    Ok(test_session_from_timeline(events))
}

fn test_runtime_settings(
    mcp_servers: Vec<SessionMcpServerRuntimeSettings>,
) -> SessionRuntimeSettings {
    SessionRuntimeSettings {
        provider: SessionProviderRuntimeSettings {
            key: "test".to_owned(),
            api: ModelApi::OpenAiResponses,
            url: String::new(),
            model_id: OPENAI_TEST_MODEL.to_owned(),
            model_cost: None,
            headers: Default::default(),
            oauth: None,
            reasoning_effort: None,
        },
        mcp_servers,
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

fn direct_events(step: usize) -> Vec<waltr_agent::Result<CompletionEvent>> {
    match step {
        0 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
            "call_direct_stdio",
            "mcp_stdio_test_echo_marker",
            json!({ "value": "stdio-mcp-tool-called-707" }),
        )))],
        1 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
            "call_direct_http",
            "mcp_http_test_echo_marker",
            json!({ "value": "http-mcp-tool-called-707" }),
        )))],
        _ => vec![
            Ok(CompletionEvent::Delta("mcp-flow-complete".to_owned())),
            Ok(CompletionEvent::Finished),
        ],
    }
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
        let service: StreamableHttpService<EchoMarkerServer, LocalSessionManager> =
            StreamableHttpService::new(
                || Ok(EchoMarkerServer::new()),
                Arc::default(),
                StreamableHttpServerConfig::default().with_sse_keep_alive(None),
            );
        let app = Router::new().nest_service("/mcp", service);
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
        .tool_results
        .iter()
        .map(|result| result.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_tool_call(session: &AgentTestSession, name: &str, context: &str) {
    assert!(
        session.tool_calls.iter().any(|call| call.name == name),
        "{context}"
    );
    assert!(
        session
            .tool_results
            .iter()
            .any(|result| result.name == name),
        "{context}"
    );
}

fn test_model() -> ModelRef {
    ModelRef::new(
        ProviderId::OpenAi,
        OPENAI_TEST_MODEL,
        ModelApi::OpenAiResponses,
    )
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EchoMarkerRequest {
    value: String,
}

#[derive(Debug, Clone)]
struct EchoMarkerServer {
    tool_router: ToolRouter<Self>,
}

impl EchoMarkerServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl EchoMarkerServer {
    #[tool(name = "echo_marker", description = "Echo a deterministic marker.")]
    fn echo_marker(
        &self,
        Parameters(EchoMarkerRequest { value }): Parameters<EchoMarkerRequest>,
    ) -> String {
        let _ = &self.tool_router;
        value
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for EchoMarkerServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Echo deterministic test markers")
    }
}
