use std::time::Duration;

use serde_json::{Value, json};
use tokio::{sync::Mutex, time::timeout};
use tokio_util::sync::CancellationToken;

use crate::Result;

use super::{
    config::McpServerConfig,
    protocol::{
        MCP_PROTOCOL_VERSION, json_rpc_notification, json_rpc_request, next_id, protocol_error,
    },
    tool::{McpToolDefinition, format_tool_result, parse_tool_definition},
    transport::McpTransport,
};

pub(in crate::mcp) struct McpServer {
    name: String,
    transport: McpTransport,
    timeout: Duration,
    state: Mutex<McpServerState>,
}

#[derive(Debug, Default)]
struct McpServerState {
    initialized: bool,
    tools: Option<Vec<McpToolDefinition>>,
}

impl McpServer {
    pub(in crate::mcp) fn new(config: McpServerConfig, timeout: Duration) -> Self {
        Self {
            name: config.name,
            transport: McpTransport::new(config.transport),
            timeout,
            state: Mutex::new(McpServerState::default()),
        }
    }

    pub(in crate::mcp) fn name(&self) -> &str {
        &self.name
    }

    pub(in crate::mcp) async fn list_tools(
        &self,
        cancellation_token: CancellationToken,
    ) -> Result<Vec<McpToolDefinition>> {
        self.ensure_initialized(cancellation_token.clone()).await?;
        let mut state = self.state.lock().await;
        if let Some(tools) = &state.tools {
            return Ok(tools.clone());
        }
        let tools = self.fetch_tools(cancellation_token).await?;
        state.tools = Some(tools.clone());
        drop(state);
        Ok(tools)
    }

    pub(in crate::mcp) async fn call_tool(
        &self,
        tool: &str,
        arguments: Value,
        cancellation_token: CancellationToken,
    ) -> Result<String> {
        self.ensure_initialized(cancellation_token.clone()).await?;
        let response = self
            .request(
                "tools/call",
                Some(json!({
                    "name": tool,
                    "arguments": arguments,
                })),
                cancellation_token,
            )
            .await?;
        Ok(format_tool_result(&response))
    }

    async fn ensure_initialized(&self, cancellation_token: CancellationToken) -> Result<()> {
        if cancellation_token.is_cancelled() {
            return Err(protocol_error("MCP request canceled"));
        }
        if self.state.lock().await.initialized {
            return Ok(());
        }

        let response = self
            .request(
                "initialize",
                Some(json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "waltr-agent",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                })),
                cancellation_token.clone(),
            )
            .await?;
        let protocol = response
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(MCP_PROTOCOL_VERSION);
        if protocol != MCP_PROTOCOL_VERSION {
            return Err(protocol_error(format!(
                "unsupported MCP protocol version `{protocol}`"
            )));
        }
        self.notify("notifications/initialized", None, cancellation_token)
            .await?;
        self.state.lock().await.initialized = true;
        Ok(())
    }

    async fn fetch_tools(
        &self,
        cancellation_token: CancellationToken,
    ) -> Result<Vec<McpToolDefinition>> {
        let mut cursor = None;
        let mut tools = Vec::new();
        loop {
            if cancellation_token.is_cancelled() {
                return Err(protocol_error("MCP request canceled"));
            }
            let params = cursor.as_ref().map(|cursor| json!({ "cursor": cursor }));
            let response = self
                .request("tools/list", params, cancellation_token.clone())
                .await?;
            for tool in response
                .get("tools")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(tool) = parse_tool_definition(tool) {
                    tools.push(tool);
                }
            }
            cursor = response
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }

    async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        cancellation_token: CancellationToken,
    ) -> Result<Value> {
        let id = next_id();
        let request = json_rpc_request(id, method, params);
        self.transport
            .request(request, Some(id), self.timeout, cancellation_token)
            .await
            .map_err(|error| match error {
                crate::LlmError::Protocol {
                    provider: _,
                    message,
                } if message == "MCP request timed out" => protocol_error(format!(
                    "MCP request `{method}` timed out after {:?}",
                    self.timeout
                )),
                other => other,
            })
    }

    async fn notify(
        &self,
        method: &str,
        params: Option<Value>,
        cancellation_token: CancellationToken,
    ) -> Result<()> {
        if cancellation_token.is_cancelled() {
            return Err(protocol_error("MCP request canceled"));
        }
        let request = json_rpc_notification(method, params);
        timeout(
            self.timeout,
            self.transport
                .request(request, None, self.timeout, cancellation_token),
        )
        .await
        .map_err(|_| protocol_error(format!("MCP notification `{method}` timed out")))??;
        Ok(())
    }
}
