use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};

use futures_util::StreamExt;
#[cfg(feature = "oauth-authorization-server")]
use mcp::server::{McpHostedTokenValidation, McpRevocationLease, McpTokenAuthorization};
use mcp::{
    Error, McpAccessToken, McpBearerChallenge, McpPrincipalId, McpProtectedResourceMetadata,
    McpTokenValidation, McpToolCall, McpToolDefinition, McpToolList, McpToolResult,
    client::{
        McpBearerProvider, McpBearerProviderError, McpClient, McpClientHandler, McpClientOptions,
        McpTransportConfig,
    },
    server::{
        BoxFuture, McpAuthorizationContext, ServerCapabilities, ServerContext, ServerHandler,
        ServerInfo, ServerResult, StreamableHttpAuthorization, StreamableHttpOptions,
        streamable_http_router_with_options,
    },
};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Notify};
use tokio_util::sync::CancellationToken;

use crate::spec::support::modern_params;

struct AuthorizationServer;

impl ServerHandler for AuthorizationServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("mcp-authorization-spec-server", "1.0.0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new().tools()
    }

    fn list_tools(
        &self,
        _cursor: Option<String>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpToolList>> {
        Box::pin(async {
            Ok(McpToolList {
                tools: vec![McpToolDefinition {
                    name: "authorized".to_owned(),
                    title: None,
                    description: None,
                    icons: Vec::new(),
                    input_schema: json!({ "type": "object" }),
                    output_schema: None,
                    annotations: None,
                    meta: None,
                }],
                next_cursor: None,
            })
        })
    }
}

struct AuthorizationClient;

impl McpClientHandler for AuthorizationClient {}

#[tokio::test]
async fn protected_resource_metadata_and_bearer_outcomes_follow_oauth_semantics() -> mcp::Result<()>
{
    let server = AuthorizedSpecServer::start(|token, context| {
        Box::pin(async move {
            assert_eq!(context.required_scopes, ["files:read"]);
            match token.0.as_str() {
                "valid" => authorized(),
                "expired" => McpTokenValidation::Unauthorized {
                    error_description: Some("expired".to_owned()),
                },
                "read-only" => McpTokenValidation::InsufficientScope {
                    required_scopes: vec!["files:read".to_owned(), "files:write".to_owned()],
                    error_description: Some("write required".to_owned()),
                },
                _ => McpTokenValidation::Unavailable,
            }
        })
    })
    .await?;

    let metadata = reqwest::get(format!(
        "{}/.well-known/oauth-protected-resource/mcp",
        server.origin
    ))
    .await?;
    assert_eq!(metadata.status(), StatusCode::OK);
    let metadata: McpProtectedResourceMetadata = metadata.json().await?;
    assert_eq!(metadata.resource, server.endpoint);
    assert_eq!(metadata.authorization_servers, ["https://auth.example.com"]);
    assert_eq!(metadata.scopes_supported, ["files:read"]);

    let missing = discover_request(&server.endpoint, None).await?;
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
    let challenge = bearer_challenge(&missing);
    assert_eq!(challenge.realm.as_deref(), Some("mcp-spec"));
    assert_eq!(challenge.scope.as_deref(), Some("files:read"));
    assert!(challenge.resource_metadata.is_some());

    let valid = discover_request(&server.endpoint, Some("bEaReR valid")).await?;
    assert_eq!(valid.status(), StatusCode::OK);
    let invalid = discover_request(&server.endpoint, Some("Bearer expired")).await?;
    assert_eq!(invalid.status(), StatusCode::UNAUTHORIZED);
    let challenge = bearer_challenge(&invalid);
    assert_eq!(challenge.error.as_deref(), Some("invalid_token"));
    assert_eq!(challenge.error_description.as_deref(), Some("expired"));

    let insufficient = discover_request(&server.endpoint, Some("Bearer read-only")).await?;
    assert_eq!(insufficient.status(), StatusCode::FORBIDDEN);
    let challenge = bearer_challenge(&insufficient);
    assert_eq!(challenge.error.as_deref(), Some("insufficient_scope"));
    assert_eq!(challenge.scope.as_deref(), Some("files:read files:write"));

    let unavailable = discover_request(&server.endpoint, Some("Bearer unavailable")).await?;
    assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(unavailable.headers().get("www-authenticate").is_none());
    Ok(())
}

