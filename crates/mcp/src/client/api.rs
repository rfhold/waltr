use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, RwLock as StdRwLock},
    time::Duration,
};

use serde_json::{Value, json};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::{
    McpAuthorizationError, McpCompletionRequest, McpCompletionResult, McpLogLevel,
    McpProgressNotification, McpPromptList, McpPromptResult, McpResourceList, McpResourceResult,
    McpResourceTemplateList, McpRoot, McpServerCapabilities, McpToolCall, McpToolList,
    McpToolResult, McpTracePropagation, Result,
    http_headers::{HeaderDescriptor, header_descriptors, parameter_headers},
    protocol::{
        JsonRpcId, JsonRpcNotification, MCP_CLIENT_CAPABILITIES_META_KEY, MCP_CLIENT_INFO_META_KEY,
        MCP_LOG_LEVEL_META_KEY, MCP_PROTOCOL_VERSION, MCP_PROTOCOL_VERSION_META_KEY,
        MCP_SUBSCRIPTION_ID_META_KEY,
    },
    trace::inject_request_context,
    types::{parse_completion, valid_progress_token},
};

use super::{
    connection::McpConnection,
    transport::{
        McpBearerProvider, McpBearerProviderError, McpTransportConfig, McpTransportError,
        McpTransportLimits, McpTransportOptions,
    },
};

#[derive(Debug, Clone)]
/// Options used when connecting an MCP client.
pub struct McpClientOptions {
    /// Opt in to the official Skills extension. Disabled by default.
    pub skills: bool,
    /// Client implementation name sent in request metadata.
    pub client_name: String,
    /// Client implementation version sent in request metadata.
    pub client_version: String,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Optional request-level trace propagation.
    pub trace_propagation: McpTracePropagation,
    /// Optional request-scoped logging level.
    pub log_level: Option<McpLogLevel>,
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
            skills: false,
            client_name: client_name.into(),
            client_version: client_version.into(),
            timeout,
            trace_propagation: McpTracePropagation::Disabled,
            log_level: None,
        }
    }

    /// Set request-level trace propagation.
    #[must_use]
    pub const fn with_skills(mut self) -> Self {
        self.skills = true;
        self
    }

    /// Set request-level trace propagation.
    #[must_use]
    pub const fn with_trace_propagation(mut self, trace_propagation: McpTracePropagation) -> Self {
        self.trace_propagation = trace_propagation;
        self
    }

    /// Request structured logs at or above this level on each ordinary request.
    #[must_use]
    pub const fn with_log_level(mut self, log_level: McpLogLevel) -> Self {
        self.log_level = Some(log_level);
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

    /// Called when Streamable HTTP authorization fails after connection startup.
    fn authorization_failed(&self, _error: McpAuthorizationError) {}

    /// Called when a dynamic Streamable HTTP bearer provider fails in the background.
    fn bearer_provider_failed(&self, _error: McpBearerProviderError) {}

    /// Called when a background transport operation fails.
    fn transport_failed(&self, _error: McpTransportError) {}
}

/// MCP client with typed request helpers.
pub struct McpClient {
    connection: Arc<McpConnection>,
    timeout: Duration,
    capabilities: McpServerCapabilities,
    instructions: Option<String>,
    trace_propagation: McpTracePropagation,
    request_meta: serde_json::Map<String, Value>,
    handler: Arc<dyn McpClientHandler>,
    tool_headers: Arc<RwLock<HashMap<String, Vec<HeaderDescriptor>>>>,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpClient")
            .field("capabilities", &self.capabilities)
            .field("instructions", &self.instructions)
            .field("trace_propagation", &self.trace_propagation)
            .finish_non_exhaustive()
    }
}

impl McpClient {
    fn require_skills(&self) -> Result<()> {
        let opted_in = self
            .request_meta
            .get(MCP_CLIENT_CAPABILITIES_META_KEY)
            .and_then(|v| v.get("extensions"))
            .and_then(|v| v.get(crate::skills::SKILLS_EXTENSION))
            .is_some();
        if !opted_in || !self.capabilities.supports_skills() {
            return Err(crate::Error::protocol(
                "Skills extension requires explicit client opt-in and server advertisement",
            ));
        }
        Ok(())
    }

