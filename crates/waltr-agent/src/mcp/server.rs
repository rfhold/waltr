use std::{collections::BTreeMap, sync::Arc, time::Duration};

use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;

use crate::Result;

use super::{
    capabilities::McpServerCapabilities,
    config::McpServerConfig,
    connection::{McpConnection, McpConnectionHandler, roots_response},
    event::McpRuntimeEvent,
    prompt::{McpPromptDefinition, format_prompt_result, parse_prompt_definition},
    protocol::{JsonRpcNotification, JsonRpcRequest, MCP_PROTOCOL_VERSION, protocol_error},
    resource::{McpResourceDefinition, format_resource_result, parse_resource_definition},
    tool::{McpToolDefinition, format_tool_result, parse_tool_definition},
};

#[derive(Debug, Clone, Default)]
pub(in crate::mcp) struct McpToolRegistry {
    pub(in crate::mcp) tools: BTreeMap<String, McpToolDefinition>,
    pub(in crate::mcp) generation: u64,
}

#[derive(Debug, Clone, Default)]
pub(in crate::mcp) struct McpPromptRegistry {
    pub(in crate::mcp) prompts: BTreeMap<String, McpPromptDefinition>,
    pub(in crate::mcp) generation: u64,
}

#[derive(Debug, Clone, Default)]
pub(in crate::mcp) struct McpResourceRegistry {
    pub(in crate::mcp) resources: BTreeMap<String, McpResourceDefinition>,
    pub(in crate::mcp) generation: u64,
}

pub(in crate::mcp) struct McpServerRuntime {
    name: String,
    connection: Arc<McpConnection>,
    timeout: Duration,
    #[allow(dead_code)]
    root_uri: String,
    events: broadcast::Sender<McpRuntimeEvent>,
    state: Mutex<McpServerState>,
}

#[derive(Debug)]
struct McpServerState {
    initialized: bool,
    capabilities: McpServerCapabilities,
    tools: McpToolRegistry,
    prompts: McpPromptRegistry,
    resources: McpResourceRegistry,
    last_sse_event_id: Option<String>,
}

impl Default for McpServerState {
    fn default() -> Self {
        Self {
            initialized: false,
            capabilities: McpServerCapabilities::default(),
            tools: McpToolRegistry::default(),
            prompts: McpPromptRegistry::default(),
            resources: McpResourceRegistry::default(),
            last_sse_event_id: None,
        }
    }
}

struct ServerHandler {
    name: String,
    root_uri: String,
    events: broadcast::Sender<McpRuntimeEvent>,
    refresh: Mutex<Option<Arc<McpServerRuntime>>>,
}

impl McpServerRuntime {
    pub(in crate::mcp) async fn start(
        config: McpServerConfig,
        timeout: Duration,
        root_uri: String,
        events: broadcast::Sender<McpRuntimeEvent>,
        cancellation_token: CancellationToken,
    ) -> Result<Arc<Self>> {
        let name = config.name.clone();
        let handler = Arc::new(ServerHandler {
            name: name.clone(),
            root_uri: root_uri.clone(),
            events: events.clone(),
            refresh: Mutex::new(None),
        });
        let connection = McpConnection::start(config.transport, handler.clone()).await?;
        let runtime = Arc::new(Self {
            name,
            connection,
            timeout,
            root_uri,
            events,
            state: Mutex::new(McpServerState::default()),
        });
        *handler.refresh.lock().await = Some(Arc::clone(&runtime));
        runtime.ensure_initialized(cancellation_token).await?;
        Ok(runtime)
    }

    pub(in crate::mcp) fn name(&self) -> &str {
        &self.name
    }

    pub(in crate::mcp) async fn tools(&self) -> Vec<McpToolDefinition> {
        self.state
            .lock()
            .await
            .tools
            .tools
            .values()
            .cloned()
            .collect()
    }

    pub(in crate::mcp) async fn prompts(&self) -> Vec<McpPromptDefinition> {
        self.state
            .lock()
            .await
            .prompts
            .prompts
            .values()
            .cloned()
            .collect()
    }

    pub(in crate::mcp) async fn resources(&self) -> Vec<McpResourceDefinition> {
        self.state
            .lock()
            .await
            .resources
            .resources
            .values()
            .cloned()
            .collect()
    }

