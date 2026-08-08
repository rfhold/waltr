use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use mcp::{
    McpProtectedResourceMetadata, McpTokenValidation,
    server::{
        ServerHandler, ServerInfo, StreamableHttpAuthorization, StreamableHttpOptions,
        streamable_http_router_with_options,
    },
};
use serde_json::{Value, json};

use crate::spec::support::{SpecServer, post_json, request_params};

#[derive(Debug)]
struct EmptyServer;

impl ServerHandler for EmptyServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("transport-spec", "1.0.0")
    }

    fn capabilities(&self) -> mcp::server::ServerCapabilities {
        mcp::server::ServerCapabilities::new()
    }
}

#[tokio::test]
async fn endpoint_is_post_only_and_rejects_header_mismatch() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(EmptyServer)).await?;
    let client = reqwest::Client::new();
    assert_eq!(
        client.get(server.endpoint()).send().await?.status(),
        reqwest::StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        client.delete(server.endpoint()).send().await?.status(),
        reqwest::StatusCode::METHOD_NOT_ALLOWED
    );

    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "server/discover",
        "params": request_params(&Value::Null)
    });
    let response = client
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", "tools/list")
        .json(&body)
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = response.json().await?;
    assert_eq!(body["error"]["code"], -32020);
    Ok(())
}

#[tokio::test]
async fn removed_lifecycle_methods_are_not_routed() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(EmptyServer)).await?;
    for method in ["initialize", "ping", "logging/setLevel"] {
        let response = post_json(
            &server.endpoint(),
            json!({
                "jsonrpc": "2.0", "id": method, "method": method,
                "params": request_params(&Value::Null)
            }),
        )
        .await?;
        assert_eq!(response.status, reqwest::StatusCode::NOT_FOUND);
        assert_eq!(response.body["error"]["code"], -32601);
    }
    Ok(())
}

#[tokio::test]
async fn request_metadata_is_required_and_version_errors_use_exact_data() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(EmptyServer)).await?;
    let client = reqwest::Client::new();
    let missing = client
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", "server/discover")
        .json(&json!({
            "jsonrpc": "2.0", "id": "missing", "method": "server/discover", "params": {}
        }))
        .send()
        .await?;
    assert_eq!(missing.status(), reqwest::StatusCode::BAD_REQUEST);
    let missing: Value = missing.json().await?;
    assert_eq!(missing["error"]["code"], -32602);

    let unsupported = client
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2025-11-25")
        .header("mcp-method", "server/discover")
        .json(&json!({
            "jsonrpc": "2.0", "id": "version", "method": "server/discover",
            "params": { "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2025-11-25",
                "io.modelcontextprotocol/clientCapabilities": {}
            }}
        }))
        .send()
        .await?;
    assert_eq!(unsupported.status(), reqwest::StatusCode::BAD_REQUEST);
    let unsupported: Value = unsupported.json().await?;
    assert_eq!(unsupported["error"]["code"], -32022);
    assert_eq!(
        unsupported["error"]["data"]["supported"],
        json!(["2026-07-28"])
    );
    assert_eq!(unsupported["error"]["data"]["requested"], "2025-11-25");
    Ok(())
}