    /// List one skill page while retaining cache metadata; no file content is prefetched.
    pub async fn list_skills(
        &self,
        cursor: Option<&str>,
        cancellation_token: CancellationToken,
    ) -> Result<crate::McpSkillList> {
        self.require_skills()?;
        let mut params = json!({});
        if let Some(cursor) = cursor {
            params["cursor"] = json!(cursor);
        }
        let raw = self
            .request("skills/list", Some(params), cancellation_token)
            .await?;
        let page: crate::McpSkillList = serde_json::from_value(raw)
            .map_err(|e| crate::Error::protocol(format!("invalid skills/list result: {e}")))?;
        let mut seen = std::collections::HashSet::new();
        for skill in &page.skills {
            crate::skills::validate_skill(skill)?;
            if !seen.insert(&skill.uri) {
                return Err(crate::Error::protocol("duplicate skill URI on list page"));
            }
        }
        Ok(page)
    }

    /// Fetch a held entry by URI without fetching or activating its files.
    pub async fn get_skill(
        &self,
        uri: &str,
        cancellation_token: CancellationToken,
    ) -> Result<crate::McpSkillResult> {
        self.require_skills()?;
        let raw = self
            .request("skills/get", Some(json!({"uri": uri})), cancellation_token)
            .await?;
        let result: crate::McpSkillResult = serde_json::from_value(raw)
            .map_err(|e| crate::Error::protocol(format!("invalid skills/get result: {e}")))?;
        crate::skills::validate_skill(&result.skill)?;
        if result.skill.uri != uri {
            return Err(crate::Error::protocol(
                "skills/get returned a different URI",
            ));
        }
        Ok(result)
    }
    /// Connect to a server, discover its capabilities, and return a typed client.
    ///
    /// # Errors
    ///
    /// Returns an error when transport startup or MCP discovery fails.
    pub async fn connect(
        server_name: impl Into<String>,
        transport: McpTransportConfig,
        handler: Arc<dyn McpClientHandler>,
        options: McpClientOptions,
        cancellation_token: CancellationToken,
    ) -> Result<Self> {
        Self::connect_with_transport_limits(
            server_name,
            transport,
            handler,
            options,
            McpTransportLimits::default(),
            cancellation_token,
        )
        .await
    }

