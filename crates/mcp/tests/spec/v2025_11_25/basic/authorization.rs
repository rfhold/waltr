use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use mcp::{
    Error, McpAccessToken, McpAuthorizationError, McpBearerChallenge, McpProtectedResourceMetadata,
    McpTokenValidation,
    client::{McpClient, McpClientHandler, McpClientOptions, McpTransportConfig},
    discover_protected_resource_metadata,
    oauth::{authorization_server_metadata_urls, protected_resource_metadata_urls},
    server::{
        BoxFuture, McpAuthorizationContext, ServerCapabilities, ServerContext, ServerHandler,
        ServerInfo, StreamableHttpAuthorization, StreamableHttpOptions,
    },
};
use reqwest::StatusCode;
use serde_json::json;
use tokio::{net::TcpListener, sync::Mutex, sync::mpsc};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct AuthorizationServer;

impl ServerHandler for AuthorizationServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("mcp-authorization-spec-server", "1.0.0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new()
    }

    fn handle_notification(
        &self,
        _notification: mcp::protocol::JsonRpcNotification,
        _context: ServerContext,
    ) -> BoxFuture<()> {
        Box::pin(async {})
    }
}

#[tokio::test]
async fn protected_resource_metadata_is_served_from_well_known_path() -> mcp::Result<()> {
    let server =
        AuthorizedSpecServer::start(|_, _| Box::pin(async { McpTokenValidation::Authorized }))
            .await?;

    let response = reqwest::Client::new()
        .get(format!(
            "{}/.well-known/oauth-protected-resource/mcp",
            server.origin
        ))
        .send()
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let metadata = response.json::<McpProtectedResourceMetadata>().await?;
    assert_eq!(metadata.resource, server.endpoint);
    assert_eq!(metadata.authorization_servers, ["https://auth.example.com"]);
    assert_eq!(metadata.scopes_supported, ["files:read"]);
    assert_eq!(metadata.bearer_methods_supported, ["header"]);
    Ok(())
}

#[tokio::test]
async fn unauthenticated_http_request_gets_bearer_challenge() -> mcp::Result<()> {
    let server =
        AuthorizedSpecServer::start(|_, _| Box::pin(async { McpTokenValidation::Authorized }))
            .await?;

    let response = initialize_request(&server.endpoint, None).await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let challenge = bearer_challenge(&response);
    assert_eq!(challenge.scope.as_deref(), Some("files:read"));
    assert_eq!(
        challenge.resource_metadata.as_deref(),
        Some(format!("{}/.well-known/oauth-protected-resource/mcp", server.origin).as_str())
    );
    Ok(())
}

#[tokio::test]
async fn invalid_token_gets_unauthorized_invalid_token_challenge() -> mcp::Result<()> {
    let server = AuthorizedSpecServer::start(|_, _| {
        Box::pin(async {
            McpTokenValidation::Unauthorized {
                error_description: Some("expired".to_owned()),
            }
        })
    })
    .await?;

    let response = initialize_request(&server.endpoint, Some("expired-token")).await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let challenge = bearer_challenge(&response);
    assert_eq!(challenge.error.as_deref(), Some("invalid_token"));
    assert_eq!(challenge.error_description.as_deref(), Some("expired"));
    Ok(())
}

#[tokio::test]
async fn insufficient_scope_gets_forbidden_scope_challenge() -> mcp::Result<()> {
    let server = AuthorizedSpecServer::start(|_, _| {
        Box::pin(async {
            McpTokenValidation::InsufficientScope {
                required_scopes: vec!["files:read".to_owned(), "files:write".to_owned()],
                error_description: Some("write required".to_owned()),
            }
        })
    })
    .await?;

    let response = initialize_request(&server.endpoint, Some("read-only-token")).await?;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let challenge = bearer_challenge(&response);
    assert_eq!(challenge.error.as_deref(), Some("insufficient_scope"));
    assert_eq!(challenge.scope.as_deref(), Some("files:read files:write"));
    assert_eq!(
        challenge.error_description.as_deref(),
        Some("write required")
    );
    Ok(())
}