#[tokio::test]
async fn client_injects_static_bearer_and_returns_structured_challenges() -> mcp::Result<()> {
    let seen = Arc::new(AtomicUsize::new(0));
    let server_seen = Arc::clone(&seen);
    let server = AuthorizedSpecServer::start(move |token, _| {
        let seen = Arc::clone(&server_seen);
        Box::pin(async move {
            if token.0 == "client-token" {
                seen.fetch_add(1, Ordering::SeqCst);
                authorized()
            } else {
                McpTokenValidation::Unauthorized {
                    error_description: Some("bad token".to_owned()),
                }
            }
        })
    })
    .await?;
    let cancellation = CancellationToken::new();
    let client = McpClient::connect(
        "authorization-spec",
        McpTransportConfig::StreamableHttp {
            endpoint: server.endpoint.clone(),
            bearer_token: Some("client-token".to_owned()),
        },
        Arc::new(AuthorizationClient),
        McpClientOptions::new("spec-client", "1", Duration::from_secs(2)),
        cancellation.clone(),
    )
    .await?;
    assert_eq!(client.list_tools(cancellation).await?.len(), 1);
    client.shutdown().await?;
    assert_eq!(seen.load(Ordering::SeqCst), 2);

    let error = McpClient::connect(
        "authorization-spec",
        McpTransportConfig::StreamableHttp {
            endpoint: server.endpoint,
            bearer_token: Some("wrong".to_owned()),
        },
        Arc::new(AuthorizationClient),
        McpClientOptions::new("spec-client", "1", Duration::from_secs(2)),
        CancellationToken::new(),
    )
    .await
    .expect_err("invalid bearer must fail discovery");
    let Error::Authorization(error) = error else {
        panic!("expected structured authorization error");
    };
    assert_eq!(error.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        error.challenge.and_then(|challenge| challenge.error),
        Some("invalid_token".to_owned())
    );
    Ok(())
}

#[tokio::test]
async fn dynamic_bearer_is_resolved_per_post_and_provider_failures_are_typed() -> mcp::Result<()> {
    let server = AuthorizedSpecServer::start(|token, _| {
        Box::pin(async move {
            assert!(token.0.starts_with("rotating-"));
            authorized()
        })
    })
    .await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let provider_calls = Arc::clone(&calls);
    let provider: Arc<dyn McpBearerProvider> = Arc::new(move || {
        let sequence = provider_calls.fetch_add(1, Ordering::SeqCst);
        async move { Ok(format!("rotating-{sequence}")) }
    });
    let cancellation = CancellationToken::new();
    let client = McpClient::connect_with_bearer_provider(
        "dynamic-authorization-spec",
        McpTransportConfig::StreamableHttp {
            endpoint: server.endpoint.clone(),
            bearer_token: Some("unused-static-token".to_owned()),
        },
        provider,
        Arc::new(AuthorizationClient),
        McpClientOptions::new("spec-client", "1", Duration::from_secs(2)),
        cancellation.clone(),
    )
    .await?;
    client.list_tools(cancellation).await?;
    client.shutdown().await?;
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let error = McpClient::connect_with_bearer_provider(
        "failed-authorization-provider",
        McpTransportConfig::StreamableHttp {
            endpoint: server.endpoint,
            bearer_token: Some("must-not-leak".to_owned()),
        },
        Arc::new(|| async { Err(McpBearerProviderError::Unavailable) }),
        Arc::new(AuthorizationClient),
        McpClientOptions::new("spec-client", "1", Duration::from_secs(2)),
        CancellationToken::new(),
    )
    .await
    .expect_err("provider failure must fail discovery");
    assert!(matches!(
        error,
        Error::BearerProvider(McpBearerProviderError::Unavailable)
    ));
    assert!(!format!("{error:?} {error}").contains("must-not-leak"));
    Ok(())
}

