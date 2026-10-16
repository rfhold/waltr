//! Focused hosted OAuth authorization-server HTTP flow coverage.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use mcp::{oauth::*, server::BoxFuture};
use reqwest::{StatusCode, redirect::Policy};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio::{net::TcpListener, sync::Mutex};
use url::Url;

const ISSUER: &str = "https://authorization.example/oauth";
const RESOURCE: &str = "https://resource.example/mcp";
const REDIRECT: &str = "https://client.example/callback";
const VERIFIER: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";

#[test]
fn hosted_server_rejects_invalid_signing_overlap_relationships() {
    let mut short_verification = test_config();
    short_verification.signing_verification_overlap = (short_verification.access_token_lifetime
        + short_verification.clock_skew)
        .checked_sub(Duration::from_millis(1))
        .unwrap();
    assert!(
        construction_error(short_verification)
            .contains("verification overlap must cover access-token lifetime plus clock skew")
    );

    let mut long_private = test_config();
    long_private.signing_private_key_overlap =
        long_private.signing_verification_overlap + Duration::from_millis(1);
    assert!(
        construction_error(long_private)
            .contains("private-key overlap must not exceed verification overlap")
    );
}

#[tokio::test]
async fn preregistration_accepts_only_secretless_public_clients() {
    let store = Arc::new(InMemoryOAuthAuthorizationStore::default());
    let server = server_with_hooks(store, Arc::new(Authenticated), Arc::new(ApprovedConsent)).await;
    let mut client = OAuthClient {
        client_id: "preregistered".to_owned(),
        registration_method: OAuthClientRegistrationMethod::Preconfigured,
        display_name: Some("Preregistered client".to_owned()),
        client_secret_digest: Some(vec![7; 32]),
        redirect_uris: vec![REDIRECT.to_owned()],
        created_at_ms: unix_ms(),
        expires_at_ms: None,
        disabled_at_ms: None,
    };
    assert!(
        server
            .preregister_public_client(client.clone(), false)
            .await
            .is_err()
    );
    client.client_secret_digest = None;
    server
        .preregister_public_client(client.clone(), false)
        .await
        .unwrap();
    assert!(
        server
            .preregister_public_client(client, false)
            .await
            .is_err()
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn hosted_authorization_code_refresh_replay_and_revocation_flow() {
    let store = Arc::new(InMemoryOAuthAuthorizationStore::default());
    let server = test_server(store.clone()).await;
    let (origin, task) = serve(server.router()).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    let metadata = client
        .get(format!(
            "{origin}/.well-known/oauth-authorization-server/oauth"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(metadata.status(), StatusCode::OK);
    assert_eq!(metadata.headers()["cache-control"], "no-store");
    let metadata: McpAuthorizationServerMetadata = metadata.json().await.unwrap();
    assert_eq!(metadata.issuer, ISSUER);
    assert_eq!(metadata.code_challenge_methods_supported, ["S256"]);

    let jwks: Value = client
        .get(format!("{origin}/oauth/jwks.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(jwks["keys"].as_array().unwrap().len(), 1);
    assert_eq!(jwks["keys"][0]["alg"], "ES256");

    let duplicate = client
        .get(format!(
            "{origin}/oauth/authorize?client_id=client&client_id=attacker"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(duplicate.status(), StatusCode::BAD_REQUEST);
    assert!(duplicate.headers().get("location").is_none());

    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes()));
    let mut authorization_url = Url::parse(&format!("{origin}/oauth/authorize")).unwrap();
    authorization_url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", "client"),
        ("redirect_uri", REDIRECT),
        ("scope", "files:read files:write"),
        ("state", "request-state"),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("resource", RESOURCE),
    ]);
    let authorization = client.get(authorization_url).send().await.unwrap();
    assert_eq!(authorization.status(), StatusCode::SEE_OTHER);
    let redirect = Url::parse(authorization.headers()["location"].to_str().unwrap()).unwrap();
    assert_eq!(
        redirect.origin().ascii_serialization(),
        "https://client.example"
    );
    let parameters = redirect
        .query_pairs()
        .into_owned()
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(parameters["state"], "request-state");
    assert_eq!(parameters["iss"], ISSUER);
    let code = parameters["code"].clone();

    let wrong_pkce = token_request(
        &client,
        &origin,
        &code,
        "X23456789abcdefghijklmnopqrstuvwxyzABCDEFG",
    )
    .await;
    assert_eq!(wrong_pkce.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        wrong_pkce.json::<Value>().await.unwrap()["error"],
        "invalid_grant"
    );

    let issued = token_request(&client, &origin, &code, VERIFIER).await;
    assert_eq!(issued.status(), StatusCode::OK);
    assert_eq!(issued.headers()["cache-control"], "no-store");
    let issued: McpTokenResponse = issued.json().await.unwrap();
    let access = issued.access_token.expose_secret().to_owned();
    let refresh = issued.refresh_token.unwrap().expose_secret().to_owned();
    assert_jwt(&access);

    let validation = server
        .authorize_token(
            McpAccessToken(access.clone()),
            mcp::server::McpAuthorizationContext {
                resource: RESOURCE.to_owned(),
                required_scopes: vec!["files:read".to_owned()],
            },
        )
        .await;
    assert!(matches!(
        validation,
        mcp::server::McpHostedTokenValidation::Authorized(_)
    ));

    let rotated = refresh_request(&client, &origin, &refresh, Some("files:read")).await;
    assert_eq!(rotated.status(), StatusCode::OK);
    let rotated: McpTokenResponse = rotated.json().await.unwrap();
    let rotated_access = rotated.access_token.expose_secret().to_owned();

    let replay = refresh_request(&client, &origin, &refresh, None).await;
    assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
    let revoked = server
        .authorize_token(
            McpAccessToken(rotated_access),
            mcp::server::McpAuthorizationContext {
                resource: RESOURCE.to_owned(),
                required_scopes: vec!["files:read".to_owned()],
            },
        )
        .await;
    assert!(matches!(
        revoked,
        mcp::server::McpHostedTokenValidation::Unauthorized { .. }
    ));

    for token in ["unknown", access.as_str()] {
        let response = client
            .post(format!("{origin}/oauth/revoke"))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(form_body(&[("token", token), ("client_id", "client")]))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.bytes().await.unwrap().is_empty());
    }
    task.abort();
}

#[tokio::test]
async fn login_resume_and_consent_denial_use_opaque_transaction() {
    const NATIVE_REDIRECT: &str = "http://127.0.0.1:49152/callback";
    let store = Arc::new(InMemoryOAuthAuthorizationStore::default());
    register_client(&store).await;
    let authenticator = Arc::new(LoginAuthenticator::default());
    let server = server_with_hooks(store, authenticator.clone(), Arc::new(PresentConsent)).await;
    let (origin, task) = serve(server.router()).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes()));
    let mut authorization_url = Url::parse(&format!("{origin}/oauth/authorize")).unwrap();
    authorization_url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", "client"),
        ("redirect_uri", NATIVE_REDIRECT),
        ("scope", "files:read"),
        ("state", "denial-state"),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("resource", RESOURCE),
    ]);
    let login = client.get(authorization_url).send().await.unwrap();
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    let correlation_cookie = login.headers()["set-cookie"].to_str().unwrap();
    assert_correlation_cookie(correlation_cookie);
    let continuation = Url::parse(login.headers()["location"].to_str().unwrap()).unwrap();
    assert_eq!(continuation.query_pairs().count(), 1);
    assert!(
        continuation
            .query_pairs()
            .any(|(name, _)| name == "transaction_id")
    );
    assert!(!continuation.as_str().contains("correlation"));

    let resumed = client
        .get(format!(
            "{origin}{}?{}",
            continuation.path(),
            continuation.query().unwrap()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resumed.status(), StatusCode::OK);
    assert_eq!(resumed.text().await.unwrap(), "consent");
    let transaction_id = continuation
        .query_pairs()
        .find(|(name, _)| name == "transaction_id")
        .unwrap()
        .1
        .into_owned();
    let denied = client
        .post(format!("{origin}/oauth/authorize/decision"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body(&[
            ("transaction_id", &transaction_id),
            ("decision", "deny"),
            ("csrf", "valid"),
        ]))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::SEE_OTHER);
    let denied = Url::parse(denied.headers()["location"].to_str().unwrap()).unwrap();
    assert_eq!(denied.port(), Some(49152));
    assert_eq!(denied.host_str(), Some("127.0.0.1"));
    assert_eq!(denied.path(), "/callback");
    let parameters = denied
        .query_pairs()
        .into_owned()
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(parameters["error"], "access_denied");
    assert_eq!(parameters["state"], "denial-state");

    let replay = client
        .get(format!(
            "{origin}{}?transaction_id={transaction_id}",
            continuation.path()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
    task.abort();
}

#[tokio::test]
async fn resumed_atomic_false_never_reaches_consent_or_clears_cookie() {
    let store = Arc::new(InMemoryOAuthAuthorizationStore::default());
    register_client(&store).await;
    let consent = Arc::new(CountingConsent::default());
    let server = server_with_hooks(
        store,
        Arc::new(MismatchedLoginAuthenticator::default()),
        consent.clone(),
    )
    .await;
    let (origin, task) = serve(server.router()).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes()));
    let mut authorization_url = Url::parse(&format!("{origin}/oauth/authorize")).unwrap();
    authorization_url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", "client"),
        ("redirect_uri", REDIRECT),
        ("scope", "files:read"),
        ("state", "atomic-false"),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("resource", RESOURCE),
    ]);
    let login = client.get(authorization_url).send().await.unwrap();
    let continuation = Url::parse(login.headers()["location"].to_str().unwrap()).unwrap();
    let resumed = client
        .get(format!(
            "{origin}{}?{}",
            continuation.path(),
            continuation.query().unwrap()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resumed.status(), StatusCode::BAD_REQUEST);
    assert!(resumed.headers().get("set-cookie").is_none());
    assert_eq!(consent.presentations.load(Ordering::SeqCst), 0);
    task.abort();
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "DCR rejection, registration, and authorization form one end-to-end flow"
)]
async fn dcr_advertises_registers_and_completes_public_authorization_flow() {
    let store = Arc::new(InMemoryOAuthAuthorizationStore::default());
    let mut config = OAuthAuthorizationServerConfig::new(
        ISSUER,
        vec![OAuthResource {
            resource: RESOURCE.to_owned(),
            scopes: vec!["files:read".to_owned()],
        }],
    );
    config.registration_endpoint = Some(format!("{ISSUER}/register"));
    let server = OAuthAuthorizationServer::new(
        config,
        store.clone(),
        Arc::new(Authenticated),
        Arc::new(ApprovedConsent),
        Arc::new(McpSystemOAuthClock),
        Arc::new(CounterEntropy::default()),
        Arc::new(
            VersionedOAuthWrappingKeyring::new("test", [("test".to_owned(), vec![7; 32])]).unwrap(),
        ),
    )
    .unwrap()
    .with_client_registration(OAuthClientRegistrationOptions {
        dynamic_registration: true,
        allow_loopback_redirects: true,
        ..OAuthClientRegistrationOptions::default()
    })
    .unwrap();
    server.generate_signing_key_candidate(true).await.unwrap();
    let (origin, task) = serve(server.router()).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();
    let metadata: McpAuthorizationServerMetadata = client
        .get(format!(
            "{origin}/.well-known/oauth-authorization-server/oauth"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        metadata.registration_endpoint.as_deref(),
        Some("https://authorization.example/oauth/register")
    );
    assert_eq!(metadata.client_id_metadata_document_supported, None);

    for body in [
        r#"{"redirect_uris":["https://client.example/callback"],"redirect_uris":["https://attacker.example/callback"],"grant_types":["authorization_code","refresh_token"],"response_types":["code"],"token_endpoint_auth_method":"none"}"#,
        r#"{"redirect_uris":["https://client.example/callback"],"grant_types":["authorization_code","refresh_token"],"response_types":["code"],"token_endpoint_auth_method":"none","client_secret":"must-not-be-accepted"}"#,
    ] {
        let rejected = client
            .post(format!("{origin}/oauth/register"))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
        assert!(
            !rejected
                .text()
                .await
                .unwrap()
                .contains("must-not-be-accepted")
        );
    }

    let registration = client
        .post(format!("{origin}/oauth/register"))
        .json(&McpPublicClientRegistrationRequest {
            application_type: mcp::McpOAuthApplicationType::Web,
            redirect_uris: vec![REDIRECT.to_owned(), REDIRECT.to_owned()],
            grant_types: vec![
                McpOAuthGrantType::RefreshToken,
                McpOAuthGrantType::AuthorizationCode,
            ],
            response_types: vec![McpOAuthResponseType::Code],
            token_endpoint_auth_method: McpTokenEndpointAuthMethod::None,
            client_name: Some("DCR client".to_owned()),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(registration.status(), StatusCode::CREATED);
    let registration: McpClientRegistrationResponse = registration.json().await.unwrap();
    assert!(registration.registration_access_token.is_none());
    assert!(registration.client_secret.is_none());
    assert_eq!(registration.redirect_uris, [REDIRECT]);

    let code = authorize_client(&client, &origin, &registration.client_id).await;
    let issued = token_request_for_client(&client, &origin, &code, &registration.client_id).await;
    assert_eq!(issued.status(), StatusCode::OK);
    let native = client
        .post(format!("{origin}/oauth/register"))
        .json(&serde_json::json!({
            "application_type": "native",
            "redirect_uris": ["http://127.0.0.1/callback"],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(native.status(), StatusCode::CREATED);
    let native: McpClientRegistrationResponse = native.json().await.unwrap();
    let redirect = "http://127.0.0.1:49200/callback";
    let code = authorize_client_redirect(&client, &origin, &native.client_id, redirect).await;
    assert_eq!(
        token_request_redirect(&client, &origin, &code, &native.client_id, redirect)
            .await
            .status(),
        StatusCode::OK
    );
    task.abort();
}

#[tokio::test]
async fn preregistered_loopback_port_exception_preserves_other_components() {
    let store = Arc::new(InMemoryOAuthAuthorizationStore::default());
    let server = server_with_hooks(store, Arc::new(Authenticated), Arc::new(ApprovedConsent)).await;
    let registered = "http://127.0.0.1:49000/callback?fixed=1";
    let native = OAuthClient {
        client_id: "native".to_owned(),
        registration_method: OAuthClientRegistrationMethod::Preconfigured,
        display_name: None,
        client_secret_digest: None,
        redirect_uris: vec![registered.to_owned()],
        created_at_ms: unix_ms(),
        expires_at_ms: None,
        disabled_at_ms: None,
    };
    assert!(
        server
            .preregister_public_client(native.clone(), false)
            .await
            .is_err()
    );
    server
        .preregister_public_client(native, true)
        .await
        .unwrap();
    let (origin, task) = serve(server.router()).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();
    for redirect in [
        "http://127.0.0.1:49152/callback?fixed=2",
        "http://127.0.0.1:49152/callback",
        "http://127.0.0.1:49152/other?fixed=1",
        "http://127.0.0.1:49152/a/../callback?fixed=1",
        "http://localhost:49152/callback?fixed=1",
        "https://127.0.0.1:49152/callback?fixed=1",
    ] {
        assert_eq!(
            authorization_response_redirect(&client, &origin, "native", redirect)
                .await
                .status(),
            StatusCode::BAD_REQUEST,
            "{redirect}"
        );
    }
    for redirect in [
        "http://127.0.0.1:49152/callback?fixed=1",
        "http://127.0.0.1/callback?fixed=1",
    ] {
        let code = authorize_client_redirect(&client, &origin, "native", redirect).await;
        assert_eq!(
            token_request_redirect(&client, &origin, &code, "native", redirect)
                .await
                .status(),
            StatusCode::OK
        );
    }
    task.abort();
}

#[tokio::test]
async fn hardened_cimd_fetcher_refuses_redirect_type_size_and_prohibited_destinations() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let router = axum::Router::new()
        .route(
            "/redirect",
            axum::routing::get(|| async { axum::response::Redirect::temporary("/document") }),
        )
        .route(
            "/wrong-type",
            axum::routing::get(|| async { ([("content-type", "text/plain")], "{}") }),
        )
        .route(
            "/large",
            axum::routing::get(|| async {
                ([("content-type", "application/json")], "x".repeat(512))
            }),
        );
    let task = tokio::spawn(async move { drop(axum::serve(listener, router).await) });
    let fetcher = HardenedOAuthClientMetadataFetcher::loopback_for_tests().with_bounds(
        64,
        Duration::from_secs(1),
        Duration::from_secs(2),
    );
    for path in ["redirect", "wrong-type", "large"] {
        assert!(fetcher.fetch(format!("{origin}/{path}")).await.is_err());
    }
    let policy = PublicOAuthCimdDestinationPolicy;
    for address in [
        "https://127.0.0.1/client.json",
        "https://10.0.0.1/client.json",
        "https://169.254.1.1/client.json",
        "https://169.254.169.254/latest/meta-data",
        "https://224.0.0.1/client.json",
        "https://192.88.99.1/client.json",
        "https://[::1]/client.json",
        "https://[fe80::1]/client.json",
        "https://[2001::1]/client.json",
        "https://[2001:2::1]/client.json",
        "https://[2001:db8::1]/client.json",
        "https://[2002:c000:0201::1]/client.json",
        "https://[3fff::1]/client.json",
        "https://[64:ff9b:1::1]/client.json",
    ] {
        assert!(policy.resolve(Url::parse(address).unwrap()).await.is_err());
    }
    for address in [
        "https://8.8.8.8/client.json",
        "https://[2606:4700:4700::1111]/client.json",
    ] {
        assert!(
            policy.resolve(Url::parse(address).unwrap()).await.is_ok(),
            "{address}"
        );
    }
    task.abort();
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn cimd_snapshot_survives_document_mutation_through_code_exchange() {
    const NATIVE_REDIRECT: &str = "http://127.0.0.1:49152/callback";
    let version = Arc::new(AtomicU8::new(0));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let client_id = format!("{origin}/client.json");
    let initial_document: mcp::McpClientMetadataDocument =
        serde_json::from_value(serde_json::json!({
            "client_id": client_id,
            "client_name": "Snapshot client",
            "application_type": "native",
            "redirect_uris": ["http://127.0.0.1/callback", "http://localhost/callback"],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none"
        }))
        .unwrap();
    initial_document
        .validate_public_client(&client_id, true)
        .unwrap();
    let document_client_id = client_id.clone();
    let version_for_route = version.clone();
    let router = axum::Router::new().route(
        "/client.json",
        axum::routing::get(move || {
            let client_id = document_client_id.clone();
            let version = version_for_route.clone();
            async move {
                let redirect = if version.load(Ordering::Relaxed) == 0 {
                    "http://127.0.0.1/callback"
                } else {
                    "https://attacker.example/callback"
                };
                (
                    [
                        ("content-type", "application/json"),
                        ("cache-control", "max-age=1"),
                    ],
                    serde_json::json!({
                        "client_id": client_id,
                        "client_name": "Snapshot client",
                        "application_type": "native",
                        "redirect_uris": [redirect],
                        "grant_types": ["authorization_code", "refresh_token"],
                        "response_types": ["code"],
                        "token_endpoint_auth_method": "none"
                    })
                    .to_string(),
                )
            }
        }),
    );
    let fixture = tokio::spawn(async move { drop(axum::serve(listener, router).await) });
    let store = Arc::new(InMemoryOAuthAuthorizationStore::default());
    let mut config = OAuthAuthorizationServerConfig::new(
        ISSUER,
        vec![OAuthResource {
            resource: RESOURCE.to_owned(),
            scopes: vec!["files:read".to_owned()],
        }],
    );
    config.registration_endpoint = Some(format!("{ISSUER}/register"));
    let server = OAuthAuthorizationServer::new(
        config,
        store.clone(),
        Arc::new(Authenticated),
        Arc::new(ApprovedConsent),
        Arc::new(McpSystemOAuthClock),
        Arc::new(CounterEntropy::default()),
        Arc::new(
            VersionedOAuthWrappingKeyring::new("test", [("test".to_owned(), vec![7; 32])]).unwrap(),
        ),
    )
    .unwrap()
    .with_client_registration(OAuthClientRegistrationOptions {
        metadata_fetcher: Some(Arc::new(
            HardenedOAuthClientMetadataFetcher::loopback_for_tests().with_loopback_redirects(true),
        )),
        allow_loopback_redirects: true,
        ..OAuthClientRegistrationOptions::default()
    })
    .unwrap();
    server.generate_signing_key_candidate(true).await.unwrap();
    let (authorization_origin, task) = serve(server.router()).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();
    for redirect in [
        "http://localhost:49152/callback",
        "http://127.0.0.2:49152/callback",
        "http://127.0.0.1:49152/other",
        "http://127.0.0.1:49152/callback?extra=1",
        "http://127.0.0.1:49152/callback#fragment",
        "http://user@127.0.0.1:49152/callback",
        "http://example.com/callback",
    ] {
        assert_eq!(
            authorization_response_redirect(&client, &authorization_origin, &client_id, redirect)
                .await
                .status(),
            StatusCode::BAD_REQUEST,
            "{redirect}"
        );
    }
    for method in [None, Some("plain")] {
        let mut url = Url::parse(&format!("{authorization_origin}/oauth/authorize")).unwrap();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes()));
        url.query_pairs_mut().extend_pairs([
            ("response_type", "code"),
            ("client_id", client_id.as_str()),
            ("redirect_uri", NATIVE_REDIRECT),
            ("scope", "files:read"),
            ("state", "pkce-state"),
            ("code_challenge", challenge.as_str()),
            ("resource", RESOURCE),
        ]);
        if let Some(method) = method {
            url.query_pairs_mut()
                .append_pair("code_challenge_method", method);
        }
        let rejected = client.get(url).send().await.unwrap();
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
        assert!(rejected.headers().get("location").is_none());
    }
    let code =
        authorize_client_redirect(&client, &authorization_origin, &client_id, NATIVE_REDIRECT)
            .await;
    let first_snapshot = store
        .client_metadata(client_id.clone(), unix_ms())
        .await
        .unwrap()
        .unwrap();
    version.store(1, Ordering::Relaxed);
    let cached_code =
        authorize_client_redirect(&client, &authorization_origin, &client_id, NATIVE_REDIRECT)
            .await;
    assert_eq!(
        token_request_redirect(
            &client,
            &authorization_origin,
            &code,
            &client_id,
            "http://127.0.0.1:49153/callback"
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    let issued = token_request_redirect(
        &client,
        &authorization_origin,
        &code,
        &client_id,
        NATIVE_REDIRECT,
    )
    .await;
    assert_eq!(issued.status(), StatusCode::OK);
    let issued: McpTokenResponse = issued.json().await.unwrap();
    let refresh = client
        .post(format!("{authorization_origin}/oauth/token"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body(&[
            ("grant_type", "refresh_token"),
            (
                "refresh_token",
                issued.refresh_token.unwrap().expose_secret(),
            ),
            ("client_id", &client_id),
            ("resource", RESOURCE),
        ]))
        .send()
        .await
        .unwrap();
    assert_eq!(refresh.status(), StatusCode::OK);
    assert_eq!(
        token_request_redirect(
            &client,
            &authorization_origin,
            &cached_code,
            &client_id,
            NATIVE_REDIRECT
        )
        .await
        .status(),
        StatusCode::OK
    );
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert_eq!(
        authorization_response_redirect(
            &client,
            &authorization_origin,
            &client_id,
            NATIVE_REDIRECT
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    let changed_snapshot = store
        .client_metadata(client_id.clone(), unix_ms())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(first_snapshot.snapshot_id, changed_snapshot.snapshot_id);
    fixture.abort();
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert_eq!(
        authorization_response_redirect(
            &client,
            &authorization_origin,
            &client_id,
            NATIVE_REDIRECT
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    task.abort();
}

async fn test_server(store: Arc<InMemoryOAuthAuthorizationStore>) -> OAuthAuthorizationServer {
    register_client(&store).await;
    server_with_hooks(store, Arc::new(Authenticated), Arc::new(ApprovedConsent)).await
}

async fn register_client(store: &Arc<InMemoryOAuthAuthorizationStore>) {
    let now = unix_ms();
    store
        .register_client(OAuthClient {
            client_id: "client".to_owned(),
            registration_method: OAuthClientRegistrationMethod::Preconfigured,
            display_name: Some("Test client".to_owned()),
            client_secret_digest: None,
            redirect_uris: vec![REDIRECT.to_owned(), "http://127.0.0.1/callback".to_owned()],
            created_at_ms: now,
            expires_at_ms: None,
            disabled_at_ms: None,
        })
        .await
        .unwrap();
}

async fn server_with_hooks(
    store: Arc<InMemoryOAuthAuthorizationStore>,
    authenticator: Arc<dyn OAuthResourceOwnerAuthenticator>,
    consent: Arc<dyn OAuthConsentHandler>,
) -> OAuthAuthorizationServer {
    let entropy = Arc::new(CounterEntropy::default());
    let keyring = Arc::new(
        VersionedOAuthWrappingKeyring::new("test", [("test".to_owned(), vec![7; 32])]).unwrap(),
    );
    let server = OAuthAuthorizationServer::new(
        OAuthAuthorizationServerConfig::new(
            ISSUER,
            vec![OAuthResource {
                resource: RESOURCE.to_owned(),
                scopes: vec!["files:read".to_owned(), "files:write".to_owned()],
            }],
        ),
        store,
        authenticator,
        consent,
        Arc::new(McpSystemOAuthClock),
        entropy,
        keyring,
    )
    .unwrap();
    server.generate_signing_key_candidate(true).await.unwrap();
    server
}

fn test_config() -> OAuthAuthorizationServerConfig {
    OAuthAuthorizationServerConfig::new(
        ISSUER,
        vec![OAuthResource {
            resource: RESOURCE.to_owned(),
            scopes: vec!["files:read".to_owned()],
        }],
    )
}

fn construction_error(config: OAuthAuthorizationServerConfig) -> String {
    OAuthAuthorizationServer::new(
        config,
        Arc::new(InMemoryOAuthAuthorizationStore::default()),
        Arc::new(Authenticated),
        Arc::new(ApprovedConsent),
        Arc::new(McpSystemOAuthClock),
        Arc::new(CounterEntropy::default()),
        Arc::new(
            VersionedOAuthWrappingKeyring::new("test", [("test".to_owned(), vec![7; 32])]).unwrap(),
        ),
    )
    .unwrap_err()
    .to_string()
}

async fn serve(router: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{address}"), task)
}

async fn authorize_client(client: &reqwest::Client, origin: &str, client_id: &str) -> String {
    authorize_client_redirect(client, origin, client_id, REDIRECT).await
}

async fn authorize_client_redirect(
    client: &reqwest::Client,
    origin: &str,
    client_id: &str,
    redirect: &str,
) -> String {
    let response = authorization_response_redirect(client, origin, client_id, redirect).await;
    if response.status() != StatusCode::SEE_OTHER {
        let status = response.status();
        let body = response.text().await.unwrap();
        panic!("authorization failed with {status}: {body}");
    }
    Url::parse(response.headers()["location"].to_str().unwrap())
        .unwrap()
        .query_pairs()
        .find(|(name, _)| name == "code")
        .unwrap()
        .1
        .into_owned()
}

async fn authorization_response_redirect(
    client: &reqwest::Client,
    origin: &str,
    client_id: &str,
    redirect: &str,
) -> reqwest::Response {
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes()));
    let mut authorization_url = Url::parse(&format!("{origin}/oauth/authorize")).unwrap();
    authorization_url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", redirect),
        ("scope", "files:read"),
        ("state", "registration-state"),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("resource", RESOURCE),
    ]);
    client.get(authorization_url).send().await.unwrap()
}

async fn token_request_for_client(
    client: &reqwest::Client,
    origin: &str,
    code: &str,
    client_id: &str,
) -> reqwest::Response {
    token_request_redirect(client, origin, code, client_id, REDIRECT).await
}

async fn token_request_redirect(
    client: &reqwest::Client,
    origin: &str,
    code: &str,
    client_id: &str,
    redirect: &str,
) -> reqwest::Response {
    client
        .post(format!("{origin}/oauth/token"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect),
            ("code_verifier", VERIFIER),
            ("client_id", client_id),
            ("resource", RESOURCE),
        ]))
        .send()
        .await
        .unwrap()
}

async fn token_request(
    client: &reqwest::Client,
    origin: &str,
    code: &str,
    verifier: &str,
) -> reqwest::Response {
    client
        .post(format!("{origin}/oauth/token"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", REDIRECT),
            ("code_verifier", verifier),
            ("client_id", "client"),
            ("resource", RESOURCE),
        ]))
        .send()
        .await
        .unwrap()
}

async fn refresh_request(
    client: &reqwest::Client,
    origin: &str,
    refresh: &str,
    scope: Option<&str>,
) -> reqwest::Response {
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh),
        ("client_id", "client"),
        ("resource", RESOURCE),
    ];
    if let Some(scope) = scope {
        form.push(("scope", scope));
    }
    client
        .post(format!("{origin}/oauth/token"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body(&form))
        .send()
        .await
        .unwrap()
}

fn form_body(values: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(values.iter().copied())
        .finish()
}

fn assert_jwt(token: &str) {
    let pieces = token.split('.').collect::<Vec<_>>();
    assert_eq!(pieces.len(), 3);
    let header: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(pieces[0]).unwrap()).unwrap();
    let claims: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(pieces[1]).unwrap()).unwrap();
    assert_eq!(header["alg"], "ES256");
    assert_eq!(header["typ"], "at+jwt");
    assert_eq!(claims["iss"], ISSUER);
    assert_eq!(claims["aud"], RESOURCE);
    assert_eq!(claims["client_id"], "client");
    assert!(claims["nbf"].as_u64().is_some());
    assert!(claims["grant_id"].as_str().is_some());
}

fn unix_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn assert_correlation_cookie(cookie: &str) {
    assert!(cookie.starts_with("__Host-mcp-oidc-"));
    for attribute in [
        "; Secure",
        "; HttpOnly",
        "; SameSite=Lax",
        "; Path=/",
        "; Max-Age=60",
    ] {
        assert!(cookie.contains(attribute), "missing {attribute}");
    }
}

struct Authenticated;
impl OAuthResourceOwnerAuthenticator for Authenticated {
    fn authenticate(
        &self,
        _: OAuthResourceOwnerAuthenticationRequest,
    ) -> BoxFuture<OAuthResourceOwnerAuthentication> {
        Box::pin(async {
            OAuthResourceOwnerAuthentication::Authenticated {
                principal_id: McpPrincipalId::new("alice").unwrap(),
                browser_correlation: None,
            }
        })
    }
}

struct ApprovedConsent;
impl OAuthConsentHandler for ApprovedConsent {
    fn present(&self, _: OAuthConsentModel) -> BoxFuture<OAuthConsentPresentation> {
        Box::pin(async { OAuthConsentPresentation::Approved })
    }
    fn validate_decision(&self, _: OAuthConsentDecisionEvidence) -> BoxFuture<bool> {
        Box::pin(async { true })
    }
}

#[derive(Default)]
struct LoginAuthenticator {
    correlation: Arc<Mutex<Option<String>>>,
}
impl OAuthResourceOwnerAuthenticator for LoginAuthenticator {
    fn authenticate(
        &self,
        request: OAuthResourceOwnerAuthenticationRequest,
    ) -> BoxFuture<OAuthResourceOwnerAuthentication> {
        let correlation = self.correlation.clone();
        Box::pin(async move {
            let mut stored = correlation.lock().await;
            if stored.is_none() {
                *stored = Some(request.browser_correlation.expose_secret().to_owned());
                OAuthResourceOwnerAuthentication::LoginRequired {
                    login_url: request.continuation_url,
                }
            } else {
                OAuthResourceOwnerAuthentication::Authenticated {
                    principal_id: McpPrincipalId::new("alice").unwrap(),
                    browser_correlation: stored.take().map(McpOAuthSecret::new),
                }
            }
        })
    }

    fn browser_correlation_cookie(
        &self,
        transaction_id: &str,
        correlation: &McpOAuthSecret,
    ) -> Option<OAuthBrowserCorrelationCookie> {
        OAuthBrowserCorrelationCookie::new(
            &format!(
                "__Host-mcp-oidc-{transaction_id}={}; Secure; HttpOnly; SameSite=Lax; Path=/; Max-Age=60",
                correlation.expose_secret()
            ),
            true,
        )
        .ok()
    }
}

struct PresentConsent;
impl OAuthConsentHandler for PresentConsent {
    fn present(&self, _: OAuthConsentModel) -> BoxFuture<OAuthConsentPresentation> {
        use axum::response::IntoResponse as _;
        Box::pin(async { OAuthConsentPresentation::Response("consent".into_response()) })
    }
    fn validate_decision(&self, evidence: OAuthConsentDecisionEvidence) -> BoxFuture<bool> {
        Box::pin(async move { evidence.csrf.expose_secret() == "valid" })
    }
}

#[derive(Default)]
struct MismatchedLoginAuthenticator(AtomicU8);
impl OAuthResourceOwnerAuthenticator for MismatchedLoginAuthenticator {
    fn authenticate(
        &self,
        request: OAuthResourceOwnerAuthenticationRequest,
    ) -> BoxFuture<OAuthResourceOwnerAuthentication> {
        let initial = self.0.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            if initial {
                OAuthResourceOwnerAuthentication::LoginRequired {
                    login_url: request.continuation_url,
                }
            } else {
                OAuthResourceOwnerAuthentication::Authenticated {
                    principal_id: McpPrincipalId::new("alice").unwrap(),
                    browser_correlation: Some(McpOAuthSecret::new("wrong-correlation")),
                }
            }
        })
    }
}

#[derive(Default)]
struct CountingConsent {
    presentations: AtomicUsize,
}
impl OAuthConsentHandler for CountingConsent {
    fn present(&self, _: OAuthConsentModel) -> BoxFuture<OAuthConsentPresentation> {
        self.presentations.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { OAuthConsentPresentation::Approved })
    }
    fn validate_decision(&self, _: OAuthConsentDecisionEvidence) -> BoxFuture<bool> {
        Box::pin(async { false })
    }
}

#[derive(Default)]
struct CounterEntropy(AtomicU8);
impl McpOAuthEntropy for CounterEntropy {
    fn fill_bytes(&self, output: &mut [u8]) -> mcp::Result<()> {
        let value = self.0.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        output.fill(value);
        Ok(())
    }
}