#[tokio::test]
async fn valid_token_allows_normal_initialize() -> mcp::Result<()> {
    let server = AuthorizedSpecServer::start(|token, context| {
        Box::pin(async move {
            assert_eq!(token.0, "valid-token");
            assert_eq!(context.required_scopes, ["files:read"]);
            McpTokenValidation::Authorized
        })
    })
    .await?;

    let response = initialize_request(&server.endpoint, Some("valid-token")).await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<serde_json::Value>().await?["result"]["protocolVersion"],
        "2025-11-25"
    );
    Ok(())
}

#[tokio::test]
async fn mixed_case_authorization_scheme_is_accepted() -> mcp::Result<()> {
    let server = AuthorizedSpecServer::start(|token, _| {
        Box::pin(async move {
            assert_eq!(token.0, "valid-token");
            McpTokenValidation::Authorized
        })
    })
    .await?;

    let response =
        initialize_request_with_authorization(&server.endpoint, Some("bEaReR valid-token")).await?;
    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn client_sends_bearer_token_on_streamable_http_requests() -> mcp::Result<()> {
    let seen_tokens = Arc::new(Mutex::new(Vec::new()));
    let server_tokens = Arc::clone(&seen_tokens);
    let server = AuthorizedSpecServer::start(move |token, _| {
        let seen_tokens = Arc::clone(&server_tokens);
        Box::pin(async move {
            seen_tokens.lock().await.push(token.0);
            McpTokenValidation::Authorized
        })
    })
    .await?;

    let client = McpClient::connect(
        "mcp-auth-spec-server",
        McpTransportConfig::StreamableHttp {
            endpoint: server.endpoint,
            bearer_token: Some("client-token".to_owned()),
        },
        Arc::new(AuthorizationClient),
        McpClientOptions::new("mcp-auth-spec-client", "1.0.0", Duration::from_secs(5)),
        CancellationToken::new(),
    )
    .await?;

    client.shutdown().await;
    assert!(
        seen_tokens
            .lock()
            .await
            .iter()
            .any(|token| token == "client-token")
    );
    Ok(())
}

#[tokio::test]
async fn client_returns_structured_authorization_error() -> mcp::Result<()> {
    let server = AuthorizedSpecServer::start(|_, _| {
        Box::pin(async {
            McpTokenValidation::Unauthorized {
                error_description: Some("bad token".to_owned()),
            }
        })
    })
    .await?;

    let error = McpClient::connect(
        "mcp-auth-spec-server",
        McpTransportConfig::StreamableHttp {
            endpoint: server.endpoint,
            bearer_token: Some("bad-token".to_owned()),
        },
        Arc::new(AuthorizationClient),
        McpClientOptions::new("mcp-auth-spec-client", "1.0.0", Duration::from_secs(5)),
        CancellationToken::new(),
    )
    .await
    .expect_err("client should fail authorization");

    let Error::Authorization(error) = error else {
        panic!("expected authorization error");
    };
    assert_eq!(error.status, StatusCode::UNAUTHORIZED);
    let challenge = error.challenge.expect("bearer challenge");
    assert_eq!(challenge.error.as_deref(), Some("invalid_token"));
    assert_eq!(challenge.error_description.as_deref(), Some("bad token"));
    Ok(())
}

#[tokio::test]
async fn client_reports_authorization_failure_from_get_polling_after_initialize() -> mcp::Result<()>
{
    let attempts = Arc::new(AtomicUsize::new(0));
    let server_attempts = Arc::clone(&attempts);
    let server = AuthorizedSpecServer::start(move |_, _| {
        let attempts = Arc::clone(&server_attempts);
        Box::pin(async move {
            if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                McpTokenValidation::Authorized
            } else {
                McpTokenValidation::Unauthorized {
                    error_description: Some("expired after initialize".to_owned()),
                }
            }
        })
    })
    .await?;
    let (tx, mut rx) = mpsc::unbounded_channel();

    let client = McpClient::connect(
        "mcp-auth-spec-server",
        McpTransportConfig::StreamableHttp {
            endpoint: server.endpoint,
            bearer_token: Some("soon-expired-token".to_owned()),
        },
        Arc::new(AuthorizationFailureClient { tx }),
        McpClientOptions::new("mcp-auth-spec-client", "1.0.0", Duration::from_secs(5)),
        CancellationToken::new(),
    )
    .await?;

    let error = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("authorization failure is reported")
        .expect("authorization error value");
    client.shutdown().await;
    assert_eq!(error.status, StatusCode::UNAUTHORIZED);
    let challenge = error.challenge.expect("bearer challenge");
    assert_eq!(challenge.error.as_deref(), Some("invalid_token"));
    assert_eq!(
        challenge.error_description.as_deref(),
        Some("expired after initialize")
    );
    Ok(())
}

