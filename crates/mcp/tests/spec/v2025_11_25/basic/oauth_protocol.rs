use std::{collections::BTreeMap, net::IpAddr, sync::Arc};

use axum::{
    Router,
    http::{HeaderValue, StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Redirect},
    routing::get,
};
use mcp::{
    McpAuthorizationResponse, McpAuthorizationServerMetadata, McpClientMetadataDocument,
    McpOAuthDestinationPolicy, McpOAuthDiscoveryClient, McpOAuthEndpointPolicy, McpOAuthGrantType,
    McpOAuthResponseType, McpOAuthSecret, McpTokenEndpointAuthMethod, McpTokenResponse,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use url::Url;

fn public_client_metadata(issuer: &str) -> McpAuthorizationServerMetadata {
    serde_json::from_value(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "revocation_endpoint": format!("{issuer}/revoke"),
        "registration_endpoint": format!("{issuer}/register"),
        "jwks_uri": format!("{issuer}/jwks"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "token_endpoint_auth_methods_supported": ["none"],
        "code_challenge_methods_supported": ["S256"],
        "access_token_signing_alg_values_supported": ["RS256"],
        "authorization_response_iss_parameter_supported": true,
        "x-vendor": {"enabled": true}
    }))
    .expect("valid metadata")
}

#[test]
fn authorization_server_metadata_preserves_extensions_and_new_fields() {
    let metadata = public_client_metadata("https://auth.example.com");
    assert_eq!(
        metadata.revocation_endpoint.as_deref(),
        Some("https://auth.example.com/revoke")
    );
    assert_eq!(
        metadata.access_token_signing_alg_values_supported,
        ["RS256"]
    );
    assert_eq!(
        metadata.extra.get("x-vendor"),
        Some(&json!({"enabled": true}))
    );
    let serialized = serde_json::to_value(metadata).expect("serialize metadata");
    assert_eq!(serialized["x-vendor"], json!({"enabled": true}));
}

#[test]
fn public_authorization_code_metadata_validation_applies_rfc_defaults_and_policy() {
    let mut metadata = public_client_metadata("https://auth.example.com");
    metadata.grant_types_supported.clear();
    metadata
        .validate_for_public_authorization_code_client(
            "https://auth.example.com",
            McpOAuthEndpointPolicy::HttpsOnly,
        )
        .expect("RFC 8414 defaults grant types");

    metadata.token_endpoint_auth_methods_supported.clear();
    assert!(
        metadata
            .validate_for_public_authorization_code_client(
                "https://auth.example.com",
                McpOAuthEndpointPolicy::HttpsOnly,
            )
            .is_err()
    );
    let mut metadata = public_client_metadata("http://127.0.0.1:8080");
    assert!(
        metadata
            .validate_for_public_authorization_code_client(
                "http://127.0.0.1:8080",
                McpOAuthEndpointPolicy::HttpsOnly,
            )
            .is_err()
    );
    metadata
        .validate_for_public_authorization_code_client(
            "http://127.0.0.1:8080",
            McpOAuthEndpointPolicy::AllowLoopbackHttp,
        )
        .expect("explicit loopback development policy");
    metadata.token_endpoint = Some("https://user:password@auth.example.com/token".to_owned());
    assert!(
        metadata
            .validate_for_public_authorization_code_client(
                "http://127.0.0.1:8080",
                McpOAuthEndpointPolicy::AllowLoopbackHttp,
            )
            .is_err()
    );
}

#[test]
fn public_authorization_code_metadata_requires_all_capabilities() {
    let base = public_client_metadata("https://auth.example.com");
    for mutate in [
        |metadata: &mut McpAuthorizationServerMetadata| metadata.response_types_supported.clear(),
        |metadata: &mut McpAuthorizationServerMetadata| {
            metadata.code_challenge_methods_supported.clear();
        },
        |metadata: &mut McpAuthorizationServerMetadata| {
            metadata.token_endpoint_auth_methods_supported.clear();
        },
        |metadata: &mut McpAuthorizationServerMetadata| metadata.token_endpoint = None,
    ] {
        let mut metadata = base.clone();
        mutate(&mut metadata);
        assert!(
            metadata
                .validate_for_public_authorization_code_client(
                    "https://auth.example.com",
                    McpOAuthEndpointPolicy::HttpsOnly,
                )
                .is_err()
        );
    }
    let mut metadata = base;
    metadata.registration_endpoint = None;
    metadata.client_id_metadata_document_supported = Some(true);
    metadata
        .validate_for_public_authorization_code_client(
            "https://auth.example.com",
            McpOAuthEndpointPolicy::HttpsOnly,
        )
        .expect("CIMD can replace DCR");
}

