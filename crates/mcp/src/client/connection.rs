use std::{collections::BTreeMap, fmt, sync::Arc, time::Duration};

use serde_json::Value;
use tokio::{
    sync::{Mutex, mpsc, oneshot},
    time,
};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::{
    Error, McpTracePropagation, Result,
    client::transport::{McpTransport, McpTransportConfig, TransportInbound},
    protocol::{
        JsonRpcId, JsonRpcInbound, JsonRpcNotification, JsonRpcRequest, cancellation_params,
        json_rpc_notification, json_rpc_request, json_rpc_result, next_id,
    },
    trace::trace_propagation_name,
};

type PendingMap = BTreeMap<JsonRpcId, oneshot::Sender<Result<Value>>>;

/// Handler for client-side inbound MCP messages.
pub trait McpConnectionHandler: Send + Sync {
    /// Handle a notification.
    fn notification(&self, notification: JsonRpcNotification);
    /// Handle a request and return a JSON-RPC response value.
    fn request(&self, request: JsonRpcRequest) -> Value;
    /// Handle a transport disconnection.
    fn disconnected(&self, error: String);
    /// Handle an SSE event id received from streamable HTTP.
    fn sse_event_id(&self, id: String);
}

/// JSON-RPC connection over one MCP transport.
pub struct McpConnection {
    transport: McpTransport,
    pending: Arc<Mutex<PendingMap>>,
    initialized: Arc<Mutex<bool>>,
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
    ) -> Result<Arc<Self>> {
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        let transport = McpTransport::start(server_name, transport, inbound_tx).await?;
        let connection = Arc::new(Self {
            transport,
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            initialized: Arc::new(Mutex::new(false)),
        });
        tokio::spawn(dispatch_loop(Arc::clone(&connection), inbound_rx, handler));
        Ok(connection)
    }

    /// Mark the connection initialized so HTTP sends the MCP protocol header.
    pub async fn mark_initialized(&self) {
        *self.initialized.lock().await = true;
    }

    /// Send a request and wait for its response.
    pub(crate) async fn request_with_trace_propagation(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
        cancellation_token: CancellationToken,
        trace_propagation: McpTracePropagation,
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
            self.request_inner(method, params, timeout, cancellation_token)
                .await
        }
        .instrument(span)
        .await
    }

    async fn request_inner(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
        cancellation_token: CancellationToken,
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
        let id = next_id();
        span.record("mcp.request_id", id.to_string());
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        if let Err(error) = self
            .send(json_rpc_request(id.clone(), method, params))
            .await
        {
            self.pending.lock().await.remove(&id);
            span.record("mcp.outcome", "error");
            tracing::debug!(%error, mcp.outcome = "error", "MCP client request send failed");
            return Err(error);
        }

        tokio::select! {
            biased;
            () = cancellation_token.cancelled() => {
                self.pending.lock().await.remove(&id);
                if method != "initialize" {
                    drop(self.notify("notifications/cancelled", Some(cancellation_params(&id, "Waltr request canceled"))).await);
                }
                span.record("mcp.outcome", "cancelled");
                tracing::debug!(mcp.outcome = "cancelled", "MCP client request cancelled while waiting");
                Err(Error::protocol("MCP request canceled"))
            }
            () = time::sleep(timeout) => {
                self.pending.lock().await.remove(&id);
                if method != "initialize" {
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
        self.send(json_rpc_notification(method, params)).await
    }

    async fn send(&self, message: Value) -> Result<()> {
        let initialized = *self.initialized.lock().await;
        self.transport.send(message, initialized).await
    }

    /// Shutdown the underlying transport.
    pub async fn shutdown(&self) {
        self.transport.shutdown().await;
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
    connection: Arc<McpConnection>,
    mut inbound: mpsc::UnboundedReceiver<TransportInbound>,
    handler: Arc<dyn McpConnectionHandler>,
) {
    while let Some(inbound) = inbound.recv().await {
        match inbound {
            TransportInbound::Message(JsonRpcInbound::Response(response)) => {
                let sender = connection.pending.lock().await.remove(&response.id);
                if let Some(sender) = sender {
                    drop(sender.send(json_rpc_result(response)));
                }
            }
            TransportInbound::Message(JsonRpcInbound::Notification(notification)) => {
                handler.notification(notification);
            }
            TransportInbound::Message(JsonRpcInbound::Request(request)) => {
                let response = handler.request(request.clone());
                if let Err(error) = connection.send(response).await {
                    handler.disconnected(format!("failed to send MCP response: {error}"));
                }
            }
            TransportInbound::SseEventId(id) => handler.sse_event_id(id),
            TransportInbound::Disconnected(error) => {
                fail_pending(&connection, &error).await;
                handler.disconnected(error);
            }
        }
    }
}

async fn fail_pending(connection: &McpConnection, error: &str) {
    let pending = std::mem::take(&mut *connection.pending.lock().await);
    for sender in pending.into_values() {
        drop(sender.send(Err(Error::protocol(error.to_owned()))));
    }
}