#[tokio::test]
async fn protected_resource_metadata_discovery_validates_mcp_required_fields() -> mcp::Result<()> {
    let server =
        AuthorizedSpecServer::start(|_, _| Box::pin(async { McpTokenValidation::Authorized }))
            .await?;
    let challenge = McpBearerChallenge {
        resource_metadata: Some(format!(
            "{}/.well-known/oauth-protected-resource/mcp",
            server.origin
        )),
        ..McpBearerChallenge::new()
    };

    let metadata = discover_protected_resource_metadata(
        &reqwest::Client::new(),
        &server.endpoint,
        Some(&challenge),
    )
    .await?;
    assert_eq!(metadata.resource, server.endpoint);
    assert_eq!(metadata.authorization_servers, ["https://auth.example.com"]);
    Ok(())
}

#[tokio::test]
async fn protected_resource_metadata_discovery_falls_back_to_root_well_known() -> mcp::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let origin = format!("http://{address}");
    let endpoint = format!("{origin}/mcp");
    let metadata =
        McpProtectedResourceMetadata::new(endpoint.clone(), ["https://auth.example.com"]);
    let router = axum::Router::new().route(
        "/.well-known/oauth-protected-resource",
        axum::routing::get(move || {
            let metadata = metadata.clone();
            async move { axum::Json(metadata) }
        }),
    );
    let task = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router).await {
            tracing::debug!(%error, "mcp protected resource metadata server stopped");
        }
    });

    let discovered =
        discover_protected_resource_metadata(&reqwest::Client::new(), &endpoint, None).await?;
    task.abort();
    assert_eq!(discovered.resource, endpoint);
    assert_eq!(
        discovered.authorization_servers,
        ["https://auth.example.com"]
    );
    Ok(())
}

#[test]
fn protected_resource_metadata_validation_rejects_invalid_mcp_metadata() {
    let endpoint = "https://mcp.example.com/mcp";
    assert!(
        McpProtectedResourceMetadata::new(endpoint, std::iter::empty::<&str>())
            .validate_for_mcp_resource(endpoint)
            .is_err()
    );
    assert!(
        McpProtectedResourceMetadata::new(
            "https://mcp.example.com/other",
            ["https://auth.example.com"]
        )
        .validate_for_mcp_resource(endpoint)
        .is_err()
    );
    assert!(
        McpProtectedResourceMetadata::new(
            "https://mcp.example.com/mcp#fragment",
            ["https://auth.example.com"],
        )
        .validate_for_mcp_resource("https://mcp.example.com/mcp#fragment")
        .is_err()
    );
}

#[test]
fn bearer_challenge_parser_finds_case_insensitive_bearer_challenge() {
    let challenge = McpBearerChallenge::parse(
        "Basic realm=\"ignored\", bEaReR scope=\"files:read\", resource_metadata=\"https://mcp.example.com/.well-known/oauth-protected-resource/mcp\"",
    )
    .expect("bearer challenge");
    assert_eq!(challenge.scope.as_deref(), Some("files:read"));
    assert_eq!(
        challenge.resource_metadata.as_deref(),
        Some("https://mcp.example.com/.well-known/oauth-protected-resource/mcp")
    );
    let challenge = McpBearerChallenge::parse_any([
        "Basic realm=\"ignored\"",
        "bearer error=\"invalid_token\"",
    ])
    .expect("bearer challenge from later header value");
    assert_eq!(challenge.error.as_deref(), Some("invalid_token"));
}

