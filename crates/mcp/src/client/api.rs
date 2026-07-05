use std::{sync::Arc, time::Duration};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    McpCompletionRequest, McpCompletionResult, McpLogLevel, McpProgressNotification, McpPromptList,
    McpPromptResult, McpResourceList, McpResourceResult, McpResourceTemplateList, McpRoot,
    McpServerCapabilities, McpToolCall, McpToolList, McpToolResult, McpTracePropagation, Result,
    protocol::{
        JsonRpcNotification, JsonRpcRequest, MCP_PROTOCOL_VERSION, json_rpc_method_not_found,
        json_rpc_success,
    },
    trace::inject_request_context,
    types::parse_completion,
};

use super::{connection::McpConnection, transport::McpTransportConfig};

#[derive(Debug, Clone)]
/// Options used when connecting an MCP client.
pub struct McpClientOptions {
    /// Client implementation name sent during initialize.
    pub client_name: String,
    /// Client implementation version sent during initialize.
    pub client_version: String,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Optional request-level trace propagation.
    pub trace_propagation: McpTracePropagation,
}

impl McpClientOptions {
    /// Create client options.
    #[must_use]
    pub fn new(
        client_name: impl Into<String>,
        client_version: impl Into<String>,
        timeout: Duration,
    ) -> Self {
        Self {
            client_name: client_name.into(),
            client_version: client_version.into(),
            timeout,
            trace_propagation: McpTracePropagation::Disabled,
        }
    }

    /// Set request-level trace propagation.
    #[must_use]
    pub fn with_trace_propagation(mut self, trace_propagation: McpTracePropagation) -> Self {
        self.trace_propagation = trace_propagation;
        self
    }
}

/// Callback hooks used by an MCP client connection.
pub trait McpClientHandler: Send + Sync + 'static {
    /// Return roots exposed to the connected server.
    fn roots(&self) -> Vec<McpRoot> {
        Vec::new()
    }

    /// Called when the server reports that its tool list changed.
    fn tools_changed(&self) {}

    /// Called when the server reports that its prompt list changed.
    fn prompts_changed(&self) {}

    /// Called when the server reports that its resource list changed.
    fn resources_changed(&self) {}

    /// Called when the server sends a logging notification.
    fn log_message(&self, _params: Value) {}

    /// Called when the server sends a progress notification.
    fn progress(&self, _progress: McpProgressNotification) {}

    /// Called when the transport disconnects.
    fn disconnected(&self, _error: String) {}

    /// Called when a Streamable HTTP SSE event id is observed.
    fn sse_event_id(&self, _id: String) {}
}

#[derive(Debug)]
/// Initialized MCP client with typed request helpers.
pub struct McpClient {
    connection: Arc<McpConnection>,
    timeout: Duration,
    capabilities: McpServerCapabilities,
    trace_propagation: McpTracePropagation,
}

impl McpClient {
    /// Connect to a server, run the initialize handshake, and return a typed client.
    pub async fn connect(
        server_name: impl Into<String>,
        transport: McpTransportConfig,
        handler: Arc<dyn McpClientHandler>,
        options: McpClientOptions,
        cancellation_token: CancellationToken,
    ) -> Result<Self> {
        let handler = Arc::new(ClientHandlerAdapter { inner: handler });
        let connection = McpConnection::start(server_name.into(), transport, handler).await?;
        let trace_propagation = options.trace_propagation;
        let initialize = connection
            .request_with_trace_propagation(
                "initialize",
                inject_request_context(
                    Some(json!({
                        "protocolVersion": MCP_PROTOCOL_VERSION,
                        "capabilities": { "roots": { "listChanged": false } },
                        "clientInfo": {
                            "name": options.client_name,
                            "version": options.client_version,
                        }
                    })),
                    trace_propagation,
                ),
                options.timeout,
                cancellation_token,
                trace_propagation,
            )
            .await?;
        let protocol = initialize
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(MCP_PROTOCOL_VERSION);
        if protocol != MCP_PROTOCOL_VERSION {
            return Err(crate::Error::protocol(format!(
                "unsupported MCP protocol version `{protocol}`"
            )));
        }
        let capabilities = McpServerCapabilities::parse(initialize.get("capabilities"));
        connection.mark_initialized().await;
        connection.notify("notifications/initialized", None).await?;
        Ok(Self {
            connection,
            timeout: options.timeout,
            capabilities,
            trace_propagation,
        })
    }

