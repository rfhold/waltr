use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    num::NonZeroUsize,
    sync::{Arc, Mutex as StdMutex, RwLock},
    time::Duration,
};

use serde_json::Value;
use tokio::{
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
    time,
};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::{
    Error, McpAuthorizationError, McpTracePropagation, Result,
    client::transport::{
        McpBearerProviderError, McpFramedChannel, McpTransport, McpTransportConfig,
        McpTransportError, McpTransportOptions, TransportInbound,
        framed::{FramedInbound, FramedTransport},
    },
    protocol::{
        JsonRpcId, JsonRpcInbound, JsonRpcNotification, cancellation_params, json_rpc_notification,
        json_rpc_request, json_rpc_result, next_id,
    },
    trace::trace_propagation_name,
};

type PendingMap = BTreeMap<JsonRpcId, oneshot::Sender<Result<Value>>>;
type PendingRequests = StdMutex<PendingMap>;
type EventRoutes = Arc<RwLock<BTreeMap<JsonRpcId, (EventNotifications, CancellationToken)>>>;

#[derive(Clone)]
struct EventNotifications {
    sender: mpsc::Sender<Result<crate::McpEventMessage>>,
    activity: Arc<RwLock<time::Instant>>,
}

/// Handler for client-side inbound MCP messages.
pub trait McpConnectionHandler: Send + Sync {
    /// Handle a notification.
    fn notification(&self, notification: JsonRpcNotification);
    /// Handle a transport disconnection.
    fn disconnected(&self, error: String);
    /// Handle an authorization failure received from streamable HTTP.
    fn authorization_failed(&self, error: McpAuthorizationError);
    /// Handle a dynamic bearer provider failure.
    fn bearer_provider_failed(&self, error: McpBearerProviderError);
    /// Handle a classified background transport failure.
    fn transport_failed(&self, error: McpTransportError);
}

/// JSON-RPC connection over one MCP transport.
pub struct McpConnection {
    transport: ConnectionTransport,
    pending: Arc<PendingRequests>,
    event_routes: EventRoutes,
    dispatch_cancellation: CancellationToken,
    dispatch_task: Mutex<Option<JoinHandle<()>>>,
    active_subscriptions: Arc<RwLock<HashSet<JsonRpcId>>>,
}

#[allow(clippy::large_enum_variant)]
enum ConnectionTransport {
    Configured(McpTransport),
    Framed(FramedTransport),
}

enum ConnectionInbound {
    Configured(mpsc::UnboundedReceiver<TransportInbound>),
    Framed(FramedInbound),
}

impl ConnectionInbound {
    async fn recv(&mut self) -> Option<TransportInbound> {
        match self {
            Self::Configured(inbound) => inbound.recv().await,
            Self::Framed(inbound) => inbound.recv().await,
        }
    }
}

impl ConnectionTransport {
    const fn kind(&self) -> &'static str {
        match self {
            Self::Configured(transport) => transport.kind(),
            Self::Framed(_) => "framed",
        }
    }
}

impl fmt::Debug for McpConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpConnection")
            .finish_non_exhaustive()
    }
}

impl McpConnection {
    /// Start a connection with unified transport runtime options.
    pub async fn start_with_transport_options(
        server_name: String,
        transport: McpTransportConfig,
        handler: Arc<dyn McpConnectionHandler>,
        active_subscriptions: Arc<RwLock<HashSet<JsonRpcId>>>,
        options: McpTransportOptions,
    ) -> Result<Arc<Self>> {
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        let transport =
            McpTransport::start_with_options(server_name, transport, inbound_tx, options).await?;
        Ok(Self::finish_start(
            ConnectionTransport::Configured(transport),
            handler,
            active_subscriptions,
            ConnectionInbound::Configured(inbound_rx),
        ))
    }

    pub(crate) fn start_framed(
        channel: Arc<dyn McpFramedChannel>,
        max_message_bytes: NonZeroUsize,
        handler: Arc<dyn McpConnectionHandler>,
        active_subscriptions: Arc<RwLock<HashSet<JsonRpcId>>>,
    ) -> Arc<Self> {
        let (transport, inbound) = FramedTransport::new(channel, max_message_bytes);
        Self::finish_start(
            ConnectionTransport::Framed(transport),
            handler,
            active_subscriptions,
            ConnectionInbound::Framed(inbound),
        )
    }