    /// Connect with explicit receive limits, discover capabilities, and return a typed client.
    ///
    /// # Errors
    ///
    /// Returns an error when transport startup or MCP discovery fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn connect_with_transport_limits(
        server_name: impl Into<String>,
        transport: McpTransportConfig,
        handler: Arc<dyn McpClientHandler>,
        options: McpClientOptions,
        transport_limits: McpTransportLimits,
        cancellation_token: CancellationToken,
    ) -> Result<Self> {
        Self::connect_with_transport_options(
            server_name,
            transport,
            handler,
            options,
            McpTransportOptions::default().with_limits(transport_limits),
            cancellation_token,
        )
        .await
    }

    /// Connect with unified transport runtime options and discover server capabilities.
    ///
    /// # Errors
    ///
    /// Returns an error when transport startup or MCP discovery fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn connect_with_transport_options(
        server_name: impl Into<String>,
        transport: McpTransportConfig,
        handler: Arc<dyn McpClientHandler>,
        options: McpClientOptions,
        transport_options: McpTransportOptions,
        cancellation_token: CancellationToken,
    ) -> Result<Self> {
        let active_subscriptions = Arc::new(StdRwLock::new(HashSet::new()));
        let adapter = Arc::new(ClientHandlerAdapter {
            inner: Arc::clone(&handler),
            active_subscriptions: Arc::clone(&active_subscriptions),
        });
        let connection = McpConnection::start_with_transport_options(
            server_name.into(),
            transport,
            adapter,
            active_subscriptions,
            transport_options,
        )
        .await?;
        Self::discover(connection, handler, options, cancellation_token).await
    }

    /// Connect using a dynamic bearer provider for every Streamable HTTP request.
    ///
    /// The provider supersedes the static token in Streamable HTTP configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when token resolution, transport startup, or discovery fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn connect_with_bearer_provider(
        server_name: impl Into<String>,
        transport: McpTransportConfig,
        bearer_provider: Arc<dyn McpBearerProvider>,
        handler: Arc<dyn McpClientHandler>,
        options: McpClientOptions,
        cancellation_token: CancellationToken,
    ) -> Result<Self> {
        Self::connect_with_bearer_provider_and_transport_limits(
            server_name,
            transport,
            bearer_provider,
            handler,
            options,
            McpTransportLimits::default(),
            cancellation_token,
        )
        .await
    }

    /// Connect using a dynamic bearer provider and explicit receive limits.
    ///
    /// The provider supersedes the static token in Streamable HTTP configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when token resolution, transport startup, or discovery fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn connect_with_bearer_provider_and_transport_limits(
        server_name: impl Into<String>,
        transport: McpTransportConfig,
        bearer_provider: Arc<dyn McpBearerProvider>,
        handler: Arc<dyn McpClientHandler>,
        options: McpClientOptions,
        transport_limits: McpTransportLimits,
        cancellation_token: CancellationToken,
    ) -> Result<Self> {
        Self::connect_with_transport_options(
            server_name,
            transport,
            handler,
            options,
            McpTransportOptions::default()
                .with_limits(transport_limits)
                .with_bearer_provider(bearer_provider),
            cancellation_token,
        )
        .await
    }

    #[allow(clippy::too_many_lines)]
    async fn discover(
        connection: Arc<McpConnection>,
        handler: Arc<dyn McpClientHandler>,
        options: McpClientOptions,
        cancellation_token: CancellationToken,
    ) -> Result<Self> {
        let trace_propagation = options.trace_propagation;
        let request_meta = client_request_meta(&options);
        let discover = match connection
            .request_with_trace_propagation(
                "server/discover",
                inject_request_context(Some(json!({ "_meta": request_meta })), trace_propagation),
                Some(options.timeout),
                cancellation_token,
                trace_propagation,
                Vec::new(),
            )
            .await
        {
            Ok(discover) => discover,
            Err(error) => {
                drop(connection.shutdown().await);
                return Err(error);
            }
        };
        let validated = (|| -> Result<(Value, Option<String>)> {
            match discover.get("resultType").and_then(Value::as_str) {
                None | Some("complete") => {}
                Some(result_type) => {
                    return Err(crate::Error::protocol(format!(
                        "unsupported MCP discovery resultType `{result_type}`"
                    )));
                }
            }
            let supports_revision = discover
                .get("supportedVersions")
                .and_then(Value::as_array)
                .is_some_and(|versions| {
                    versions
                        .iter()
                        .any(|version| version == MCP_PROTOCOL_VERSION)
                });
            if !supports_revision {
                return Err(crate::Error::protocol(
                    "server does not advertise MCP protocol version `2026-07-28`",
                ));
            }
            let capabilities = discover
                .get("capabilities")
                .filter(|capabilities| capabilities.is_object())
                .cloned()
                .ok_or_else(|| {
                    crate::Error::protocol("server discovery capabilities must be an object")
                })?;
            if discover.get("ttlMs").and_then(Value::as_u64).is_none() {
                return Err(crate::Error::protocol(
                    "server discovery ttlMs must be a non-negative integer",
                ));
            }
            if !matches!(
                discover.get("cacheScope").and_then(Value::as_str),
                Some("public" | "private")
            ) {
                return Err(crate::Error::protocol(
                    "server discovery cacheScope must be `public` or `private`",
                ));
            }
            let instructions = discover
                .get("instructions")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|instructions| !instructions.trim().is_empty());
            Ok((capabilities, instructions))
        })();
        let (capabilities_value, instructions) = match validated {
            Ok(validated) => validated,
            Err(error) => {
                drop(connection.shutdown().await);
                return Err(error);
            }
        };
        let capabilities = McpServerCapabilities::parse(Some(&capabilities_value));
        Ok(Self {
            connection,
            timeout: options.timeout,
            capabilities,
            instructions,
            trace_propagation,
            request_meta,
            handler,
            tool_headers: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Return capabilities advertised by the server.
    #[must_use]
    pub const fn capabilities(&self) -> &McpServerCapabilities {
        &self.capabilities
    }

    /// Return optional server instructions from the discovery response.
    #[must_use]
    pub fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    /// Listen for list-change notifications advertised by the server.
    ///
    /// # Errors
    ///
    /// Returns an error when the subscription cannot be opened or closes unexpectedly.
    pub async fn listen_for_list_changes(
        &self,
        cancellation_token: CancellationToken,
    ) -> Result<()> {
        let mut notifications = serde_json::Map::new();
        if self
            .capabilities
            .tools
            .as_ref()
            .is_some_and(|capability| capability.list_changed)
        {
            notifications.insert("toolsListChanged".to_owned(), Value::Bool(true));
        }
        if self
            .capabilities
            .prompts
            .as_ref()
            .is_some_and(|capability| capability.list_changed)
        {
            notifications.insert("promptsListChanged".to_owned(), Value::Bool(true));
        }
        if self
            .capabilities
            .resources
            .as_ref()
            .is_some_and(|capability| capability.list_changed)
        {
            notifications.insert("resourcesListChanged".to_owned(), Value::Bool(true));
        }
        if notifications.is_empty() {
            return Ok(());
        }
        self.request_with_timeout(
            "subscriptions/listen",
            Some(json!({ "notifications": notifications })),
            cancellation_token,
            None,
        )
        .await?;
        Ok(())
    }

    /// Call a server tool.
    ///
    /// # Errors
    ///
    /// Returns an error when the request fails or the server rejects the tool call.
    pub async fn call_tool(
        &self,
        call: McpToolCall,
        cancellation_token: CancellationToken,
    ) -> Result<McpToolResult> {
        if call
            .progress_token
            .as_ref()
            .is_some_and(|token| !valid_progress_token(token))
        {
            return Err(crate::Error::protocol(
                "MCP progress token must be a string or integer",
            ));
        }
        self.request("tools/call", Some(call.to_params()), cancellation_token)
            .await
            .map(McpToolResult::new)
    }

    /// Get a server prompt.
    ///
    /// # Errors
    ///
    /// Returns an error when the request fails or the server rejects the prompt request.
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
    ///
    /// # Errors
    ///
    /// Returns an error when the request fails or the resource cannot be read.
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
    ///
    /// # Errors
    ///
    /// Returns an error when the completion request fails or its response is invalid.
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

    /// List all tools, following pagination cursors.
    ///
    /// # Errors
    ///
    /// Returns an error when any tools page cannot be loaded or parsed.
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
    ///
    /// # Errors
    ///
    /// Returns an error when the tools page cannot be loaded or parsed.
    pub async fn list_tools_page(
        &self,
        cursor: Option<&str>,
        cancellation_token: CancellationToken,
    ) -> Result<McpToolList> {
        let response = self
            .request(
                "tools/list",
                cursor.map(|cursor| json!({ "cursor": cursor })),
                cancellation_token,
            )
            .await?;
        let mut page = McpToolList::parse(&response)?;
        let mut descriptors = self.tool_headers.write().await;
        page.tools.retain(|tool| match header_descriptors(&tool.input_schema) {
            Ok(headers) => {
                descriptors.insert(tool.name.clone(), headers);
                true
            }
            Err(error) => {
                tracing::warn!(tool.name = %tool.name, %error, "excluding MCP tool with invalid x-mcp-header schema");
                descriptors.remove(&tool.name);
                false
            }
        });
        Ok(page)
    }

    /// List all prompts, following pagination cursors.
    ///
    /// # Errors
    ///
    /// Returns an error when any prompts page cannot be loaded or parsed.
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
    ///
    /// # Errors
    ///
    /// Returns an error when the prompts page cannot be loaded or parsed.
    pub async fn list_prompts_page(
        &self,
        cursor: Option<&str>,
        cancellation_token: CancellationToken,
    ) -> Result<McpPromptList> {
        let response = self
            .request(
                "prompts/list",
                cursor.map(|cursor| json!({ "cursor": cursor })),
                cancellation_token,
            )
            .await?;
        McpPromptList::parse(&response)
    }

    /// List all resources, following pagination cursors.
    ///
    /// # Errors
    ///
    /// Returns an error when any resources page cannot be loaded or parsed.
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
    ///
    /// # Errors
    ///
    /// Returns an error when the resources page cannot be loaded or parsed.
    pub async fn list_resources_page(
        &self,
        cursor: Option<&str>,
        cancellation_token: CancellationToken,
    ) -> Result<McpResourceList> {
        let response = self
            .request(
                "resources/list",
                cursor.map(|cursor| json!({ "cursor": cursor })),
                cancellation_token,
            )
            .await?;
        McpResourceList::parse(&response)
    }

    /// List all resource templates, following pagination cursors.
    ///
    /// # Errors
    ///
    /// Returns an error when any resource-template page cannot be loaded or parsed.
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
    ///
    /// # Errors
    ///
    /// Returns an error when the resource-template page cannot be loaded or parsed.
    pub async fn list_resource_templates_page(
        &self,
        cursor: Option<&str>,
        cancellation_token: CancellationToken,
    ) -> Result<McpResourceTemplateList> {
        let response = self
            .request(
                "resources/templates/list",
                cursor.map(|cursor| json!({ "cursor": cursor })),
                cancellation_token,
            )
            .await?;
        McpResourceTemplateList::parse(&response)
    }

    /// Shut down the client transport.
    ///
    /// # Errors
    ///
    /// Returns an error when graceful transport shutdown fails.
    pub async fn shutdown(&self) -> Result<()> {
        self.connection.shutdown().await
    }

    async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        cancellation_token: CancellationToken,
    ) -> Result<Value> {
        self.request_with_timeout(method, params, cancellation_token, Some(self.timeout))
            .await
    }

    #[allow(clippy::too_many_lines)]
    async fn request_with_timeout(
        &self,
        method: &str,
        params: Option<Value>,
        cancellation_token: CancellationToken,
        timeout: Option<Duration>,
    ) -> Result<Value> {
        let mut params = params.unwrap_or_else(|| json!({}));
        loop {
            let params_object = params
                .as_object_mut()
                .ok_or_else(|| crate::Error::protocol("MCP request params must be an object"))?;
            let mut meta = params_object
                .remove("_meta")
                .and_then(|meta| meta.as_object().cloned())
                .unwrap_or_default();
            meta.extend(self.request_meta.clone());
            params_object.insert("_meta".to_owned(), Value::Object(meta));
            let result = self
                .connection
                .request_with_trace_propagation(
                    method,
                    inject_request_context(Some(params.clone()), self.trace_propagation),
                    timeout,
                    cancellation_token.clone(),
                    self.trace_propagation,
                    self.http_headers(method, &params).await?,
                )
                .await?;
            let result_type = result
                .get("resultType")
                .and_then(Value::as_str)
                .unwrap_or("complete");
            match result_type {
                "complete" => return Ok(result),
                "input_required" => {}
                other => {
                    return Err(crate::Error::protocol(format!(
                        "unsupported MCP resultType `{other}`"
                    )));
                }
            }
            if !matches!(method, "tools/call" | "prompts/get" | "resources/read") {
                return Err(crate::Error::protocol(format!(
                    "input_required result is not valid for `{method}`"
                )));
            }
            let mut responses = serde_json::Map::new();
            if let Some(requests) = result.get("inputRequests") {
                let requests = requests.as_object().ok_or_else(|| {
                    crate::Error::protocol("input_required inputRequests must be an object")
                })?;
                for (key, request) in requests {
                    match request.get("method").and_then(Value::as_str) {
                        Some("roots/list") => {
                            responses.insert(
                                key.clone(),
                                json!({
                                    "resultType": "complete",
                                    "roots": self.handler.roots().iter().map(McpRoot::to_value).collect::<Vec<_>>()
                                }),
                            );
                        }
                        Some(method) => {
                            return Err(crate::Error::protocol(format!(
                                "unsupported MCP input request `{method}`"
                            )));
                        }
                        None => {
                            return Err(crate::Error::protocol("MCP input request omitted method"));
                        }
                    }
                }
            }
            if responses.is_empty() && result.get("requestState").is_none() {
                return Err(crate::Error::protocol(
                    "input_required result omitted inputRequests and requestState",
                ));
            }
            let params_object = params.as_object_mut().expect("params validated above");
            params_object.remove("inputResponses");
            params_object.remove("requestState");
            if !responses.is_empty() {
                params_object.insert("inputResponses".to_owned(), Value::Object(responses));
            }
            if let Some(state) = result.get("requestState") {
                params_object.insert("requestState".to_owned(), state.clone());
            }
        }
    }

    async fn http_headers(&self, method: &str, params: &Value) -> Result<Vec<(String, String)>> {
        if method != "tools/call" {
            return Ok(Vec::new());
        }
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Ok(Vec::new());
        };
        let descriptors = self.tool_headers.read().await.get(name).cloned();
        let Some(descriptors) = descriptors else {
            return Ok(Vec::new());
        };
        parameter_headers(&descriptors, params.get("arguments"))
    }
}