#[tokio::test]
async fn request_cancellation_interrupts_dynamic_bearer_resolution() -> mcp::Result<()> {
    let server = AuthorizedSpecServer::start(|_, _| Box::pin(async { authorized() })).await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let provider: Arc<dyn McpBearerProvider> = Arc::new({
        let calls = Arc::clone(&calls);
        let started = Arc::clone(&started);
        move || {
            let sequence = calls.fetch_add(1, Ordering::SeqCst);
            let started = Arc::clone(&started);
            async move {
                if sequence == 1 {
                    started.notify_one();
                    std::future::pending::<()>().await;
                }
                Ok("valid".to_owned())
            }
        }
    });
    let client = Arc::new(
        McpClient::connect_with_bearer_provider(
            "cancelled-authorization-provider",
            McpTransportConfig::StreamableHttp {
                endpoint: server.endpoint,
                bearer_token: None,
            },
            provider,
            Arc::new(AuthorizationClient),
            McpClientOptions::new("spec-client", "1", Duration::from_secs(2)),
            CancellationToken::new(),
        )
        .await?,
    );
    let cancellation = CancellationToken::new();
    let request = tokio::spawn({
        let client = Arc::clone(&client);
        let cancellation = cancellation.clone();
        async move { client.list_tools(cancellation).await }
    });
    started.notified().await;
    cancellation.cancel();
    let error = tokio::time::timeout(Duration::from_secs(1), request)
        .await
        .expect("request cancellation must interrupt token resolution")
        .expect("request task")
        .expect_err("cancelled request must fail");
    assert!(matches!(error, Error::Protocol(_)));
    client.shutdown().await?;
    Ok(())
}

struct ExpiringStreamingServer;

impl ServerHandler for ExpiringStreamingServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("mcp-expiring-request-spec-server", "1.0.0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new().tools()
    }

    fn list_tools(
        &self,
        _cursor: Option<String>,
        context: ServerContext,
    ) -> BoxFuture<ServerResult<McpToolList>> {
        AuthorizationServer.list_tools(None, context)
    }

    fn call_tool(
        &self,
        _call: McpToolCall,
        context: ServerContext,
    ) -> BoxFuture<ServerResult<McpToolResult>> {
        Box::pin(async move {
            context
                .progress(json!(9), 1.0, None, Some("authorized".to_owned()))
                .await
                .map_err(|error| mcp::server::ServerError::internal(error.to_string()))?;
            std::future::pending::<()>().await;
            unreachable!()
        })
    }
}

#[tokio::test]
async fn access_token_expiry_terminates_the_active_request_stream() -> mcp::Result<()> {
    let server =
        AuthorizedSpecServer::start_with_handler(Arc::new(ExpiringStreamingServer), |_, _| {
            Box::pin(async {
                McpTokenValidation::Authorized {
                    principal_id: McpPrincipalId::new("expiring-spec").expect("principal"),
                    expires_at: Some(SystemTime::now() + Duration::from_secs(1)),
                }
            })
        })
        .await?;
    let mut params = modern_params(json!({ "name": "authorized", "arguments": {} }));
    params["_meta"]["progressToken"] = json!(9);
    let response = reqwest::Client::new()
        .post(server.endpoint)
        .header("authorization", "Bearer expiring")
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", mcp::protocol::MCP_PROTOCOL_VERSION)
        .header("mcp-method", "tools/call")
        .header("mcp-name", "authorized")
        .json(&json!({
            "jsonrpc": "2.0", "id": "expiring", "method": "tools/call", "params": params
        }))
        .send()
        .await?;
    let mut stream = response.bytes_stream();
    let first = stream.next().await.expect("progress frame")?;
    assert!(String::from_utf8_lossy(&first).contains("notifications/progress"));
    let end = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("token expiry closes the stream");
    assert!(end.is_none(), "expiry must end without a terminal result");
    Ok(())
}

