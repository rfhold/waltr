use std::{
    collections::HashMap,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::{Mutex, mpsc},
};
use tokio_stream as stream;
use tracing::Instrument as _;
use uuid::Uuid;

use crate::{
    Error, Result,
    protocol::{
        JsonRpcId, JsonRpcInbound, JsonRpcNotification, JsonRpcRequest, MCP_PROTOCOL_VERSION,
        json_rpc_error, json_rpc_method_not_found, json_rpc_notification, json_rpc_success,
        parse_json_rpc_message, parse_json_rpc_str,
    },
    trace::{request_meta, set_span_parent_from_meta},
    types::{
        McpCompletionRequest, McpPromptList, McpPromptResult, McpResourceList, McpResourceResult,
        McpResourceTemplateList, McpToolCall, McpToolList, McpToolResult, require_string,
    },
};

/// Boxed MCP server future.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Server implementation metadata.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    /// Server name.
    pub name: String,
    /// Server version.
    pub version: String,
}

impl ServerInfo {
    /// Create server metadata.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
        }
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
}

impl std::fmt::Debug for ServerContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerContext")
            .field("request_id", &self.request_id)
            .field(
                "request_meta_keys",
                &self.request_meta.as_ref().map(serde_json::Map::len),
            )
            .finish()
    }
}

impl ServerContext {
    /// Return the current request id, if this context belongs to a request.
    #[must_use]
    pub fn request_id(&self) -> Option<&JsonRpcId> {
        self.request_id.as_ref()
    }

    /// Return request `_meta`, if present and object-shaped.
    #[must_use]
    pub fn request_meta(&self) -> Option<&serde_json::Map<String, Value>> {
        self.request_meta.as_ref()
    }

    /// Return one value from request `_meta`.
    #[must_use]
    pub fn meta_value(&self, key: &str) -> Option<&Value> {
        self.request_meta.as_ref().and_then(|meta| meta.get(key))
    }

