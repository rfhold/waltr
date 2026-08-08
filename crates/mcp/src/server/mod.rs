use std::{
    collections::{BTreeMap, HashSet},
    convert::Infallible,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, SystemTime},
};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::{Mutex, mpsc},
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;
use url::Url;

use crate::{
    Error, Result,
    http_headers::{decode_header_value, expected_parameter_values, header_descriptors},
    oauth::{
        McpAccessToken, McpBearerChallenge, McpPrincipalId, McpProtectedResourceMetadata,
        McpTokenValidation, protected_resource_metadata_urls,
    },
    protocol::{
        HEADER_MISMATCH, JsonRpcId, JsonRpcInbound, JsonRpcNotification, JsonRpcRequest,
        MCP_CLIENT_CAPABILITIES_META_KEY, MCP_LOG_LEVEL_META_KEY, MCP_PROTOCOL_VERSION,
        MCP_PROTOCOL_VERSION_META_KEY, MCP_SERVER_INFO_META_KEY, MCP_SUBSCRIPTION_ID_META_KEY,
        UNSUPPORTED_PROTOCOL_VERSION, json_rpc_error, json_rpc_error_with_data,
        json_rpc_method_not_found, json_rpc_notification, json_rpc_success, parse_json_rpc_message,
    },
    trace::{request_meta, set_span_parent_from_meta},
    types::{
        McpCompletionRequest, McpIcon, McpPromptList, McpPromptResult, McpResourceList,
        McpResourceResult, McpResourceTemplateList, McpToolCall, McpToolList, McpToolResult,
        icons_to_value, require_string,
    },
};

/// Axum router returned by Streamable HTTP server helpers.
pub type StreamableHttpRouter = Router;

/// JSON value type used by generated server handlers.
pub type JsonValue = Value;

/// Boxed MCP server future.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Server implementation metadata.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    /// Server name.
    pub name: String,
    /// Server version.
    pub version: String,
    /// Optional human-readable server description.
    pub description: Option<String>,
    /// Optional server icon metadata.
    pub icons: Vec<McpIcon>,
}

impl ServerInfo {
    /// Create server metadata.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            description: None,
            icons: Vec::new(),
        }
    }

    /// Attach a human-readable server description.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Attach server icon metadata.
    #[must_use]
    pub fn with_icons(mut self, icons: Vec<McpIcon>) -> Self {
        self.icons = icons;
        self
    }
}

/// Server capabilities advertised by discovery.
#[derive(Debug, Clone, Default)]
pub struct ServerCapabilities {
    /// Tools capability object.
    pub tools: Option<Value>,
    /// Prompts capability object.
    pub prompts: Option<Value>,
    /// Resources capability object.
    pub resources: Option<Value>,
    /// Completion capability object.
    pub completions: Option<Value>,
    /// Logging capability object.
    pub logging: Option<Value>,
    /// Extension capabilities keyed by extension identifier.
    pub extensions: Option<Value>,
}

impl ServerCapabilities {
    /// Create empty server capabilities.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Advertise tools support.
    #[must_use]
    pub fn tools(mut self) -> Self {
        self.tools = Some(json!({ "listChanged": false }));
        self
    }

    /// Advertise prompts support.
    #[must_use]
    pub fn prompts(mut self) -> Self {
        self.prompts = Some(json!({ "listChanged": false }));
        self
    }

    /// Advertise resources support.
    #[must_use]
    pub fn resources(mut self) -> Self {
        self.resources = Some(json!({ "subscribe": false, "listChanged": false }));
        self
    }

    /// Advertise completions support.
    #[must_use]
    pub fn completions(mut self) -> Self {
        self.completions = Some(json!({}));
        self
    }

    /// Advertise logging support.
    #[must_use]
    pub fn logging(mut self) -> Self {
        self.logging = Some(json!({}));
        self
    }

    /// Advertise extension capabilities.
    #[must_use]
    pub fn extensions(mut self, capabilities: Value) -> Self {
        self.extensions = Some(capabilities);
        self
    }

    fn to_value(&self) -> Value {
        let mut object = serde_json::Map::new();
        if let Some(value) = &self.tools {
            object.insert("tools".to_owned(), value.clone());
        }
        if let Some(value) = &self.prompts {
            object.insert("prompts".to_owned(), value.clone());
        }
        if let Some(value) = &self.resources {
            object.insert("resources".to_owned(), value.clone());
        }
        if let Some(value) = &self.completions {
            object.insert("completions".to_owned(), value.clone());
        }
        if let Some(value) = &self.logging {
            object.insert("logging".to_owned(), value.clone());
        }
        if let Some(value) = &self.extensions {
            object.insert("extensions".to_owned(), value.clone());
        }
        Value::Object(object)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Error returned by a typed MCP server handler.
pub struct ServerError {
    code: i64,
    message: String,
}

impl ServerError {
    /// Create an invalid-params JSON-RPC error.
    #[must_use]
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }

    /// Create an internal JSON-RPC error.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: message.into(),
        }
    }

    /// Create a method-not-found JSON-RPC error.
    #[must_use]
    pub fn method_not_found(method: impl Into<String>) -> Self {
        Self {
            code: -32601,
            message: method.into(),
        }
    }

    /// Create a resource-not-found MCP error.
    #[must_use]
    pub fn resource_not_found(uri: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: format!("resource not found: {}", uri.into()),
        }
    }
}

/// Result returned by typed MCP server handler methods.
pub type ServerResult<T> = std::result::Result<T, ServerError>;

/// Per-request server context.
#[derive(Clone)]
pub struct ServerContext {
    sender: NotificationSender,
    request_id: Option<JsonRpcId>,
    request_meta: Option<serde_json::Map<String, Value>>,
    protocol_version: Option<String>,
    client_capabilities: Option<Value>,
    subscription_id: Option<Value>,
    cancellation: CancellationToken,
    input_responses: Option<Value>,
    request_state: Option<String>,
    last_progress: Arc<Mutex<Option<f64>>>,
}

impl std::fmt::Debug for ServerContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerContext")
            .field("request_id", &self.request_id)
            .field("protocol_version", &self.protocol_version)
            .field("input_responses", &self.input_responses)
            .field("request_state", &self.request_state)
            .field("subscription_id", &self.subscription_id)
            .field(
                "request_meta_keys",
                &self.request_meta.as_ref().map(serde_json::Map::len),
            )
            .finish_non_exhaustive()
    }
}

impl ServerContext {
    /// Return the current request id, if this context belongs to a request.
    #[must_use]
    pub const fn request_id(&self) -> Option<&JsonRpcId> {
        self.request_id.as_ref()
    }

    /// Return request `_meta`, if present and object-shaped.
    #[must_use]
    pub const fn request_meta(&self) -> Option<&serde_json::Map<String, Value>> {
        self.request_meta.as_ref()
    }

    /// Return one value from request `_meta`.
    #[must_use]
    pub fn meta_value(&self, key: &str) -> Option<&Value> {
        self.request_meta.as_ref().and_then(|meta| meta.get(key))
    }

    /// Return the validated client capabilities declared for this request.
    #[must_use]
    pub const fn client_capabilities(&self) -> Option<&Value> {
        self.client_capabilities.as_ref()
    }

    /// Return input responses supplied on an MRTR retry.
    #[must_use]
    pub const fn input_responses(&self) -> Option<&Value> {
        self.input_responses.as_ref()
    }

    /// Return opaque request state supplied on an MRTR retry.
    #[must_use]
    pub fn request_state(&self) -> Option<&str> {
        self.request_state.as_deref()
    }