#[test]
fn authorization_discovery_urls_follow_mcp_priority_order() {
    assert_eq!(
        protected_resource_metadata_urls("https://mcp.example.com/public/mcp"),
        [
            "https://mcp.example.com/.well-known/oauth-protected-resource/public/mcp",
            "https://mcp.example.com/.well-known/oauth-protected-resource",
        ]
    );
    assert_eq!(
        protected_resource_metadata_urls("https://mcp.example.com/public/mcp?tenant=a#ignored"),
        [
            "https://mcp.example.com/.well-known/oauth-protected-resource/public/mcp?tenant=a",
            "https://mcp.example.com/.well-known/oauth-protected-resource",
        ]
    );
    assert_eq!(
        authorization_server_metadata_urls("https://auth.example.com/tenant1"),
        [
            "https://auth.example.com/.well-known/oauth-authorization-server/tenant1",
            "https://auth.example.com/.well-known/openid-configuration/tenant1",
            "https://auth.example.com/tenant1/.well-known/openid-configuration",
        ]
    );
}

struct AuthorizationClient;

impl McpClientHandler for AuthorizationClient {}

struct AuthorizationFailureClient {
    tx: mpsc::UnboundedSender<McpAuthorizationError>,
}

impl McpClientHandler for AuthorizationFailureClient {
    fn authorization_failed(&self, error: McpAuthorizationError) {
        drop(self.tx.send(error));
    }
}

struct AuthorizedSpecServer {
    endpoint: String,
    origin: String,
    _task: tokio::task::JoinHandle<()>,
}

impl AuthorizedSpecServer {
    async fn start(
        validator: impl Fn(McpAccessToken, McpAuthorizationContext) -> BoxFuture<McpTokenValidation>
        + Send
        + Sync
        + 'static,
    ) -> mcp::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address: SocketAddr = listener.local_addr()?;
        let origin = format!("http://{address}");
        let endpoint = format!("{origin}/mcp");
        let metadata =
            McpProtectedResourceMetadata::new(endpoint.clone(), ["https://auth.example.com"])
                .with_scopes(["files:read"])
                .with_resource_name("Authorization spec MCP server");
        let authorization = StreamableHttpAuthorization::new(metadata, validator)?
            .with_required_scopes(["files:read"])
            .with_realm("mcp-spec");
        let router = mcp::server::streamable_http_router_with_options(
            Arc::new(AuthorizationServer),
            StreamableHttpOptions::default().with_authorization(authorization),
        );
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router).await {
                tracing::debug!(%error, "mcp authorization spec server stopped");
            }
        });
        Ok(Self {
            endpoint,
            origin,
            _task: task,
        })
    }
}

async fn initialize_request(
    endpoint: &str,
    bearer_token: Option<&str>,
) -> mcp::Result<reqwest::Response> {
    let client = reqwest::Client::new();
    let mut request = client
        .post(endpoint)
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "auth-initialize",
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {
                    "name": "mcp-auth-spec-client",
                    "version": "1.0.0"
                }
            }
        }));
    if let Some(bearer_token) = bearer_token {
        request = request.bearer_auth(bearer_token);
    }
    Ok(request.send().await?)
}

async fn initialize_request_with_authorization(
    endpoint: &str,
    authorization: Option<&str>,
) -> mcp::Result<reqwest::Response> {
    let client = reqwest::Client::new();
    let mut request = client
        .post(endpoint)
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "auth-initialize",
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {
                    "name": "mcp-auth-spec-client",
                    "version": "1.0.0"
                }
            }
        }));
    if let Some(authorization) = authorization {
        request = request.header(reqwest::header::AUTHORIZATION, authorization);
    }
    Ok(request.send().await?)
}

fn bearer_challenge(response: &reqwest::Response) -> McpBearerChallenge {
    response
        .headers()
        .get_all(reqwest::header::WWW_AUTHENTICATE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(McpBearerChallenge::parse)
        .expect("bearer challenge")
}