fn client_request_meta(options: &McpClientOptions) -> serde_json::Map<String, Value> {
    let mut meta = serde_json::Map::from_iter([
        (
            MCP_PROTOCOL_VERSION_META_KEY.to_owned(),
            Value::String(MCP_PROTOCOL_VERSION.to_owned()),
        ),
        (
            MCP_CLIENT_CAPABILITIES_META_KEY.to_owned(),
            json!({ "roots": {} }),
        ),
        (
            MCP_CLIENT_INFO_META_KEY.to_owned(),
            json!({ "name": options.client_name, "version": options.client_version }),
        ),
    ]);
    if let Some(level) = options.log_level {
        meta.insert(MCP_LOG_LEVEL_META_KEY.to_owned(), json!(level.to_string()));
    }
    if options.skills {
        meta.get_mut(MCP_CLIENT_CAPABILITIES_META_KEY)
            .expect("client capabilities")["extensions"] =
            json!({crate::skills::SKILLS_EXTENSION: {}});
    }
    meta
}

struct ClientHandlerAdapter {
    inner: Arc<dyn McpClientHandler>,
    active_subscriptions: Arc<StdRwLock<HashSet<JsonRpcId>>>,
}

impl super::connection::McpConnectionHandler for ClientHandlerAdapter {
    fn notification(&self, notification: JsonRpcNotification) {
        let list_change = matches!(
            notification.method.as_str(),
            "notifications/tools/list_changed"
                | "notifications/prompts/list_changed"
                | "notifications/resources/list_changed"
        );
        if list_change && !self.belongs_to_active_subscription(notification.params.as_ref()) {
            return;
        }
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

    fn disconnected(&self, error: String) {
        self.inner.disconnected(error);
    }

    fn authorization_failed(&self, error: McpAuthorizationError) {
        self.inner.authorization_failed(error);
    }

    fn bearer_provider_failed(&self, error: McpBearerProviderError) {
        self.inner.bearer_provider_failed(error);
    }

    fn transport_failed(&self, error: McpTransportError) {
        self.inner.transport_failed(error);
    }
}

impl ClientHandlerAdapter {
    fn belongs_to_active_subscription(&self, params: Option<&Value>) -> bool {
        let Some(id) = params
            .and_then(|params| params.get("_meta"))
            .and_then(|meta| meta.get(MCP_SUBSCRIPTION_ID_META_KEY))
            .and_then(|id| serde_json::from_value::<JsonRpcId>(id.clone()).ok())
        else {
            return false;
        };
        self.active_subscriptions
            .read()
            .expect("active subscription lock poisoned")
            .contains(&id)
    }
}
