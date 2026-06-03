//! Model Context Protocol client integration.

mod config;
mod name;
mod protocol;
mod runtime;
mod server;
mod tool;
mod transport;

use std::{collections::HashMap, process::Stdio, sync::Arc, time::Duration};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
    time::timeout,
};

use crate::{AgentTool, LlmError, Result, ToolDefinition};

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// MCP server configuration collection.
#[derive(Debug, Clone)]
pub struct McpConfig {
    servers: Vec<McpServerConfig>,
    timeout: Duration,
}

impl McpConfig {
    /// Create an empty MCP configuration.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            servers: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Add one MCP server.
    #[must_use]
    pub fn with_server(mut self, server: McpServerConfig) -> Self {
        self.servers.push(server);
        self
    }

    /// Configure per-request timeout.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Convert this MCP configuration into host tools.
    ///
    /// # Errors
    ///
    /// Returns an error if any configured MCP server cannot be initialized or listed.
    pub async fn into_agent_tools(self) -> Result<Vec<AgentTool>> {
        let runtime = Arc::new(McpRuntime::new(self.servers, self.timeout));
        runtime.direct_agent_tools().await
    }
}

impl Default for McpConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// One configured MCP server.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    name: String,
    transport: McpTransportConfig,
}

impl McpServerConfig {
    /// Create a stdio MCP server configuration.
    #[must_use]
    pub fn stdio(
        name: impl Into<String>,
        command: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            name: name.into(),
            transport: McpTransportConfig::Stdio {
                command: command.into(),
                args: args.into_iter().map(Into::into).collect(),
            },
        }
    }

    /// Create a Streamable HTTP MCP server configuration.
    #[must_use]
    pub fn streamable_http(name: impl Into<String>, endpoint: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            transport: McpTransportConfig::StreamableHttp {
                endpoint: endpoint.into(),
            },
        }
    }
}

/// MCP transport configuration.
#[derive(Debug, Clone)]
pub enum McpTransportConfig {
    /// MCP stdio subprocess transport.
    Stdio {
        /// Command to spawn.
        command: String,
        /// Command arguments.
        args: Vec<String>,
    },
    /// MCP Streamable HTTP endpoint.
    StreamableHttp {
        /// MCP endpoint URL.
        endpoint: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct McpToolDefinition {
    name: String,
    description: String,
    input_schema: Value,
}

struct McpRuntime {
    servers: HashMap<String, Arc<McpServer>>,
}

impl McpRuntime {
    fn new(configs: Vec<McpServerConfig>, timeout: Duration) -> Self {
        Self {
            servers: configs
                .into_iter()
                .map(|config| {
                    let name = config.name.clone();
                    (name, Arc::new(McpServer::new(config, timeout)))
                })
                .collect(),
        }
    }

    async fn direct_agent_tools(self: &Arc<Self>) -> Result<Vec<AgentTool>> {
        let mut tools = Vec::new();
        for server in self.servers.values() {
            for tool in server.list_tools().await? {
                let runtime = Arc::clone(self);
                let server_name = server.name().to_owned();
                let tool_name = tool.name.clone();
                let definition = ToolDefinition::new(
                    direct_tool_name(&server_name, &tool_name),
                    format!(
                        "MCP tool `{tool_name}` from server `{server_name}`. {}",
                        tool.description
                    ),
                    tool.input_schema.clone(),
                );
                tools.push(AgentTool::with_context_async(
                    definition,
                    move |call, _context| {
                        let runtime = Arc::clone(&runtime);
                        let server_name = server_name.clone();
                        let tool_name = tool_name.clone();
                        Box::pin(async move {
                            runtime
                                .call_tool(&server_name, &tool_name, call.arguments.clone())
                                .await
                        })
                    },
                ));
            }
        }
        Ok(tools)
    }

    async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: Value,
    ) -> Result<String> {
        self.server(server_name)?
            .call_tool(tool_name, arguments)
            .await
    }

    fn server(&self, server_name: &str) -> Result<Arc<McpServer>> {
        self.servers
            .get(server_name)
            .cloned()
            .ok_or_else(|| protocol_error(format!("unknown MCP server `{server_name}`")))
    }
}

struct McpServer {
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
    fn new(config: McpServerConfig, timeout: Duration) -> Self {
        Self {
            name: config.name,
            transport: McpTransport::new(config.transport),
            timeout,
            state: Mutex::new(McpServerState::default()),
        }
    }

    fn name(&self) -> &str {
        &self.name
    }