    /// Wait until the transport cancels this request.
    pub async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }

    /// Send a notification to the peer.
    ///
    /// # Errors
    ///
    /// Returns an error when the peer notification channel is closed.
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        let mut params = params.unwrap_or_else(|| json!({}));
        if let Some(subscription_id) = &self.subscription_id {
            let meta = params
                .as_object_mut()
                .ok_or_else(|| Error::protocol("MCP notification params must be an object"))?
                .entry("_meta")
                .or_insert_with(|| json!({}));
            meta.as_object_mut()
                .ok_or_else(|| Error::protocol("MCP notification params._meta must be an object"))?
                .insert(
                    MCP_SUBSCRIPTION_ID_META_KEY.to_owned(),
                    subscription_id.clone(),
                );
        }
        self.sender
            .send(json_rpc_notification(method, Some(params)))
            .await
    }

    /// Send a logging notification.
    ///
    /// # Errors
    ///
    /// Returns an error when the peer notification channel is closed.
    pub async fn log(&self, level: &str, logger: Option<&str>, data: Value) -> Result<()> {
        let Some(requested) = self
            .meta_value(MCP_LOG_LEVEL_META_KEY)
            .and_then(Value::as_str)
        else {
            return Ok(());
        };
        let Ok(requested) = requested.parse::<crate::McpLogLevel>() else {
            return Err(Error::protocol("invalid request log level"));
        };
        let Ok(level_value) = level.parse::<crate::McpLogLevel>() else {
            return Err(Error::protocol("invalid emitted log level"));
        };
        if level_value < requested {
            return Ok(());
        }
        let mut params = serde_json::Map::from_iter([
            ("level".to_owned(), Value::String(level.to_owned())),
            ("data".to_owned(), data),
        ]);
        if let Some(logger) = logger {
            params.insert("logger".to_owned(), Value::String(logger.to_owned()));
        }
        self.notify("notifications/message", Some(Value::Object(params)))
            .await
    }

    /// Send a progress notification.
    ///
    /// # Errors
    ///
    /// Returns an error when the peer notification channel is closed.
    pub async fn progress(
        &self,
        token: Value,
        progress: f64,
        total: Option<f64>,
        message: Option<String>,
    ) -> Result<()> {
        let active_token = self
            .progress_token()
            .ok_or_else(|| Error::protocol("request did not declare a progress token"))?;
        if !crate::types::valid_progress_token(&token) || token != active_token {
            return Err(Error::protocol(
                "progress token does not match the active request",
            ));
        }
        if !progress.is_finite() || total.is_some_and(|total| !total.is_finite()) {
            return Err(Error::protocol("progress values must be finite"));
        }
        let mut last_progress = self.last_progress.lock().await;
        if last_progress.is_some_and(|last| progress <= last) {
            return Err(Error::protocol("progress must strictly increase"));
        }
        *last_progress = Some(progress);
        drop(last_progress);
        let mut params = serde_json::Map::from_iter([
            ("progressToken".to_owned(), token),
            ("progress".to_owned(), json!(progress)),
        ]);
        if let Some(total) = total {
            params.insert("total".to_owned(), json!(total));
        }
        if let Some(message) = message {
            params.insert("message".to_owned(), Value::String(message));
        }
        self.notify("notifications/progress", Some(Value::Object(params)))
            .await
    }

    /// Return the current request progress token, if present.
    #[must_use]
    pub fn progress_token(&self) -> Option<Value> {
        self.meta_value("progressToken").cloned()
    }
}

#[derive(Clone)]
enum NotificationSender {
    Stdio(SharedWriter),
    Channel(mpsc::Sender<Value>),
}

impl NotificationSender {
    async fn send(&self, message: Value) -> Result<()> {
        match self {
            Self::Stdio(stdout) => write_json_line(stdout, &message).await,
            Self::Channel(sender) => sender
                .send(message)
                .await
                .map_err(|_| Error::protocol("MCP HTTP notification receiver closed")),
        }
    }
}