    fn finish_start(
        transport: ConnectionTransport,
        handler: Arc<dyn McpConnectionHandler>,
        active_subscriptions: Arc<RwLock<HashSet<JsonRpcId>>>,
        inbound_rx: ConnectionInbound,
    ) -> Arc<Self> {
        let pending = Arc::new(PendingRequests::default());
        let dispatch_cancellation = CancellationToken::new();
        let event_routes = Arc::new(RwLock::new(BTreeMap::new()));
        let task = tokio::spawn(dispatch_loop(
            Arc::clone(&pending),
            Arc::clone(&event_routes),
            inbound_rx,
            handler,
            dispatch_cancellation.clone(),
        ));
        Arc::new(Self {
            transport,
            pending,
            event_routes,
            dispatch_cancellation,
            dispatch_task: Mutex::new(Some(task)),
            active_subscriptions,
        })
    }

    /// Send a request and wait for its response.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub(crate) async fn request_with_trace_propagation(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Option<Duration>,
        cancellation_token: CancellationToken,
        trace_propagation: McpTracePropagation,
        http_headers: Vec<(String, String)>,
    ) -> Result<Value> {
        let span = tracing::info_span!(
            "mcp.client.request",
            rpc.system = "jsonrpc",
            rpc.jsonrpc.version = "2.0",
            rpc.method = %method,
            mcp.method = %method,
            mcp.request_id = tracing::field::Empty,
            mcp.transport = self.transport.kind(),
            mcp.trace_propagation = trace_propagation_name(trace_propagation),
            mcp.outcome = tracing::field::Empty,
            rpc.jsonrpc.error_code = tracing::field::Empty,
        );
        async move {
            self.request_inner(
                method,
                params,
                timeout,
                cancellation_token,
                http_headers,
                None,
            )
            .await
        }
        .instrument(span)
        .await
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn request_inner(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Option<Duration>,
        cancellation_token: CancellationToken,
        http_headers: Vec<(String, String)>,
        event_sender: Option<EventNotifications>,
    ) -> Result<Value> {
        let span = tracing::Span::current();
        if cancellation_token.is_cancelled() {
            span.record("mcp.outcome", "cancelled");
            tracing::debug!(
                mcp.outcome = "cancelled",
                "MCP client request cancelled before send"
            );
            return Err(Error::protocol("MCP request canceled"));
        }
        let request_cancellation = cancellation_token.child_token();
        let deadline = timeout.map(|timeout| time::Instant::now() + timeout);
        let id = next_id();
        let _event_route = event_sender.map(|sender| {
            self.event_routes
                .write()
                .expect("event routes lock poisoned")
                .insert(id.clone(), (sender, request_cancellation.clone()));
            EventRoute {
                id: id.clone(),
                routes: Arc::clone(&self.event_routes),
            }
        });
        let _subscription = (method == "subscriptions/listen").then(|| {
            self.active_subscriptions
                .write()
                .expect("active subscription lock poisoned")
                .insert(id.clone());
            ActiveSubscription {
                id: id.clone(),
                active: Arc::clone(&self.active_subscriptions),
            }
        });
        span.record("mcp.request_id", id.to_string());
        let (tx, rx) = oneshot::channel();
        let mut event_cancellation = cancellation_params(&id, "Event stream canceled");
        if let Some(meta) = params.as_ref().and_then(|v| v.get("_meta")) {
            event_cancellation["_meta"] = meta.clone();
        }
        self.pending
            .lock()
            .expect("pending requests lock poisoned")
            .insert(id.clone(), tx);
        let pending = PendingRequest {
            requests: &self.pending,
            id: &id,
        };
        let send_result = tokio::select! {
            result = self.send(
                json_rpc_request(id.clone(), method, params),
                Some(&request_cancellation),
                &http_headers,
            ) => result,
            () = request_cancellation.cancelled() => Err(Error::protocol("MCP request canceled")),
            () = wait_for_deadline(deadline) => {
                request_cancellation.cancel();
                Err(Error::protocol("MCP request timed out"))
            }
        };
        if let Err(error) = send_result {
            drop(pending);
            if method == "events/stream" && request_cancellation.is_cancelled() {
                drop(
                    time::timeout(
                        Duration::from_secs(2),
                        self.notify("notifications/cancelled", Some(event_cancellation)),
                    )
                    .await,
                );
            }
            span.record("mcp.outcome", "error");
            tracing::debug!(%error, mcp.outcome = "error", "MCP client request send failed");
            return Err(error);
        }

        tokio::select! {
            biased;
            () = request_cancellation.cancelled() => {
                drop(pending);
                if method == "events/stream" {
                    drop(time::timeout(Duration::from_secs(2), self.notify("notifications/cancelled", Some(event_cancellation))).await);
                } else if matches!(self.transport, ConnectionTransport::Framed(_)) {
                    self.notify_framed_cancellation(&id, "MCP request canceled").await;
                } else if self.transport.kind() == "stdio" {
                    drop(self.notify("notifications/cancelled", Some(cancellation_params(&id, "Waltr request canceled"))).await);
                }
                span.record("mcp.outcome", "cancelled");
                tracing::debug!(mcp.outcome = "cancelled", "MCP client request cancelled while waiting");
                Err(Error::protocol("MCP request canceled"))
            }
            () = wait_for_deadline(deadline) => {
                request_cancellation.cancel();
                drop(pending);
                if matches!(self.transport, ConnectionTransport::Framed(_)) {
                    self.notify_framed_cancellation(&id, "MCP request timed out").await;
                } else if self.transport.kind() == "stdio" || method == "events/stream" {
                    drop(self.notify("notifications/cancelled", Some(cancellation_params(&id, "Waltr request timed out"))).await);
                }
                span.record("mcp.outcome", "timeout");
                tracing::debug!(mcp.outcome = "timeout", "MCP client request timed out");
                Err(Error::protocol("MCP request timed out"))
            }
            response = rx => {
                match response.map_err(|_| Error::protocol("MCP connection closed before response"))? {
                    Ok(value) => {
                        span.record("mcp.outcome", "success");
                        Ok(value)
                    }
                    Err(error) => {
                        span.record("mcp.outcome", "error");
                        if let Some(code) = json_rpc_error_code(&error) {
                            span.record("rpc.jsonrpc.error_code", code);
                            tracing::debug!(rpc.jsonrpc.error_code = code, mcp.outcome = "error", "MCP client request failed with JSON-RPC error");
                        } else {
                            tracing::debug!(%error, mcp.outcome = "error", "MCP client request failed");
                        }
                        Err(error)
                    }
                }
            },
            error = self.framed_backlog_overflowed() => {
                span.record("mcp.outcome", "error");
                Err(error)
            }
        }
    }

    /// Start a bounded event request with an independent idle deadline.
    pub(crate) fn event_stream(
        self: &Arc<Self>,
        params: Value,
        headers: Vec<(String, String)>,
        cancellation: &CancellationToken,
    ) -> crate::McpEventStream {
        let cancellation = cancellation.child_token();
        let (sender, receiver) = mpsc::channel(64);
        let (completion_tx, completion) = oneshot::channel();
        let connection = Arc::clone(self);
        let token = cancellation.clone();
        let cancellation_guard = token.clone().drop_guard();
        let activity = Arc::new(RwLock::new(time::Instant::now()));
        let task = tokio::spawn(async move {
            let _cancellation_guard = cancellation_guard;
            let request = connection.request_inner(
                "events/stream",
                Some(params),
                None,
                token.clone(),
                headers,
                Some(EventNotifications {
                    sender,
                    activity: Arc::clone(&activity),
                }),
            );
            tokio::pin!(request);
            let idle = async {
                loop {
                    let deadline = *activity.read().expect("event activity lock poisoned")
                        + Duration::from_mins(1);
                    time::sleep_until(deadline).await;
                    if time::Instant::now()
                        .duration_since(*activity.read().expect("event activity lock poisoned"))
                        >= Duration::from_mins(1)
                    {
                        break;
                    }
                }
            };
            let result = tokio::select! {
                biased;
                result = &mut request => result,
                () = idle => {
                    token.cancel();
                    drop(request.await);
                    Err(Error::protocol("MCP event stream idle for 60 seconds"))
                }
            };
            drop(completion_tx.send(result.and_then(|value| event_completion(&value))));
        });
        crate::McpEventStream {
            receiver,
            cancellation,
            task: Some(task),
            completion: Some(completion),
            finished: false,
        }
    }

    /// Send a notification.
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        self.send(json_rpc_notification(method, params), None, &[])
            .await
    }

    async fn notify_framed_cancellation(&self, id: &JsonRpcId, reason: &str) {
        drop(
            time::timeout(
                Duration::from_secs(2),
                self.notify(
                    "notifications/cancelled",
                    Some(cancellation_params(id, reason)),
                ),
            )
            .await,
        );
    }

    async fn send(
        &self,
        message: Value,
        cancellation: Option<&CancellationToken>,
        http_headers: &[(String, String)],
    ) -> Result<()> {
        match &self.transport {
            ConnectionTransport::Configured(transport) => {
                transport.send(message, cancellation, http_headers).await
            }
            ConnectionTransport::Framed(transport) => transport.send(message, cancellation).await,
        }
    }

    /// Wait for explicit shutdown or terminal transport closure.
    pub(crate) async fn closed(&self) {
        tokio::select! {
            () = self.dispatch_cancellation.cancelled() => {},
            _ = self.framed_backlog_overflowed() => {},
        }
    }

    async fn framed_backlog_overflowed(&self) -> Error {
        match &self.transport {
            ConnectionTransport::Framed(transport) => transport.backlog_overflowed().await,
            ConnectionTransport::Configured(_) => std::future::pending().await,
        }
    }

    /// Shutdown the underlying transport.
    pub async fn shutdown(&self) -> Result<()> {
        let result = match &self.transport {
            ConnectionTransport::Configured(transport) => transport.shutdown().await,
            ConnectionTransport::Framed(transport) => {
                self.dispatch_cancellation.cancel();
                fail_pending(&self.pending, "MCP connection shut down");
                transport.shutdown().await
            }
        };
        self.dispatch_cancellation.cancel();
        fail_pending(&self.pending, "MCP connection shut down");
        let task = {
            let mut dispatch_task = self.dispatch_task.lock().await;
            dispatch_task.take()
        };
        if let Some(task) = task {
            drop(task.await);
        }
        result
    }
}

