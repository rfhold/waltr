use std::{
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::StreamExt;
use mcp::{
    Error, McpAccessToken, McpAuthorizationError, McpBearerChallenge, McpOAuthDestinationPolicy,
    McpOAuthDiscoveryClient, McpOAuthEndpointPolicy, McpPrincipalId, McpProtectedResourceMetadata,
    McpTokenValidation,
    client::{McpClient, McpClientHandler, McpClientOptions, McpTransportConfig},
    oauth::{authorization_server_metadata_urls, protected_resource_metadata_urls},
    server::{
        BoxFuture, InMemoryStreamableHttpSessionStore, McpAuthorizationContext,
        StreamableHttpAbandonInitialization, StreamableHttpAbandonInitializationRequest,
        StreamableHttpAppendEvents, StreamableHttpAppendEventsRequest,
        StreamableHttpClaimInitialization, StreamableHttpCleanupExpired,
        StreamableHttpCompleteInitialization, StreamableHttpCompleteInitializationRequest,
        StreamableHttpCreateSession, StreamableHttpCreateSessionRequest,
        StreamableHttpDeleteSession, StreamableHttpInspectSession, StreamableHttpPrepareDelivery,
        StreamableHttpPrepareDeliveryRequest, StreamableHttpSessionKey, StreamableHttpSessionOwner,
        StreamableHttpSessionRequest, StreamableHttpSessionStore, StreamableHttpSessionStoreError,
        StreamableHttpSessionStoreResult, streamable_http_router_with_options,
    },
};
use reqwest::StatusCode;
use serde_json::json;
use tokio::{net::TcpListener, sync::Mutex, sync::mpsc};
use tokio_util::sync::CancellationToken;

type AuthorizationValidator = Arc<
    dyn Fn(McpAccessToken, McpAuthorizationContext) -> BoxFuture<McpTokenValidation> + Send + Sync,
>;

fn authorized() -> McpTokenValidation {
    McpTokenValidation::Authorized {
        principal_id: McpPrincipalId::new("authorization-spec").expect("valid principal"),
        expires_at: None,
    }
}

#[derive(Clone)]
struct AuthorizationServer {
    validator: AuthorizationValidator,
}

#[mcp::mcp_server(
    name = "mcp-authorization-spec-server",
    version = "1.0.0",
    auth(
        scopes = ["files:read"],
        required_scopes = ["files:read"],
        realm = "mcp-spec",
        resource_name = "Authorization spec MCP server"
    )
)]
impl AuthorizationServer {
    fn new(validator: AuthorizationValidator) -> Self {
        Self { validator }
    }

    #[token_validator]
    async fn validate_token(
        &self,
        token: McpAccessToken,
        context: McpAuthorizationContext,
    ) -> McpTokenValidation {
        (self.validator)(token, context).await
    }
}

#[tokio::test]
async fn protected_resource_metadata_is_served_from_well_known_path() -> mcp::Result<()> {
    let server = AuthorizedSpecServer::start(|_, _| Box::pin(async { authorized() })).await?;

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
    let server = AuthorizedSpecServer::start(|_, _| Box::pin(async { authorized() })).await?;

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
async fn authorization_rejection_precedes_all_session_store_access() -> mcp::Result<()> {
    let accesses = Arc::new(AtomicUsize::new(0));
    let store = Arc::new(RecordingFailingStore {
        accesses: Arc::clone(&accesses),
    });
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let endpoint = format!("http://{address}/mcp");
    let server = AuthorizationServer::new(Arc::new(|_, _| Box::pin(async { authorized() })));
    let metadata = AuthorizationServer::protected_resource_metadata(
        endpoint.clone(),
        ["https://auth.example.com"],
    );
    let options = server
        .streamable_http_options(metadata)?
        .with_session_store(store);
    let router = streamable_http_router_with_options(Arc::new(server), options);
    let task = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router).await {
            tracing::debug!(%error, "mcp authorization ordering server stopped");
        }
    });
    let client = reqwest::Client::new();

    let post = initialize_request(&endpoint, None).await?;
    assert_eq!(post.status(), StatusCode::UNAUTHORIZED);
    let get = client
        .get(&endpoint)
        .header("accept", "text/event-stream")
        .header("mcp-session-id", "unauthorized-session")
        .header("mcp-protocol-version", "2025-11-25")
        .send()
        .await?;
    assert_eq!(get.status(), StatusCode::UNAUTHORIZED);
    let delete = client
        .delete(&endpoint)
        .header("mcp-session-id", "unauthorized-session")
        .header("mcp-protocol-version", "2025-11-25")
        .send()
        .await?;
    assert_eq!(delete.status(), StatusCode::UNAUTHORIZED);

    task.abort();
    assert_eq!(accesses.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn origin_rejection_precedes_json_authorization_and_store_access() -> mcp::Result<()> {
    let validator_calls = Arc::new(AtomicUsize::new(0));
    let validator_counter = validator_calls.clone();
    let accesses = Arc::new(AtomicUsize::new(0));
    let store = Arc::new(RecordingFailingStore {
        accesses: accesses.clone(),
    });
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let endpoint = format!("http://{address}/mcp");
    let server = AuthorizationServer::new(Arc::new(move |_, _| {
        validator_counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { authorized() })
    }));
    let metadata = AuthorizationServer::protected_resource_metadata(
        endpoint.clone(),
        ["https://auth.example.com"],
    );
    let options = server
        .streamable_http_options(metadata)?
        .with_session_store(store);
    let router = streamable_http_router_with_options(Arc::new(server), options);
    let task = tokio::spawn(async move {
        drop(axum::serve(listener, router).await);
    });
    let response = reqwest::Client::new()
        .post(endpoint)
        .header("origin", "https://blocked.test")
        .header("authorization", "Bearer valid")
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body("{")
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(validator_calls.load(Ordering::SeqCst), 0);
    assert_eq!(accesses.load(Ordering::SeqCst), 0);
    task.abort();
    Ok(())
}