    /// Send a notification to the peer.
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        self.sender
            .send(json_rpc_notification(method, params))
            .await
    }

    /// Send a logging notification.
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
pub async fn serve_stdio(handler: Arc<dyn ServerHandler>) -> Result<()> {
    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
    let context = ServerContext {
        sender: NotificationSender::Stdio(Arc::clone(&stdout)),
        request_id: None,
        request_meta: None,
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
#[must_use]
pub fn streamable_http_router(handler: Arc<dyn ServerHandler>) -> Router {
    Router::new()
        .route("/mcp", post(http_post))
        .with_state(HttpState {
            handler,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        })
}

/// Serve Streamable HTTP MCP at `/mcp` on the supplied listener.
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
    sessions: Arc<Mutex<HashMap<String, HttpSessionState>>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum HttpSessionState {
    #[default]
    Uninitialized,
    InitializeResponded,
    Initialized,
}

async fn http_post(
    State(state): State<HttpState>,
    headers: HeaderMap,
    axum::Json(value): axum::Json<Value>,
) -> Response {
    if !accepts_streamable_http_post(&headers) {
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
    if let Some(version) = headers
        .get("mcp-protocol-version")
        .and_then(|value| value.to_str().ok())
        && version != MCP_PROTOCOL_VERSION
    {
        tracing::warn!(
            mcp.transport = "streamable_http",
            mcp.protocol_version = %version,
            "MCP HTTP request rejected due to unsupported protocol version"
        );
        return (StatusCode::BAD_REQUEST, "unsupported MCP protocol version").into_response();
    }
    let session_id = headers
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    let (tx, rx) = mpsc::channel(16);
    let context = ServerContext {
        sender: NotificationSender::Channel(tx),
        request_id: None,
        request_meta: None,
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
    let span = http_server_request_span(&inbound, &session_id);
    set_span_parent_from_meta(&span, inbound_meta(&inbound));
    async move {
        let lifecycle_error = lifecycle_error(&state, &session_id, &inbound).await;
        if let Some(response) = lifecycle_error {
            tracing::Span::current().record("mcp.outcome", "error");
            record_response_error(&response);
            tracing::debug!(mcp.outcome = "error", "MCP HTTP lifecycle rejected request");
            let mut headers = HeaderMap::new();
            if let Ok(value) = HeaderValue::from_str(&session_id) {
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
            return (StatusCode::OK, headers, axum::Json(response)).into_response();
        }
        let response = match inbound {
            JsonRpcInbound::Request(request) => {
                let is_initialize = request.method == "initialize";
                let mut context = context;
                context.request_id = Some(request.id.clone());
                context.request_meta = request_meta(request.params.as_ref()).cloned();
                let response = handle_request(state.handler, request, context).await;
                if is_initialize {
                    state
                        .sessions
                        .lock()
                        .await
                        .insert(session_id.clone(), HttpSessionState::InitializeResponded);
                }
                Some(response)
            }
            JsonRpcInbound::Notification(notification) => {
                if notification.method == "notifications/initialized" {
                    state
                        .sessions
                        .lock()
                        .await
                        .insert(session_id.clone(), HttpSessionState::Initialized);
                }
                state
                    .handler
                    .handle_notification(notification, context)
                    .await;
                None
            }
            JsonRpcInbound::Response(_) => None,
        };

        let mut notifications = Vec::new();
        let mut rx = rx;
        rx.close();
        while let Some(notification) = rx.recv().await {
            notifications.push(notification);
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
            let id = next_sse_id();
            let data = serde_json::to_string(&message).expect("json-rpc message serializes");
            Ok::<_, std::convert::Infallible>(format!("id: {id}\ndata: {data}\n\n"))
        }));
        (StatusCode::OK, headers, Body::from_stream(stream)).into_response()
    }
    .instrument(span)
    .await
}

async fn lifecycle_error(
    state: &HttpState,
    session_id: &str,
    inbound: &JsonRpcInbound,
) -> Option<Value> {
    match inbound {
        JsonRpcInbound::Request(request) => {
            let session_state = state
                .sessions
                .lock()
                .await
                .get(session_id)
                .copied()
                .unwrap_or_default();
            match request.method.as_str() {
                "initialize" if session_state != HttpSessionState::Uninitialized => {
                    Some(json_rpc_error(
                        request.id.clone(),
                        -32600,
                        "MCP session is already initialized",
                    ))
                }
                "initialize" | "ping" => None,
                _ if session_state == HttpSessionState::Initialized => None,
                _ => Some(json_rpc_error(
                    request.id.clone(),
                    -32600,
                    "MCP session has not completed initialization",
                )),
            }
        }
        JsonRpcInbound::Notification(notification)
            if notification.method == "notifications/initialized" =>
        {
            None
        }
        JsonRpcInbound::Notification(_) | JsonRpcInbound::Response(_) => None,
    }
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

async fn handle_request(
    handler: Arc<dyn ServerHandler>,
    request: JsonRpcRequest,
    context: ServerContext,
) -> Value {
    match request.method.as_str() {
        "initialize" => {
            let mut result = json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": handler.capabilities().to_value(),
                "serverInfo": {
                    "name": handler.server_info().name,
                    "version": handler.server_info().version,
                }
            });
            if let Some(instructions) = handler
                .instructions()
                .filter(|instructions| !instructions.trim().is_empty())
            {
                result["instructions"] = Value::String(instructions);
            }
            json_rpc_success(request.id, result)
        }
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
                        })
                        .map(|value| value),
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
        request.context_arguments = arguments.clone();
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
    Ok(())
}

fn next_sse_id() -> u64 {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Convenience fallback for handlers.
#[must_use]
pub fn method_not_found(request: JsonRpcRequest) -> Value {
    json_rpc_method_not_found(request.id, &request.method)
}
