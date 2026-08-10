use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use serde_json::Value;
use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use mcp::client::{McpClient, McpClientHandler, McpClientOptions, McpTransportLimits};
use mcp::{
    McpCompletionRequest, McpCompletionResult, McpLogLevel, McpProgressNotification,
    McpPromptDefinition, McpResourceDefinition, McpResourceTemplateDefinition, McpRoot,
    McpServerCapabilities, McpToolCall, McpToolContent, McpToolDefinition, McpTracePropagation,
};

use crate::{
    LlmError, Result, ToolProgressSink, ToolResultContent, ToolResultContentBlock, ToolResultImage,
};

use super::{config::McpServerConfig, event::McpRuntimeEvent};

const WALTR_MCP_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug)]
pub(in crate::mcp) struct McpAgentToolResult {
    pub(in crate::mcp) content: ToolResultContent,
    pub(in crate::mcp) is_error: bool,
}

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
    transport_kind: &'static str,
    #[allow(dead_code)]
    root_uri: String,
    events: broadcast::Sender<McpRuntimeEvent>,
    active_progress: Arc<std::sync::Mutex<BTreeMap<String, ToolProgressSink>>>,
    state: Mutex<McpServerState>,
}

#[derive(Debug, Default)]
struct McpServerState {
    initialized: bool,
    capabilities: McpServerCapabilities,
    instructions: Option<String>,
    tools: McpToolRegistry,
    prompts: McpPromptRegistry,
    resources: McpResourceRegistry,
}

struct ServerHandler {
    name: String,
    root_uri: String,
    events: broadcast::Sender<McpRuntimeEvent>,
    active_progress: Arc<std::sync::Mutex<BTreeMap<String, ToolProgressSink>>>,
    refresh: Mutex<Option<Arc<McpServerRuntime>>>,
    cancellation: CancellationToken,
    tools_epoch: Arc<AtomicU64>,
    prompts_epoch: Arc<AtomicU64>,
    resources_epoch: Arc<AtomicU64>,
}