/// MCP server handler.
pub trait ServerHandler: Send + Sync + 'static {
    /// Server metadata.
    fn server_info(&self) -> ServerInfo;
    /// Server capabilities.
    fn capabilities(&self) -> ServerCapabilities;
    /// Optional instructions sent to clients during discovery.
    fn instructions(&self) -> Option<String> {
        None
    }
    /// List tools.
    fn list_tools(
        &self,
        _cursor: Option<String>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpToolList>> {
        Box::pin(async { Err(ServerError::method_not_found("tools/list")) })
    }
    /// Call a tool.
    fn call_tool(
        &self,
        _call: McpToolCall,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpToolResult>> {
        Box::pin(async { Err(ServerError::method_not_found("tools/call")) })
    }
    /// List prompts.
    fn list_prompts(
        &self,
        _cursor: Option<String>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpPromptList>> {
        Box::pin(async { Err(ServerError::method_not_found("prompts/list")) })
    }
    /// Get a prompt.
    fn get_prompt(
        &self,
        _name: String,
        _arguments: Option<Value>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpPromptResult>> {
        Box::pin(async { Err(ServerError::method_not_found("prompts/get")) })
    }
    /// List resources.
    fn list_resources(
        &self,
        _cursor: Option<String>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpResourceList>> {
        Box::pin(async { Err(ServerError::method_not_found("resources/list")) })
    }
    /// List resource templates.
    fn list_resource_templates(
        &self,
        _cursor: Option<String>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpResourceTemplateList>> {
        Box::pin(async { Err(ServerError::method_not_found("resources/templates/list")) })
    }
    /// Read a resource.
    fn read_resource(
        &self,
        _uri: String,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpResourceResult>> {
        Box::pin(async { Err(ServerError::method_not_found("resources/read")) })
    }
    /// Complete a resource template argument.
    fn complete(
        &self,
        _request: McpCompletionRequest,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<crate::McpCompletionResult>> {
        Box::pin(async { Err(ServerError::method_not_found("completion/complete")) })
    }
    /// Produce filtered subscription notifications until the listener closes.
    fn listen(&self, _filter: Value, context: ServerContext) -> BoxFuture<ServerResult<()>> {
        Box::pin(async move {
            context.cancelled().await;
            Ok(())
        })
    }
    /// Handle a notification.
    fn handle_notification(
        &self,
        _notification: JsonRpcNotification,
        _context: ServerContext,
    ) -> BoxFuture<()> {
        Box::pin(async {})
    }
}

/// Serve MCP over stdin/stdout newline-delimited JSON-RPC.
///
/// # Errors
///
/// Returns an error when stdin cannot be read or a response cannot be written.
pub async fn serve_stdio(handler: Arc<dyn ServerHandler>) -> Result<()> {
    serve_stream(handler, tokio::io::stdin(), tokio::io::stdout()).await
}

/// Serve MCP over a newline-delimited asynchronous byte stream.
///
/// This is the transport-neutral stream loop used by [`serve_stdio`].
///
/// # Errors
///
/// Returns an error when the input or output stream fails.
#[allow(clippy::too_many_lines)]
pub async fn serve_stream<R, W>(handler: Arc<dyn ServerHandler>, reader: R, writer: W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let stdout: SharedWriter = Arc::new(Mutex::new(Box::new(writer)));
    let context = ServerContext {
        sender: NotificationSender::Stdio(Arc::clone(&stdout)),
        request_id: None,
        request_meta: None,
        protocol_version: None,
        client_capabilities: None,
        subscription_id: None,
        cancellation: CancellationToken::new(),
        input_responses: None,
        request_state: None,
        last_progress: Arc::new(Mutex::new(None)),
    };
    let active = Arc::new(Mutex::new(BTreeMap::<JsonRpcId, CancellationToken>::new()));
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        let value = match serde_json::from_str::<Value>(&line) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, mcp.transport = "stdio", "MCP stdio JSON parse failed");
                write_json_line(
                    &stdout,
                    &json_rpc_error(JsonRpcId::Null, -32700, "parse error"),
                )
                .await?;
                continue;
            }
        };
        let inbound = match parse_json_rpc_message(value) {
            Ok(inbound) => inbound,
            Err(error) => {
                tracing::warn!(%error, mcp.transport = "stdio", "MCP stdio JSON-RPC validation failed");
                write_json_line(
                    &stdout,
                    &json_rpc_error(JsonRpcId::Null, -32600, "invalid request"),
                )
                .await?;
                continue;
            }
        };
        match inbound {
            JsonRpcInbound::Request(request) => {
                let meta = match validate_request_meta(request.params.as_ref()) {
                    Ok(meta) => meta,
                    Err(error) => {
                        write_json_line(&stdout, &meta_error_response(request.id, error)).await?;
                        continue;
                    }
                };
                let handler = Arc::clone(&handler);
                let stdout = Arc::clone(&stdout);
                let active = Arc::clone(&active);
                let mut request_context = context.clone();
                request_context.request_id = Some(request.id.clone());
                request_context.request_meta = Some(meta.raw);
                request_context.protocol_version = Some(meta.protocol_version);
                request_context.client_capabilities = Some(meta.client_capabilities);
                let cancellation = CancellationToken::new();
                request_context.cancellation = cancellation.clone();
                active
                    .lock()
                    .await
                    .insert(request.id.clone(), cancellation.clone());
                let span = server_request_span(
                    &request.method,
                    Some(&request.id.to_string()),
                    "stdio",
                    None,
                );
                set_span_parent_from_meta(&span, request_context.request_meta.as_ref());
                tokio::spawn(
                    async move {
                        let request_id = request.id.clone();
                        tokio::select! {
                            response = handle_request(handler, request, request_context) => {
                                record_response_outcome(&response);
                                if let Err(error) = write_json_line(&stdout, &response).await {
                                    tracing::debug!(%error, "failed to write MCP stdio response");
                                }
                            }
                            () = cancellation.cancelled() => {}
                        }
                        active.lock().await.remove(&request_id);
                    }
                    .instrument(span),
                );
            }
            JsonRpcInbound::Notification(notification) => {
                if notification.method == "notifications/cancelled" {
                    if let Some(id) = notification
                        .params
                        .as_ref()
                        .and_then(|params| params.get("requestId"))
                        .and_then(|id| serde_json::from_value::<JsonRpcId>(id.clone()).ok())
                        && let Some(cancellation) = active.lock().await.get(&id)
                    {
                        cancellation.cancel();
                    }
                    continue;
                }
                let Ok(meta) = validate_request_meta(notification.params.as_ref()) else {
                    tracing::warn!(mcp.method = %notification.method, "ignoring MCP notification with invalid request metadata");
                    continue;
                };
                let handler = Arc::clone(&handler);
                let request_meta = Some(meta.raw);
                let span = server_request_span(&notification.method, None, "stdio", None);
                set_span_parent_from_meta(&span, request_meta.as_ref());
                let mut notification_context = context.clone();
                notification_context.request_meta = request_meta;
                notification_context.protocol_version = Some(meta.protocol_version);
                notification_context.client_capabilities = Some(meta.client_capabilities);
                async move {
                    handler
                        .handle_notification(notification, notification_context)
                        .await;
                    tracing::Span::current().record("mcp.outcome", "success");
                }
                .instrument(span)
                .await;
            }
            JsonRpcInbound::Response(_) => {}
        }
    }
    Ok(())
}

/// Create an Axum router serving Streamable HTTP MCP at `/mcp`.
pub fn streamable_http_router(handler: Arc<dyn ServerHandler>) -> Router {
    streamable_http_router_with_options(handler, StreamableHttpOptions::default())
}

type TokenValidator = Arc<
    dyn Fn(McpAccessToken, McpAuthorizationContext) -> BoxFuture<McpTokenValidation> + Send + Sync,
>;

type HostedTokenAuthorizer = Arc<
    dyn Fn(McpAccessToken, McpAuthorizationContext) -> BoxFuture<McpHostedTokenValidation>
        + Send
        + Sync,
>;

#[derive(Clone)]
enum HttpTokenAuthorizer {
    External(TokenValidator),
    Hosted(HostedTokenAuthorizer),
}

/// Hosted access-token authorization with an optional active revocation lease.
#[derive(Clone)]
pub struct McpTokenAuthorization {
    /// Stable session owner.
    pub principal_id: McpPrincipalId,
    /// Credential expiry.
    pub expires_at: Option<SystemTime>,
    /// Active revocation and authoritative revalidation lease.
    pub revocation: Option<McpRevocationLease>,
}

impl std::fmt::Debug for McpTokenAuthorization {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpTokenAuthorization")
            .field("principal_id", &self.principal_id)
            .field("expires_at", &self.expires_at)
            .field("revocation", &self.revocation.is_some())
            .finish()
    }
}

/// Hosted token authorization result.
#[derive(Clone, Debug)]
pub enum McpHostedTokenValidation {
    /// Token is currently authorized.
    Authorized(McpTokenAuthorization),
    /// Token is invalid or revoked.
    Unauthorized {
        /// Optional safe bearer diagnostic.
        error_description: Option<String>,
    },
    /// Required scopes are absent.
    InsufficientScope {
        /// Scopes required by this resource operation.
        required_scopes: Vec<String>,
        /// Optional safe bearer diagnostic.
        error_description: Option<String>,
    },
    /// Authorization state could not be checked.
    Unavailable,
}

type LeaseValidator = Arc<dyn Fn() -> BoxFuture<std::result::Result<bool, ()>> + Send + Sync>;

/// Cloneable stream lease invalidated by local events and bounded authoritative checks.
#[derive(Clone)]
pub struct McpRevocationLease {
    validator: LeaseValidator,
    poll_interval: Duration,
    notifications: tokio::sync::watch::Receiver<u64>,
}

impl std::fmt::Debug for McpRevocationLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpRevocationLease")
            .field("poll_interval", &self.poll_interval)
            .finish_non_exhaustive()
    }
}

impl McpRevocationLease {
    /// Create a lease from an authoritative validator and process-local event receiver.
    #[cfg(feature = "oauth-authorization-server")]
    pub fn new(
        validator: impl Fn() -> BoxFuture<std::result::Result<bool, ()>> + Send + Sync + 'static,
        poll_interval: Duration,
        notifications: tokio::sync::watch::Receiver<u64>,
    ) -> Self {
        Self {
            validator: Arc::new(validator),
            poll_interval,
            notifications,
        }
    }

    async fn validate(&self) -> bool {
        matches!((self.validator)().await, Ok(true))
    }
}

/// Streamable HTTP authorization context passed to token validators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpAuthorizationContext {
    /// Protected resource identifier expected by this MCP server.
    pub resource: String,
    /// Scopes configured as required for the request.
    pub required_scopes: Vec<String>,
}

/// Streamable HTTP OAuth protected resource configuration.
#[derive(Clone)]
pub struct StreamableHttpAuthorization {
    metadata: McpProtectedResourceMetadata,
    required_scopes: Vec<String>,
    realm: Option<String>,
    validator: HttpTokenAuthorizer,
}

impl std::fmt::Debug for StreamableHttpAuthorization {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StreamableHttpAuthorization")
            .field("metadata", &self.metadata)
            .field("required_scopes", &self.required_scopes)
            .field("realm", &self.realm)
            .field("validator", &true)
            .finish()
    }
}

impl StreamableHttpAuthorization {
    /// Create OAuth authorization config with protected resource metadata and a validator.
    ///
    /// # Errors
    ///
    /// Returns an error when the protected-resource metadata is invalid.
    pub fn new(
        metadata: McpProtectedResourceMetadata,
        validator: impl Fn(McpAccessToken, McpAuthorizationContext) -> BoxFuture<McpTokenValidation>
        + Send
        + Sync
        + 'static,
    ) -> Result<Self> {
        metadata.validate_for_mcp_resource(&metadata.resource)?;
        Ok(Self {
            metadata,
            required_scopes: Vec::new(),
            realm: None,
            validator: HttpTokenAuthorizer::External(Arc::new(validator)),
        })
    }