#[tokio::test]
async fn notifications_return_empty_accepted_responses() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(EmptyServer)).await?;
    let response = reqwest::Client::new()
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", "notifications/cancelled")
        .json(&json!({
            "jsonrpc": "2.0", "method": "notifications/cancelled",
            "params": crate::spec::support::modern_params(json!({ "requestId": 1 }))
        }))
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    assert!(response.bytes().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn origin_policy_is_default_deny_canonical_and_checked_first() -> mcp::Result<()> {
    let default_endpoint = start_with_options(StreamableHttpOptions::default()).await?;
    assert_eq!(
        raw_post(&default_endpoint)
            .header("origin", "https://example.com")
            .send()
            .await?
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    assert_eq!(
        raw_post(&default_endpoint).send().await?.status(),
        reqwest::StatusCode::OK
    );

    let options = StreamableHttpOptions::default()
        .try_with_allowed_origins(["https://EXAMPLE.com:443", "http://localhost:80"])?;
    let endpoint = start_with_options(options).await?;
    assert_eq!(
        raw_post(&endpoint)
            .header("origin", "https://example.com")
            .send()
            .await?
            .status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        raw_post(&endpoint)
            .header("origin", "http://localhost")
            .send()
            .await?
            .status(),
        reqwest::StatusCode::OK
    );

    let malformed_body = reqwest::Client::new()
        .post(&endpoint)
        .header("origin", "https://attacker.example")
        .header("accept", "text/plain")
        .header("content-type", "text/plain")
        .body("not json")
        .send()
        .await?;
    assert_eq!(malformed_body.status(), reqwest::StatusCode::FORBIDDEN);

    let mut duplicate = reqwest::header::HeaderMap::new();
    duplicate.append("origin", "https://example.com".parse().expect("header"));
    duplicate.append("origin", "https://example.com".parse().expect("header"));
    assert_eq!(
        raw_post(&endpoint)
            .headers(duplicate)
            .send()
            .await?
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    assert!(
        StreamableHttpOptions::default()
            .try_with_allowed_origins(["https://example.com/path"])
            .is_err()
    );

    Ok(())
}

#[tokio::test]
async fn origin_rejection_precedes_authorization() -> mcp::Result<()> {
    let validator_calls = Arc::new(AtomicUsize::new(0));
    let validator_probe = Arc::clone(&validator_calls);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/mcp", listener.local_addr()?);
    let metadata =
        McpProtectedResourceMetadata::new(endpoint.clone(), ["https://auth.example.com"]);
    let authorization = StreamableHttpAuthorization::new(metadata, move |_, _| {
        validator_probe.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { McpTokenValidation::Unavailable })
    })?;
    let router = streamable_http_router_with_options(
        Arc::new(EmptyServer),
        StreamableHttpOptions::default().with_authorization(authorization),
    );
    tokio::spawn(async move {
        drop(axum::serve(listener, router).await);
    });
    assert_eq!(
        raw_post(&endpoint)
            .header("origin", "https://attacker.example")
            .header("authorization", "Bearer must-not-be-validated")
            .send()
            .await?
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    assert_eq!(validator_calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn http_admission_rejects_invalid_media_json_and_oversized_bodies() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(EmptyServer)).await?;
    let endpoint = server.endpoint();
    let client = reqwest::Client::new();

    let invalid_accept = client
        .post(&endpoint)
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await?;
    assert_eq!(invalid_accept.status(), reqwest::StatusCode::NOT_ACCEPTABLE);

    let invalid_content_type = client
        .post(&endpoint)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "text/plain")
        .body("{}")
        .send()
        .await?;
    assert_eq!(
        invalid_content_type.status(),
        reqwest::StatusCode::NOT_ACCEPTABLE
    );

    let malformed = client
        .post(&endpoint)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body("{broken")
        .send()
        .await?;
    assert_eq!(malformed.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(!malformed.text().await?.is_empty());

    let oversized = client
        .post(endpoint)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body("x".repeat(1_048_577))
        .send()
        .await?;
    assert_eq!(oversized.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
    Ok(())
}

async fn start_with_options(options: StreamableHttpOptions) -> mcp::Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/mcp", listener.local_addr()?);
    let router = streamable_http_router_with_options(Arc::new(EmptyServer), options);
    tokio::spawn(async move {
        drop(axum::serve(listener, router).await);
    });
    Ok(endpoint)
}

fn raw_post(endpoint: &str) -> reqwest::RequestBuilder {
    reqwest::Client::new()
        .post(endpoint)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", "server/discover")
        .json(&json!({
            "jsonrpc": "2.0", "id": "origin", "method": "server/discover",
            "params": request_params(&Value::Null)
        }))
}
