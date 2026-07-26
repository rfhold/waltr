use std::{
    collections::HashSet,
    convert::Infallible,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
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
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::{Mutex, Notify, mpsc, oneshot},
};
use tokio_stream as stream;
use tracing::Instrument as _;
use url::Url;
use uuid::Uuid;

mod session;

#[cfg(feature = "streamable-http-postgres")]
pub use session::PostgresStreamableHttpSessionStore;
#[cfg(feature = "streamable-http-sqlite")]
pub use session::SqliteStreamableHttpSessionStore;
pub use session::{
    InMemoryStreamableHttpSessionStore, StreamableHttpAbandonInitialization,
    StreamableHttpAbandonInitializationRequest, StreamableHttpAppendEvents,
    StreamableHttpAppendEventsRequest, StreamableHttpClaimInitialization,
    StreamableHttpCleanupExpired, StreamableHttpCompleteInitialization,
    StreamableHttpCompleteInitializationRequest, StreamableHttpCreateSession,
    StreamableHttpCreateSessionRequest, StreamableHttpDeleteSession, StreamableHttpEvent,
    StreamableHttpEventId, StreamableHttpEventIdParseError, StreamableHttpInspectSession,
    StreamableHttpPrepareDelivery, StreamableHttpPrepareDeliveryRequest, StreamableHttpSession,
    StreamableHttpSessionKey, StreamableHttpSessionLifecycle, StreamableHttpSessionOwner,
    StreamableHttpSessionRequest, StreamableHttpSessionStore, StreamableHttpSessionStoreConfig,
    StreamableHttpSessionStoreError, StreamableHttpSessionStoreResult,
};