    /// Create authorization backed by hosted token validation and active revocation leases.
    ///
    /// # Errors
    ///
    /// Returns an error when the protected-resource metadata is invalid.
    pub fn hosted(
        metadata: McpProtectedResourceMetadata,
        authorizer: impl Fn(
            McpAccessToken,
            McpAuthorizationContext,
        ) -> BoxFuture<McpHostedTokenValidation>
        + Send
        + Sync
        + 'static,
    ) -> Result<Self> {
        metadata.validate_for_mcp_resource(&metadata.resource)?;
        Ok(Self {
            metadata,
            required_scopes: Vec::new(),
            realm: None,
            validator: HttpTokenAuthorizer::Hosted(Arc::new(authorizer)),
        })
    }

    /// Set scopes advertised in authorization challenges and passed to the validator.
    #[must_use]
    pub fn with_required_scopes(
        mut self,
        scopes: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.required_scopes = scopes.into_iter().map(Into::into).collect();
        self
    }

    /// Set the optional bearer realm.
    #[must_use]
    pub fn with_realm(mut self, realm: impl Into<String>) -> Self {
        self.realm = Some(realm.into());
        self
    }
}

/// Streamable HTTP server options.
#[derive(Clone)]
pub struct StreamableHttpOptions {
    allowed_origins: Arc<HashSet<CanonicalOrigin>>,
    authorization: Option<StreamableHttpAuthorization>,
    serve_root_protected_resource_metadata: bool,
}

impl Default for StreamableHttpOptions {
    fn default() -> Self {
        Self {
            allowed_origins: Arc::new(HashSet::new()),
            authorization: None,
            serve_root_protected_resource_metadata: true,
        }
    }
}

impl std::fmt::Debug for StreamableHttpOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StreamableHttpOptions")
            .field("allowed_origins", &self.allowed_origins)
            .field("authorization", &self.authorization)
            .field(
                "serve_root_protected_resource_metadata",
                &self.serve_root_protected_resource_metadata,
            )
            .finish()
    }
}