#[test]
fn public_authorization_code_metadata_allows_preregistered_clients_only() {
    let mut metadata = public_client_metadata("https://auth.example.com");
    metadata.registration_endpoint = None;
    metadata.client_id_metadata_document_supported = None;

    metadata
        .validate_for_public_authorization_code_client(
            "https://auth.example.com",
            McpOAuthEndpointPolicy::HttpsOnly,
        )
        .expect("preregistered public clients do not require a registration capability");
}

#[test]
fn authorization_response_parser_rejects_duplicates_and_malformed_outcomes() {
    assert!(McpAuthorizationResponse::parse_query("code=a&code=b&state=s").is_err());
    assert!(McpAuthorizationResponse::parse_query("code=a&error=access_denied&state=s").is_err());
    assert!(McpAuthorizationResponse::parse_query("code=a").is_err());
    assert!(McpAuthorizationResponse::parse_query("error=not_standard").is_err());
    assert!(McpAuthorizationResponse::parse_query("code=a&state=s").is_ok());
}

#[test]
fn client_metadata_requires_exact_client_id_and_preserves_extensions() {
    let document = McpClientMetadataDocument {
        client_id: "https://client.example.com/metadata.json".to_owned(),
        redirect_uris: vec!["http://127.0.0.1:4567/callback".to_owned()],
        grant_types: vec![McpOAuthGrantType::AuthorizationCode],
        response_types: vec![McpOAuthResponseType::Code],
        token_endpoint_auth_method: McpTokenEndpointAuthMethod::None,
        client_name: Some("Example native client".to_owned()),
        extra: BTreeMap::from([("x-client-extension".to_owned(), json!(true))]),
    };
    document
        .validate_client_id("https://client.example.com/metadata.json")
        .expect("exact client ID");
    assert!(
        document
            .validate_client_id("https://client.example.com/other.json")
            .is_err()
    );
    assert_eq!(
        serde_json::to_value(document).expect("serialize client metadata")["x-client-extension"],
        true
    );
}

#[test]
fn client_metadata_validation_requires_exact_public_client_profile() {
    let mut document = McpClientMetadataDocument {
        client_id: "https://client.example/metadata".to_owned(),
        redirect_uris: vec!["https://client.example/callback".to_owned()],
        grant_types: vec![
            McpOAuthGrantType::AuthorizationCode,
            McpOAuthGrantType::RefreshToken,
        ],
        response_types: vec![McpOAuthResponseType::Code],
        token_endpoint_auth_method: McpTokenEndpointAuthMethod::None,
        client_name: None,
        extra: BTreeMap::from([("code_challenge_methods".to_owned(), json!(["S256"]))]),
    };
    document
        .validate_public_client("https://client.example/metadata", false)
        .expect("valid CIMD public profile");
    document
        .extra
        .insert("code_challenge_methods".to_owned(), json!(["plain"]));
    assert!(
        document
            .validate_public_client("https://client.example/metadata", false)
            .is_err()
    );
}

#[test]
fn oauth_secrets_are_redacted_from_debug_output() {
    let secret = "a-very-sensitive-token";
    let response = McpTokenResponse {
        access_token: McpOAuthSecret::new(secret),
        token_type: mcp::McpOAuthTokenType::Bearer,
        expires_in: Some(300),
        refresh_token: Some(McpOAuthSecret::new("refresh-sensitive")),
        scope: None,
    };
    let debug = format!("{response:?}");
    assert!(!debug.contains(secret));
    assert!(!debug.contains("refresh-sensitive"));
    assert!(debug.contains("[REDACTED]"));
    assert!(!format!("{:?}", mcp::McpAccessToken(secret.to_owned())).contains(secret));
}

#[tokio::test]
async fn hardened_discovery_supports_path_issuers_and_refuses_redirects() -> mcp::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://{}", listener.local_addr()?);
    let issuer = format!("{origin}/tenant");
    let metadata = serde_json::to_string(&public_client_metadata(&issuer))?;
    let router = Router::new()
        .route(
            "/.well-known/oauth-authorization-server/tenant",
            get(move || {
                let metadata = metadata.clone();
                async move { ([(CONTENT_TYPE, "application/json")], metadata) }
            }),
        )
        .route(
            "/redirect",
            get(|| async { Redirect::temporary("/metadata") }),
        )
        .route(
            "/metadata",
            get(|| async { ([(CONTENT_TYPE, "application/json")], "{}") }),
        );
    let task = tokio::spawn(async move { drop(axum::serve(listener, router).await) });
    let client = loopback_discovery_client()?;
    let discovered = client
        .discover_authorization_server_metadata(&issuer)
        .await?;
    assert_eq!(discovered.issuer, issuer);
    assert!(
        client
            .get_json::<Value>(&format!("{origin}/redirect"))
            .await
            .is_err()
    );
    task.abort();
    Ok(())
}