#[allow(clippy::too_many_arguments)]
impl McpServerRuntime {
    #[allow(clippy::too_many_lines)]
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
            cancellation: cancellation_token.clone(),
            tools_epoch: Arc::new(AtomicU64::new(0)),
            prompts_epoch: Arc::new(AtomicU64::new(0)),
            resources_epoch: Arc::new(AtomicU64::new(0)),
        });
        let mut options = McpClientOptions::new("waltr-agent", env!("CARGO_PKG_VERSION"), timeout)
            .with_trace_propagation(trace_propagation);
        if let Some(level) = log_level {
            options = options.with_log_level(level);
        }
        let client = if let Some(provider) = config.bearer_provider {
            McpClient::connect_with_bearer_provider_and_transport_limits(
                name.clone(),
                config.transport,
                provider,
                handler.clone(),
                options,
                McpTransportLimits::new(WALTR_MCP_MESSAGE_BYTES, WALTR_MCP_MESSAGE_BYTES),
                cancellation_token.clone(),
            )
            .await?
        } else {
            McpClient::connect_with_transport_limits(
                name.clone(),
                config.transport,
                handler.clone(),
                options,
                McpTransportLimits::new(WALTR_MCP_MESSAGE_BYTES, WALTR_MCP_MESSAGE_BYTES),
                cancellation_token.clone(),
            )
            .await?
        };
        let runtime = Arc::new(Self {
            name,
            client,
            transport_kind,
            root_uri,
            events,
            active_progress,
            state: Mutex::new(McpServerState::default()),
        });
        *handler.refresh.lock().await = Some(Arc::clone(&runtime));
        runtime
            .finish_initialization(cancellation_token.clone())
            .await?;
        let capabilities = runtime.client.capabilities();
        let has_list_changes = capabilities
            .tools
            .as_ref()
            .is_some_and(|capability| capability.list_changed)
            || capabilities
                .prompts
                .as_ref()
                .is_some_and(|capability| capability.list_changed)
            || capabilities
                .resources
                .as_ref()
                .is_some_and(|capability| capability.list_changed);
        if has_list_changes {
            let subscription_runtime = Arc::clone(&runtime);
            tokio::spawn(async move {
                while !cancellation_token.is_cancelled() {
                    if let Err(error) = subscription_runtime
                        .client
                        .listen_for_list_changes(cancellation_token.clone())
                        .await
                    {
                        tracing::warn!(
                            server = %subscription_runtime.name,
                            %error,
                            "MCP list-change subscription ended"
                        );
                    }
                    tokio::select! {
                        () = cancellation_token.cancelled() => break,
                        () = tokio::time::sleep(Duration::from_secs(1)) => {}
                    }
                }
            });
        }
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
    ) -> Result<McpAgentToolResult> {
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
            mcp_tool_result(&response)
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
        self.refresh_tools(cancellation_token.clone()).await?;
        self.refresh_prompts(cancellation_token.clone()).await?;
        self.refresh_resources(cancellation_token).await?;
        drop(self.events.send(McpRuntimeEvent::ServerInitialized {
            server_key: self.name.clone(),
        }));
        Ok(())
    }

    async fn refresh_tools(&self, cancellation_token: CancellationToken) -> Result<()> {
        self.refresh_tools_at(cancellation_token, None).await
    }

    async fn refresh_tools_at(
        &self,
        cancellation_token: CancellationToken,
        epoch: Option<(Arc<AtomicU64>, u64)>,
    ) -> Result<()> {
        if self.state.lock().await.capabilities.tools.is_none() {
            return Ok(());
        }
        let mut tools = BTreeMap::new();
        for tool in self.client.list_tools(cancellation_token).await? {
            tools.insert(tool.name.clone(), tool);
        }
        if epoch
            .as_ref()
            .is_some_and(|(current, expected)| current.load(Ordering::Acquire) != *expected)
        {
            return Ok(());
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
        self.refresh_prompts_at(cancellation_token, None).await
    }

    async fn refresh_prompts_at(
        &self,
        cancellation_token: CancellationToken,
        epoch: Option<(Arc<AtomicU64>, u64)>,
    ) -> Result<()> {
        if self.state.lock().await.capabilities.prompts.is_none() {
            return Ok(());
        }
        let mut prompts = BTreeMap::new();
        for prompt in self.client.list_prompts(cancellation_token).await? {
            prompts.insert(prompt.name.clone(), prompt);
        }
        if epoch
            .as_ref()
            .is_some_and(|(current, expected)| current.load(Ordering::Acquire) != *expected)
        {
            return Ok(());
        }
        let mut state = self.state.lock().await;
        state.prompts.prompts = prompts;
        state.prompts.generation += 1;
        drop(state);
        Ok(())
    }

    async fn refresh_resources(&self, cancellation_token: CancellationToken) -> Result<()> {
        self.refresh_resources_at(cancellation_token, None).await
    }

    async fn refresh_resources_at(
        &self,
        cancellation_token: CancellationToken,
        epoch: Option<(Arc<AtomicU64>, u64)>,
    ) -> Result<()> {
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
        let mut resource_templates = BTreeMap::new();
        for resource_template in self
            .client
            .list_resource_templates(cancellation_token)
            .await?
        {
            resource_templates.insert(resource_template.uri_template.clone(), resource_template);
        }
        if epoch
            .as_ref()
            .is_some_and(|(current, expected)| current.load(Ordering::Acquire) != *expected)
        {
            return Ok(());
        }
        let mut state = self.state.lock().await;
        state.resources.resources = resources;
        state.resources.resource_templates = resource_templates;
        state.resources.generation += 1;
        drop(state);
        Ok(())
    }

    pub(in crate::mcp) async fn shutdown(&self) -> Result<()> {
        self.client.shutdown().await?;
        Ok(())
    }
}