impl StreamableHttpOptions {
    /// Allow canonical HTTP Origins. Present Origins are denied by default.
    ///
    /// # Errors
    ///
    /// Returns an error when an origin is not a canonical HTTP or HTTPS origin.
    pub fn try_with_allowed_origins(
        mut self,
        origins: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<Self> {
        let allowed_origins = origins
            .into_iter()
            .map(|origin| canonical_origin(origin.as_ref()))
            .collect::<Result<HashSet<_>>>()?;
        self.allowed_origins = Arc::new(allowed_origins);
        Ok(self)
    }

    /// Set OAuth protected resource authorization for Streamable HTTP requests.
    #[must_use]
    pub fn with_authorization(mut self, authorization: StreamableHttpAuthorization) -> Self {
        self.authorization = Some(authorization);
        self
    }

    /// Relinquish the root protected-resource metadata route to an integrating host.
    #[must_use]
    pub const fn without_root_protected_resource_metadata(mut self) -> Self {
        self.serve_root_protected_resource_metadata = false;
        self
    }
}

/// Create an Axum router serving Streamable HTTP MCP at `/mcp` with options.
pub fn streamable_http_router_with_options(
    handler: Arc<dyn ServerHandler>,
    options: StreamableHttpOptions,
) -> Router {
    let serve_root_metadata = options.serve_root_protected_resource_metadata;
    let router = Router::new().route("/mcp", post(http_post)).route(
        "/.well-known/oauth-protected-resource/{*resource_path}",
        get(http_protected_resource_metadata),
    );
    let router = if serve_root_metadata {
        router.route(
            "/.well-known/oauth-protected-resource",
            get(http_protected_resource_metadata),
        )
    } else {
        router
    };
    router.with_state(HttpState {
        handler,
        allowed_origins: options.allowed_origins,
        authorization: options.authorization,
    })
}

/// Serve Streamable HTTP MCP at `/mcp` on the supplied listener.
///
/// # Errors
///
/// Returns an error when the listener address is unavailable or serving fails.
pub async fn serve_streamable_http(
    listener: TcpListener,
    handler: Arc<dyn ServerHandler>,
) -> Result<SocketAddr> {
    let address = listener.local_addr()?;
    axum::serve(listener, streamable_http_router(handler))
        .await
        .map_err(Error::Io)?;
    Ok(address)
}

#[derive(Clone)]
struct HttpState {
    handler: Arc<dyn ServerHandler>,
    allowed_origins: Arc<HashSet<CanonicalOrigin>>,
    authorization: Option<StreamableHttpAuthorization>,
}

// Keeping validation and dispatch together makes the stateless boundary fail-closed.
#[allow(clippy::too_many_lines)]
async fn http_post(State(state): State<HttpState>, request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    let headers = parts.headers;
    if !origin_allowed(&state, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !accepts_streamable_http_post(&headers) || !has_json_content_type(&headers) {
        return (
            StatusCode::NOT_ACCEPTABLE,
            "MCP Streamable HTTP POST requires JSON and SSE content negotiation",
        )
            .into_response();
    }
    let authorization = match authorize_http_request(&state, &headers).await {
        Ok(authorization) => authorization,
        Err(response) => return response,
    };
    let Ok(body) = to_bytes(body, 1024 * 1024).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let inbound = match serde_json::from_slice::<Value>(&body)
        .map_err(Error::from)
        .and_then(parse_json_rpc_message)
    {
        Ok(inbound) => inbound,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let (method, params, id) = match &inbound {
        JsonRpcInbound::Request(request) => (
            request.method.as_str(),
            request.params.as_ref(),
            Some(request.id.clone()),
        ),
        JsonRpcInbound::Notification(notification) => (
            notification.method.as_str(),
            notification.params.as_ref(),
            None,
        ),
        JsonRpcInbound::Response(response) => {
            return json_rpc_http_error(
                StatusCode::BAD_REQUEST,
                response.id.clone(),
                -32600,
                "MCP clients must not send JSON-RPC responses",
                None,
            );
        }
    };
    let fallback_id = id.clone().unwrap_or(JsonRpcId::Null);
    let meta = match validate_request_meta(params) {
        Ok(meta) => meta,
        Err(MetaError::UnsupportedVersion(requested)) => {
            return json_rpc_http_error(
                StatusCode::BAD_REQUEST,
                fallback_id,
                UNSUPPORTED_PROTOCOL_VERSION,
                "unsupported MCP protocol version",
                Some(json!({
                    "supported": [MCP_PROTOCOL_VERSION],
                    "requested": requested,
                })),
            );
        }
        Err(MetaError::Invalid(message)) => {
            return json_rpc_http_error(
                StatusCode::BAD_REQUEST,
                fallback_id,
                -32602,
                message,
                None,
            );
        }
    };
    if let Err((header_name, header_value, body_value)) =
        validate_mirrored_headers(&headers, method, params, &meta.protocol_version)
    {
        return json_rpc_http_error(
            StatusCode::BAD_REQUEST,
            fallback_id,
            HEADER_MISMATCH,
            "HTTP request metadata does not match the MCP body",
            Some(json!({
                "header": header_name,
                "headerValue": header_value,
                "bodyValue": body_value,
            })),
        );
    }
    if id.is_some() && !request_method_available(state.handler.as_ref(), method) {
        return json_rpc_http_error(
            StatusCode::NOT_FOUND,
            fallback_id,
            -32601,
            format!("method not found: {method}"),
            None,
        );
    }

    let span = http_server_request_span(&inbound, &meta.protocol_version);
    set_span_parent_from_meta(&span, inbound_meta(&inbound));
    match inbound {
        JsonRpcInbound::Notification(notification) => {
            let context = ServerContext {
                sender: NotificationSender::Channel(mpsc::channel(1).0),
                request_id: None,
                request_meta: Some(meta.raw),
                protocol_version: Some(meta.protocol_version),
                client_capabilities: Some(meta.client_capabilities),
                subscription_id: None,
                cancellation: CancellationToken::new(),
                input_responses: None,
                request_state: None,
                last_progress: Arc::new(Mutex::new(None)),
            };
            tokio::spawn(
                async move {
                    state
                        .handler
                        .handle_notification(notification, context)
                        .await;
                }
                .instrument(span),
            );
            StatusCode::ACCEPTED.into_response()
        }
        JsonRpcInbound::Request(request) => {
            let (sender, receiver) = mpsc::channel(32);
            let cancellation = CancellationToken::new();
            let context = ServerContext {
                sender: NotificationSender::Channel(sender.clone()),
                request_id: Some(request.id.clone()),
                request_meta: Some(meta.raw),
                protocol_version: Some(meta.protocol_version),
                client_capabilities: Some(meta.client_capabilities),
                subscription_id: None,
                cancellation: cancellation.clone(),
                input_responses: None,
                request_state: None,
                last_progress: Arc::new(Mutex::new(None)),
            };
            if request.method == "tools/call"
                && let Err((header_name, header_value, body_value)) =
                    validate_tool_parameter_headers(
                        state.handler.as_ref(),
                        &headers,
                        request.params.as_ref(),
                        context.clone(),
                    )
                    .await
            {
                return json_rpc_http_error(
                    StatusCode::BAD_REQUEST,
                    request.id,
                    HEADER_MISMATCH,
                    "HTTP tool parameter header does not match the MCP body",
                    Some(json!({
                        "header": header_name,
                        "headerValue": header_value,
                        "bodyValue": body_value,
                    })),
                );
            }
            let handler_cancellation = cancellation.clone();
            tokio::spawn(
                async move {
                    tokio::select! {
                        response = handle_request(Arc::clone(&state.handler), request, context) => {
                            record_response_outcome(&response);
                            drop(sender.send(response).await);
                        }
                        () = authorized_request_ended(authorization) => {}
                        () = handler_cancellation.cancelled() => {}
                    }
                }
                .instrument(span),
            );
            let stream = CancellableResponseStream::new(receiver, cancellation);
            let mut response = Body::from_stream(stream).into_response();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            );
            response
                .headers_mut()
                .insert("x-accel-buffering", HeaderValue::from_static("no"));
            response
        }
        JsonRpcInbound::Response(_) => unreachable!(),
    }
}

#[derive(Debug)]
struct ValidatedRequestMeta {
    raw: serde_json::Map<String, Value>,
    protocol_version: String,
    client_capabilities: Value,
}

enum MetaError {
    Invalid(String),
    UnsupportedVersion(String),
}

fn meta_error_response(id: JsonRpcId, error: MetaError) -> Value {
    match error {
        MetaError::UnsupportedVersion(requested) => json_rpc_error_with_data(
            id,
            UNSUPPORTED_PROTOCOL_VERSION,
            "unsupported MCP protocol version",
            Some(json!({
                "supported": [MCP_PROTOCOL_VERSION],
                "requested": requested,
            })),
        ),
        MetaError::Invalid(message) => json_rpc_error(id, -32602, message),
    }
}

struct CancellableResponseStream {
    inner: ReceiverStream<Value>,
    cancellation: CancellationToken,
}

impl CancellableResponseStream {
    fn new(receiver: mpsc::Receiver<Value>, cancellation: CancellationToken) -> Self {
        Self {
            inner: ReceiverStream::new(receiver),
            cancellation,
        }
    }
}

impl futures_util::Stream for CancellableResponseStream {
    type Item = std::result::Result<axum::body::Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(context).map(|message| {
            message.map(|message| {
                let data = serde_json::to_string(&message).expect("MCP response serializes");
                Ok(axum::body::Bytes::from(format!("data: {data}\n\n")))
            })
        })
    }
}

impl Drop for CancellableResponseStream {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

fn validate_request_meta(
    params: Option<&Value>,
) -> std::result::Result<ValidatedRequestMeta, MetaError> {
    let raw = params
        .and_then(|params| params.get("_meta"))
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| MetaError::Invalid("request params._meta must be an object".to_owned()))?;
    let protocol_version = raw
        .get(MCP_PROTOCOL_VERSION_META_KEY)
        .and_then(Value::as_str)
        .ok_or_else(|| MetaError::Invalid("request metadata omitted protocolVersion".to_owned()))?
        .to_owned();
    if protocol_version != MCP_PROTOCOL_VERSION {
        return Err(MetaError::UnsupportedVersion(protocol_version));
    }
    let client_capabilities = raw
        .get(MCP_CLIENT_CAPABILITIES_META_KEY)
        .filter(|value| value.is_object())
        .cloned()
        .ok_or_else(|| {
            MetaError::Invalid("request metadata omitted clientCapabilities".to_owned())
        })?;
    if let Some(level) = raw.get(MCP_LOG_LEVEL_META_KEY)
        && level
            .as_str()
            .and_then(|level| level.parse::<crate::McpLogLevel>().ok())
            .is_none()
    {
        return Err(MetaError::Invalid("invalid request log level".to_owned()));
    }
    if raw
        .get("progressToken")
        .is_some_and(|token| !crate::types::valid_progress_token(token))
    {
        return Err(MetaError::Invalid(
            "progress token must be a string or integer".to_owned(),
        ));
    }
    Ok(ValidatedRequestMeta {
        raw,
        protocol_version,
        client_capabilities,
    })
}

fn validate_mirrored_headers(
    headers: &HeaderMap,
    method: &str,
    params: Option<&Value>,
    protocol_version: &str,
) -> std::result::Result<(), (String, Option<String>, String)> {
    mirrored_header(headers, "mcp-protocol-version", protocol_version)?;
    mirrored_header(headers, "mcp-method", method)?;
    if let Some(name) = mirrored_request_name(method, params) {
        mirrored_encoded_header(headers, "mcp-name", name)?;
    }
    Ok(())
}

fn mirrored_encoded_header(
    headers: &HeaderMap,
    name: &'static str,
    expected: &str,
) -> std::result::Result<(), (String, Option<String>, String)> {
    let actual =
        single_header(headers, name).map_err(|_| (name.to_owned(), None, expected.to_owned()))?;
    let decoded = actual
        .as_deref()
        .and_then(|actual| decode_header_value(actual).ok());
    if decoded.as_deref() == Some(expected) {
        Ok(())
    } else {
        Err((name.to_owned(), actual, expected.to_owned()))
    }
}

fn mirrored_header(
    headers: &HeaderMap,
    name: &'static str,
    expected: &str,
) -> std::result::Result<(), (String, Option<String>, String)> {
    let actual =
        single_header(headers, name).map_err(|_| (name.to_owned(), None, expected.to_owned()))?;
    if actual.as_deref() == Some(expected) {
        Ok(())
    } else {
        Err((name.to_owned(), actual, expected.to_owned()))
    }
}

fn mirrored_request_name<'a>(method: &str, params: Option<&'a Value>) -> Option<&'a str> {
    match method {
        "tools/call" | "prompts/get" => params?.get("name")?.as_str(),
        "resources/read" => params?.get("uri")?.as_str(),
        _ => None,
    }
}