use crate::{
    Error, Result,
    oauth::{
        McpAccessToken, McpBearerChallenge, McpPrincipalId, McpProtectedResourceMetadata,
        McpTokenValidation, protected_resource_metadata_urls,
    },
    protocol::{
        JsonRpcId, JsonRpcInbound, JsonRpcNotification, JsonRpcRequest, MCP_PROTOCOL_VERSION,
        json_rpc_error, json_rpc_method_not_found, json_rpc_notification, json_rpc_success,
        parse_json_rpc_message, parse_json_rpc_str,
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

/// Server capabilities advertised during initialize.
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
    /// Tasks capability object.
    pub tasks: Option<Value>,
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

    /// Advertise task support with a caller-supplied capability object.
    #[must_use]
    pub fn tasks(mut self, capability: Value) -> Self {
        self.tasks = Some(capability);
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
        if let Some(value) = &self.tasks {
            object.insert("tasks".to_owned(), value.clone());
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
            code: -32002,
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
    idempotency_key: Option<String>,
}

impl std::fmt::Debug for ServerContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerContext")
            .field("request_id", &self.request_id)
            .field("idempotency_key", &self.idempotency_key)
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

    /// Return the durable operation key for an idempotent handler invocation.
    #[must_use]
    pub fn idempotency_key(&self) -> Option<&str> {
        self.idempotency_key.as_deref()
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

    /// Send a notification to the peer.
    ///
    /// # Errors
    ///
    /// Returns an error when the peer notification channel is closed.
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        self.sender
            .send(json_rpc_notification(method, params))
            .await
    }

    /// Send a logging notification.
    ///
    /// # Errors
    ///
    /// Returns an error when the peer notification channel is closed.
    pub async fn log(&self, level: &str, logger: Option<&str>, data: Value) -> Result<()> {
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
    Stdio(Arc<Mutex<tokio::io::Stdout>>),
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
    /// Optional instructions sent to clients during initialize.
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
    /// Set logging level.
    fn set_log_level(
        &self,
        _level: String,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<()>> {
        Box::pin(async { Err(ServerError::method_not_found("logging/setLevel")) })
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
    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
    let context = ServerContext {
        sender: NotificationSender::Stdio(Arc::clone(&stdout)),
        request_id: None,
        request_meta: None,
        idempotency_key: None,
    };
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        match parse_json_rpc_str(&line).inspect_err(|error| {
            tracing::warn!(%error, mcp.transport = "stdio", "MCP stdio JSON-RPC parse failed");
        })? {
            JsonRpcInbound::Request(request) => {
                let handler = Arc::clone(&handler);
                let stdout = Arc::clone(&stdout);
                let mut request_context = context.clone();
                request_context.request_id = Some(request.id.clone());
                request_context.request_meta = request_meta(request.params.as_ref()).cloned();
                let span = server_request_span(
                    &request.method,
                    Some(&request.id.to_string()),
                    "stdio",
                    None,
                    None,
                );
                set_span_parent_from_meta(&span, request_context.request_meta.as_ref());
                tokio::spawn(
                    async move {
                        let response = handle_request(handler, request, request_context).await;
                        record_response_outcome(&response);
                        if let Err(error) = write_json_line(&stdout, &response).await {
                            tracing::debug!(%error, "failed to write MCP stdio response");
                        }
                    }
                    .instrument(span),
                );
            }
            JsonRpcInbound::Notification(notification) => {
                let handler = Arc::clone(&handler);
                let request_meta = request_meta(notification.params.as_ref()).cloned();
                let span = server_request_span(&notification.method, None, "stdio", None, None);
                set_span_parent_from_meta(&span, request_meta.as_ref());
                let mut notification_context = context.clone();
                notification_context.request_meta = request_meta;
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
    session_store: Arc<dyn StreamableHttpSessionStore>,
    serve_root_protected_resource_metadata: bool,
}

impl Default for StreamableHttpOptions {
    fn default() -> Self {
        Self {
            allowed_origins: Arc::new(HashSet::new()),
            authorization: None,
            session_store: Arc::new(InMemoryStreamableHttpSessionStore::default()),
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
            .field("session_store", &true)
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

    /// Set the persistence store for Streamable HTTP sessions and queued events.
    #[must_use]
    pub fn with_session_store(mut self, store: Arc<dyn StreamableHttpSessionStore>) -> Self {
        self.session_store = store;
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
    let router = Router::new()
        .route("/mcp", post(http_post).get(http_get).delete(http_delete))
        .route(
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
        session_store: options.session_store,
        allowed_origins: options.allowed_origins,
        authorization: options.authorization,
        delivery_notify: Arc::new(Notify::new()),
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
    session_store: Arc<dyn StreamableHttpSessionStore>,
    allowed_origins: Arc<HashSet<CanonicalOrigin>>,
    authorization: Option<StreamableHttpAuthorization>,
    delivery_notify: Arc<Notify>,
}

// Keeping the ordered validation and session transition pipeline together makes fail-closed review easier.
#[allow(clippy::too_many_lines)]
async fn http_post(State(state): State<HttpState>, request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    let headers = parts.headers;
    if !origin_allowed(&state, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !accepts_streamable_http_post(&headers) || !has_json_content_type(&headers) {
        tracing::warn!(
            mcp.transport = "streamable_http",
            "MCP HTTP request rejected due to invalid Accept header"
        );
        return (
            StatusCode::NOT_ACCEPTABLE,
            "MCP Streamable HTTP POST requires Accept: application/json and text/event-stream",
        )
            .into_response();
    }
    let authorization = match authorize_http_request(&state, &headers).await {
        Ok(authorization) => authorization,
        Err(response) => return response,
    };
    let session_header = match single_header(&headers, "mcp-session-id") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let presented_protocol_version = match single_header(&headers, "mcp-protocol-version") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let prevalidated_session = if let Some(session_id) = &session_header {
        match state
            .session_store
            .inspect_session(StreamableHttpSessionRequest {
                key: StreamableHttpSessionKey {
                    session_id: session_id.clone(),
                    owner: authorization.owner.clone(),
                },
                presented_protocol_version: presented_protocol_version.clone(),
                now: SystemTime::now(),
            })
            .await
        {
            Ok(StreamableHttpInspectSession::Session(session)) => Some(session),
            Ok(StreamableHttpInspectSession::NotFound) => {
                return StatusCode::NOT_FOUND.into_response();
            }
            Ok(StreamableHttpInspectSession::ProtocolVersionMismatch) => {
                return (StatusCode::BAD_REQUEST, "MCP protocol version mismatch").into_response();
            }
            Err(error) => return session_store_error_response(error),
        }
    } else {
        None
    };
    let Ok(body) = to_bytes(body, 1024 * 1024).await else {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            "MCP request body is too large",
        )
            .into_response();
    };
    let value = match serde_json::from_slice::<Value>(&body) {
        Ok(value) => value,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let inbound = match parse_json_rpc_message(value) {
        Ok(inbound) => inbound,
        Err(error) => {
            tracing::warn!(
                %error,
                mcp.transport = "streamable_http",
                "MCP HTTP JSON-RPC parse failed"
            );
            return (StatusCode::BAD_REQUEST, error.to_string()).into_response();
        }
    };
    let is_initialize = matches!(
        &inbound,
        JsonRpcInbound::Request(request) if request.method == "initialize"
    );
    let mut session_id = match (is_initialize, session_header) {
        (true, None) => String::new(),
        (true, Some(_)) => {
            return (
                StatusCode::BAD_REQUEST,
                "MCP initialize request must not include a session id",
            )
                .into_response();
        }
        (false, Some(session_id)) => session_id,
        (false, None) => {
            return (StatusCode::BAD_REQUEST, "missing MCP session id").into_response();
        }
    };
    if is_initialize {
        let JsonRpcInbound::Request(initialize) = &inbound else {
            unreachable!()
        };
        if let Err(message) = validate_initialize_params(initialize.params.as_ref()) {
            return initialize_error_response(initialize.id.clone(), -32602, message);
        }
        let negotiated = initialize_result(&state.handler)["protocolVersion"]
            .as_str()
            .expect("initialize result has protocolVersion")
            .to_owned();
        let mut created = false;
        for _ in 0..3 {
            session_id = Uuid::now_v7().to_string();
            let request = StreamableHttpCreateSessionRequest {
                key: StreamableHttpSessionKey {
                    session_id: session_id.clone(),
                    owner: authorization.owner.clone(),
                },
                protocol_version: negotiated.clone(),
                now: SystemTime::now(),
            };
            match state.session_store.create_session(request).await {
                Ok(StreamableHttpCreateSession::Created(_)) => {
                    created = true;
                    break;
                }
                Ok(StreamableHttpCreateSession::AlreadyExists) => {}
                Ok(StreamableHttpCreateSession::CapacityExceeded) => {
                    return StatusCode::TOO_MANY_REQUESTS.into_response();
                }
                Err(error) => return session_store_error_response(error),
            }
        }
        if !created {
            return session_store_server_error_response();
        }
    }
    let session_request = StreamableHttpSessionRequest {
        key: StreamableHttpSessionKey {
            session_id: session_id.clone(),
            owner: authorization.owner.clone(),
        },
        presented_protocol_version,
        now: SystemTime::now(),
    };
    let (tx, rx) = mpsc::channel(16);
    let mut context = ServerContext {
        sender: NotificationSender::Channel(tx),
        request_id: None,
        request_meta: None,
        idempotency_key: None,
    };
    let span = http_server_request_span(&inbound, &session_id);
    set_span_parent_from_meta(&span, inbound_meta(&inbound));
    async move {
        let is_initialized_notification = matches!(
            &inbound,
            JsonRpcInbound::Notification(notification)
                if notification.method == "notifications/initialized"
        );
        let initialization_claim_id = if is_initialized_notification {
            Some(
                match state
                    .session_store
                    .claim_initialization(session_request.clone())
                    .await
                {
                    Ok(StreamableHttpClaimInitialization::Claimed {
                        claim_id,
                        operation_id,
                    }) => (claim_id, operation_id),
                    Ok(StreamableHttpClaimInitialization::NotFound) => {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    Ok(StreamableHttpClaimInitialization::ProtocolVersionMismatch) => {
                        return (StatusCode::BAD_REQUEST, "MCP protocol version mismatch")
                            .into_response();
                    }
                    Ok(StreamableHttpClaimInitialization::LifecycleMismatch) => {
                        if matches!(
                            state
                                .session_store
                                .inspect_session(refreshed_session_request(&session_request))
                                .await,
                            Ok(StreamableHttpInspectSession::Session(session))
                                if session.lifecycle == StreamableHttpSessionLifecycle::Initialized
                        ) {
                            return StatusCode::ACCEPTED.into_response();
                        }
                        return (
                            StatusCode::BAD_REQUEST,
                            "MCP session is not awaiting the initialized notification",
                        )
                            .into_response();
                    }
                    Ok(StreamableHttpClaimInitialization::InitializationInProgress {
                        retry_after,
                    }) => {
                        return retry_after_response(StatusCode::SERVICE_UNAVAILABLE, retry_after);
                    }
                    Err(error) => return session_store_error_response(error),
                },
            )
        } else if !is_initialize {
            let lifecycle = prevalidated_session
                .as_ref()
                .expect("non-initialize session was prevalidated")
                .lifecycle;
            if let Some(response) = lifecycle_error(lifecycle, &inbound) {
                return lifecycle_error_response(&session_id, response);
            }
            None
        } else {
            None
        };
        let (handler_done, handler_completion) = oneshot::channel();
        let notification_collector = tokio::spawn(collect_notifications(
            rx,
            handler_completion,
            state.session_store.config(),
        ));
        let response = match inbound {
            JsonRpcInbound::Request(request) => {
                let mut context = context;
                context.request_id = Some(request.id.clone());
                context.request_meta = request_meta(request.params.as_ref()).cloned();
                let response = handle_request(Arc::clone(&state.handler), request, context).await;
                Some(response)
            }
            JsonRpcInbound::Notification(notification) => {
                if is_initialized_notification {
                    let (claim_id, operation_id) = initialization_claim_id
                        .as_ref()
                        .expect("initialized notification has a claim");
                    context.idempotency_key = Some(operation_id.clone());
                    let handler = Arc::clone(&state.handler);
                    if tokio::time::timeout(Duration::from_secs(30), async move {
                        handler.handle_notification(notification, context).await;
                    })
                    .await
                    .is_err()
                    {
                        match state
                            .session_store
                            .abandon_initialization(StreamableHttpAbandonInitializationRequest {
                                session: refreshed_session_request(&session_request),
                                claim_id: claim_id.clone(),
                            })
                            .await
                        {
                            Ok(StreamableHttpAbandonInitialization::Abandoned) => {}
                            Ok(_) => return session_store_server_error_response(),
                            Err(error) => return session_store_error_response(error),
                        }
                        return session_store_server_error_response();
                    }
                } else {
                    state
                        .handler
                        .handle_notification(notification, context)
                        .await;
                }
                None
            }
            JsonRpcInbound::Response(_) => {
                drop(context);
                None
            }
        };
        drop(handler_done);

        let Ok(collected) = notification_collector.await else {
            return session_store_server_error_response();
        };
        if collected.capacity_exceeded {
            if let Some((claim_id, _)) = initialization_claim_id.clone() {
                match state
                    .session_store
                    .abandon_initialization(StreamableHttpAbandonInitializationRequest {
                        session: refreshed_session_request(&session_request),
                        claim_id,
                    })
                    .await
                {
                    Ok(StreamableHttpAbandonInitialization::Abandoned) => {}
                    Ok(_) => return session_store_server_error_response(),
                    Err(error) => return session_store_error_response(error),
                }
            }
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
        let notifications = collected.messages;

        if is_initialized_notification {
            let (claim_id, _) =
                initialization_claim_id.expect("initialized notification has a claim");
            match state
                .session_store
                .complete_initialization(StreamableHttpCompleteInitializationRequest {
                    session: refreshed_session_request(&session_request),
                    claim_id,
                    messages: notifications,
                })
                .await
            {
                Ok(StreamableHttpCompleteInitialization::Completed(_)) => {
                    state.delivery_notify.notify_waiters();
                    return StatusCode::ACCEPTED.into_response();
                }
                Ok(StreamableHttpCompleteInitialization::AlreadyCompleted) => {
                    return StatusCode::ACCEPTED.into_response();
                }
                Ok(StreamableHttpCompleteInitialization::NotFound) => {
                    return StatusCode::NOT_FOUND.into_response();
                }
                Ok(StreamableHttpCompleteInitialization::CapacityExceeded) => {
                    return StatusCode::TOO_MANY_REQUESTS.into_response();
                }
                Ok(_) => {
                    return (StatusCode::BAD_REQUEST, "MCP initialization claim failed")
                        .into_response();
                }
                Err(error) => return session_store_error_response(error),
            }
        }

        let mut headers = HeaderMap::new();
        if let Ok(value) = HeaderValue::from_str(&session_id) {
            headers.insert("mcp-session-id", value);
        }
        headers.insert(
            "mcp-protocol-version",
            HeaderValue::from_static(MCP_PROTOCOL_VERSION),
        );

        let Some(response) = response else {
            if !notifications.is_empty() {
                match state
                    .session_store
                    .append_events(StreamableHttpAppendEventsRequest {
                        session: refreshed_session_request(&session_request),
                        messages: notifications,
                    })
                    .await
                {
                    Ok(StreamableHttpAppendEvents::Events(_)) => {}
                    Ok(StreamableHttpAppendEvents::NotFound) => {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    Ok(StreamableHttpAppendEvents::CapacityExceeded) => {
                        return StatusCode::TOO_MANY_REQUESTS.into_response();
                    }
                    Ok(_) => {
                        return (StatusCode::BAD_REQUEST, "MCP session state mismatch")
                            .into_response();
                    }
                    Err(error) => return session_store_error_response(error),
                }
                state.delivery_notify.notify_waiters();
            }
            tracing::Span::current().record("mcp.outcome", "success");
            return (StatusCode::ACCEPTED, headers).into_response();
        };
        record_response_outcome(&response);
        if notifications.is_empty() {
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            return (StatusCode::OK, headers, axum::Json(response)).into_response();
        }
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        let mut messages = notifications;
        messages.push(response);
        let stream = stream::iter(messages.into_iter().map(|message| {
            let data = serde_json::to_string(&message).expect("json-rpc message serializes");
            Ok::<_, std::convert::Infallible>(format!("data: {data}\n\n"))
        }));
        (StatusCode::OK, headers, Body::from_stream(stream)).into_response()
    }
    .instrument(span)
    .await
}

// This is one ordered authorization, session validation, and stream-start pipeline.
#[allow(clippy::too_many_lines)]
async fn http_get(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if !origin_allowed(&state, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !accepts_streamable_http_get(&headers) {
        return (
            StatusCode::NOT_ACCEPTABLE,
            "MCP Streamable HTTP GET requires Accept: text/event-stream",
        )
            .into_response();
    }
    let authorization = match authorize_http_request(&state, &headers).await {
        Ok(authorization) => authorization,
        Err(response) => return response,
    };
    let session_id = match single_header(&headers, "mcp-session-id") {
        Ok(Some(value)) if !value.is_empty() => value,
        Ok(_) => return (StatusCode::BAD_REQUEST, "missing MCP session id").into_response(),
        Err(response) => return response,
    };
    let presented_protocol_version = match single_header(&headers, "mcp-protocol-version") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let last_event_id = match single_header(&headers, "last-event-id") {
        Ok(Some(value)) => match value.parse().ok() {
            Some(id) => Some(id),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    "invalid Last-Event-ID; expected an MCP stream event ID",
                )
                    .into_response();
            }
        },
        Ok(None) => None,
        Err(response) => return response,
    };
    let session_request = StreamableHttpSessionRequest {
        key: StreamableHttpSessionKey {
            session_id: session_id.clone(),
            owner: authorization.owner,
        },
        presented_protocol_version,
        now: SystemTime::now(),
    };
    if authorization
        .expires_at
        .is_some_and(|expires_at| expires_at <= SystemTime::now())
    {
        match state
            .session_store
            .inspect_session(refreshed_session_request(&session_request))
            .await
        {
            Ok(StreamableHttpInspectSession::Session(session))
                if session.lifecycle == StreamableHttpSessionLifecycle::Initialized => {}
            Ok(StreamableHttpInspectSession::Session(_)) => {
                return (
                    StatusCode::BAD_REQUEST,
                    "MCP session has not completed initialization",
                )
                    .into_response();
            }
            Ok(StreamableHttpInspectSession::NotFound) => {
                return StatusCode::NOT_FOUND.into_response();
            }
            Ok(StreamableHttpInspectSession::ProtocolVersionMismatch) => {
                return (StatusCode::BAD_REQUEST, "MCP protocol version mismatch").into_response();
            }
            Err(error) => return session_store_error_response(error),
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        headers.insert(
            "mcp-protocol-version",
            HeaderValue::from_static(MCP_PROTOCOL_VERSION),
        );
        if let Ok(value) = HeaderValue::from_str(&session_id) {
            headers.insert("mcp-session-id", value);
        }
        return (StatusCode::OK, headers, Body::empty()).into_response();
    }
    let mut revocation = authorization.revocation;
    let initial = match prepare_delivery_before_expiry(
        &state,
        &session_request,
        last_event_id.clone(),
        authorization.expires_at,
        &mut revocation,
    )
    .await
    {
        PrepareBeforeExpiry::Expired => {
            return empty_get_response(&session_id);
        }
        PrepareBeforeExpiry::Store(Ok(StreamableHttpPrepareDelivery::NotFound)) => {
            return StatusCode::NOT_FOUND.into_response();
        }
        PrepareBeforeExpiry::Store(Ok(StreamableHttpPrepareDelivery::LifecycleMismatch)) => {
            return (
                StatusCode::BAD_REQUEST,
                "MCP session has not completed initialization",
            )
                .into_response();
        }
        PrepareBeforeExpiry::Store(Ok(StreamableHttpPrepareDelivery::ProtocolVersionMismatch)) => {
            return (StatusCode::BAD_REQUEST, "MCP protocol version mismatch").into_response();
        }
        PrepareBeforeExpiry::Store(Ok(StreamableHttpPrepareDelivery::InvalidCursor)) => {
            return (StatusCode::BAD_REQUEST, "invalid Last-Event-ID cursor").into_response();
        }
        PrepareBeforeExpiry::Store(Ok(StreamableHttpPrepareDelivery::DeliveryBusy {
            retry_after,
        })) => {
            return retry_after_response(StatusCode::CONFLICT, retry_after);
        }
        PrepareBeforeExpiry::Store(Ok(StreamableHttpPrepareDelivery::Idle {
            priming_event_id,
        })) => StreamStart::Idle(priming_event_id),
        PrepareBeforeExpiry::Store(Ok(StreamableHttpPrepareDelivery::Events {
            events, ..
        })) => StreamStart::Events(events),
        PrepareBeforeExpiry::Store(Err(error)) => return session_store_error_response(error),
    };
    if credential_expired(authorization.expires_at) {
        return empty_get_response(&session_id);
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        "mcp-protocol-version",
        HeaderValue::from_static(MCP_PROTOCOL_VERSION),
    );
    if let Ok(value) = HeaderValue::from_str(&session_id) {
        headers.insert("mcp-session-id", value);
    }
    let (sender, receiver) = mpsc::channel(2);
    tokio::spawn(run_get_stream(GetStreamContext {
        state,
        session_request,
        initial,
        prime: last_event_id.is_none(),
        credential_expires_at: authorization.expires_at,
        revocation,
        sender,
    }));
    (
        StatusCode::OK,
        headers,
        Body::from_stream(stream::wrappers::ReceiverStream::new(receiver)),
    )
        .into_response()
}

enum StreamStart {
    Idle(StreamableHttpEventId),
    Events(Vec<StreamableHttpEvent>),
}

async fn prepare_delivery(
    state: &HttpState,
    session: &StreamableHttpSessionRequest,
    acknowledged_event_id: Option<StreamableHttpEventId>,
) -> StreamableHttpSessionStoreResult<StreamableHttpPrepareDelivery> {
    state
        .session_store
        .prepare_delivery(StreamableHttpPrepareDeliveryRequest {
            session: refreshed_session_request(session),
            acknowledged_event_id,
        })
        .await
}

enum PrepareBeforeExpiry {
    Expired,
    Store(StreamableHttpSessionStoreResult<StreamableHttpPrepareDelivery>),
}

async fn prepare_delivery_before_expiry(
    state: &HttpState,
    session: &StreamableHttpSessionRequest,
    acknowledged_event_id: Option<StreamableHttpEventId>,
    credential_expires_at: Option<SystemTime>,
    revocation: &mut Option<McpRevocationLease>,
) -> PrepareBeforeExpiry {
    if credential_expired(credential_expires_at) {
        return PrepareBeforeExpiry::Expired;
    }
    loop {
        let prepare = prepare_delivery(state, session, acknowledged_event_id.clone());
        let poll_interval = revocation
            .as_ref()
            .map_or(Duration::MAX, |lease| lease.poll_interval);
        tokio::select! {
            biased;
            () = tokio::time::sleep(credential_expires_at.map_or(Duration::MAX, credential_expiry_delay)), if credential_expires_at.is_some() => return PrepareBeforeExpiry::Expired,
            result = async { revocation.as_mut().expect("guarded by branch").notifications.changed().await }, if revocation.is_some() => {
                if result.is_err() || !revocation.as_ref().expect("lease exists").validate().await {
                    return PrepareBeforeExpiry::Expired;
                }
            }
            () = tokio::time::sleep(poll_interval), if revocation.is_some() => {
                if !revocation.as_ref().expect("lease exists").validate().await {
                    return PrepareBeforeExpiry::Expired;
                }
            }
            result = prepare => return PrepareBeforeExpiry::Store(result),
        }
    }
}

struct GetStreamContext {
    state: HttpState,
    session_request: StreamableHttpSessionRequest,
    initial: StreamStart,
    prime: bool,
    credential_expires_at: Option<SystemTime>,
    revocation: Option<McpRevocationLease>,
    sender: mpsc::Sender<std::result::Result<String, Infallible>>,
}

// Stream priming, keepalive, expiry, and delivery ordering form one state machine.
#[allow(clippy::too_many_lines)]
async fn run_get_stream(context: GetStreamContext) {
    let GetStreamContext {
        state,
        session_request,
        initial,
        prime,
        credential_expires_at,
        mut revocation,
        sender,
    } = context;
    if !send_with_authorization(
        &sender,
        "retry: 1000\n\n".to_owned(),
        credential_expires_at,
        &mut revocation,
    )
    .await
    {
        return;
    }
    match initial {
        StreamStart::Events(events) => {
            if prime
                && let Some(first) = events.first()
                && !send_with_authorization(
                    &sender,
                    format!("id: {}:0\n\n", first.id.stream_id),
                    credential_expires_at,
                    &mut revocation,
                )
                .await
            {
                return;
            }
            for event in events {
                if !send_with_authorization(
                    &sender,
                    format_sse_event(&event),
                    credential_expires_at,
                    &mut revocation,
                )
                .await
                {
                    return;
                }
            }
            return;
        }
        StreamStart::Idle(priming_event_id) if prime => {
            if !send_with_authorization(
                &sender,
                format!("id: {priming_event_id}\n\n"),
                credential_expires_at,
                &mut revocation,
            )
            .await
            {
                return;
            }
        }
        StreamStart::Idle(_) => {}
    }

    let idle = tokio::time::sleep(Duration::from_secs(30));
    let keepalive = tokio::time::sleep(Duration::from_secs(15));
    tokio::pin!(idle, keepalive);
    loop {
        let revocation_poll = revocation
            .as_ref()
            .map_or(Duration::MAX, |lease| lease.poll_interval);
        tokio::select! {
            biased;
            () = tokio::time::sleep(credential_expires_at.map_or(Duration::MAX, credential_expiry_delay)), if credential_expires_at.is_some() => return,
            result = async { revocation.as_mut().expect("guarded by branch").notifications.changed().await }, if revocation.is_some() => {
                if result.is_err() || !revocation.as_ref().expect("lease exists").validate().await { return; }
            }
            () = tokio::time::sleep(revocation_poll), if revocation.is_some() => {
                if !revocation.as_ref().expect("lease exists").validate().await { return; }
            }
            () = state.delivery_notify.notified() => {}
            () = tokio::time::sleep(Duration::from_secs(1)) => {}
            () = &mut keepalive => {
                if !send_with_authorization(&sender, ": keepalive\n\n".to_owned(), credential_expires_at, &mut revocation).await {
                    return;
                }
                keepalive.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(15));
                continue;
            }
            () = &mut idle => return,
        }

        match prepare_delivery_before_expiry(
            &state,
            &session_request,
            None,
            credential_expires_at,
            &mut revocation,
        )
        .await
        {
            PrepareBeforeExpiry::Store(Ok(StreamableHttpPrepareDelivery::Events {
                events,
                ..
            })) => {
                for event in events {
                    if !send_with_authorization(
                        &sender,
                        format_sse_event(&event),
                        credential_expires_at,
                        &mut revocation,
                    )
                    .await
                    {
                        return;
                    }
                }
                return;
            }
            PrepareBeforeExpiry::Store(Ok(StreamableHttpPrepareDelivery::Idle { .. })) => {}
            PrepareBeforeExpiry::Store(Ok(StreamableHttpPrepareDelivery::DeliveryBusy {
                retry_after,
            })) => {
                tokio::time::sleep(retry_after.min(Duration::from_secs(1))).await;
            }
            PrepareBeforeExpiry::Expired
            | PrepareBeforeExpiry::Store(
                Ok(
                    StreamableHttpPrepareDelivery::NotFound
                    | StreamableHttpPrepareDelivery::ProtocolVersionMismatch
                    | StreamableHttpPrepareDelivery::LifecycleMismatch
                    | StreamableHttpPrepareDelivery::InvalidCursor,
                )
                | Err(_),
            ) => return,
        }
    }
}

fn format_sse_event(event: &StreamableHttpEvent) -> String {
    let data = serde_json::to_string(&event.message).expect("json-rpc message serializes");
    format!("id: {}\ndata: {data}\n\n", event.id)
}

async fn send_before_expiry(
    sender: &mpsc::Sender<std::result::Result<String, Infallible>>,
    frame: String,
    credential_expires_at: Option<SystemTime>,
) -> bool {
    if credential_expired(credential_expires_at) {
        return false;
    }
    if let Some(delay) = credential_expires_at.map(credential_expiry_delay) {
        tokio::select! {
            biased;
            () = tokio::time::sleep(delay) => false,
            result = sender.send(Ok(frame)) => result.is_ok(),
        }
    } else {
        sender.send(Ok(frame)).await.is_ok()
    }
}

async fn send_with_authorization(
    sender: &mpsc::Sender<std::result::Result<String, Infallible>>,
    frame: String,
    credential_expires_at: Option<SystemTime>,
    revocation: &mut Option<McpRevocationLease>,
) -> bool {
    let Some(lease) = revocation.as_mut() else {
        return send_before_expiry(sender, frame, credential_expires_at).await;
    };
    loop {
        if credential_expired(credential_expires_at) {
            return false;
        }
        let validator = lease.validator.clone();
        let validation = validator();
        tokio::pin!(validation);
        let valid = tokio::select! {
            biased;
            () = tokio::time::sleep(credential_expires_at.map_or(Duration::MAX, credential_expiry_delay)), if credential_expires_at.is_some() => return false,
            result = lease.notifications.changed() => {
                if result.is_err() { return false; }
                continue;
            }
            result = &mut validation => matches!(result, Ok(true)),
        };
        if !valid {
            return false;
        }
        let reserve = sender.reserve();
        tokio::pin!(reserve);
        let permit = tokio::select! {
            biased;
            () = tokio::time::sleep(credential_expires_at.map_or(Duration::MAX, credential_expiry_delay)), if credential_expires_at.is_some() => return false,
            result = lease.notifications.changed() => {
                if result.is_err() { return false; }
                continue;
            }
            () = tokio::time::sleep(lease.poll_interval) => continue,
            result = &mut reserve => match result {
                Ok(permit) => permit,
                Err(_) => return false,
            },
        };
        match lease.notifications.has_changed() {
            Ok(true) => continue,
            Ok(false) => {}
            Err(_) => return false,
        }
        permit.send(Ok(frame));
        return true;
    }
}

fn credential_expiry_delay(expires_at: SystemTime) -> Duration {
    expires_at
        .duration_since(SystemTime::now())
        .unwrap_or(Duration::ZERO)
}

fn credential_expired(expires_at: Option<SystemTime>) -> bool {
    expires_at
        .map(credential_expiry_delay)
        .is_some_and(|delay| delay.is_zero())
}

fn empty_get_response(session_id: &str) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        "mcp-protocol-version",
        HeaderValue::from_static(MCP_PROTOCOL_VERSION),
    );
    if let Ok(value) = HeaderValue::from_str(session_id) {
        headers.insert("mcp-session-id", value);
    }
    (StatusCode::OK, headers, Body::empty()).into_response()
}

async fn http_delete(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if !origin_allowed(&state, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let authorization = match authorize_http_request(&state, &headers).await {
        Ok(authorization) => authorization,
        Err(response) => return response,
    };
    let session_id = match single_header(&headers, "mcp-session-id") {
        Ok(Some(value)) if !value.is_empty() => value,
        Ok(_) => return (StatusCode::BAD_REQUEST, "missing MCP session id").into_response(),
        Err(response) => return response,
    };
    let presented_protocol_version = match single_header(&headers, "mcp-protocol-version") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let removed = match state
        .session_store
        .delete_session(StreamableHttpSessionRequest {
            key: StreamableHttpSessionKey {
                session_id,
                owner: authorization.owner,
            },
            presented_protocol_version,
            now: SystemTime::now(),
        })
        .await
    {
        Ok(StreamableHttpDeleteSession::Deleted) => true,
        Ok(StreamableHttpDeleteSession::NotFound) => false,
        Ok(StreamableHttpDeleteSession::ProtocolVersionMismatch) => {
            return (StatusCode::BAD_REQUEST, "MCP protocol version mismatch").into_response();
        }
        Err(error) => return session_store_error_response(error),
    };
    if removed {
        state.delivery_notify.notify_waiters();
        StatusCode::ACCEPTED.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
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
            owner: StreamableHttpSessionOwner::Anonymous,
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
            principal_id,
            expires_at,
            revocation,
        }) => Ok(AuthorizedHttpRequest {
            owner: StreamableHttpSessionOwner::Authenticated(principal_id),
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
    owner: StreamableHttpSessionOwner,
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

fn lifecycle_error(
    lifecycle: StreamableHttpSessionLifecycle,
    inbound: &JsonRpcInbound,
) -> Option<Value> {
    match inbound {
        JsonRpcInbound::Request(request) => match request.method.as_str() {
            "initialize" => Some(json_rpc_error(
                request.id.clone(),
                -32600,
                "MCP session is already initialized",
            )),
            "ping" => None,
            _ if lifecycle == StreamableHttpSessionLifecycle::Initialized => None,
            _ => Some(json_rpc_error(
                request.id.clone(),
                -32600,
                "MCP session has not completed initialization",
            )),
        },
        JsonRpcInbound::Notification(_) | JsonRpcInbound::Response(_) => None,
    }
}

fn lifecycle_error_response(session_id: &str, response: Value) -> Response {
    tracing::Span::current().record("mcp.outcome", "error");
    record_response_error(&response);
    tracing::debug!(mcp.outcome = "error", "MCP HTTP lifecycle rejected request");
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(session_id) {
        headers.insert("mcp-session-id", value);
    }
    headers.insert(
        "mcp-protocol-version",
        HeaderValue::from_static(MCP_PROTOCOL_VERSION),
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    (StatusCode::OK, headers, axum::Json(response)).into_response()
}

fn session_store_error_response(error: StreamableHttpSessionStoreError) -> Response {
    tracing::error!(
        %error,
        mcp.transport = "streamable_http",
        "MCP HTTP session store operation failed"
    );
    session_store_server_error_response()
}

fn session_store_server_error_response() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "MCP Streamable HTTP session storage error",
    )
        .into_response()
}

fn retry_after_response(status: StatusCode, retry_after: std::time::Duration) -> Response {
    let seconds = retry_after.as_secs().max(1);
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
        headers.insert(header::RETRY_AFTER, value);
    }
    (status, headers).into_response()
}

fn refreshed_session_request(
    request: &StreamableHttpSessionRequest,
) -> StreamableHttpSessionRequest {
    StreamableHttpSessionRequest {
        key: request.key.clone(),
        presented_protocol_version: request.presented_protocol_version.clone(),
        now: SystemTime::now(),
    }
}

struct CollectedNotifications {
    messages: Vec<Value>,
    capacity_exceeded: bool,
}

async fn collect_notifications(
    mut receiver: mpsc::Receiver<Value>,
    handler_completion: oneshot::Receiver<()>,
    config: StreamableHttpSessionStoreConfig,
) -> CollectedNotifications {
    let mut messages = Vec::new();
    let mut pending_bytes = 0_usize;
    let mut capacity_exceeded = false;
    let mut handler_completion = std::pin::pin!(handler_completion);
    loop {
        tokio::select! {
            _ = &mut handler_completion => {
                receiver.close();
                break;
            }
            message = receiver.recv() => {
                let Some(message) = message else {
                    return CollectedNotifications { messages, capacity_exceeded };
                };
                collect_notification(
                    message,
                    &config,
                    &mut messages,
                    &mut pending_bytes,
                    &mut capacity_exceeded,
                );
            }
        }
    }
    while let Some(message) = receiver.recv().await {
        collect_notification(
            message,
            &config,
            &mut messages,
            &mut pending_bytes,
            &mut capacity_exceeded,
        );
    }
    CollectedNotifications {
        messages,
        capacity_exceeded,
    }
}

fn collect_notification(
    message: Value,
    config: &StreamableHttpSessionStoreConfig,
    messages: &mut Vec<Value>,
    pending_bytes: &mut usize,
    capacity_exceeded: &mut bool,
) {
    let message_bytes = serialized_size_bounded(&message, config.max_event_bytes);
    let fits = message_bytes.is_some_and(|message_bytes| {
        messages.len() < config.max_events_per_session
            && pending_bytes
                .checked_add(message_bytes)
                .is_some_and(|total| total <= config.max_pending_bytes_per_session)
    });
    if fits {
        *pending_bytes += message_bytes.expect("checked bounded message size");
        messages.push(message);
    } else {
        *capacity_exceeded = true;
    }
}

fn serialized_size_bounded(message: &Value, limit: usize) -> Option<usize> {
    let mut counter = BoundedSizeCounter { bytes: 0, limit };
    serde_json::to_writer(&mut counter, message)
        .ok()
        .map(|()| counter.bytes)
}

struct BoundedSizeCounter {
    bytes: usize,
    limit: usize,
}

impl std::io::Write for BoundedSizeCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let bytes = self
            .bytes
            .checked_add(buffer.len())
            .filter(|bytes| *bytes <= self.limit)
            .ok_or_else(|| std::io::Error::other("serialized MCP notification exceeds capacity"))?;
        self.bytes = bytes;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
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
fn single_header(
    headers: &HeaderMap,
    name: &'static str,
) -> std::result::Result<Option<String>, Response> {
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

fn accepts_streamable_http_get(headers: &HeaderMap) -> bool {
    let Some(accept) = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    accept
        .split(',')
        .map(|value| value.trim().split(';').next().unwrap_or_default().trim())
        .any(|value| value == "text/event-stream" || value == "*/*")
}

fn validate_initialize_params(params: Option<&Value>) -> std::result::Result<(), String> {
    let params = params
        .and_then(Value::as_object)
        .ok_or_else(|| "initialize params must be an object".to_owned())?;
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| "initialize protocolVersion must be a string".to_owned())?;
    if version != MCP_PROTOCOL_VERSION {
        return Err(format!("unsupported MCP protocol version `{version}`"));
    }
    params
        .get("capabilities")
        .and_then(Value::as_object)
        .ok_or_else(|| "initialize capabilities must be an object".to_owned())?;
    let client_info = params
        .get("clientInfo")
        .and_then(Value::as_object)
        .ok_or_else(|| "initialize clientInfo must be an object".to_owned())?;
    for field in ["name", "version"] {
        if client_info
            .get(field)
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Err(format!(
                "initialize clientInfo.{field} must be a nonempty string"
            ));
        }
    }
    Ok(())
}

fn initialize_result(handler: &Arc<dyn ServerHandler>) -> Value {
    let server_info = handler.server_info();
    let mut result = json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": handler.capabilities().to_value(),
        "serverInfo": {
            "name": server_info.name,
            "version": server_info.version,
        }
    });
    if let Some(description) = server_info.description {
        result["serverInfo"]["description"] = Value::String(description);
    }
    if !server_info.icons.is_empty() {
        result["serverInfo"]["icons"] = icons_to_value(&server_info.icons);
    }
    if let Some(instructions) = handler
        .instructions()
        .filter(|instructions| !instructions.trim().is_empty())
    {
        result["instructions"] = Value::String(instructions);
    }
    result
}

fn initialize_error_response(id: JsonRpcId, code: i64, message: String) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    (
        StatusCode::OK,
        headers,
        axum::Json(json_rpc_error(id, code, message)),
    )
        .into_response()
}

// Keeping the complete method dispatch table together makes protocol coverage auditable.
#[allow(clippy::too_many_lines)]
async fn handle_request(
    handler: Arc<dyn ServerHandler>,
    request: JsonRpcRequest,
    context: ServerContext,
) -> Value {
    match request.method.as_str() {
        "initialize" => json_rpc_success(request.id, initialize_result(&handler)),
        "ping" => json_rpc_success(request.id, json!({})),
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
        "logging/setLevel" => {
            let result = request
                .params
                .as_ref()
                .ok_or_else(|| "missing logging params".to_owned())
                .and_then(|params| require_string(params, "level"))
                .and_then(|level| {
                    if is_valid_log_level(&level) {
                        Ok(level)
                    } else {
                        Err(format!("invalid logging level: {level}"))
                    }
                })
                .map_err(ServerError::invalid_params);
            match result {
                Ok(level) => server_result(
                    request.id,
                    handler
                        .set_log_level(level, context)
                        .await
                        .map(|()| json!({})),
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
    }
}

fn server_result(id: JsonRpcId, result: ServerResult<Value>) -> Value {
    match result {
        Ok(result) => json_rpc_success(id, result),
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
    let target = match reference_type.as_str() {
        "ref/resource" => require_string(reference, "uri")?,
        "ref/prompt" => require_string(reference, "name")?,
        _ => return Err(format!("unsupported completion ref type: {reference_type}")),
    };
    let argument = params
        .get("argument")
        .ok_or_else(|| "missing completion argument".to_owned())?;
    let mut request = McpCompletionRequest::new(
        target,
        require_string(argument, "name")?,
        require_string(argument, "value")?,
    );
    if let Some(arguments) = params
        .get("context")
        .and_then(|context| context.get("arguments"))
        .and_then(Value::as_object)
    {
        request.context_arguments.clone_from(arguments);
    }
    Ok(request)
}

fn is_valid_log_level(level: &str) -> bool {
    matches!(
        level,
        "debug" | "info" | "notice" | "warning" | "error" | "critical" | "alert" | "emergency"
    )
}

fn http_server_request_span(inbound: &JsonRpcInbound, session_id: &str) -> tracing::Span {
    match inbound {
        JsonRpcInbound::Request(request) => server_request_span(
            &request.method,
            Some(&request.id.to_string()),
            "streamable_http",
            Some(session_id),
            Some(MCP_PROTOCOL_VERSION),
        ),
        JsonRpcInbound::Notification(notification) => server_request_span(
            &notification.method,
            None,
            "streamable_http",
            Some(session_id),
            Some(MCP_PROTOCOL_VERSION),
        ),
        JsonRpcInbound::Response(response) => server_request_span(
            "",
            Some(&response.id.to_string()),
            "streamable_http",
            Some(session_id),
            Some(MCP_PROTOCOL_VERSION),
        ),
    }
}

fn server_request_span(
    method: &str,
    id: Option<&str>,
    transport: &'static str,
    session_id: Option<&str>,
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
        mcp.session_id = session_id.unwrap_or(""),
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

async fn write_json_line(stdout: &Arc<Mutex<tokio::io::Stdout>>, message: &Value) -> Result<()> {
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
    #[cfg(feature = "oauth-authorization-server")]
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

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

    #[tokio::test]
    async fn elapsed_expiry_wins_against_ready_event_sender() {
        let (sender, mut receiver) = mpsc::channel(1);
        assert!(
            !send_before_expiry(
                &sender,
                "data: must-not-send\n\n".to_owned(),
                Some(SystemTime::UNIX_EPOCH),
            )
            .await
        );
        assert!(receiver.try_recv().is_err());
        assert_eq!(
            credential_expiry_delay(SystemTime::UNIX_EPOCH),
            Duration::ZERO
        );
    }

    #[cfg(feature = "oauth-authorization-server")]
    #[tokio::test]
    async fn queued_initial_event_is_not_emitted_after_revocation() {
        let valid = Arc::new(AtomicBool::new(true));
        let (notifications, receiver) = tokio::sync::watch::channel(0_u64);
        let lease_valid = valid.clone();
        let mut lease = Some(McpRevocationLease::new(
            move || {
                let valid = lease_valid.clone();
                Box::pin(async move { Ok(valid.load(Ordering::SeqCst)) })
            },
            Duration::from_mins(1),
            receiver,
        ));
        let (sender, mut events) = mpsc::channel(1);
        sender.send(Ok("retry: 1000\n\n".to_owned())).await.unwrap();
        let task = tokio::spawn(async move {
            send_with_authorization(
                &sender,
                "data: revoked-initial-event\n\n".to_owned(),
                None,
                &mut lease,
            )
            .await
        });
        tokio::task::yield_now().await;
        valid.store(false, Ordering::SeqCst);
        notifications.send_modify(|generation| *generation += 1);
        assert_eq!(events.recv().await.unwrap().unwrap(), "retry: 1000\n\n");
        assert!(!task.await.unwrap());
        assert!(events.try_recv().is_err());
    }

    #[cfg(feature = "oauth-authorization-server")]
    #[tokio::test]
    async fn retry_frame_is_not_emitted_after_lease_failure_or_expiry() {
        let (_notifications, receiver) = tokio::sync::watch::channel(0_u64);
        let mut failed_lease = Some(McpRevocationLease::new(
            || Box::pin(async { Err(()) }),
            Duration::from_mins(1),
            receiver,
        ));
        let (sender, mut events) = mpsc::channel(1);
        assert!(
            !send_with_authorization(
                &sender,
                "retry: 1000\n\n".to_owned(),
                None,
                &mut failed_lease,
            )
            .await
        );

        let (_notifications, receiver) = tokio::sync::watch::channel(0_u64);
        let mut valid_lease = Some(McpRevocationLease::new(
            || Box::pin(async { Ok(true) }),
            Duration::from_mins(1),
            receiver,
        ));
        assert!(
            !send_with_authorization(
                &sender,
                "retry: 1000\n\n".to_owned(),
                Some(SystemTime::UNIX_EPOCH),
                &mut valid_lease,
            )
            .await
        );
        assert!(events.try_recv().is_err());
    }
}