#[tokio::test]
async fn wrong_owner_is_rejected_before_malformed_or_oversized_body() -> mcp::Result<()> {
    let server = AuthorizedSpecServer::start(|token, _| {
        Box::pin(async move {
            McpTokenValidation::Authorized {
                principal_id: McpPrincipalId::new(format!("owner:{}", token.0)).unwrap(),
                expires_at: None,
            }
        })
    })
    .await?;
    let initialize = initialize_request(&server.endpoint, Some("one")).await?;
    let session_id = initialize
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .expect("owner-bound session id")
        .to_owned();
    drop(initialize.bytes().await?);
    let client = reqwest::Client::new();
    for body in ["{".to_owned(), "x".repeat(1024 * 1024 + 1)] {
        let response = client
            .post(&server.endpoint)
            .bearer_auth("two")
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-session-id", &session_id)
            .header("mcp-protocol-version", "2025-11-25")
            .body(body)
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    Ok(())
}

#[tokio::test]
async fn unavailable_validator_returns_service_unavailable_before_storage_access() -> mcp::Result<()>
{
    let accesses = Arc::new(AtomicUsize::new(0));
    let store = Arc::new(RecordingFailingStore {
        accesses: Arc::clone(&accesses),
    });
    let server = AuthorizedSpecServer::start_with_store(
        |_, _| Box::pin(async { McpTokenValidation::Unavailable }),
        store,
    )
    .await?;
    let client = reqwest::Client::new();

    let post = initialize_request(&server.endpoint, Some("unavailable-token")).await?;
    assert_service_unavailable_without_challenge(&post);
    let get = client
        .get(&server.endpoint)
        .header("accept", "text/event-stream")
        .header("mcp-session-id", "unavailable-session")
        .header("mcp-protocol-version", "2025-11-25")
        .bearer_auth("unavailable-token")
        .send()
        .await?;
    assert_service_unavailable_without_challenge(&get);
    let delete = client
        .delete(&server.endpoint)
        .header("mcp-session-id", "unavailable-session")
        .header("mcp-protocol-version", "2025-11-25")
        .bearer_auth("unavailable-token")
        .send()
        .await?;
    assert_service_unavailable_without_challenge(&delete);

    assert_eq!(accesses.load(Ordering::SeqCst), 0);
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
            authorized()
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
            authorized()
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
            authorized()
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

    client.shutdown().await?;
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
async fn client_treats_validator_unavailability_as_a_server_failure() -> mcp::Result<()> {
    let server =
        AuthorizedSpecServer::start(|_, _| Box::pin(async { McpTokenValidation::Unavailable }))
            .await?;

    let error = McpClient::connect(
        "mcp-auth-spec-server",
        McpTransportConfig::StreamableHttp {
            endpoint: server.endpoint,
            bearer_token: Some("unavailable-token".to_owned()),
        },
        Arc::new(AuthorizationClient),
        McpClientOptions::new("mcp-auth-spec-client", "1.0.0", Duration::from_secs(5)),
        CancellationToken::new(),
    )
    .await
    .expect_err("client should fail on validator unavailability");

    assert!(matches!(
        error,
        Error::Protocol(message) if message.contains("503 Service Unavailable")
    ));
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
                authorized()
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
    let shutdown = client
        .shutdown()
        .await
        .expect_err("expired bearer token must fail graceful DELETE");
    assert!(matches!(shutdown, Error::Authorization(_)));
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
async fn authorized_get_stream_ends_at_credential_expiry() -> mcp::Result<()> {
    let server = AuthorizedSpecServer::start(|_, _| {
        Box::pin(async {
            McpTokenValidation::Authorized {
                principal_id: McpPrincipalId::new("expiring-stream").unwrap(),
                expires_at: Some(SystemTime::now() + Duration::from_millis(250)),
            }
        })
    })
    .await?;
    let client = reqwest::Client::new();
    let initialize = initialize_request(&server.endpoint, Some("short-lived")).await?;
    let session_id = initialize
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .expect("authorized session id")
        .to_owned();
    drop(initialize.bytes().await?);
    assert_eq!(
        client
            .post(&server.endpoint)
            .bearer_auth("short-lived")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", &session_id)
            .header("mcp-protocol-version", "2025-11-25")
            .json(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .send()
            .await?
            .status(),
        StatusCode::ACCEPTED
    );
    let response = client
        .get(&server.endpoint)
        .bearer_auth("short-lived")
        .header("accept", "text/event-stream")
        .header("mcp-session-id", session_id)
        .header("mcp-protocol-version", "2025-11-25")
        .send()
        .await?;
    let mut body = response.bytes_stream();
    let first = tokio::time::timeout(Duration::from_secs(1), body.next())
        .await
        .expect("priming data arrives")
        .expect("priming body")?;
    assert!(String::from_utf8_lossy(&first).contains("retry: 1000"));
    assert!(
        tokio::time::timeout(Duration::from_secs(1), async {
            while body.next().await.is_some() {}
        })
        .await
        .is_ok()
    );
    Ok(())
}

#[cfg(feature = "oauth-authorization-server")]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn hosted_get_stream_ends_on_notification_poll_and_store_failure() -> mcp::Result<()> {
    use std::sync::atomic::AtomicBool;

    use mcp::server::{McpHostedTokenValidation, McpRevocationLease, McpTokenAuthorization};
    use tokio::sync::watch;

    let valid = Arc::new(AtomicBool::new(true));
    let unavailable = Arc::new(AtomicBool::new(false));
    let (revocations, _) = watch::channel(0_u64);
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let endpoint = format!("http://{address}/mcp");
    let metadata = McpProtectedResourceMetadata::new(&endpoint, ["https://auth.example.com"])
        .with_scopes(["files:read"]);
    let authorizer_valid = valid.clone();
    let authorizer_unavailable = unavailable.clone();
    let authorizer_revocations = revocations.clone();
    let authorization = mcp::server::StreamableHttpAuthorization::hosted(metadata, move |_, _| {
        let valid = authorizer_valid.clone();
        let unavailable = authorizer_unavailable.clone();
        let receiver = authorizer_revocations.subscribe();
        Box::pin(async move {
            let lease_valid = valid.clone();
            let lease_unavailable = unavailable.clone();
            McpHostedTokenValidation::Authorized(McpTokenAuthorization {
                principal_id: McpPrincipalId::new("lease-owner").unwrap(),
                expires_at: None,
                revocation: Some(McpRevocationLease::new(
                    move || {
                        let valid = lease_valid.clone();
                        let unavailable = lease_unavailable.clone();
                        Box::pin(async move {
                            if unavailable.load(Ordering::SeqCst) {
                                Err(())
                            } else {
                                Ok(valid.load(Ordering::SeqCst))
                            }
                        })
                    },
                    Duration::from_millis(75),
                    receiver,
                )),
            })
        })
    })?
    .with_required_scopes(["files:read"]);
    let handler: Arc<dyn mcp::server::ServerHandler> =
        Arc::new(AuthorizationServer::new(Arc::new(|_, _| {
            Box::pin(async { authorized() })
        })));
    let router = streamable_http_router_with_options(
        handler,
        mcp::server::StreamableHttpOptions::default().with_authorization(authorization),
    );
    let task = tokio::spawn(async move { axum::serve(listener, router).await });
    let response = initialized_hosted_get(&endpoint).await?;
    let mut body = response.bytes_stream();
    assert!(body.next().await.is_some());
    valid.store(false, Ordering::SeqCst);
    revocations.send_modify(|generation| *generation += 1);
    tokio::time::timeout(Duration::from_secs(1), async {
        while body.next().await.is_some() {}
    })
    .await
    .expect("revocation closes stream");

    valid.store(true, Ordering::SeqCst);
    let response = initialized_hosted_get(&endpoint).await?;
    let mut body = response.bytes_stream();
    assert!(body.next().await.is_some());
    valid.store(false, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(1), async {
        while body.next().await.is_some() {}
    })
    .await
    .expect("authoritative polling closes revoked stream");

    valid.store(true, Ordering::SeqCst);
    let response = initialized_hosted_get(&endpoint).await?;
    let mut body = response.bytes_stream();
    assert!(body.next().await.is_some());
    unavailable.store(true, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(1), async {
        while body.next().await.is_some() {}
    })
    .await
    .expect("store failure closes stream");
    task.abort();
    Ok(())
}

#[cfg(feature = "oauth-authorization-server")]
async fn initialized_hosted_get(endpoint: &str) -> mcp::Result<reqwest::Response> {
    let client = reqwest::Client::new();
    let initialize = initialize_request(endpoint, Some("hosted")).await?;
    let session_id = initialize.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_owned();
    drop(initialize.bytes().await?);
    assert_eq!(
        client
            .post(endpoint)
            .bearer_auth("hosted")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", &session_id)
            .header("mcp-protocol-version", "2025-11-25")
            .json(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .send()
            .await?
            .status(),
        StatusCode::ACCEPTED
    );
    Ok(client
        .get(endpoint)
        .bearer_auth("hosted")
        .header("accept", "text/event-stream")
        .header("mcp-session-id", session_id)
        .header("mcp-protocol-version", "2025-11-25")
        .send()
        .await?)
}

#[tokio::test]
async fn already_expired_get_lease_closes_before_claiming_or_delivering_events() -> mcp::Result<()>
{
    let store = Arc::new(InMemoryStreamableHttpSessionStore::default());
    let server = AuthorizedSpecServer::start_with_store(
        |token, _| {
            Box::pin(async move {
                McpTokenValidation::Authorized {
                    principal_id: McpPrincipalId::new("expiry-owner").unwrap(),
                    expires_at: (token.0 == "expired").then_some(UNIX_EPOCH),
                }
            })
        },
        store.clone(),
    )
    .await?;
    let client = reqwest::Client::new();
    let initialize = initialize_request(&server.endpoint, Some("valid")).await?;
    let session_id = initialize
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .expect("authorized session id")
        .to_owned();
    drop(initialize.bytes().await?);
    assert_eq!(
        client
            .post(&server.endpoint)
            .bearer_auth("valid")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", &session_id)
            .header("mcp-protocol-version", "2025-11-25")
            .json(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .send()
            .await?
            .status(),
        StatusCode::ACCEPTED
    );
    assert!(matches!(
        store
            .append_events(StreamableHttpAppendEventsRequest {
                session: StreamableHttpSessionRequest {
                    key: StreamableHttpSessionKey {
                        session_id: session_id.clone(),
                        owner: StreamableHttpSessionOwner::Authenticated(
                            McpPrincipalId::new("expiry-owner").unwrap(),
                        ),
                    },
                    presented_protocol_version: Some("2025-11-25".to_owned()),
                    now: SystemTime::now(),
                },
                messages: vec![json!({ "expiry_marker": true })],
            })
            .await
            .unwrap(),
        StreamableHttpAppendEvents::Events(_)
    ));
    let expired = client
        .get(&server.endpoint)
        .bearer_auth("expired")
        .header("accept", "text/event-stream")
        .header("mcp-session-id", &session_id)
        .header("mcp-protocol-version", "2025-11-25")
        .send()
        .await?;
    assert_eq!(expired.status(), StatusCode::OK);
    assert!(expired.bytes().await?.is_empty());
    let valid = client
        .get(&server.endpoint)
        .bearer_auth("valid")
        .header("accept", "text/event-stream")
        .header("mcp-session-id", session_id)
        .header("mcp-protocol-version", "2025-11-25")
        .send()
        .await?;
    assert!(valid.text().await?.contains("expiry_marker"));
    Ok(())
}

#[tokio::test]
async fn credential_expiry_cancels_a_blocked_delivery_prepare() -> mcp::Result<()> {
    let store = Arc::new(BlockingPrepareStore {
        inner: InMemoryStreamableHttpSessionStore::default(),
    });
    let server = AuthorizedSpecServer::start_with_store(
        |_, _| {
            Box::pin(async {
                McpTokenValidation::Authorized {
                    principal_id: McpPrincipalId::new("blocked-expiry-owner").unwrap(),
                    expires_at: Some(SystemTime::now() + Duration::from_millis(75)),
                }
            })
        },
        store,
    )
    .await?;
    let client = reqwest::Client::new();
    let initialize = initialize_request(&server.endpoint, Some("valid")).await?;
    let session_id = initialize
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .unwrap()
        .to_owned();
    drop(initialize.bytes().await?);
    assert_eq!(
        client
            .post(&server.endpoint)
            .bearer_auth("valid")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", &session_id)
            .header("mcp-protocol-version", "2025-11-25")
            .json(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .send()
            .await?
            .status(),
        StatusCode::ACCEPTED
    );
    let response = tokio::time::timeout(
        Duration::from_secs(1),
        client
            .get(&server.endpoint)
            .bearer_auth("valid")
            .header("accept", "text/event-stream")
            .header("mcp-session-id", session_id)
            .header("mcp-protocol-version", "2025-11-25")
            .send(),
    )
    .await
    .expect("credential expiry cancels blocked store")?;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.bytes().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn protected_resource_metadata_discovery_validates_mcp_required_fields() -> mcp::Result<()> {
    let server = AuthorizedSpecServer::start(|_, _| Box::pin(async { authorized() })).await?;
    let challenge = McpBearerChallenge {
        resource_metadata: Some(format!(
            "{}/.well-known/oauth-protected-resource/mcp",
            server.origin
        )),
        ..McpBearerChallenge::new()
    };

    let metadata = loopback_discovery_client()?
        .discover_protected_resource_metadata(&server.endpoint, Some(&challenge))
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

    let discovered = loopback_discovery_client()?
        .discover_protected_resource_metadata(&endpoint, None)
        .await?;
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
    let challenge =
        McpBearerChallenge::parse("Bearer error=\"invalid_token\", Basic realm=\"unrelated\"")
            .expect("bearer challenge before another scheme");
    assert_eq!(challenge.error.as_deref(), Some("invalid_token"));
    assert!(McpBearerChallenge::parse("Bearer scope=one, scope=two").is_none());
    assert!(McpBearerChallenge::parse("Bearer scope=\"unterminated").is_none());
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
        protected_resource_metadata_urls("https://mcp.example.com/public/mcp?tenant=a"),
        [
            "https://mcp.example.com/.well-known/oauth-protected-resource/public/mcp?tenant=a",
            "https://mcp.example.com/.well-known/oauth-protected-resource",
        ]
    );
    assert!(
        protected_resource_metadata_urls(
            "https://mcp.example.com/public/mcp?tenant=secret#ambiguous"
        )
        .is_empty()
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

#[derive(Debug)]
struct AllowLoopbackDiscovery;

impl McpOAuthDestinationPolicy for AllowLoopbackDiscovery {
    fn validate(&self, _destination: &reqwest::Url) -> mcp::Result<()> {
        Ok(())
    }

    fn validate_address(&self, address: IpAddr) -> mcp::Result<()> {
        if address.is_loopback() {
            Ok(())
        } else {
            Err(Error::protocol("test destination is not loopback"))
        }
    }
}

fn loopback_discovery_client() -> mcp::Result<McpOAuthDiscoveryClient> {
    Ok(
        McpOAuthDiscoveryClient::new(McpOAuthEndpointPolicy::AllowLoopbackHttp)?
            .with_destination_policy(Arc::new(AllowLoopbackDiscovery)),
    )
}

struct AuthorizationClient;

impl McpClientHandler for AuthorizationClient {}

struct AuthorizationFailureClient {
    tx: mpsc::UnboundedSender<McpAuthorizationError>,
}

struct BlockingPrepareStore {
    inner: InMemoryStreamableHttpSessionStore,
}

impl StreamableHttpSessionStore for BlockingPrepareStore {
    fn config(&self) -> mcp::server::StreamableHttpSessionStoreConfig {
        self.inner.config()
    }

    fn create_session(
        &self,
        request: StreamableHttpCreateSessionRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpCreateSession>> {
        self.inner.create_session(request)
    }

    fn inspect_session(
        &self,
        request: StreamableHttpSessionRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpInspectSession>> {
        self.inner.inspect_session(request)
    }

    fn claim_initialization(
        &self,
        request: StreamableHttpSessionRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpClaimInitialization>> {
        self.inner.claim_initialization(request)
    }

    fn complete_initialization(
        &self,
        request: StreamableHttpCompleteInitializationRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpCompleteInitialization>> {
        self.inner.complete_initialization(request)
    }

    fn abandon_initialization(
        &self,
        request: StreamableHttpAbandonInitializationRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpAbandonInitialization>> {
        self.inner.abandon_initialization(request)
    }

    fn append_events(
        &self,
        request: StreamableHttpAppendEventsRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpAppendEvents>> {
        self.inner.append_events(request)
    }

    fn prepare_delivery(
        &self,
        _request: StreamableHttpPrepareDeliveryRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpPrepareDelivery>> {
        Box::pin(std::future::pending())
    }

    fn delete_session(
        &self,
        request: StreamableHttpSessionRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpDeleteSession>> {
        self.inner.delete_session(request)
    }

    fn cleanup_expired(
        &self,
        now: SystemTime,
        limit: usize,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpCleanupExpired>> {
        self.inner.cleanup_expired(now, limit)
    }
}

struct RecordingFailingStore {
    accesses: Arc<AtomicUsize>,
}

impl RecordingFailingStore {
    fn fail<T>(&self) -> BoxFuture<StreamableHttpSessionStoreResult<T>> {
        self.accesses.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(StreamableHttpSessionStoreError::Unavailable) })
    }
}

impl StreamableHttpSessionStore for RecordingFailingStore {
    fn config(&self) -> mcp::server::StreamableHttpSessionStoreConfig {
        mcp::server::StreamableHttpSessionStoreConfig::default()
    }

    fn create_session(
        &self,
        _request: StreamableHttpCreateSessionRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpCreateSession>> {
        self.fail()
    }

    fn inspect_session(
        &self,
        _request: StreamableHttpSessionRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpInspectSession>> {
        self.fail()
    }

    fn claim_initialization(
        &self,
        _request: StreamableHttpSessionRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpClaimInitialization>> {
        self.fail()
    }

    fn complete_initialization(
        &self,
        _request: StreamableHttpCompleteInitializationRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpCompleteInitialization>> {
        self.fail()
    }

    fn abandon_initialization(
        &self,
        _request: StreamableHttpAbandonInitializationRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpAbandonInitialization>> {
        self.fail()
    }

    fn append_events(
        &self,
        _request: StreamableHttpAppendEventsRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpAppendEvents>> {
        self.fail()
    }

    fn prepare_delivery(
        &self,
        _request: StreamableHttpPrepareDeliveryRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpPrepareDelivery>> {
        self.fail()
    }

    fn delete_session(
        &self,
        _request: StreamableHttpSessionRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpDeleteSession>> {
        self.fail()
    }

    fn cleanup_expired(
        &self,
        _now: std::time::SystemTime,
        _limit: usize,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpCleanupExpired>> {
        self.fail()
    }
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
        Self::start_configured(Arc::new(validator), None).await
    }

    async fn start_with_store(
        validator: impl Fn(McpAccessToken, McpAuthorizationContext) -> BoxFuture<McpTokenValidation>
        + Send
        + Sync
        + 'static,
        store: Arc<dyn StreamableHttpSessionStore>,
    ) -> mcp::Result<Self> {
        Self::start_configured(Arc::new(validator), Some(store)).await
    }

    async fn start_configured(
        validator: AuthorizationValidator,
        store: Option<Arc<dyn StreamableHttpSessionStore>>,
    ) -> mcp::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address: SocketAddr = listener.local_addr()?;
        let origin = format!("http://{address}");
        let endpoint = format!("{origin}/mcp");
        let server = AuthorizationServer::new(validator);
        let metadata = AuthorizationServer::protected_resource_metadata(
            endpoint.clone(),
            ["https://auth.example.com"],
        );
        let mut options = server.streamable_http_options(metadata)?;
        if let Some(store) = store {
            options = options.with_session_store(store);
        }
        let router = streamable_http_router_with_options(Arc::new(server), options);
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

fn assert_service_unavailable_without_challenge(response: &reqwest::Response) {
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        response
            .headers()
            .get_all(reqwest::header::WWW_AUTHENTICATE)
            .iter()
            .next()
            .is_none()
    );
}
