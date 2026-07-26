use std::{collections::BTreeMap, sync::Arc, time::Duration};

use serde_json::Value;
use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use mcp::client::{McpClient, McpClientHandler, McpClientOptions};
use mcp::{
    McpCompletionRequest, McpCompletionResult, McpLogLevel, McpProgressNotification,
    McpPromptDefinition, McpResourceDefinition, McpResourceTemplateDefinition, McpRoot,
    McpServerCapabilities, McpToolCall, McpToolDefinition, McpTracePropagation,
};

use crate::{LlmError, Result, ToolProgressSink};

use super::{config::McpServerConfig, event::McpRuntimeEvent};

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
    client: McpClient,
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
    instructions: Option<String>,
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
            instructions: None,
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
        trace_propagation: McpTracePropagation,
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
        let client = McpClient::connect(
            name.clone(),
            config.transport,
            handler.clone(),
            McpClientOptions::new("waltr-agent", env!("CARGO_PKG_VERSION"), timeout)
                .with_trace_propagation(trace_propagation),
            cancellation_token.clone(),
        )
        .await?;
        let runtime = Arc::new(Self {
            name,
            client,
            log_level,
            transport_kind,
            root_uri,
            events,
            active_progress,
            state: Mutex::new(McpServerState::default()),
        });
        *handler.refresh.lock().await = Some(Arc::clone(&runtime));
        runtime.finish_initialization(cancellation_token).await?;
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

    pub(in crate::mcp) async fn instructions(&self) -> Option<String> {
        self.state.lock().await.instructions.clone()
    }

    pub(in crate::mcp) async fn call_tool(
        &self,
        tool: &str,
        arguments: Value,
        call_id: &str,
        progress: ToolProgressSink,
        cancellation_token: CancellationToken,
    ) -> Result<String> {
        let span = tracing::info_span!(
            "mcp.agent.operation",
            mcp.server = %self.name,
            mcp.transport = self.transport_kind,
            rpc.method = "tools/call",
            mcp.tool.name = %tool,
        );
        async move {
            self.ensure_initialized(cancellation_token.clone()).await?;
            let progress_token = format!("{}:{tool}:{call_id}", self.name);
            self.active_progress
                .lock()
                .map_err(|_| protocol_error("MCP progress registry lock poisoned"))?
                .insert(progress_token.clone(), progress);
            let response = self
                .client
                .call_tool(
                    McpToolCall::new(tool, arguments).with_progress_token(progress_token.clone()),
                    cancellation_token,
                )
                .await;
            drop(
                self.active_progress
                    .lock()
                    .map(|mut active| active.remove(&progress_token)),
            );
            let response = response?;
            Ok(response.format_text())
        }
        .instrument(span)
        .await
    }

    pub(in crate::mcp) async fn get_prompt(
        &self,
        prompt: &str,
        arguments: Option<Value>,
        cancellation_token: CancellationToken,
    ) -> Result<String> {
        let span = tracing::info_span!(
            "mcp.agent.operation",
            mcp.server = %self.name,
            mcp.transport = self.transport_kind,
            rpc.method = "prompts/get",
            mcp.prompt.name = %prompt,
        );
        async move {
            self.ensure_initialized(cancellation_token.clone()).await?;
            let response = self
                .client
                .get_prompt(prompt, arguments, cancellation_token)
                .await?;
            Ok(response.format_text())
        }
        .instrument(span)
        .await
    }

    pub(in crate::mcp) async fn read_resource(
        &self,
        uri: &str,
        cancellation_token: CancellationToken,
    ) -> Result<String> {
        let span = tracing::info_span!(
            "mcp.agent.operation",
            mcp.server = %self.name,
            mcp.transport = self.transport_kind,
            rpc.method = "resources/read",
            mcp.resource.uri = %uri,
        );
        async move {
            self.ensure_initialized(cancellation_token.clone()).await?;
            let response = self.client.read_resource(uri, cancellation_token).await?;
            Ok(response.format_text())
        }
        .instrument(span)
        .await
    }

    pub(in crate::mcp) async fn complete_resource_template(
        &self,
        uri_template: &str,
        argument_name: &str,
        argument_value: &str,
        context_arguments: serde_json::Map<String, Value>,
        cancellation_token: CancellationToken,
    ) -> Result<Option<McpCompletionResult>> {
        let span = tracing::info_span!(
            "mcp.agent.operation",
            mcp.server = %self.name,
            mcp.transport = self.transport_kind,
            rpc.method = "completion/complete",
            mcp.resource_template.uri_template = %uri_template,
        );
        async move {
            self.ensure_initialized(cancellation_token.clone()).await?;
            if !self.state.lock().await.capabilities.completions {
                return Ok(None);
            }

            let mut request =
                McpCompletionRequest::new(uri_template, argument_name, argument_value);
            request.context_arguments = context_arguments;

            let response = self
                .client
                .complete_resource_template(request, cancellation_token)
                .await?;
            Ok(Some(response))
        }
        .instrument(span)
        .await
    }

    async fn ensure_initialized(&self, cancellation_token: CancellationToken) -> Result<()> {
        if self.state.lock().await.initialized {
            return Ok(());
        }
        self.finish_initialization(cancellation_token).await
    }

    async fn finish_initialization(&self, cancellation_token: CancellationToken) -> Result<()> {
        if self.state.lock().await.initialized {
            return Ok(());
        }
        let capabilities = self.client.capabilities().clone();
        let instructions = self.client.instructions().map(str::to_owned);
        {
            let mut state = self.state.lock().await;
            state.capabilities = capabilities;
            state.instructions = instructions;
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
        self.client.set_log_level(level, cancellation_token).await?;
        Ok(())
    }

    async fn refresh_tools(&self, cancellation_token: CancellationToken) -> Result<()> {
        if self.state.lock().await.capabilities.tools.is_none() {
            return Ok(());
        }
        let mut tools = BTreeMap::new();
        for tool in self.client.list_tools(cancellation_token).await? {
            tools.insert(tool.name.clone(), tool);
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
        let mut prompts = BTreeMap::new();
        for prompt in self.client.list_prompts(cancellation_token).await? {
            prompts.insert(prompt.name.clone(), prompt);
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
        let mut resources = BTreeMap::new();
        for resource in self
            .client
            .list_resources(cancellation_token.clone())
            .await?
        {
            resources.insert(resource.uri.clone(), resource);
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
        let mut resource_templates = BTreeMap::new();
        for resource_template in self
            .client
            .list_resource_templates(cancellation_token)
            .await?
        {
            resource_templates.insert(resource_template.uri_template.clone(), resource_template);
        }
        let mut state = self.state.lock().await;
        state.resources.resource_templates = resource_templates;
        state.resources.generation += 1;
        Ok(())
    }

    pub(in crate::mcp) async fn shutdown(&self) {
        drop(self.client.shutdown().await);
    }
}

impl McpClientHandler for ServerHandler {
    fn roots(&self) -> Vec<McpRoot> {
        vec![McpRoot::new(
            self.root_uri.clone(),
            Some("workspace".to_owned()),
        )]
    }

    fn tools_changed(&self) {
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

    fn prompts_changed(&self) {
        let name = self.name.clone();
        let refresh = self
            .refresh
            .try_lock()
            .ok()
            .and_then(|runtime| runtime.clone());
        if let Some(runtime) = refresh {
            tokio::spawn(async move {
                if let Err(error) = runtime.refresh_prompts(CancellationToken::new()).await {
                    tracing::warn!(server = %name, %error, "failed to refresh MCP prompts");
                }
            });
        }
    }

    fn resources_changed(&self) {
        let name = self.name.clone();
        let refresh = self
            .refresh
            .try_lock()
            .ok()
            .and_then(|runtime| runtime.clone());
        if let Some(runtime) = refresh {
            tokio::spawn(async move {
                if let Err(error) = runtime.refresh_resources(CancellationToken::new()).await {
                    tracing::warn!(server = %name, %error, "failed to refresh MCP resources");
                }
            });
        }
    }

    fn log_message(&self, params: Value) {
        self.log_message(params);
    }

    fn progress(&self, progress: McpProgressNotification) {
        if let Some(sink) = self
            .active_progress
            .lock()
            .ok()
            .and_then(|active| active.get(&progress_token_key(&progress.token)).cloned())
        {
            let rendered = render_progress_message(
                progress.message.as_deref(),
                progress.progress,
                progress.total,
            );
            if let Err(error) = sink.info(rendered) {
                tracing::warn!(server = %self.name, %error, "failed to emit MCP progress");
            }
        }
        drop(self.events.send(McpRuntimeEvent::Progress {
            server_key: self.name.clone(),
            token: progress.token,
            progress: progress.progress,
            total: progress.total,
            message: progress.message,
        }));
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

fn protocol_error(message: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        provider: "mcp",
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use serde_json::json;
    use tokio::sync::{Mutex, broadcast};

    use mcp::McpLogLevel;

    use super::{McpRuntimeEvent, ServerHandler};

    #[test]
    fn normalizes_mcp_log_notification_events() {
        let (events, mut receiver) = broadcast::channel(8);
        let handler = ServerHandler {
            name: "test_server".to_owned(),
            root_uri: "file:///workspace".to_owned(),
            events,
            active_progress: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
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