    pub(in crate::mcp) async fn call_tool(
        &self,
        tool: &str,
        arguments: Value,
        cancellation_token: CancellationToken,
    ) -> Result<String> {
        self.ensure_initialized(cancellation_token.clone()).await?;
        let params = json!({
            "name": tool,
            "arguments": arguments,
            "_meta": {
                "progressToken": format!("{}:{tool}", self.name),
            }
        });
        let response = self
            .connection
            .request("tools/call", Some(params), self.timeout, cancellation_token)
            .await?;
        Ok(format_tool_result(&response))
    }

    pub(in crate::mcp) async fn get_prompt(
        &self,
        prompt: &str,
        arguments: Option<Value>,
        cancellation_token: CancellationToken,
    ) -> Result<String> {
        self.ensure_initialized(cancellation_token.clone()).await?;
        let mut params = json!({
            "name": prompt,
            "_meta": {
                "progressToken": format!("{}:{prompt}", self.name),
            }
        });
        if let Some(arguments) = arguments {
            params["arguments"] = arguments;
        }
        let response = self
            .connection
            .request(
                "prompts/get",
                Some(params),
                self.timeout,
                cancellation_token,
            )
            .await?;
        Ok(format_prompt_result(&response))
    }

    pub(in crate::mcp) async fn read_resource(
        &self,
        uri: &str,
        cancellation_token: CancellationToken,
    ) -> Result<String> {
        self.ensure_initialized(cancellation_token.clone()).await?;
        let params = json!({
            "uri": uri,
            "_meta": {
                "progressToken": format!("{}:{uri}", self.name),
            }
        });
        let response = self
            .connection
            .request(
                "resources/read",
                Some(params),
                self.timeout,
                cancellation_token,
            )
            .await?;
        Ok(format_resource_result(&response))
    }

    async fn ensure_initialized(&self, cancellation_token: CancellationToken) -> Result<()> {
        if self.state.lock().await.initialized {
            return Ok(());
        }
        let response = self
            .connection
            .request(
                "initialize",
                Some(json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {
                        "roots": { "listChanged": false }
                    },
                    "clientInfo": {
                        "name": "waltr-agent",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                })),
                self.timeout,
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
        let capabilities = McpServerCapabilities::parse(response.get("capabilities"));
        self.connection
            .notify("notifications/initialized", None)
            .await?;
        self.connection.mark_initialized().await;
        {
            let mut state = self.state.lock().await;
            state.capabilities = capabilities;
            state.initialized = true;
        }
        self.refresh_tools(cancellation_token.clone()).await?;
        self.refresh_prompts(cancellation_token.clone()).await?;
        self.refresh_resources(cancellation_token).await?;
        drop(self.events.send(McpRuntimeEvent::ServerInitialized {
            server_key: self.name.clone(),
        }));
        Ok(())
    }