    async fn list_tools(&self) -> Result<Vec<McpToolDefinition>> {
        self.ensure_initialized().await?;
        let mut state = self.state.lock().await;
        if let Some(tools) = &state.tools {
            return Ok(tools.clone());
        }
        let tools = self.fetch_tools().await?;
        state.tools = Some(tools.clone());
        drop(state);
        Ok(tools)
    }

    async fn call_tool(&self, tool: &str, arguments: Value) -> Result<String> {
        self.ensure_initialized().await?;
        let response = self
            .request(
                "tools/call",
                Some(json!({
                    "name": tool,
                    "arguments": arguments,
                })),
            )
            .await?;
        Ok(format_tool_result(&response))
    }

    async fn ensure_initialized(&self) -> Result<()> {
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
        self.notify("notifications/initialized", None).await?;
        self.state.lock().await.initialized = true;
        Ok(())
    }

    async fn fetch_tools(&self) -> Result<Vec<McpToolDefinition>> {
        let mut cursor = None;
        let mut tools = Vec::new();
        loop {
            let params = cursor.as_ref().map(|cursor| json!({ "cursor": cursor }));
            let response = self.request("tools/list", params).await?;
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

    async fn request(&self, method: &str, params: Option<Value>) -> Result<Value> {
        let id = next_id();
        let request = json_rpc_request(id, method, params);
        let response = timeout(self.timeout, self.transport.request(request, Some(id))).await;
        response.unwrap_or_else(|_| {
            Err(protocol_error(format!(
                "MCP request `{method}` timed out after {:?}",
                self.timeout
            )))
        })
    }

    async fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        let request = json_rpc_notification(method, params);
        timeout(self.timeout, self.transport.request(request, None))
            .await
            .map_err(|_| protocol_error(format!("MCP notification `{method}` timed out")))??;
        Ok(())
    }
}

enum McpTransport {
    Stdio(Box<StdioTransport>),
    StreamableHttp(StreamableHttpTransport),
}

impl McpTransport {
    fn new(config: McpTransportConfig) -> Self {
        match config {
            McpTransportConfig::Stdio { command, args } => {
                Self::Stdio(Box::new(StdioTransport::new(command, args)))
            }
            McpTransportConfig::StreamableHttp { endpoint } => {
                Self::StreamableHttp(StreamableHttpTransport::new(endpoint))
            }
        }
    }

    async fn request(&self, request: Value, id: Option<u64>) -> Result<Value> {
        match self {
            Self::Stdio(transport) => transport.request(request, id).await,
            Self::StreamableHttp(transport) => transport.request(request, id).await,
        }
    }
}

struct StdioTransport {
    command: String,
    args: Vec<String>,
    connection: Mutex<Option<StdioConnection>>,
}

struct StdioConnection {
    _child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
}

impl StdioTransport {
    fn new(command: String, args: Vec<String>) -> Self {
        Self {
            command,
            args,
            connection: Mutex::new(None),
        }
    }

    #[allow(clippy::significant_drop_tightening)]
    async fn request(&self, request: Value, id: Option<u64>) -> Result<Value> {
        let mut connection = self.connection.lock().await;
        if connection.is_none() {
            *connection = Some(self.spawn()?);
        }
        let connection = connection.as_mut().expect("stdio connection initialized");
        let line = serde_json::to_string(&request)?;
        connection.stdin.write_all(line.as_bytes()).await?;
        connection.stdin.write_all(b"\n").await?;
        connection.stdin.flush().await?;
        let Some(id) = id else {
            return Ok(json!(null));
        };
        loop {
            let Some(line) = connection.stdout.next_line().await? else {
                return Err(protocol_error("MCP stdio server closed stdout"));
            };
            let message: JsonRpcMessage = serde_json::from_str(&line)?;
            if message.id == Some(id) {
                return json_rpc_result(message);
            }
        }
    }

    fn spawn(&self) -> Result<StdioConnection> {
        let mut child = Command::new(&self.command)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| protocol_error("missing stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| protocol_error("missing stdout"))?;
        Ok(StdioConnection {
            _child: child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
        })
    }
}

struct StreamableHttpTransport {
    endpoint: String,
    client: reqwest::Client,
    session_id: Mutex<Option<String>>,
}

impl StreamableHttpTransport {
    fn new(endpoint: String) -> Self {
        Self {
            endpoint,
            client: reqwest::Client::new(),
            session_id: Mutex::new(None),
        }
    }

