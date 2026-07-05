use std::{collections::BTreeMap, sync::Arc, time::Duration};

use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;

use crate::{Result, ToolProgressSink};

use super::{
    capabilities::McpServerCapabilities,
    config::{McpServerConfig, set_log_level_params},
    connection::{McpConnection, McpConnectionHandler, roots_response},
    event::McpRuntimeEvent,
    log::McpLogLevel,
    prompt::{McpPromptDefinition, format_prompt_result, parse_prompt_definition},
    protocol::{JsonRpcNotification, JsonRpcRequest, MCP_PROTOCOL_VERSION, protocol_error},
    resource::{
        McpCompletionResult, McpResourceDefinition, McpResourceTemplateDefinition,
        format_resource_result, parse_completion_result, parse_resource_definition,
        parse_resource_template_definition,
    },
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
    pub(in crate::mcp) resource_templates: BTreeMap<String, McpResourceTemplateDefinition>,
    pub(in crate::mcp) generation: u64,
}

pub(in crate::mcp) struct McpServerRuntime {
    name: String,
    connection: Arc<McpConnection>,
    timeout: Duration,
    log_level: Option<McpLogLevel>,
    transport_kind: &'static str,
    #[allow(dead_code)]
    root_uri: String,
    events: broadcast::Sender<McpRuntimeEvent>,
    active_progress: Arc<std::sync::Mutex<BTreeMap<String, ToolProgressSink>>>,
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
    active_progress: Arc<std::sync::Mutex<BTreeMap<String, ToolProgressSink>>>,
    refresh: Mutex<Option<Arc<McpServerRuntime>>>,
}

