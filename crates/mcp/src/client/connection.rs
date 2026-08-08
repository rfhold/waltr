use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    sync::{Arc, RwLock},
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
        McpBearerProvider, McpBearerProviderError, McpTransport, McpTransportConfig,
        McpTransportError, TransportInbound,
    },
    protocol::{
        JsonRpcId, JsonRpcInbound, JsonRpcNotification, cancellation_params, json_rpc_notification,
        json_rpc_request, json_rpc_result, next_id,
    },
    trace::trace_propagation_name,
};

type PendingMap = BTreeMap<JsonRpcId, oneshot::Sender<Result<Value>>>;

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
    transport: McpTransport,
    pending: Arc<Mutex<PendingMap>>,
    dispatch_cancellation: CancellationToken,
    dispatch_task: Mutex<Option<JoinHandle<()>>>,
    active_subscriptions: Arc<RwLock<HashSet<JsonRpcId>>>,
}

impl fmt::Debug for McpConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpConnection")
            .finish_non_exhaustive()
    }
}

impl McpConnection {
    /// Start a connection.
    pub async fn start(
        server_name: String,
        transport: McpTransportConfig,
        handler: Arc<dyn McpConnectionHandler>,
        active_subscriptions: Arc<RwLock<HashSet<JsonRpcId>>>,
    ) -> Result<Arc<Self>> {
        Self::start_with_optional_bearer_provider(
            server_name,
            transport,
            handler,
            active_subscriptions,
            None,
        )
        .await
    }

    /// Start a connection with a dynamic Streamable HTTP bearer provider.
    pub async fn start_with_bearer_provider(
        server_name: String,
        transport: McpTransportConfig,
        handler: Arc<dyn McpConnectionHandler>,
        active_subscriptions: Arc<RwLock<HashSet<JsonRpcId>>>,
        bearer_provider: Arc<dyn McpBearerProvider>,
    ) -> Result<Arc<Self>> {
        Self::start_with_optional_bearer_provider(
            server_name,
            transport,
            handler,
            active_subscriptions,
            Some(bearer_provider),
        )
        .await
    }

    async fn start_with_optional_bearer_provider(
        server_name: String,
        transport: McpTransportConfig,
        handler: Arc<dyn McpConnectionHandler>,
        active_subscriptions: Arc<RwLock<HashSet<JsonRpcId>>>,
        bearer_provider: Option<Arc<dyn McpBearerProvider>>,
    ) -> Result<Arc<Self>> {
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        let transport = match bearer_provider {
            Some(provider) => {
                McpTransport::start_with_bearer_provider(
                    server_name,
                    transport,
                    inbound_tx,
                    provider,
                )
                .await?
            }
            None => McpTransport::start(server_name, transport, inbound_tx).await?,
        };
        let pending = Arc::new(Mutex::new(BTreeMap::new()));
        let dispatch_cancellation = CancellationToken::new();
        let connection = Arc::new(Self {
            transport,
            pending: Arc::clone(&pending),
            dispatch_cancellation: dispatch_cancellation.clone(),
            dispatch_task: Mutex::new(None),
            active_subscriptions,
        });
        let task = tokio::spawn(dispatch_loop(
            pending,
            inbound_rx,
            handler,
            dispatch_cancellation,
        ));
        *connection.dispatch_task.lock().await = Some(task);
        Ok(connection)
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
            self.request_inner(method, params, timeout, cancellation_token, http_headers)
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
        self.pending.lock().await.insert(id.clone(), tx);
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
            self.pending.lock().await.remove(&id);
            span.record("mcp.outcome", "error");
            tracing::debug!(%error, mcp.outcome = "error", "MCP client request send failed");
            return Err(error);
        }

        tokio::select! {
            biased;
            () = request_cancellation.cancelled() => {
                self.pending.lock().await.remove(&id);
                if self.transport.kind() == "stdio" {
                    drop(self.notify("notifications/cancelled", Some(cancellation_params(&id, "Waltr request canceled"))).await);
                }
                span.record("mcp.outcome", "cancelled");
                tracing::debug!(mcp.outcome = "cancelled", "MCP client request cancelled while waiting");
                Err(Error::protocol("MCP request canceled"))
            }
            () = wait_for_deadline(deadline) => {
                request_cancellation.cancel();
                self.pending.lock().await.remove(&id);
                if self.transport.kind() == "stdio" {
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
        }
    }

    /// Send a notification.
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        self.send(json_rpc_notification(method, params), None, &[])
            .await
    }

    async fn send(
        &self,
        message: Value,
        cancellation: Option<&CancellationToken>,
        http_headers: &[(String, String)],
    ) -> Result<()> {
        self.transport
            .send(message, cancellation, http_headers)
            .await
    }

    /// Shutdown the underlying transport.
    pub async fn shutdown(&self) -> Result<()> {
        let result = self.transport.shutdown().await;
        self.dispatch_cancellation.cancel();
        fail_pending(&self.pending, "MCP connection shut down").await;
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

struct ActiveSubscription {
    id: JsonRpcId,
    active: Arc<RwLock<HashSet<JsonRpcId>>>,
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
    pending: Arc<Mutex<PendingMap>>,
    mut inbound: mpsc::UnboundedReceiver<TransportInbound>,
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
                let sender = pending.lock().await.remove(&response.id);
                if let Some(sender) = sender {
                    drop(sender.send(json_rpc_result(response)));
                }
            }
            TransportInbound::Message(JsonRpcInbound::Notification(notification)) => {
                handler.notification(notification);
            }
            TransportInbound::Message(JsonRpcInbound::Request(request)) => {
                handler.disconnected(format!(
                    "server sent forbidden MCP request `{}`",
                    request.method
                ));
            }
            TransportInbound::AuthorizationFailed(error) => {
                fail_pending_with_authorization(&pending, &error).await;
                handler.authorization_failed(error);
            }
            TransportInbound::BearerProviderFailed(error) => {
                fail_pending_with_bearer_provider(&pending, error).await;
                handler.bearer_provider_failed(error);
            }
            TransportInbound::TransportFailed(error) => {
                fail_pending(&pending, &error.message).await;
                handler.transport_failed(error);
            }
            TransportInbound::RequestFailed(id, error) => {
                let sender = {
                    let mut pending = pending.lock().await;
                    pending.remove(&id)
                };
                if let Some(sender) = sender {
                    drop(sender.send(Err(Error::protocol(error))));
                }
            }
            TransportInbound::Disconnected(error) => {
                fail_pending(&pending, &error).await;
                handler.disconnected(error);
            }
        }
    }
}

async fn fail_pending_with_bearer_provider(
    pending: &Arc<Mutex<PendingMap>>,
    error: McpBearerProviderError,
) {
    let requests = std::mem::take(&mut *pending.lock().await);
    for sender in requests.into_values() {
        drop(sender.send(Err(Error::BearerProvider(error))));
    }
}

async fn fail_pending(pending: &Arc<Mutex<PendingMap>>, error: &str) {
    let requests = std::mem::take(&mut *pending.lock().await);
    for sender in requests.into_values() {
        drop(sender.send(Err(Error::protocol(error.to_owned()))));
    }
}

async fn fail_pending_with_authorization(
    pending: &Arc<Mutex<PendingMap>>,
    error: &McpAuthorizationError,
) {
    let requests = std::mem::take(&mut *pending.lock().await);
    for sender in requests.into_values() {
        drop(sender.send(Err(Error::Authorization(Box::new(error.clone())))));
    }
}