fn mcp_tool_result(response: &mcp::McpToolResult) -> Result<McpAgentToolResult> {
    let parsed = response
        .content()
        .map_err(|error| protocol_error(error.to_string()))?;
    let mut blocks = Vec::with_capacity(parsed.content.len() + 1);
    for block in parsed.content {
        match block {
            McpToolContent::Text { text } => blocks.push(ToolResultContentBlock::Text {
                text: text.to_owned(),
            }),
            McpToolContent::Image { data, mime_type } => {
                let image = ToolResultImage::new(data, mime_type)
                    .map_err(|error| protocol_error(error.to_string()))?;
                blocks.push(ToolResultContentBlock::Image(image));
            }
            McpToolContent::Unsupported { kind, .. } => {
                blocks.push(ToolResultContentBlock::Text {
                    text: format!("[unsupported MCP content type: {kind}]"),
                });
            }
        }
    }
    if let Some(structured) = parsed.structured_content {
        blocks.push(ToolResultContentBlock::Text {
            text: format!("structuredContent: {structured}"),
        });
    }
    if blocks.is_empty() {
        blocks.push(ToolResultContentBlock::Text {
            text: response.raw.to_string(),
        });
    }
    if parsed.is_error {
        match blocks.first_mut() {
            Some(ToolResultContentBlock::Text { text }) => {
                text.insert_str(0, "MCP tool error: ");
            }
            _ => blocks.insert(
                0,
                ToolResultContentBlock::Text {
                    text: "MCP tool error".to_owned(),
                },
            ),
        }
    }
    let content = ToolResultContent::from_blocks(blocks)
        .map_err(|error| protocol_error(error.to_string()))?;
    Ok(McpAgentToolResult {
        content,
        is_error: parsed.is_error,
    })
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
            let epoch = self.tools_epoch.fetch_add(1, Ordering::AcqRel) + 1;
            let current = Arc::clone(&self.tools_epoch);
            let cancellation = self.cancellation.child_token();
            tokio::spawn(async move {
                if let Err(error) = runtime
                    .refresh_tools_at(cancellation, Some((current, epoch)))
                    .await
                {
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
            let epoch = self.prompts_epoch.fetch_add(1, Ordering::AcqRel) + 1;
            let current = Arc::clone(&self.prompts_epoch);
            let cancellation = self.cancellation.child_token();
            tokio::spawn(async move {
                if let Err(error) = runtime
                    .refresh_prompts_at(cancellation, Some((current, epoch)))
                    .await
                {
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
            let epoch = self.resources_epoch.fetch_add(1, Ordering::AcqRel) + 1;
            let current = Arc::clone(&self.resources_epoch);
            let cancellation = self.cancellation.child_token();
            tokio::spawn(async move {
                if let Err(error) = runtime
                    .refresh_resources_at(cancellation, Some((current, epoch)))
                    .await
                {
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
}

impl ServerHandler {
    #[allow(clippy::needless_pass_by_value)]
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

#[allow(clippy::too_many_arguments)]
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

    use mcp::{McpLogLevel, McpToolResult};

    use crate::{MAX_TOOL_RESULT_IMAGE_BYTES, ToolResultContentBlock};

    use super::{McpRuntimeEvent, ServerHandler, mcp_tool_result};

    #[test]
    fn normalizes_mcp_log_notification_events() {
        let (events, mut receiver) = broadcast::channel(8);
        let handler = ServerHandler {
            name: "test_server".to_owned(),
            root_uri: "file:///workspace".to_owned(),
            events,
            active_progress: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            refresh: Mutex::new(None),
            cancellation: tokio_util::sync::CancellationToken::new(),
            tools_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            prompts_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            resources_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
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

    #[test]
    fn converts_validated_mcp_images_and_error_metadata() {
        let result = mcp_tool_result(&McpToolResult::new(json!({
            "content": [
                { "type": "text", "text": "snapshot" },
                {
                    "type": "image",
                    "data": "iVBORw0KGgo=",
                    "mimeType": "image/png"
                }
            ],
            "structuredContent": { "camera": "front" },
            "isError": true
        })))
        .expect("valid image result");

        assert!(result.is_error);
        assert_eq!(result.content.blocks().len(), 3);
        assert!(matches!(
            &result.content.blocks()[0],
            ToolResultContentBlock::Text { text } if text == "MCP tool error: snapshot"
        ));
        assert!(matches!(
            &result.content.blocks()[1],
            ToolResultContentBlock::Image(image)
                if image.media_type() == "image/png"
                    && image.decoded_bytes() == 8
                    && image.sha256().len() == 64
        ));
        assert!(result.content.as_str().contains("structuredContent"));
        assert!(!result.content.as_str().contains("iVBOR"));
    }

    #[test]
    fn rejects_invalid_mcp_image_encoding_media_and_magic() {
        for image in [
            json!({ "type": "image", "data": "not-base64", "mimeType": "image/png" }),
            json!({ "type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/gif" }),
            json!({ "type": "image", "data": "aW1hZ2U=", "mimeType": "image/png" }),
        ] {
            let error = mcp_tool_result(&McpToolResult::new(json!({ "content": [image] })))
                .expect_err("invalid image must fail");
            assert!(error.to_string().contains("mcp protocol error"));
            assert!(!error.to_string().contains("not-base64"));
            assert!(!error.to_string().contains("iVBOR"));
        }
    }

    #[test]
    fn rejects_mcp_images_before_decoding_excessive_input() {
        let oversized = "A".repeat(MAX_TOOL_RESULT_IMAGE_BYTES.div_ceil(3) * 4 + 1);
        let error = mcp_tool_result(&McpToolResult::new(json!({
            "content": [{
                "type": "image",
                "data": oversized,
                "mimeType": "image/png"
            }]
        })))
        .expect_err("oversized image must fail");

        assert!(error.to_string().contains("size limit"));
    }
}
