//! MCP agent integration tests.

use std::{fs, net::SocketAddr, sync::Arc, time::Duration};

use axum::Router;
use futures_util::StreamExt;
use waltr_agent::{
    AgentRuntime, AgentSessions, CHANNEL_TOOL_CALL, CHANNEL_TOOL_RESULT, CompletionEvent, LlmError,
    LlmStream, MemorySessionStore, ModelApi, ModelRef, ProviderId, SessionStore, ToolCall,
    mcp::{McpConfig, McpServerConfig},
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
use tokio_util::sync::CancellationToken;

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

    let agent = AgentRuntime::builder(test_model(), provider.clone())
        .with_mcp_servers(
            McpConfig::new()
                .with_server(stdio_server(&stdio_command))
                .with_server(http_server.server_config()),
        )
        .await?
        .build();
    let session = AgentTestSession::run(&agent, &provider, "call direct MCP tools").await?;
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
async fn mcp_tool_discovery_observes_cancellation() -> waltr_agent::Result<()> {
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let provider = Arc::new(ScriptedProvider::from_turns(
        "unused_canceled_mcp_provider",
        vec![vec![CompletionEvent::Finished]],
    ));
    let error = AgentRuntime::builder(test_model(), provider)
        .with_mcp_servers_with_cancellation(
            McpConfig::new().with_server(stdio_server("command-that-should-not-run")),
            cancellation,
        )
        .await
        .expect_err("canceled MCP discovery should fail");

    assert!(matches!(
        error,
        LlmError::Protocol {
            provider: "mcp",
            message
        } if message == "MCP request canceled"
    ));

    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn mcp_cancellation_kills_stdio_tool_work() -> waltr_agent::Result<()> {
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
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_mcp_servers(McpConfig::new().with_server(stdio_server(&stdio_command)))
        .await?
        .build();
    let store = MemorySessionStore::new();
    let sessions = AgentSessions::new(store.clone());
    let session_id = sessions.create().await?.id().clone();
    let cancellation = CancellationToken::new();
    let mut stream = sessions
        .prompt(&agent, session_id.clone(), "run delayed MCP write")
        .with_cancellation_token(cancellation.clone())
        .stream();

    let first = stream
        .next()
        .await
        .expect("tool call event")
        .expect("tool call event");
    assert!(crate::session::channel_is(&first, CHANNEL_TOOL_CALL));

    tokio::select! {
        event = stream.next() => panic!("MCP tool finished before cancellation: {event:#?}"),
        () = tokio::time::sleep(Duration::from_secs(1)) => cancellation.cancel(),
    }

    let remaining = stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| event.expect("agent event"))
        .collect::<Vec<_>>();
    tokio::time::sleep(Duration::from_secs(5)).await;

    assert!(
        remaining
            .iter()
            .all(|event| !crate::session::channel_is(event, CHANNEL_TOOL_RESULT)),
        "canceled run should not finish the MCP tool: {remaining:#?}"
    );
    assert!(
        !marker.exists() || fs::read_to_string(&marker)?.is_empty(),
        "canceled MCP tool should not write marker"
    );
    let record = store.load(&session_id).await.expect("stored session");
    assert!(record.snapshot().pending_work.iter().any(|work| {
        work.id == "mcp-cancel-call"
            && matches!(
                work.status,
                waltr_agent::PendingWorkStatus::Canceled(
                    waltr_agent::CancelReason::SessionInterrupted
                )
            )
    }));
    assert!(matches!(
        record.snapshot().status,
        waltr_agent::SessionStatus::Suspended(waltr_agent::Suspension::Interrupted { .. })
    ));

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
    let recovery_agent = AgentRuntime::builder(test_model(), recovery_provider.clone())
        .with_mcp_servers(McpConfig::new().with_server(stdio_server(&stdio_command)))
        .await?
        .build();
    let recovery =
        AgentTestSession::run(&recovery_agent, &recovery_provider, "recover MCP stdio").await?;
    assert!(
        tool_result_text(&recovery).contains("stdio-after-cancel-909"),
        "{}",
        crate::session::session_context(&recovery_provider, &recovery)
    );

    Ok(())
}

fn stdio_server(command: &str) -> McpServerConfig {
    McpServerConfig::stdio("stdio_test", command, std::iter::empty::<&str>())
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

    fn server_config(&self) -> McpServerConfig {
        McpServerConfig::streamable_http("http_test", format!("http://{}/mcp", self.address))
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