#[cfg(feature = "oauth-authorization-server")]
#[tokio::test]
async fn token_revocation_terminates_the_active_request_stream() -> mcp::Result<()> {
    let (revoked, notifications) = tokio::sync::watch::channel(0_u64);
    let server = AuthorizedSpecServer::start_hosted_with_handler(
        Arc::new(ExpiringStreamingServer),
        move |_, _| {
            let notifications = notifications.clone();
            Box::pin(async move {
                McpHostedTokenValidation::Authorized(McpTokenAuthorization {
                    principal_id: McpPrincipalId::new("revoked-spec").expect("principal"),
                    expires_at: None,
                    revocation: Some(McpRevocationLease::new(
                        || Box::pin(async { Ok(false) }),
                        Duration::from_mins(1),
                        notifications,
                    )),
                })
            })
        },
    )
    .await?;
    let mut params = modern_params(json!({ "name": "authorized", "arguments": {} }));
    params["_meta"]["progressToken"] = json!(9);
    let response = reqwest::Client::new()
        .post(server.endpoint)
        .header("authorization", "Bearer revoked")
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", mcp::protocol::MCP_PROTOCOL_VERSION)
        .header("mcp-method", "tools/call")
        .header("mcp-name", "authorized")
        .json(&json!({
            "jsonrpc": "2.0", "id": "revoked", "method": "tools/call", "params": params
        }))
        .send()
        .await?;
    let mut stream = response.bytes_stream();
    let first = stream.next().await.expect("progress frame")?;
    assert!(String::from_utf8_lossy(&first).contains("notifications/progress"));
    revoked.send(1).expect("revocation listener");
    let end = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("revocation closes the stream");
    assert!(
        end.is_none(),
        "revocation must end without a terminal result"
    );
    Ok(())
}

fn authorized() -> McpTokenValidation {
    McpTokenValidation::Authorized {
        principal_id: McpPrincipalId::new("authorization-spec").expect("valid principal"),
        expires_at: None,
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
        Self::start_with_handler(Arc::new(AuthorizationServer), validator).await
    }

    async fn start_with_handler(
        handler: Arc<dyn ServerHandler>,
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
                .with_scopes(["files:read"]);
        let authorization = StreamableHttpAuthorization::new(metadata, validator)?
            .with_required_scopes(["files:read"])
            .with_realm("mcp-spec");
        let router = streamable_http_router_with_options(
            handler,
            StreamableHttpOptions::default().with_authorization(authorization),
        );
        let task = tokio::spawn(async move {
            drop(axum::serve(listener, router).await);
        });
        Ok(Self {
            endpoint,
            origin,
            _task: task,
        })
    }

    #[cfg(feature = "oauth-authorization-server")]
    async fn start_hosted_with_handler(
        handler: Arc<dyn ServerHandler>,
        authorizer: impl Fn(
            McpAccessToken,
            McpAuthorizationContext,
        ) -> BoxFuture<McpHostedTokenValidation>
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
                .with_scopes(["files:read"]);
        let authorization = StreamableHttpAuthorization::hosted(metadata, authorizer)?
            .with_required_scopes(["files:read"])
            .with_realm("mcp-spec");
        let router = streamable_http_router_with_options(
            handler,
            StreamableHttpOptions::default().with_authorization(authorization),
        );
        let task = tokio::spawn(async move {
            drop(axum::serve(listener, router).await);
        });
        Ok(Self {
            endpoint,
            origin,
            _task: task,
        })
    }
}

async fn discover_request(
    endpoint: &str,
    authorization: Option<&str>,
) -> mcp::Result<reqwest::Response> {
    let mut request = reqwest::Client::new()
        .post(endpoint)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", mcp::protocol::MCP_PROTOCOL_VERSION)
        .header("mcp-method", "server/discover")
        .json(&json!({
            "jsonrpc": "2.0", "id": "auth-discover", "method": "server/discover",
            "params": modern_params(Value::Object(serde_json::Map::default()))
        }));
    if let Some(authorization) = authorization {
        request = request.header(reqwest::header::AUTHORIZATION, authorization);
    }
    Ok(request.send().await?)
}

fn bearer_challenge(response: &reqwest::Response) -> McpBearerChallenge {
    McpBearerChallenge::parse_any(
        response
            .headers()
            .get_all(reqwest::header::WWW_AUTHENTICATE)
            .iter()
            .filter_map(|value| value.to_str().ok()),
    )
    .expect("Bearer challenge")
}