    /// Return capabilities advertised by the server.
    #[must_use]
    pub fn capabilities(&self) -> &McpServerCapabilities {
        &self.capabilities
    }

    /// Call a server tool.
    pub async fn call_tool(
        &self,
        call: McpToolCall,
        cancellation_token: CancellationToken,
    ) -> Result<McpToolResult> {
        self.request("tools/call", Some(call.to_params()), cancellation_token)
            .await
            .map(McpToolResult::new)
    }

    /// Get a server prompt.
    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<Value>,
        cancellation_token: CancellationToken,
    ) -> Result<McpPromptResult> {
        let mut params = json!({ "name": name });
        if let Some(arguments) = arguments {
            params["arguments"] = arguments;
        }
        self.request("prompts/get", Some(params), cancellation_token)
            .await
            .map(McpPromptResult::new)
    }

    /// Read a server resource.
    pub async fn read_resource(
        &self,
        uri: &str,
        cancellation_token: CancellationToken,
    ) -> Result<McpResourceResult> {
        self.request(
            "resources/read",
            Some(json!({ "uri": uri })),
            cancellation_token,
        )
        .await
        .map(McpResourceResult::new)
    }

    /// Complete a resource-template argument.
    pub async fn complete_resource_template(
        &self,
        request: McpCompletionRequest,
        cancellation_token: CancellationToken,
    ) -> Result<McpCompletionResult> {
        let response = self
            .request(
                "completion/complete",
                Some(request.to_params()),
                cancellation_token,
            )
            .await?;
        Ok(parse_completion(&response))
    }

    /// Set the server logging level.
    pub async fn set_log_level(
        &self,
        level: McpLogLevel,
        cancellation_token: CancellationToken,
    ) -> Result<()> {
        self.request(
            "logging/setLevel",
            Some(json!({ "level": level.to_string() })),
            cancellation_token,
        )
        .await?;
        Ok(())
    }

    /// List all tools, following pagination cursors.
    pub async fn list_tools(
        &self,
        cancellation_token: CancellationToken,
    ) -> Result<Vec<crate::McpToolDefinition>> {
        let mut cursor = None;
        let mut items = Vec::new();
        loop {
            let page = self
                .list_tools_page(cursor.as_deref(), cancellation_token.clone())
                .await?;
            items.extend(page.tools);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(items);
            }
        }
    }

    /// List one page of tools.
    pub async fn list_tools_page(
        &self,
        cursor: Option<&str>,
        cancellation_token: CancellationToken,
    ) -> Result<McpToolList> {
        let response = self
            .request("tools/list", cursor_params(cursor), cancellation_token)
            .await?;
        McpToolList::parse(&response)
    }

    /// List all prompts, following pagination cursors.
    pub async fn list_prompts(
        &self,
        cancellation_token: CancellationToken,
    ) -> Result<Vec<crate::McpPromptDefinition>> {
        let mut cursor = None;
        let mut items = Vec::new();
        loop {
            let page = self
                .list_prompts_page(cursor.as_deref(), cancellation_token.clone())
                .await?;
            items.extend(page.prompts);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(items);
            }
        }
    }

    /// List one page of prompts.
    pub async fn list_prompts_page(
        &self,
        cursor: Option<&str>,
        cancellation_token: CancellationToken,
    ) -> Result<McpPromptList> {
        let response = self
            .request("prompts/list", cursor_params(cursor), cancellation_token)
            .await?;
        McpPromptList::parse(&response)
    }

    /// List all resources, following pagination cursors.
    pub async fn list_resources(
        &self,
        cancellation_token: CancellationToken,
    ) -> Result<Vec<crate::McpResourceDefinition>> {
        let mut cursor = None;
        let mut items = Vec::new();
        loop {
            let page = self
                .list_resources_page(cursor.as_deref(), cancellation_token.clone())
                .await?;
            items.extend(page.resources);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(items);
            }
        }
    }

    /// List one page of resources.
    pub async fn list_resources_page(
        &self,
        cursor: Option<&str>,
        cancellation_token: CancellationToken,
    ) -> Result<McpResourceList> {
        let response = self
            .request("resources/list", cursor_params(cursor), cancellation_token)
            .await?;
        McpResourceList::parse(&response)
    }

    /// List all resource templates, following pagination cursors.
    pub async fn list_resource_templates(
        &self,
        cancellation_token: CancellationToken,
    ) -> Result<Vec<crate::McpResourceTemplateDefinition>> {
        let mut cursor = None;
        let mut items = Vec::new();
        loop {
            let page = self
                .list_resource_templates_page(cursor.as_deref(), cancellation_token.clone())
                .await?;
            items.extend(page.resource_templates);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(items);
            }
        }
    }

    /// List one page of resource templates.
    pub async fn list_resource_templates_page(
        &self,
        cursor: Option<&str>,
        cancellation_token: CancellationToken,
    ) -> Result<McpResourceTemplateList> {
        let response = self
            .request(
                "resources/templates/list",
                cursor_params(cursor),
                cancellation_token,
            )
            .await?;
        McpResourceTemplateList::parse(&response)
    }

    /// Shut down the client transport.
    pub async fn shutdown(&self) {
        self.connection.shutdown().await;
    }

    async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        cancellation_token: CancellationToken,
    ) -> Result<Value> {
        self.connection
            .request_with_trace_propagation(
                method,
                trace_params(method, params, self.trace_propagation),
                self.timeout,
                cancellation_token,
                self.trace_propagation,
            )
            .await
    }
}

