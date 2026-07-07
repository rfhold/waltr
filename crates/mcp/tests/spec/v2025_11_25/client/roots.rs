use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use mcp::{
    McpRoot,
    client::{McpClient, McpClientHandler, McpClientOptions, McpTransportConfig},
    protocol::MCP_PROTOCOL_VERSION,
};
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Mutex, time};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct RootsClient;

impl McpClientHandler for RootsClient {
    fn roots(&self) -> Vec<McpRoot> {
        vec![
            McpRoot::new(
                "file:///home/user/projects/myproject",
                Some("My Project".to_owned()),
            ),
            McpRoot::new("file:///home/user/repos/backend", None),
        ]
    }
}

#[derive(Clone, Default)]
struct RootsPeerState {
    messages: Arc<Mutex<Vec<Value>>>,
    request: PeerRequest,
}

#[derive(Clone, Copy, Default)]
enum PeerRequest {
    #[default]
    RootsList,
    Ping,
}

#[tokio::test]
async fn client_declares_roots_capability_during_initialization() -> mcp::Result<()> {
    let peer = RootsPeer::start().await?;

    let client = connect_roots_client(peer.endpoint()).await?;

    let messages = peer.messages().await;
    let initialize = messages
        .iter()
        .find(|message| message["method"] == "initialize")
        .expect("initialize request captured");
    assert_eq!(initialize["jsonrpc"], "2.0");
    assert_eq!(
        initialize["params"]["protocolVersion"],
        MCP_PROTOCOL_VERSION
    );
    assert_eq!(
        initialize["params"]["capabilities"],
        json!({ "roots": { "listChanged": false } })
    );

    client.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn roots_list_request_returns_exposed_file_uri_roots() -> mcp::Result<()> {
    let peer = RootsPeer::start().await?;

    let client = connect_roots_client(peer.endpoint()).await?;
    let messages = peer.wait_for_messages(3).await;
    let response = messages
        .iter()
        .find(|message| message["id"] == "roots-list-1" && message.get("result").is_some())
        .expect("roots/list response captured");

    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(
        response["result"],
        json!({
            "roots": [
                {
                    "uri": "file:///home/user/projects/myproject",
                    "name": "My Project",
                },
                {
                    "uri": "file:///home/user/repos/backend",
                },
            ],
        })
    );
    assert!(response.get("error").is_none());

    client.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn client_ping_request_returns_empty_result_object() -> mcp::Result<()> {
    let peer = RootsPeer::start_with(PeerRequest::Ping).await?;

    let client = connect_roots_client(peer.endpoint()).await?;
    let messages = peer.wait_for_messages(3).await;
    let response = messages
        .iter()
        .find(|message| message["id"] == "ping-1" && message.get("result").is_some())
        .expect("ping response captured");

    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["result"], json!({}));
    assert!(response.get("error").is_none());

    client.shutdown().await;
    Ok(())
}

async fn connect_roots_client(endpoint: String) -> mcp::Result<McpClient> {
    McpClient::connect(
        "mcp-spec-roots-server",
        McpTransportConfig::StreamableHttp { endpoint },
        Arc::new(RootsClient),
        McpClientOptions::new("mcp-spec-roots-client", "1.0.0", Duration::from_secs(5)),
        CancellationToken::new(),
    )
    .await
}

struct RootsPeer {
    endpoint: String,
    state: RootsPeerState,
}

impl RootsPeer {
    async fn start() -> mcp::Result<Self> {
        Self::start_with(PeerRequest::RootsList).await
    }

    async fn start_with(request: PeerRequest) -> mcp::Result<Self> {
        let state = RootsPeerState {
            request,
            ..RootsPeerState::default()
        };
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let router = Router::new()
            .route("/mcp", post(roots_peer_post))
            .with_state(state.clone());
        tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router).await {
                tracing::debug!(%error, "roots spec peer stopped");
            }
        });

        Ok(Self {
            endpoint: format!("http://{address}/mcp"),
            state,
        })
    }

    fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    async fn messages(&self) -> Vec<Value> {
        self.state.messages.lock().await.clone()
    }

    async fn wait_for_messages(&self, count: usize) -> Vec<Value> {
        time::timeout(Duration::from_secs(5), async {
            loop {
                let messages = self.messages().await;
                if messages.len() >= count {
                    return messages;
                }
                time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for roots peer messages")
    }
}

async fn roots_peer_post(
    State(state): State<RootsPeerState>,
    axum::Json(message): axum::Json<Value>,
) -> Response {
    state.messages.lock().await.push(message.clone());
    let mut headers = HeaderMap::new();
    headers.insert(
        "mcp-session-id",
        HeaderValue::from_static("roots-spec-session"),
    );
    headers.insert(
        "mcp-protocol-version",
        HeaderValue::from_static(MCP_PROTOCOL_VERSION),
    );

    match message.get("method").and_then(Value::as_str) {
        Some("initialize") => {
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            (
                StatusCode::OK,
                headers,
                axum::Json(json!({
                    "jsonrpc": "2.0",
                    "id": message["id"].clone(),
                    "result": {
                        "protocolVersion": MCP_PROTOCOL_VERSION,
                        "capabilities": {},
                        "serverInfo": {
                            "name": "mcp-spec-roots-server",
                            "version": "1.0.0",
                        },
                    },
                })),
            )
                .into_response()
        }
        Some("notifications/initialized") => {
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            let (id, method) = match state.request {
                PeerRequest::RootsList => ("roots-list-1", "roots/list"),
                PeerRequest::Ping => ("ping-1", "ping"),
            };
            (
                StatusCode::OK,
                headers,
                axum::Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": method,
                })),
            )
                .into_response()
        }
        _ => (StatusCode::ACCEPTED, headers).into_response(),
    }
}