    async fn refresh_tools(&self, cancellation_token: CancellationToken) -> Result<()> {
        if self.state.lock().await.capabilities.tools.is_none() {
            return Ok(());
        }
        let mut cursor = None;
        let mut tools = BTreeMap::new();
        loop {
            let params = cursor.as_ref().map(|cursor| json!({ "cursor": cursor }));
            let response = self
                .connection
                .request(
                    "tools/list",
                    params,
                    self.timeout,
                    cancellation_token.clone(),
                )
                .await?;
            for tool in response
                .get("tools")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(tool) = parse_tool_definition(tool) {
                    tools.insert(tool.name.clone(), tool);
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
        let generation = {
            let mut state = self.state.lock().await;
            state.tools.tools = tools;
            state.tools.generation += 1;
            state.tools.generation
        };
        drop(self.events.send(McpRuntimeEvent::ToolsChanged {
            server_key: self.name.clone(),
            generation,
        }));
        Ok(())
    }

    async fn refresh_prompts(&self, cancellation_token: CancellationToken) -> Result<()> {
        if self.state.lock().await.capabilities.prompts.is_none() {
            return Ok(());
        }
        let mut cursor = None;
        let mut prompts = BTreeMap::new();
        loop {
            let params = cursor.as_ref().map(|cursor| json!({ "cursor": cursor }));
            let response = self
                .connection
                .request(
                    "prompts/list",
                    params,
                    self.timeout,
                    cancellation_token.clone(),
                )
                .await?;
            for prompt in response
                .get("prompts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(prompt) = parse_prompt_definition(prompt) {
                    prompts.insert(prompt.name.clone(), prompt);
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
        let mut state = self.state.lock().await;
        state.prompts.prompts = prompts;
        state.prompts.generation += 1;
        Ok(())
    }

    async fn refresh_resources(&self, cancellation_token: CancellationToken) -> Result<()> {
        if self.state.lock().await.capabilities.resources.is_none() {
            return Ok(());
        }
        let mut cursor = None;
        let mut resources = BTreeMap::new();
        loop {
            let params = cursor.as_ref().map(|cursor| json!({ "cursor": cursor }));
            let response = self
                .connection
                .request(
                    "resources/list",
                    params,
                    self.timeout,
                    cancellation_token.clone(),
                )
                .await?;
            for resource in response
                .get("resources")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(resource) = parse_resource_definition(resource) {
                    resources.insert(resource.uri.clone(), resource);
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
        let mut state = self.state.lock().await;
        state.resources.resources = resources;
        state.resources.generation += 1;
        Ok(())
    }

    pub(in crate::mcp) async fn shutdown(&self) {
        self.connection.shutdown().await;
    }
}

impl McpConnectionHandler for ServerHandler {
    fn notification(&self, notification: JsonRpcNotification) {
        match notification.method.as_str() {
            "notifications/tools/list_changed" => {
                let name = self.name.clone();
                let events = self.events.clone();
                let refresh = self
                    .refresh
                    .try_lock()
                    .ok()
                    .and_then(|runtime| runtime.clone());
                if let Some(runtime) = refresh {
                    tokio::spawn(async move {
                        if let Err(error) = runtime.refresh_tools(CancellationToken::new()).await {
                            tracing::warn!(server = %name, %error, "failed to refresh MCP tools");
                        }
                    });
                } else {
                    drop(events.send(McpRuntimeEvent::ToolsChanged {
                        server_key: name,
                        generation: 0,
                    }));
                }
            }
            "notifications/prompts/list_changed" => {
                let name = self.name.clone();
                let refresh = self
                    .refresh
                    .try_lock()
                    .ok()
                    .and_then(|runtime| runtime.clone());
                if let Some(runtime) = refresh {
                    tokio::spawn(async move {
                        if let Err(error) = runtime.refresh_prompts(CancellationToken::new()).await
                        {
                            tracing::warn!(server = %name, %error, "failed to refresh MCP prompts");
                        }
                    });
                }
            }
            "notifications/resources/list_changed" => {
                let name = self.name.clone();
                let refresh = self
                    .refresh
                    .try_lock()
                    .ok()
                    .and_then(|runtime| runtime.clone());
                if let Some(runtime) = refresh {
                    tokio::spawn(async move {
                        if let Err(error) =
                            runtime.refresh_resources(CancellationToken::new()).await
                        {
                            tracing::warn!(server = %name, %error, "failed to refresh MCP resources");
                        }
                    });
                }
            }
            "notifications/message" => {
                let params = notification.params.unwrap_or(Value::Null);
                let level = params
                    .get("level")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                let logger = params
                    .get("logger")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                let data = params.get("data").cloned().unwrap_or(Value::Null);
                tracing::debug!(server = %self.name, ?level, ?logger, data = %data, "mcp log message");
                drop(self.events.send(McpRuntimeEvent::LogMessage {
                    server_key: self.name.clone(),
                    level,
                    logger,
                    data,
                }));
            }
            "notifications/progress" => {
                let params = notification.params.unwrap_or(Value::Null);
                let Some(token) = params.get("progressToken").cloned() else {
                    return;
                };
                drop(
                    self.events.send(McpRuntimeEvent::Progress {
                        server_key: self.name.clone(),
                        token,
                        progress: params.get("progress").and_then(Value::as_f64),
                        total: params.get("total").and_then(Value::as_f64),
                        message: params
                            .get("message")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned),
                    }),
                );
            }
            _ => {}
        }
    }

    fn request(&self, request: JsonRpcRequest) -> Value {
        roots_response(request, &self.root_uri)
    }

    fn disconnected(&self, error: String) {
        drop(self.events.send(McpRuntimeEvent::ServerDisconnected {
            server_key: self.name.clone(),
            error,
        }));
    }

    fn sse_event_id(&self, id: String) {
        let refresh = self
            .refresh
            .try_lock()
            .ok()
            .and_then(|runtime| runtime.clone());
        if let Some(runtime) = refresh {
            tokio::spawn(async move {
                runtime.state.lock().await.last_sse_event_id = Some(id);
            });
        }
    }
}
