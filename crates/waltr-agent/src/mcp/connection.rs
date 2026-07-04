use std::{collections::BTreeMap, sync::Arc, time::Duration};

use serde_json::Value;
use tokio::{
    sync::{Mutex, mpsc, oneshot},
    time,
};
use tokio_util::sync::CancellationToken;

use crate::Result;

use super::{
    config::McpTransportConfig,
    protocol::{
        JsonRpcId, JsonRpcInbound, JsonRpcNotification, JsonRpcRequest, cancellation_params,
        json_rpc_method_not_found, json_rpc_notification, json_rpc_request, json_rpc_result,
        json_rpc_success, next_id, protocol_error,
    },
    transport::{McpTransport, TransportInbound},
};

type PendingMap = BTreeMap<JsonRpcId, oneshot::Sender<Result<Value>>>;

pub(in crate::mcp) trait McpConnectionHandler: Send + Sync {
    fn notification(&self, notification: JsonRpcNotification);
    fn request(&self, request: JsonRpcRequest) -> Value;
    fn disconnected(&self, error: String);
    fn sse_event_id(&self, id: String);
}

pub(in crate::mcp) struct McpConnection {
    transport: McpTransport,
    pending: Arc<Mutex<PendingMap>>,
    initialized: Arc<Mutex<bool>>,
}

impl McpConnection {
    pub(in crate::mcp) async fn start(
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

    pub(in crate::mcp) async fn mark_initialized(&self) {
        *self.initialized.lock().await = true;
    }

    pub(in crate::mcp) async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
        cancellation_token: CancellationToken,
    ) -> Result<Value> {
        if cancellation_token.is_cancelled() {
            return Err(protocol_error("MCP request canceled"));
        }
        let id = next_id();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        self.send(json_rpc_request(id.clone(), method, params))
            .await?;

        let response = tokio::select! {
            biased;
            () = cancellation_token.cancelled() => {
                self.pending.lock().await.remove(&id);
                if method != "initialize" {
                    drop(
                        self.notify(
                            "notifications/cancelled",
                            Some(cancellation_params(&id, "Waltr request canceled")),
                        )
                        .await,
                    );
                }
                return Err(protocol_error("MCP request canceled"));
            }
            () = time::sleep(timeout) => {
                self.pending.lock().await.remove(&id);
                if method != "initialize" {
                    drop(
                        self.notify(
                            "notifications/cancelled",
                            Some(cancellation_params(&id, "Waltr request timed out")),
                        )
                        .await,
                    );
                }
                return Err(protocol_error("MCP request timed out"));
            }
            response = rx => response.map_err(|_| protocol_error("MCP connection closed before response"))?,
        };
        response
    }

    pub(in crate::mcp) async fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        self.send(json_rpc_notification(method, params)).await
    }

    async fn send(&self, message: Value) -> Result<()> {
        let initialized = *self.initialized.lock().await;
        self.transport.send(message, initialized).await
    }

    pub(in crate::mcp) async fn shutdown(&self) {
        self.transport.shutdown().await;
    }
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
        drop(sender.send(Err(protocol_error(error.to_owned()))));
    }
}

pub(in crate::mcp) fn roots_response(request: JsonRpcRequest, root_uri: &str) -> Value {
    if request.method == "roots/list" {
        json_rpc_success(
            request.id,
            serde_json::json!({
                "roots": [{
                    "uri": root_uri,
                    "name": "workspace"
                }]
            }),
        )
    } else {
        json_rpc_method_not_found(request.id, &request.method)
    }
}