fn event_completion(result: &Value) -> Result<()> {
    if !result.is_object() {
        return Err(Error::protocol("events/stream result must be an object"));
    }
    match result.get("resultType") {
        None => Ok(()),
        Some(Value::String(value)) if value == "complete" => Ok(()),
        Some(Value::String(value)) => Err(Error::protocol(format!(
            "unsupported events/stream resultType `{value}`"
        ))),
        Some(_) => Err(Error::protocol("events/stream resultType must be a string")),
    }
}

struct ActiveSubscription {
    id: JsonRpcId,
    active: Arc<RwLock<HashSet<JsonRpcId>>>,
}

struct PendingRequest<'a> {
    requests: &'a PendingRequests,
    id: &'a JsonRpcId,
}

impl Drop for PendingRequest<'_> {
    fn drop(&mut self) {
        remove_pending(self.requests, self.id);
    }
}

struct EventRoute {
    id: JsonRpcId,
    routes: EventRoutes,
}
impl Drop for EventRoute {
    fn drop(&mut self) {
        self.routes
            .write()
            .expect("event routes lock poisoned")
            .remove(&self.id);
    }
}

impl Drop for ActiveSubscription {
    fn drop(&mut self) {
        self.active
            .write()
            .expect("active subscription lock poisoned")
            .remove(&self.id);
    }
}