async fn validate_tool_parameter_headers(
    handler: &dyn ServerHandler,
    headers: &HeaderMap,
    params: Option<&Value>,
    context: ServerContext,
) -> std::result::Result<(), (String, Option<String>, String)> {
    let Some(params) = params else {
        return Ok(());
    };
    let Some(tool_name) = params.get("name").and_then(Value::as_str) else {
        return Ok(());
    };
    let mut cursor: Option<String> = None;
    for _ in 0..32 {
        let page = handler
            .list_tools(cursor.clone(), context.clone())
            .await
            .map_err(|_| {
                (
                    "Mcp-Param-*".to_owned(),
                    None,
                    "resolvable tool schema".to_owned(),
                )
            })?;
        if let Some(tool) = page.tools.iter().find(|tool| tool.name == tool_name) {
            let descriptors = header_descriptors(&tool.input_schema).map_err(|_| {
                (
                    "Mcp-Param-*".to_owned(),
                    None,
                    "valid x-mcp-header schema".to_owned(),
                )
            })?;
            let expected = expected_parameter_values(&descriptors, params.get("arguments"))
                .map_err(|_| {
                    (
                        "Mcp-Param-*".to_owned(),
                        None,
                        "typed tool argument".to_owned(),
                    )
                })?;
            let expected = expected.into_iter().collect::<BTreeMap<_, _>>();
            for descriptor in descriptors {
                let name = format!("mcp-param-{}", descriptor.name);
                let body_value = expected.get(&name);
                let header_value = single_header(headers, &name).map_err(|_| {
                    (
                        name.clone(),
                        None,
                        body_value.cloned().unwrap_or_else(|| "absent".to_owned()),
                    )
                })?;
                match body_value {
                    Some(body_value) => {
                        let decoded = header_value
                            .as_deref()
                            .and_then(|value| decode_header_value(value).ok());
                        if decoded.as_deref() != Some(body_value.as_str()) {
                            return Err((name, header_value, body_value.clone()));
                        }
                    }
                    None if header_value.is_some() => {
                        return Err((name, header_value, "absent".to_owned()));
                    }
                    None => {}
                }
            }
            return Ok(());
        }
        let Some(next) = page.next_cursor else {
            return Err((
                "Mcp-Param-*".to_owned(),
                None,
                "known tool schema".to_owned(),
            ));
        };
        cursor = Some(next);
    }
    Err((
        "Mcp-Param-*".to_owned(),
        None,
        "tool schema within pagination limit".to_owned(),
    ))
}

fn request_method_available(handler: &dyn ServerHandler, method: &str) -> bool {
    let capabilities = handler.capabilities();
    match method {
        "server/discover" | "subscriptions/listen" => true,
        "tools/list" | "tools/call" => capabilities.tools.is_some(),
        "prompts/list" | "prompts/get" => capabilities.prompts.is_some(),
        "resources/list" | "resources/templates/list" | "resources/read" => {
            capabilities.resources.is_some()
        }
        "completion/complete" => capabilities.completions.is_some(),
        _ => false,
    }
}

fn json_rpc_http_error(
    status: StatusCode,
    id: JsonRpcId,
    code: i64,
    message: impl Into<String>,
    data: Option<Value>,
) -> Response {
    (
        status,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        axum::Json(json_rpc_error_with_data(id, code, message, data)),
    )
        .into_response()
}

async fn authorized_request_ended(mut authorization: AuthorizedHttpRequest) {
    let expiry = async {
        match authorization.expires_at {
            Some(expires_at) => {
                let duration = expires_at
                    .duration_since(SystemTime::now())
                    .unwrap_or_default();
                tokio::time::sleep(duration).await;
            }
            None => std::future::pending().await,
        }
    };
    let revocation = async {
        let Some(mut lease) = authorization.revocation.take() else {
            return std::future::pending().await;
        };
        loop {
            tokio::select! {
                () = tokio::time::sleep(lease.poll_interval) => {}
                changed = lease.notifications.changed() => if changed.is_err() { return },
            }
            if !lease.validate().await {
                return;
            }
        }
    };
    tokio::select! { () = expiry => {}, () = revocation => {} }
}

async fn http_protected_resource_metadata(State(state): State<HttpState>) -> Response {
    let Some(authorization) = &state.authorization else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    (
        StatusCode::OK,
        headers,
        axum::Json(authorization.metadata.clone()),
    )
        .into_response()
}

async fn authorize_http_request(
    state: &HttpState,
    headers: &HeaderMap,
) -> std::result::Result<AuthorizedHttpRequest, Response> {
    let Some(authorization) = &state.authorization else {
        return Ok(AuthorizedHttpRequest {
            expires_at: None,
            revocation: None,
        });
    };
    let Some(token) = bearer_token(headers) else {
        return Err(authorization_challenge_response(
            authorization,
            StatusCode::UNAUTHORIZED,
            None,
            None,
        ));
    };
    let context = McpAuthorizationContext {
        resource: authorization.metadata.resource.clone(),
        required_scopes: authorization.required_scopes.clone(),
    };
    let validation = match &authorization.validator {
        HttpTokenAuthorizer::External(validator) => match validator(token, context).await {
            McpTokenValidation::Authorized {
                principal_id,
                expires_at,
            } => McpHostedTokenValidation::Authorized(McpTokenAuthorization {
                principal_id,
                expires_at,
                revocation: None,
            }),
            McpTokenValidation::Unavailable => McpHostedTokenValidation::Unavailable,
            McpTokenValidation::Unauthorized { error_description } => {
                McpHostedTokenValidation::Unauthorized { error_description }
            }
            McpTokenValidation::InsufficientScope {
                required_scopes,
                error_description,
            } => McpHostedTokenValidation::InsufficientScope {
                required_scopes,
                error_description,
            },
        },
        HttpTokenAuthorizer::Hosted(authorizer) => authorizer(token, context).await,
    };
    match validation {
        McpHostedTokenValidation::Authorized(McpTokenAuthorization {
            principal_id: _,
            expires_at,
            revocation,
        }) => Ok(AuthorizedHttpRequest {
            expires_at,
            revocation,
        }),
        McpHostedTokenValidation::Unavailable => {
            Err(StatusCode::SERVICE_UNAVAILABLE.into_response())
        }
        McpHostedTokenValidation::Unauthorized { error_description } => {
            Err(authorization_challenge_response(
                authorization,
                StatusCode::UNAUTHORIZED,
                Some("invalid_token"),
                error_description.as_deref(),
            ))
        }
        McpHostedTokenValidation::InsufficientScope {
            required_scopes,
            error_description,
        } => Err(insufficient_scope_response(
            authorization,
            &required_scopes,
            error_description.as_deref(),
        )),
    }
}

struct AuthorizedHttpRequest {
    expires_at: Option<SystemTime>,
    revocation: Option<McpRevocationLease>,
}

fn bearer_token(headers: &HeaderMap) -> Option<McpAccessToken> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?.to_str().ok()?.trim();
    if values.next().is_some() {
        return None;
    }
    let split = value.find(char::is_whitespace)?;
    let (scheme, token) = value.split_at(split);
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() || token.contains(char::is_whitespace) || token.contains(',') {
        return None;
    }
    Some(McpAccessToken(token.to_owned()))
}

fn authorization_challenge_response(
    authorization: &StreamableHttpAuthorization,
    status: StatusCode,
    error: Option<&str>,
    error_description: Option<&str>,
) -> Response {
    let mut challenge = base_challenge(authorization);
    challenge.error = error.map(ToOwned::to_owned);
    challenge.error_description = error_description.map(ToOwned::to_owned);
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(&challenge.to_header_value()) {
        headers.insert(header::WWW_AUTHENTICATE, value);
    }
    (status, headers).into_response()
}

fn insufficient_scope_response(
    authorization: &StreamableHttpAuthorization,
    required_scopes: &[String],
    error_description: Option<&str>,
) -> Response {
    let mut challenge = base_challenge(authorization);
    challenge.error = Some("insufficient_scope".to_owned());
    challenge.scope =
        scope_value(required_scopes).or_else(|| scope_value(&authorization.required_scopes));
    challenge.error_description = error_description.map(ToOwned::to_owned);
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(&challenge.to_header_value()) {
        headers.insert(header::WWW_AUTHENTICATE, value);
    }
    (StatusCode::FORBIDDEN, headers).into_response()
}

fn base_challenge(authorization: &StreamableHttpAuthorization) -> McpBearerChallenge {
    let mut challenge = McpBearerChallenge::new();
    challenge.realm.clone_from(&authorization.realm);
    challenge.resource_metadata =
        protected_resource_metadata_urls(&authorization.metadata.resource)
            .into_iter()
            .next();
    challenge.scope = scope_value(&authorization.required_scopes);
    challenge
}

fn scope_value(scopes: &[String]) -> Option<String> {
    if scopes.is_empty() {
        None
    } else {
        Some(scopes.join(" "))
    }
}

