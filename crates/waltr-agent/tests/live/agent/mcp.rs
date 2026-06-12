//! MCP agent integration tests.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use axum::Router;
use waltr_agent::{
    Agent, AgentTranscriptEvent, BoxStreamFuture, CompletionRequest, LlmError, LlmStream, ModelRef,
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
use tokio::{net::TcpListener, sync::oneshot};

use crate::{live_provider, session::AgentTestSession};

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn live_mcp_agent_loop_covers_stdio_and_streamable_http_direct_tools()
-> waltr_agent::Result<()> {
    let live =
        live_provider::selected_from_env("WALTR_AGENT_LIVE_PROVIDER", "openai_responses_websocket")?;
    run_live_mcp(live.provider, live.model).await
}

async fn run_live_mcp(provider: impl LlmStream, model: ModelRef) -> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let http_server = HttpMcpTestServer::start().await?;
    let provider = ToolCapturingProvider::new(provider);

    let agent = Agent::builder(model)
        .with_mcp_servers(
            McpConfig::new()
                .with_server(stdio_server(&stdio_command))
                .with_server(http_server.server_config()),
        )
        .await?
        .build();
    let prompt = "Use the mcp_stdio_test_echo_marker tool exactly once with value stdio-mcp-tool-called-707. Use the mcp_http_test_echo_marker tool exactly once with value http-mcp-tool-called-707. After both tool calls return, report a concise final status containing stdio-mcp-tool-called-707, http-mcp-tool-called-707, and mcp-flow-complete.";

    let session = AgentTestSession::run(&agent, &provider, prompt).await?;
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
    require_transcript_tool_execution(provider.name(), &session, &context)?;
    assert_tool_call(&session, "mcp_stdio_test_echo_marker", &context)?;
    assert_tool_call(&session, "mcp_http_test_echo_marker", &context)?;
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

fn stdio_server(command: &str) -> McpServerConfig {
    McpServerConfig::stdio("stdio_test", command, std::iter::empty::<&str>())
}

struct ToolCapturingProvider<P> {
    inner: P,
    tools_by_turn: Mutex<Vec<Vec<String>>>,
}

impl<P> ToolCapturingProvider<P> {
    const fn new(inner: P) -> Self {
        Self {
            inner,
            tools_by_turn: Mutex::new(Vec::new()),
        }
    }

    fn all_tool_names(&self) -> Vec<String> {
        self.tools_by_turn
            .lock()
            .expect("captured tools")
            .iter()
            .flatten()
            .cloned()
            .collect()
    }

    fn tool_context(&self) -> String {
        let turns = self.tools_by_turn.lock().expect("captured tools").clone();
        if turns.is_empty() {
            return "captured provider tools: no provider requests captured".to_owned();
        }
        let lines = turns
            .iter()
            .enumerate()
            .map(|(index, tools)| format!("turn {index}: {}", tools.join(", ")))
            .collect::<Vec<_>>()
            .join("\n");
        format!("captured provider tools:\n{lines}")
    }
}

impl<P: LlmStream> LlmStream for ToolCapturingProvider<P> {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        self.tools_by_turn
            .lock()
            .expect("captured tools")
            .push(request.tools.iter().map(|tool| tool.name.clone()).collect());
        self.inner.stream_request(request)
    }
}

fn assert_tool_was_surfaced(provider: &ToolCapturingProvider<impl LlmStream>, name: &str) {
    let tools = provider.all_tool_names();
    assert!(
        tools.iter().any(|tool| tool == name),
        "{}",
        provider.tool_context()
    );
}

fn assert_tool_was_not_surfaced(provider: &ToolCapturingProvider<impl LlmStream>, name: &str) {
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
    let Some(snapshot) = &session.final_snapshot else {
        return String::new();
    };
    snapshot
        .state
        .events
        .iter()
        .filter_map(|event| match event {
            waltr_agent::AgentTranscriptEvent::ToolResult { result, .. } => {
                Some(result.content.as_str())
            }
            waltr_agent::AgentTranscriptEvent::Thinking { .. }
            | waltr_agent::AgentTranscriptEvent::Text { .. }
            | waltr_agent::AgentTranscriptEvent::ToolCall { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_tool_call(
    session: &AgentTestSession,
    name: &str,
    context: &str,
) -> waltr_agent::Result<()> {
    let snapshot = session
        .final_snapshot
        .as_ref()
        .ok_or_else(|| LlmError::Protocol {
            provider: "mcp",
            message: "agent stream ended without final snapshot".to_owned(),
        })?;
    assert!(
        snapshot.state.events.iter().any(
            |event| matches!(event, AgentTranscriptEvent::ToolCall { call, .. } if call.name == name)
        ),
        "{context}"
    );
    assert!(
        snapshot.state.events.iter().any(
            |event| matches!(event, AgentTranscriptEvent::ToolResult { result, .. } if result.name == name)
        ),
        "{context}"
    );
    Ok(())
}

fn require_transcript_tool_execution(
    provider: &'static str,
    session: &AgentTestSession,
    context: &str,
) -> waltr_agent::Result<()> {
    let has_tool_call_and_result = session.final_snapshot.as_ref().is_some_and(|snapshot| {
        snapshot
            .state
            .events
            .iter()
            .any(|event| matches!(event, AgentTranscriptEvent::ToolCall { .. }))
            && snapshot
                .state
                .events
                .iter()
                .any(|event| matches!(event, AgentTranscriptEvent::ToolResult { .. }))
    });
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