async fn wait_for_deadline(deadline: Option<time::Instant>) {
    match deadline {
        Some(deadline) => time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn json_rpc_error_code(error: &Error) -> Option<i64> {
    let message = error.to_string();
    message
        .strip_prefix("JSON-RPC error ")?
        .split(':')
        .next()?
        .parse()
        .ok()
}

async fn dispatch_loop(
    pending: Arc<PendingRequests>,
    event_routes: EventRoutes,
    mut inbound: ConnectionInbound,
    handler: Arc<dyn McpConnectionHandler>,
    cancellation: CancellationToken,
) {
    loop {
        let inbound = tokio::select! {
            () = cancellation.cancelled() => break,
            inbound = inbound.recv() => inbound,
        };
        let Some(inbound) = inbound else { break };
        match inbound {
            TransportInbound::Message(JsonRpcInbound::Response(response)) => {
                let sender = remove_pending(&pending, &response.id);
                if let Some(sender) = sender {
                    drop(sender.send(json_rpc_result(response)));
                }
            }
            TransportInbound::Message(JsonRpcInbound::Notification(notification)) => {
                if notification.method.starts_with("notifications/events/") {
                    let params = notification.params.clone().unwrap_or(Value::Null);
                    let id = params
                        .get("_meta")
                        .and_then(|v| v.get(crate::protocol::MCP_SUBSCRIPTION_ID_META_KEY))
                        .and_then(|v| serde_json::from_value::<JsonRpcId>(v.clone()).ok());
                    if let Some((sink, token)) = id.and_then(|id| {
                        event_routes
                            .read()
                            .expect("event routes lock poisoned")
                            .get(&id)
                            .cloned()
                    }) {
                        *sink.activity.write().expect("event activity lock poisoned") =
                            time::Instant::now();
                        let message = crate::McpEventMessage::parse(&notification.method, params);
                        let terminal = matches!(
                            &message,
                            Ok(crate::McpEventMessage::Terminated { .. }) | Err(_)
                        );
                        if sink.sender.try_send(message).is_err() || terminal {
                            token.cancel();
                        }
                    }
                    continue;
                }
                handler.notification(notification);
            }
            TransportInbound::Message(JsonRpcInbound::Request(request)) => {
                handler.disconnected(format!(
                    "server sent forbidden MCP request `{}`",
                    request.method
                ));
            }
            TransportInbound::AuthorizationFailed(error) => {
                fail_pending_with_authorization(&pending, &error);
                handler.authorization_failed(error);
            }
            TransportInbound::BearerProviderFailed(error) => {
                fail_pending_with_bearer_provider(&pending, error);
                handler.bearer_provider_failed(error);
            }
            TransportInbound::TransportFailed(error) => {
                fail_pending(&pending, &error.message);
                handler.transport_failed(error);
            }
            TransportInbound::RequestFailed(id, error) => {
                let sender = remove_pending(&pending, &id);
                if let Some(sender) = sender {
                    drop(sender.send(Err(Error::protocol(error))));
                }
            }
            TransportInbound::Disconnected(error) => {
                fail_pending(&pending, &error);
                handler.disconnected(error);
                cancellation.cancel();
            }
        }
    }
    cancellation.cancel();
}

fn remove_pending(
    pending: &PendingRequests,
    id: &JsonRpcId,
) -> Option<oneshot::Sender<Result<Value>>> {
    pending
        .lock()
        .expect("pending requests lock poisoned")
        .remove(id)
}

fn take_pending(pending: &PendingRequests) -> PendingMap {
    std::mem::take(&mut *pending.lock().expect("pending requests lock poisoned"))
}

fn fail_pending_with_bearer_provider(pending: &PendingRequests, error: McpBearerProviderError) {
    for sender in take_pending(pending).into_values() {
        drop(sender.send(Err(Error::BearerProvider(error))));
    }
}

fn fail_pending(pending: &PendingRequests, error: &str) {
    for sender in take_pending(pending).into_values() {
        drop(sender.send(Err(Error::protocol(error.to_owned()))));
    }
}

fn fail_pending_with_authorization(pending: &PendingRequests, error: &McpAuthorizationError) {
    for sender in take_pending(pending).into_values() {
        drop(sender.send(Err(Error::Authorization(Box::new(error.clone())))));
    }
}