fn origin_allowed(state: &HttpState, headers: &HeaderMap) -> bool {
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let Some(origin) = origins.next() else {
        return true;
    };
    if origins.next().is_some() {
        return false;
    }
    origin
        .to_str()
        .ok()
        .and_then(|origin| canonical_origin(origin).ok())
        .is_some_and(|origin| state.allowed_origins.contains(&origin))
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CanonicalOrigin {
    scheme: String,
    host: String,
    port: u16,
}

fn canonical_origin(value: &str) -> Result<CanonicalOrigin> {
    if value == "null" || value.trim() != value {
        return Err(Error::protocol("invalid MCP allowed Origin"));
    }
    let (_, authority) = value
        .split_once("://")
        .ok_or_else(|| Error::protocol("invalid MCP allowed Origin"))?;
    if authority.is_empty()
        || authority
            .bytes()
            .any(|byte| matches!(byte, b'/' | b'?' | b'#'))
    {
        return Err(Error::protocol("invalid MCP allowed Origin"));
    }
    let url = Url::parse(value).map_err(|_| Error::protocol("invalid MCP allowed Origin"))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::protocol("invalid MCP allowed Origin"));
    }
    Ok(CanonicalOrigin {
        scheme: url.scheme().to_owned(),
        host: url
            .host_str()
            .ok_or_else(|| Error::protocol("invalid MCP allowed Origin"))?
            .to_ascii_lowercase(),
        port: url
            .port_or_known_default()
            .ok_or_else(|| Error::protocol("invalid MCP allowed Origin"))?,
    })
}

// Axum responses are returned directly to keep rejection paths allocation-free.
#[allow(clippy::result_large_err)]
fn single_header(headers: &HeaderMap, name: &str) -> std::result::Result<Option<String>, Response> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err((StatusCode::BAD_REQUEST, format!("duplicate {name} header")).into_response());
    }
    let value = value
        .to_str()
        .map_err(|_| (StatusCode::BAD_REQUEST, format!("invalid {name} header")).into_response())?;
    if value.is_empty() {
        return Err((StatusCode::BAD_REQUEST, format!("invalid {name} header")).into_response());
    }
    Ok(Some(value.to_owned()))
}

fn has_json_content_type(headers: &HeaderMap) -> bool {
    single_header(headers, "content-type")
        .ok()
        .flatten()
        .and_then(|value| value.split(';').next().map(str::trim).map(str::to_owned))
        .is_some_and(|value| value.eq_ignore_ascii_case("application/json"))
}

fn accepts_streamable_http_post(headers: &HeaderMap) -> bool {
    let Some(accept) = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    accept
        .split(',')
        .map(|value| value.trim().split(';').next().unwrap_or_default().trim())
        .any(|value| value == "application/json" || value == "*/*")
        && accept
            .split(',')
            .map(|value| value.trim().split(';').next().unwrap_or_default().trim())
            .any(|value| value == "text/event-stream" || value == "*/*")
}

// Keeping the complete method dispatch table together makes protocol coverage auditable.
#[allow(clippy::too_many_lines)]
async fn handle_request(
    handler: Arc<dyn ServerHandler>,
    request: JsonRpcRequest,
    mut context: ServerContext,
) -> Value {
    let request_method = request.method.clone();
    if matches!(
        request.method.as_str(),
        "tools/call" | "prompts/get" | "resources/read"
    ) {
        match parse_retry_fields(request.params.as_ref()) {
            Ok((input_responses, request_state)) => {
                context.input_responses = input_responses;
                context.request_state = request_state;
            }
            Err(message) => return json_rpc_error(request.id, -32602, message),
        }
    }
    let mut response = match request.method.as_str() {
        "server/discover" => json_rpc_success(request.id, discover_result(&handler)),
        "subscriptions/listen" => {
            let requested = request
                .params
                .as_ref()
                .and_then(|params| params.get("notifications"))
                .and_then(Value::as_object);
            let Some(requested) = requested else {
                return json_rpc_error(
                    request.id,
                    -32602,
                    "subscriptions/listen requires an object notifications filter",
                );
            };
            let capabilities = handler.capabilities();
            let mut supported = serde_json::Map::new();
            for (filter, capability) in [
                ("toolsListChanged", capabilities.tools.as_ref()),
                ("promptsListChanged", capabilities.prompts.as_ref()),
                ("resourcesListChanged", capabilities.resources.as_ref()),
            ] {
                let requested = requested.get(filter).and_then(Value::as_bool) == Some(true);
                let advertised = capability
                    .and_then(Value::as_object)
                    .and_then(|capability| capability.get("listChanged"))
                    .and_then(Value::as_bool)
                    == Some(true);
                if requested && advertised {
                    supported.insert(filter.to_owned(), Value::Bool(true));
                }
            }
            let filter = Value::Object(supported);
            let subscription_id = serde_json::to_value(&request.id).unwrap_or(Value::Null);
            let mut subscription_context = context;
            subscription_context.subscription_id = Some(subscription_id.clone());
            let acknowledged = subscription_context
                .notify(
                    "notifications/subscriptions/acknowledged",
                    Some(json!({ "notifications": filter })),
                )
                .await;
            match acknowledged {
                Ok(()) => server_result(
                    request.id,
                    handler
                        .listen(filter, subscription_context)
                        .await
                        .map(|()| {
                            json!({
                                "_meta": { (MCP_SUBSCRIPTION_ID_META_KEY): subscription_id }
                            })
                        }),
                ),
                Err(error) => json_rpc_error(request.id, -32603, error.to_string()),
            }
        }
        method if method.starts_with("notifications/") => {
            tracing::debug!(
                rpc.jsonrpc.error_code = -32600,
                mcp.outcome = "error",
                "MCP server rejected notification method sent as request"
            );
            json_rpc_error(
                request.id,
                -32600,
                format!("notification method sent as request: {method}"),
            )
        }
        "tools/list" => server_result(
            request.id,
            handler
                .list_tools(cursor_param(request.params.as_ref()), context)
                .await
                .map(|result| result.to_value()),
        ),
        "tools/call" => {
            let result = request
                .params
                .as_ref()
                .ok_or_else(|| "missing tool call params".to_owned())
                .and_then(McpToolCall::from_params)
                .map_err(ServerError::invalid_params);
            match result {
                Ok(call) => server_result(
                    request.id,
                    handler
                        .call_tool(call, context)
                        .await
                        .map(|result| result.raw),
                ),
                Err(error) => server_error_response(request.id, error),
            }
        }
        "prompts/list" => server_result(
            request.id,
            handler
                .list_prompts(cursor_param(request.params.as_ref()), context)
                .await
                .map(|result| result.to_value()),
        ),
        "prompts/get" => {
            let result = request
                .params
                .as_ref()
                .ok_or_else(|| "missing prompt params".to_owned())
                .and_then(|params| {
                    Ok((
                        require_string(params, "name")?,
                        params.get("arguments").cloned(),
                    ))
                })
                .map_err(ServerError::invalid_params);
            match result {
                Ok((name, arguments)) => server_result(
                    request.id,
                    handler
                        .get_prompt(name, arguments, context)
                        .await
                        .map(|result| result.raw),
                ),
                Err(error) => server_error_response(request.id, error),
            }
        }
        "resources/list" => server_result(
            request.id,
            handler
                .list_resources(cursor_param(request.params.as_ref()), context)
                .await
                .map(|result| result.to_value()),
        ),
        "resources/templates/list" => server_result(
            request.id,
            handler
                .list_resource_templates(cursor_param(request.params.as_ref()), context)
                .await
                .map(|result| result.to_value()),
        ),
        "resources/read" => {
            let result = request
                .params
                .as_ref()
                .ok_or_else(|| "missing resource params".to_owned())
                .and_then(|params| require_string(params, "uri"))
                .map_err(ServerError::invalid_params);
            match result {
                Ok(uri) => server_result(
                    request.id,
                    handler
                        .read_resource(uri, context)
                        .await
                        .map(|result| result.raw),
                ),
                Err(error) => server_error_response(request.id, error),
            }
        }
        "completion/complete" => {
            let result =
                completion_request(request.params.as_ref()).map_err(ServerError::invalid_params);
            match result {
                Ok(completion) => server_result(
                    request.id,
                    handler
                        .complete(completion, context)
                        .await
                        .and_then(|completion| {
                            if completion.values.len() > 100 {
                                return Err(ServerError::invalid_params(
                                    "completion values must not exceed 100 items",
                                ));
                            }
                            Ok(json!({
                                "completion": {
                                    "values": completion.values,
                                    "total": completion.total,
                                    "hasMore": completion.has_more,
                                }
                            }))
                        }),
                ),
                Err(error) => server_error_response(request.id, error),
            }
        }
        _ => {
            tracing::debug!(
                rpc.jsonrpc.error_code = -32601,
                mcp.outcome = "error",
                "MCP server method not found"
            );
            json_rpc_method_not_found(request.id, &request.method)
        }
    };
    decorate_success_response(&mut response, &handler, &request_method);
    response
}