#[tokio::test]
async fn hardened_discovery_bounds_and_validates_json_responses() -> mcp::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://{}", listener.local_addr()?);
    let router = Router::new()
        .route(
            "/large",
            get(|| async {
                (
                    [(CONTENT_TYPE, "application/json")],
                    format!("{{\"value\":\"{}\"}}", "x".repeat(128)),
                )
            }),
        )
        .route(
            "/malformed",
            get(|| async { ([(CONTENT_TYPE, "application/json")], "{") }),
        )
        .route(
            "/duplicate",
            get(|| async {
                (
                    [(CONTENT_TYPE, "application/json")],
                    "{\"value\":1,\"value\":2}",
                )
            }),
        )
        .route(
            "/wrong-type",
            get(|| async { ([(CONTENT_TYPE, "text/plain")], "{}") }),
        )
        .route(
            "/secret-body",
            get(|| async {
                let mut response = (
                    StatusCode::BAD_REQUEST,
                    "response-body-secret-must-not-leak",
                )
                    .into_response();
                response
                    .headers_mut()
                    .insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
                response
            }),
        );
    let task = tokio::spawn(async move { drop(axum::serve(listener, router).await) });
    let client = loopback_discovery_client()?.with_max_response_bytes(32);
    assert!(
        client
            .get_json::<Value>(&format!("{origin}/large"))
            .await
            .is_err()
    );
    assert!(
        client
            .get_json::<Value>(&format!("{origin}/malformed"))
            .await
            .is_err()
    );
    assert!(
        client
            .get_json::<Value>(&format!("{origin}/duplicate"))
            .await
            .is_err()
    );
    assert!(
        client
            .get_json::<Value>(&format!("{origin}/wrong-type"))
            .await
            .is_err()
    );
    let error = client
        .get_json::<Value>(&format!("{origin}/secret-body"))
        .await
        .expect_err("error response");
    assert!(!format!("{error:?} {error}").contains("response-body-secret"));
    task.abort();
    Ok(())
}

#[derive(Debug)]
struct RejectAllDestinations;

impl McpOAuthDestinationPolicy for RejectAllDestinations {
    fn validate(&self, _destination: &Url) -> mcp::Result<()> {
        Err(mcp::Error::protocol(
            "destination rejected by caller policy",
        ))
    }
}

#[derive(Debug)]
struct AllowLoopbackDestinations;

impl McpOAuthDestinationPolicy for AllowLoopbackDestinations {
    fn validate(&self, _destination: &Url) -> mcp::Result<()> {
        Ok(())
    }

    fn validate_address(&self, address: IpAddr) -> mcp::Result<()> {
        if address.is_loopback() {
            Ok(())
        } else {
            Err(mcp::Error::protocol("test destination is not loopback"))
        }
    }
}

fn loopback_discovery_client() -> mcp::Result<McpOAuthDiscoveryClient> {
    Ok(
        McpOAuthDiscoveryClient::new(McpOAuthEndpointPolicy::AllowLoopbackHttp)?
            .with_destination_policy(Arc::new(AllowLoopbackDestinations)),
    )
}

#[tokio::test]
async fn hardened_discovery_enforces_scheme_and_destination_policy_before_io() -> mcp::Result<()> {
    let https_only = McpOAuthDiscoveryClient::new(McpOAuthEndpointPolicy::HttpsOnly)?;
    assert!(
        https_only
            .get_json::<Value>("http://127.0.0.1:1/metadata")
            .await
            .is_err()
    );
    let loopback_without_address_policy =
        McpOAuthDiscoveryClient::new(McpOAuthEndpointPolicy::AllowLoopbackHttp)?;
    assert!(
        loopback_without_address_policy
            .get_json::<Value>("http://127.0.0.1:1/metadata")
            .await
            .is_err()
    );
    assert!(
        https_only
            .get_json::<Value>("https://user:secret@example.com/metadata")
            .await
            .is_err()
    );
    assert!(
        https_only
            .get_json::<Value>("https://example.com/metadata#fragment")
            .await
            .is_err()
    );

    let destination_restricted =
        https_only.with_destination_policy(Arc::new(RejectAllDestinations));
    let error = destination_restricted
        .get_json::<Value>("https://example.com/metadata")
        .await
        .expect_err("caller destination policy rejects before request");
    assert!(error.to_string().contains("destination rejected"));
    Ok(())
}