fn trace_params(
    method: &str,
    params: Option<Value>,
    trace_propagation: McpTracePropagation,
) -> Option<Value> {
    if method == "ping" {
        params
    } else {
        inject_request_context(params, trace_propagation)
    }
}

struct ClientHandlerAdapter {
    inner: Arc<dyn McpClientHandler>,
}

impl super::connection::McpConnectionHandler for ClientHandlerAdapter {
    fn notification(&self, notification: JsonRpcNotification) {
        match notification.method.as_str() {
            "notifications/tools/list_changed" => self.inner.tools_changed(),
            "notifications/prompts/list_changed" => self.inner.prompts_changed(),
            "notifications/resources/list_changed" => self.inner.resources_changed(),
            "notifications/message" => self
                .inner
                .log_message(notification.params.unwrap_or(Value::Null)),
            "notifications/progress" => {
                let params = notification.params.unwrap_or(Value::Null);
                if let Some(token) = params.get("progressToken").cloned() {
                    self.inner.progress(McpProgressNotification {
                        token,
                        progress: params.get("progress").and_then(Value::as_f64),
                        total: params.get("total").and_then(Value::as_f64),
                        message: params
                            .get("message")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned),
                    });
                }
            }
            _ => {}
        }
    }

    fn request(&self, request: JsonRpcRequest) -> Value {
        if request.method == "ping" {
            return json_rpc_success(request.id, json!({}));
        }
        if request.method == "roots/list" {
            return json_rpc_success(
                request.id,
                json!({ "roots": self.inner.roots().iter().map(McpRoot::to_value).collect::<Vec<_>>() }),
            );
        }
        json_rpc_method_not_found(request.id, &request.method)
    }

    fn disconnected(&self, error: String) {
        self.inner.disconnected(error);
    }

    fn sse_event_id(&self, id: String) {
        self.inner.sse_event_id(id);
    }
}

fn cursor_params(cursor: Option<&str>) -> Option<Value> {
    cursor.map(|cursor| json!({ "cursor": cursor }))
}