impl McpServerRuntime {
    pub(in crate::mcp) async fn start(
        config: McpServerConfig,
        timeout: Duration,
        log_level: Option<McpLogLevel>,
        root_uri: String,
        events: broadcast::Sender<McpRuntimeEvent>,
        cancellation_token: CancellationToken,
    ) -> Result<Arc<Self>> {
        let name = config.name.clone();
        let transport_kind = config.transport.kind();
        let active_progress = Arc::new(std::sync::Mutex::new(BTreeMap::new()));
        let handler = Arc::new(ServerHandler {
            name: name.clone(),
            root_uri: root_uri.clone(),
            events: events.clone(),
            active_progress: Arc::clone(&active_progress),
            refresh: Mutex::new(None),
        });
        let connection =
            McpConnection::start(name.clone(), config.transport, handler.clone()).await?;
        let runtime = Arc::new(Self {
            name,
            connection,
            timeout,
            log_level,
            transport_kind,
            root_uri,
            events,
            active_progress,
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

    pub(in crate::mcp) async fn resource_templates(&self) -> Vec<McpResourceTemplateDefinition> {
        self.state
            .lock()
            .await
            .resources
            .resource_templates
            .values()
            .cloned()
            .collect()
    }

    pub(in crate::mcp) async fn call_tool(
        &self,
        tool: &str,
        arguments: Value,
        call_id: &str,
        progress: ToolProgressSink,
        cancellation_token: CancellationToken,
    ) -> Result<String> {
        self.ensure_initialized(cancellation_token.clone()).await?;
        let progress_token = format!("{}:{tool}:{call_id}", self.name);
        let params = json!({
            "name": tool,
            "arguments": arguments,
            "_meta": {
                "progressToken": progress_token,
            }
        });
        self.active_progress
            .lock()
            .map_err(|_| protocol_error("MCP progress registry lock poisoned"))?
            .insert(progress_token.clone(), progress);
        let response = self
            .connection
            .request("tools/call", Some(params), self.timeout, cancellation_token)
            .await;
        drop(
            self.active_progress
                .lock()
                .map(|mut active| active.remove(&progress_token)),
        );
        let response = response?;
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

    pub(in crate::mcp) async fn complete_resource_template(
        &self,
        uri_template: &str,
        argument_name: &str,
        argument_value: &str,
        context_arguments: serde_json::Map<String, Value>,
        cancellation_token: CancellationToken,
    ) -> Result<Option<McpCompletionResult>> {
        self.ensure_initialized(cancellation_token.clone()).await?;
        if !self.state.lock().await.capabilities.completions {
            return Ok(None);
        }

        let mut params = json!({
            "ref": {
                "type": "ref/resource",
                "uri": uri_template,
            },
            "argument": {
                "name": argument_name,
                "value": argument_value,
            },
        });
        if !context_arguments.is_empty() {
            params["context"] = json!({
                "arguments": context_arguments,
            });
        }

        let response = self
            .connection
            .request(
                "completion/complete",
                Some(params),
                self.timeout,
                cancellation_token,
            )
            .await?;
        Ok(Some(parse_completion_result(&response)))
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
        self.configure_logging(cancellation_token.clone()).await?;
        self.refresh_tools(cancellation_token.clone()).await?;
        self.refresh_prompts(cancellation_token.clone()).await?;
        self.refresh_resources(cancellation_token).await?;
        drop(self.events.send(McpRuntimeEvent::ServerInitialized {
            server_key: self.name.clone(),
        }));
        Ok(())
    }

    async fn configure_logging(&self, cancellation_token: CancellationToken) -> Result<()> {
        let Some(level) = self.log_level else {
            return Ok(());
        };
        if !self.state.lock().await.capabilities.logging {
            return Ok(());
        }
        self.connection
            .request(
                "logging/setLevel",
                Some(set_log_level_params(level)),
                self.timeout,
                cancellation_token,
            )
            .await?;
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
        drop(state);
        self.refresh_resource_templates(cancellation_token).await?;
        Ok(())
    }

    async fn refresh_resource_templates(
        &self,
        cancellation_token: CancellationToken,
    ) -> Result<()> {
        if self.state.lock().await.capabilities.resources.is_none() {
            return Ok(());
        }
        let mut cursor = None;
        let mut resource_templates = BTreeMap::new();
        loop {
            let params = cursor.as_ref().map(|cursor| json!({ "cursor": cursor }));
            let response = self
                .connection
                .request(
                    "resources/templates/list",
                    params,
                    self.timeout,
                    cancellation_token.clone(),
                )
                .await?;
            for resource_template in response
                .get("resourceTemplates")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(resource_template) =
                    parse_resource_template_definition(resource_template)
                {
                    resource_templates
                        .insert(resource_template.uri_template.clone(), resource_template);
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
        state.resources.resource_templates = resource_templates;
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
                self.log_message(notification.params.unwrap_or(Value::Null));
            }
            "notifications/progress" => {
                let params = notification.params.unwrap_or(Value::Null);
                let Some(token) = params.get("progressToken").cloned() else {
                    return;
                };
                let progress = params.get("progress").and_then(Value::as_f64);
                let total = params.get("total").and_then(Value::as_f64);
                let message = params
                    .get("message")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                if let Some(sink) = self
                    .active_progress
                    .lock()
                    .ok()
                    .and_then(|active| active.get(&progress_token_key(&token)).cloned())
                {
                    let rendered = render_progress_message(message.as_deref(), progress, total);
                    if let Err(error) = sink.info(rendered) {
                        tracing::warn!(server = %self.name, %error, "failed to emit MCP progress");
                    }
                }
                drop(self.events.send(McpRuntimeEvent::Progress {
                    server_key: self.name.clone(),
                    token,
                    progress,
                    total,
                    message,
                }));
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

impl ServerHandler {
    fn log_message(&self, params: Value) {
        let raw_level = params
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or("debug");
        let level = raw_level.parse().unwrap_or_else(|()| {
            tracing::warn!(
                mcp.server = %self.name,
                mcp.transport = self.transport_kind(),
                mcp.source = "notification",
                mcp.log.level = raw_level,
                "invalid MCP log level"
            );
            McpLogLevel::Debug
        });
        let logger = params
            .get("logger")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let data = params.get("data").cloned().unwrap_or(Value::Null);
        let logging_capability = self.logging_capability();
        trace_mcp_log(
            &self.name,
            self.transport_kind(),
            logging_capability,
            level,
            logger.as_deref(),
            &data,
        );
        drop(self.events.send(McpRuntimeEvent::LogMessage {
            server_key: self.name.clone(),
            level,
            logger,
            data,
        }));
    }

    fn transport_kind(&self) -> &'static str {
        self.refresh
            .try_lock()
            .ok()
            .and_then(|runtime| runtime.as_ref().map(|runtime| runtime.transport_kind))
            .unwrap_or("unknown")
    }

    fn logging_capability(&self) -> bool {
        self.refresh
            .try_lock()
            .ok()
            .and_then(|runtime| runtime.clone())
            .and_then(|runtime| {
                runtime
                    .state
                    .try_lock()
                    .ok()
                    .map(|state| state.capabilities.logging)
            })
            .unwrap_or(false)
    }
}

fn progress_token_key(token: &Value) -> String {
    token
        .as_str()
        .map_or_else(|| token.to_string(), ToOwned::to_owned)
}

fn render_progress_message(
    message: Option<&str>,
    progress: Option<f64>,
    total: Option<f64>,
) -> String {
    match (message, progress, total) {
        (Some(message), Some(progress), Some(total)) => format!("{message} ({progress}/{total})"),
        (Some(message), Some(progress), None) => format!("{message} ({progress})"),
        (Some(message), None, _) => message.to_owned(),
        (None, Some(progress), Some(total)) => format!("MCP progress: {progress}/{total}"),
        (None, Some(progress), None) => format!("MCP progress: {progress}"),
        (None, None, _) => "MCP progress".to_owned(),
    }
}

fn trace_mcp_log(
    server: &str,
    transport: &'static str,
    logging_capability: bool,
    level: McpLogLevel,
    logger: Option<&str>,
    data: &Value,
) {
    match level {
        McpLogLevel::Debug => tracing::debug!(
            mcp.server = %server,
            mcp.transport = transport,
            mcp.source = "notification",
            mcp.log.level = level.as_str(),
            mcp.logger = logger,
            mcp.capability.logging = logging_capability,
            data = %data,
            "mcp log message"
        ),
        McpLogLevel::Info | McpLogLevel::Notice => tracing::info!(
            mcp.server = %server,
            mcp.transport = transport,
            mcp.source = "notification",
            mcp.log.level = level.as_str(),
            mcp.logger = logger,
            mcp.capability.logging = logging_capability,
            data = %data,
            "mcp log message"
        ),
        McpLogLevel::Warning => tracing::warn!(
            mcp.server = %server,
            mcp.transport = transport,
            mcp.source = "notification",
            mcp.log.level = level.as_str(),
            mcp.logger = logger,
            mcp.capability.logging = logging_capability,
            data = %data,
            "mcp log message"
        ),
        McpLogLevel::Error
        | McpLogLevel::Critical
        | McpLogLevel::Alert
        | McpLogLevel::Emergency => tracing::error!(
            mcp.server = %server,
            mcp.transport = transport,
            mcp.source = "notification",
            mcp.log.level = level.as_str(),
            mcp.logger = logger,
            mcp.capability.logging = logging_capability,
            data = %data,
            "mcp log message"
        ),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::sync::{Mutex, broadcast};

    use super::{McpRuntimeEvent, ServerHandler};
    use crate::mcp::log::McpLogLevel;

    #[test]
    fn normalizes_mcp_log_notification_events() {
        let (events, mut receiver) = broadcast::channel(8);
        let handler = ServerHandler {
            name: "test_server".to_owned(),
            root_uri: "file:///workspace".to_owned(),
            events,
            refresh: Mutex::new(None),
        };

        handler.log_message(json!({
            "level": "warning",
            "logger": "test.logger",
            "data": { "message": "hello" }
        }));

        let event = receiver.try_recv().expect("log event");
        assert!(matches!(
            event,
            McpRuntimeEvent::LogMessage {
                server_key,
                level: McpLogLevel::Warning,
                logger,
                data,
            } if server_key == "test_server"
                && logger.as_deref() == Some("test.logger")
                && data == json!({ "message": "hello" })
        ));
    }
}