fn parse_retry_fields(
    params: Option<&Value>,
) -> std::result::Result<(Option<Value>, Option<String>), String> {
    let Some(params) = params else {
        return Ok((None, None));
    };
    let input_responses = match params.get("inputResponses") {
        Some(value) if value.is_object() => Some(value.clone()),
        Some(_) => return Err("inputResponses must be an object".to_owned()),
        None => None,
    };
    let request_state = match params.get("requestState") {
        Some(value) => Some(
            value
                .as_str()
                .ok_or_else(|| "requestState must be a string".to_owned())?
                .to_owned(),
        ),
        None => None,
    };
    Ok((input_responses, request_state))
}

fn discover_result(handler: &Arc<dyn ServerHandler>) -> Value {
    let mut result = json!({
        "supportedVersions": [MCP_PROTOCOL_VERSION],
        "capabilities": handler.capabilities().to_value(),
        "ttlMs": 0,
        "cacheScope": "private",
    });
    if let Some(instructions) = handler
        .instructions()
        .filter(|instructions| !instructions.trim().is_empty())
    {
        result["instructions"] = Value::String(instructions);
    }
    result
}

fn decorate_success_response(response: &mut Value, handler: &Arc<dyn ServerHandler>, method: &str) {
    let Some(result) = response.get_mut("result").and_then(Value::as_object_mut) else {
        return;
    };
    result
        .entry("resultType")
        .or_insert_with(|| Value::String("complete".to_owned()));
    let complete = result.get("resultType").and_then(Value::as_str) == Some("complete");
    if complete
        && (method == "server/discover" || method.ends_with("/list") || method == "resources/read")
    {
        result.entry("ttlMs").or_insert_with(|| json!(0));
        result
            .entry("cacheScope")
            .or_insert_with(|| Value::String("private".to_owned()));
    }
    let meta = result.entry("_meta").or_insert_with(|| json!({}));
    meta[MCP_SERVER_INFO_META_KEY] = server_info_value(handler.server_info());
}

fn server_info_value(server_info: ServerInfo) -> Value {
    let mut value = json!({ "name": server_info.name, "version": server_info.version });
    if let Some(description) = server_info.description {
        value["description"] = Value::String(description);
    }
    if !server_info.icons.is_empty() {
        value["icons"] = icons_to_value(&server_info.icons);
    }
    value
}

fn server_result(id: JsonRpcId, result: ServerResult<Value>) -> Value {
    match result {
        Ok(result) if result.is_object() => json_rpc_success(id, result),
        Ok(_) => json_rpc_error(id, -32603, "MCP success result must be an object"),
        Err(error) => server_error_response(id, error),
    }
}

fn server_error_response(id: JsonRpcId, error: ServerError) -> Value {
    tracing::debug!(
        rpc.jsonrpc.error_code = error.code,
        mcp.outcome = "error",
        "MCP server handler returned error"
    );
    json_rpc_error(id, error.code, error.message)
}

fn cursor_param(params: Option<&Value>) -> Option<String> {
    params
        .and_then(|params| params.get("cursor"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn completion_request(params: Option<&Value>) -> std::result::Result<McpCompletionRequest, String> {
    let params = params.ok_or_else(|| "missing completion params".to_owned())?;
    let reference = params
        .get("ref")
        .ok_or_else(|| "missing completion ref".to_owned())?;
    let reference_type = require_string(reference, "type")?;
    let argument = params
        .get("argument")
        .ok_or_else(|| "missing completion argument".to_owned())?;
    let argument_name = require_string(argument, "name")?;
    let argument_value = require_string(argument, "value")?;
    let mut request = match reference_type.as_str() {
        "ref/resource" => McpCompletionRequest::new(
            require_string(reference, "uri")?,
            argument_name,
            argument_value,
        ),
        "ref/prompt" => McpCompletionRequest::for_prompt(
            require_string(reference, "name")?,
            argument_name,
            argument_value,
        ),
        _ => return Err(format!("unsupported completion ref type: {reference_type}")),
    };
    if let Some(arguments) = params
        .get("context")
        .and_then(|context| context.get("arguments"))
        .and_then(Value::as_object)
    {
        request.context_arguments.clone_from(arguments);
    }
    Ok(request)
}

fn http_server_request_span(inbound: &JsonRpcInbound, protocol_version: &str) -> tracing::Span {
    match inbound {
        JsonRpcInbound::Request(request) => server_request_span(
            &request.method,
            Some(&request.id.to_string()),
            "streamable_http",
            Some(protocol_version),
        ),
        JsonRpcInbound::Notification(notification) => server_request_span(
            &notification.method,
            None,
            "streamable_http",
            Some(protocol_version),
        ),
        JsonRpcInbound::Response(response) => server_request_span(
            "",
            Some(&response.id.to_string()),
            "streamable_http",
            Some(protocol_version),
        ),
    }
}

fn server_request_span(
    method: &str,
    id: Option<&str>,
    transport: &'static str,
    protocol_version: Option<&str>,
) -> tracing::Span {
    tracing::info_span!(
        "mcp.server.request",
        rpc.system = "jsonrpc",
        rpc.jsonrpc.version = "2.0",
        rpc.method = %method,
        mcp.method = %method,
        mcp.request_id = id.unwrap_or(""),
        mcp.transport = transport,
        mcp.protocol_version = protocol_version.unwrap_or(""),
        mcp.outcome = tracing::field::Empty,
        rpc.jsonrpc.error_code = tracing::field::Empty,
    )
}

fn inbound_meta(inbound: &JsonRpcInbound) -> Option<&serde_json::Map<String, Value>> {
    match inbound {
        JsonRpcInbound::Request(request) => request_meta(request.params.as_ref()),
        JsonRpcInbound::Notification(notification) => request_meta(notification.params.as_ref()),
        JsonRpcInbound::Response(_) => None,
    }
}

fn record_response_outcome(response: &Value) {
    if record_response_error(response).is_some() {
        tracing::Span::current().record("mcp.outcome", "error");
    } else {
        tracing::Span::current().record("mcp.outcome", "success");
    }
}

fn record_response_error(response: &Value) -> Option<i64> {
    let code = response
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(Value::as_i64)?;
    tracing::Span::current().record("rpc.jsonrpc.error_code", code);
    Some(code)
}

type SharedWriter = Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>;

async fn write_json_line(stdout: &SharedWriter, message: &Value) -> Result<()> {
    let line = serde_json::to_string(message)?;
    let mut stdout = stdout.lock().await;
    stdout.write_all(line.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    drop(stdout);
    Ok(())
}

/// Convenience fallback for handlers.
#[must_use]
pub fn method_not_found(request: JsonRpcRequest) -> Value {
    json_rpc_method_not_found(request.id, &request.method)
}

#[cfg(test)]
mod transport_tests {
    use super::*;

    #[test]
    fn bearer_token_rejects_duplicate_or_combined_values() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer first"),
        );
        headers.append(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer second"),
        );
        assert!(bearer_token(&headers).is_none());

        headers.clear();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer first, Bearer second"),
        );
        assert!(bearer_token(&headers).is_none());
    }
}
