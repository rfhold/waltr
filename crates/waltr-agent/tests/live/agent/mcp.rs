//! MCP agent integration tests.

use std::{net::SocketAddr, sync::Arc};

use axum::Router;
use waltr_agent::{
    AgentRuntime, CHANNEL_TOOL_CALL, CHANNEL_TOOL_RESULT, LlmError, LlmStream, ModelRef,
    TimelineContent,
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

    let agent = AgentRuntime::builder(model, Arc::clone(&provider))
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
    require_timeline_tool_execution(provider.name(), &session, &context)?;
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

fn assert_tool_call(
    session: &AgentTestSession,
    name: &str,
    context: &str,
) -> waltr_agent::Result<()> {
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
    Ok(())
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