    async fn request(&self, request: Value, id: Option<u64>) -> Result<Value> {
        let session_id = self.session_id.lock().await.clone();
        let mut builder = self
            .client
            .post(&self.endpoint)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .json(&request);
        if let Some(session_id) = &session_id {
            builder = builder
                .header("mcp-session-id", session_id)
                .header("mcp-protocol-version", MCP_PROTOCOL_VERSION);
        }
        let response = builder.send().await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND && session_id.is_some() {
            *self.session_id.lock().await = None;
            return Err(protocol_error("MCP HTTP session expired"));
        }
        if !response.status().is_success() {
            return Err(protocol_error(format!(
                "MCP HTTP request failed with status {}",
                response.status()
            )));
        }
        if let Some(session_id) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            *self.session_id.lock().await = Some(session_id.to_owned());
        }
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        if content_type.contains("text/event-stream") {
            parse_sse_response(response, id).await
        } else if id.is_none() {
            Ok(json!(null))
        } else {
            json_rpc_result(response.json::<JsonRpcMessage>().await?)
        }
    }
}

#[derive(Debug, Deserialize)]
struct JsonRpcMessage {
    id: Option<u64>,
    result: Option<Value>,
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

#[derive(Debug, Serialize)]
struct JsonRpcRequest<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcNotification<'a> {
    jsonrpc: &'static str,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

fn json_rpc_request(id: u64, method: &str, params: Option<Value>) -> Value {
    serde_json::to_value(JsonRpcRequest {
        jsonrpc: "2.0",
        id,
        method,
        params,
    })
    .expect("request serializes")
}

fn json_rpc_notification(method: &str, params: Option<Value>) -> Value {
    serde_json::to_value(JsonRpcNotification {
        jsonrpc: "2.0",
        method,
        params,
    })
    .expect("notification serializes")
}

fn json_rpc_result(message: JsonRpcMessage) -> Result<Value> {
    if let Some(error) = message.error {
        return Err(protocol_error(format!(
            "JSON-RPC error {}: {}",
            error.code, error.message
        )));
    }
    Ok(message.result.unwrap_or(Value::Null))
}

async fn parse_sse_response(response: reqwest::Response, id: Option<u64>) -> Result<Value> {
    let Some(id) = id else {
        return Ok(json!(null));
    };
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    while let Some(chunk) = stream.next().await {
        buffer.push_str(&String::from_utf8_lossy(&chunk?));
        while let Some(index) = buffer.find("\n\n") {
            let event = buffer[..index].to_owned();
            buffer = buffer[index + 2..].to_owned();
            let data = event
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim_start)
                .collect::<Vec<_>>()
                .join("\n");
            if data.is_empty() {
                continue;
            }
            let message: JsonRpcMessage = serde_json::from_str(&data)?;
            if message.id == Some(id) {
                return json_rpc_result(message);
            }
        }
    }
    Err(protocol_error(format!(
        "MCP SSE response ended before JSON-RPC id {id}"
    )))
}

fn parse_tool_definition(value: &Value) -> Option<McpToolDefinition> {
    let name = value.get("name")?.as_str()?.to_owned();
    let description = value
        .get("description")
        .or_else(|| value.get("title"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let input_schema = value
        .get("inputSchema")
        .cloned()
        .unwrap_or_else(empty_object_schema);
    Some(McpToolDefinition {
        name,
        description,
        input_schema,
    })
}

fn format_tool_result(result: &Value) -> String {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut parts = Vec::new();
    for content in result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match content.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = content.get("text").and_then(Value::as_str) {
                    parts.push(text.to_owned());
                }
            }
            Some(other) => parts.push(format!("[unsupported MCP content type: {other}]")),
            None => {}
        }
    }
    if let Some(structured) = result.get("structuredContent") {
        parts.push(format!("structuredContent: {structured}"));
    }
    let content = if parts.is_empty() {
        result.to_string()
    } else {
        parts.join("\n")
    };
    if is_error {
        format!("MCP tool error: {content}")
    } else {
        content
    }
}

fn direct_tool_name(server: &str, tool: &str) -> String {
    format!(
        "mcp_{}_{}",
        sanitize_tool_part(server),
        sanitize_tool_part(tool)
    )
}

fn sanitize_tool_part(part: &str) -> String {
    part.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn empty_object_schema() -> Value {
    json!({ "type": "object", "properties": {} })
}

fn next_id() -> u64 {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn protocol_error(message: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        provider: "mcp",
        message: message.into(),
    }
}
