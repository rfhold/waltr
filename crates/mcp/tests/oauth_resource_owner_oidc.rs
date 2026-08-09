//! One-shot OIDC configuration, cookie, and persistence lifecycle coverage.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use mcp::oauth::server::*;
use mcp::{McpOAuthSecret, McpPrincipalId};
use p256::{
    SecretKey,
    elliptic_curve::sec1::ToEncodedPoint as _,
    pkcs8::{EncodePrivateKey as _, LineEnding},
};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::{net::TcpListener, sync::Mutex};
use url::Url;

fn attempt() -> OidcResourceOwnerAttempt {
    OidcResourceOwnerAttempt {
        transaction_id: "transaction-1".to_owned(),
        state_digest: vec![1; 32],
        browser_correlation_digest: vec![2; 32],
        nonce: "nonce".to_owned(),
        pkce_verifier: "a".repeat(43),
        created_at_ms: 1_000,
        expires_at_ms: 61_000,
        callback_claimed_at_ms: None,
        completed_at_ms: None,
        principal_id: None,
        consumed_at_ms: None,
    }
}

#[test]
fn config_requires_openid_and_exact_secure_local_endpoints() {
    let build = |scopes, policy| {
        OidcResourceOwnerConfig::new(
            "https://identity.example/tenant",
            "mcp-host",
            None,
            "https://authorization.example/oauth/oidc/callback",
            "https://authorization.example/oauth/authorize",
            scopes,
            Duration::from_mins(1),
            policy,
        )
    };
    assert!(
        build(
            vec!["openid".to_owned(), "profile".to_owned()],
            OidcEndpointPolicy::HttpsOnly
        )
        .is_ok()
    );
    assert!(build(vec!["profile".to_owned()], OidcEndpointPolicy::HttpsOnly).is_err());
    assert!(
        OidcResourceOwnerConfig::new(
            "http://provider.example",
            "client",
            None,
            "http://127.0.0.1:3000/oidc/callback",
            "http://127.0.0.1:3000/authorize",
            vec!["openid".to_owned()],
            Duration::from_mins(1),
            OidcEndpointPolicy::LoopbackTest,
        )
        .is_err()
    );
}

#[tokio::test]
async fn discovery_applies_validated_total_http_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/.well-known/openid-configuration",
        get(|| async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Json(json!({}))
        }),
    );
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = OidcResourceOwnerConfig::new(
        &origin,
        "client",
        None,
        format!("{origin}/oidc/callback"),
        format!("{origin}/authorize"),
        vec!["openid".to_owned()],
        Duration::from_mins(1),
        OidcEndpointPolicy::LoopbackTest,
    )
    .unwrap();
    assert_eq!(base.http_timeout(), Duration::from_secs(10));
    assert!(base.clone().with_http_timeout(Duration::ZERO).is_err());
    assert!(
        base.clone()
            .with_http_timeout(Duration::from_secs(61))
            .is_err()
    );
    let bounded = base.with_http_timeout(Duration::from_millis(20)).unwrap();
    let started = std::time::Instant::now();
    let result = OidcResourceOwnerAuthenticator::discover(
        bounded,
        Arc::new(InMemoryOidcResourceOwnerStore::default()),
        Arc::new(OidcSubjectPrincipalMapper),
        Arc::new(McpSystemOAuthClock),
        Arc::new(CounterEntropy::default()),
        reqwest::Client::builder().timeout(Duration::from_secs(30)),
    )
    .await;
    assert_eq!(result.unwrap_err(), OidcResourceOwnerError::Unavailable);
    assert!(started.elapsed() < Duration::from_secs(1));
    task.abort();
}

#[test]
fn cookie_directive_rejects_non_host_only_or_incomplete_cookies() {
    let secure = "__Host-mcp-oidc-tx=value; HttpOnly; SameSite=Lax; Path=/; Max-Age=60; Secure";
    assert!(OAuthBrowserCorrelationCookie::new(secure, true).is_ok());
    assert!(
        OAuthBrowserCorrelationCookie::new(&format!("{secure}; Domain=example.test"), true)
            .is_err()
    );
    assert!(OAuthBrowserCorrelationCookie::new("__Host-mcp-oidc-tx=value; Path=/", true).is_err());
    assert!(
        OAuthBrowserCorrelationCookie::new(
            "mcp-oidc-tx=value; HttpOnly; SameSite=Lax; Path=/; Max-Age=60",
            false
        )
        .is_ok()
    );
}

#[test]
fn public_oidc_debug_output_redacts_secrets_and_verified_claims() {
    let mut attempt = attempt();
    attempt.nonce = "nonce-secret-value".to_owned();
    let debug = format!("{attempt:?}");
    assert!(!debug.contains(&attempt.nonce));
    assert!(!debug.contains(&attempt.pkce_verifier));
    let identity = OidcVerifiedIdentity {
        issuer: "https://identity.example".to_owned(),
        subject: "subject".to_owned(),
        audiences: vec!["client".to_owned()],
        claims: serde_json::Map::from_iter([(
            "nonce".to_owned(),
            Value::String("secret-nonce".to_owned()),
        )]),
    };
    assert!(!format!("{identity:?}").contains("secret-nonce"));
}

#[tokio::test]
async fn memory_store_reads_completion_until_finalized_exactly_once() {
    let store = InMemoryOidcResourceOwnerStore::default();
    assert!(store.create(attempt()).await.unwrap());
    assert!(!store.create(attempt()).await.unwrap());
    assert!(store.by_state(vec![9; 32], 2_000).await.unwrap().is_none());
    assert!(store.by_state(vec![1; 32], 2_000).await.unwrap().is_some());

    let claimed = store
        .claim_callback("transaction-1".to_owned(), 2_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.pkce_verifier, "a".repeat(43));
    assert!(
        store
            .claim_callback("transaction-1".to_owned(), 2_001)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .complete(
                "transaction-1".to_owned(),
                McpPrincipalId::new("subject-1").unwrap(),
                2_002,
            )
            .await
            .unwrap()
    );
    assert!(
        store
            .completed("transaction-1".to_owned(), vec![3; 32], 2_003)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .completed("transaction-1".to_owned(), vec![2; 32], 2_003)
            .await
            .unwrap()
            .unwrap()
            .as_str(),
        "subject-1"
    );
    assert_eq!(
        store
            .completed("transaction-1".to_owned(), vec![2; 32], 2_004)
            .await
            .unwrap()
            .unwrap()
            .as_str(),
        "subject-1"
    );
    assert!(
        store
            .finalize_completed("transaction-1".to_owned(), 2_005)
            .await
            .unwrap()
    );
    assert!(
        store
            .completed("transaction-1".to_owned(), vec![2; 32], 2_006)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !store
            .finalize_completed("transaction-1".to_owned(), 2_007)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn memory_store_expires_and_cleans_attempts() {
    let store = InMemoryOidcResourceOwnerStore::default();
    store.create(attempt()).await.unwrap();
    assert!(store.by_state(vec![1; 32], 61_000).await.unwrap().is_none());
    assert_eq!(store.cleanup(61_000, 1).await.unwrap(), 1);
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the browser test keeps one security-sensitive redirect and cookie flow contiguous"
)]
async fn browser_login_callback_and_resume_are_one_shot_and_exactly_bound() {
    let provider_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider_origin = format!("http://{}", provider_listener.local_addr().unwrap());
    let signing = test_signing_key();
    let provider_state = Arc::new(TestProvider {
        issuer: provider_origin.clone(),
        encoding: signing.0,
        jwk: signing.1,
        nonce: Mutex::new(None),
        challenge: Mutex::new(None),
    });
    let provider = Router::new()
        .route("/.well-known/openid-configuration", get(test_metadata))
        .route("/jwks", get(test_jwks))
        .route("/token", post(test_token))
        .with_state(provider_state.clone());
    let provider_task =
        tokio::spawn(async move { axum::serve(provider_listener, provider).await.unwrap() });

    let local_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_origin = format!("http://{}", local_listener.local_addr().unwrap());
    let authenticator = OidcResourceOwnerAuthenticator::discover(
        OidcResourceOwnerConfig::new(
            provider_origin,
            "mcp-host",
            None,
            format!("{local_origin}/oauth/oidc/callback"),
            format!("{local_origin}/oauth/authorize"),
            vec!["openid".to_owned(), "profile".to_owned()],
            Duration::from_mins(1),
            OidcEndpointPolicy::LoopbackTest,
        )
        .unwrap(),
        Arc::new(InMemoryOidcResourceOwnerStore::default()),
        Arc::new(OidcSubjectPrincipalMapper),
        Arc::new(McpSystemOAuthClock),
        Arc::new(CounterEntropy::default()),
        reqwest::Client::builder(),
    )
    .await
    .unwrap();
    let local = authenticator.router();
    let local_task = tokio::spawn(async move { axum::serve(local_listener, local).await.unwrap() });

    let transaction_id = "transaction-one";
    let correlation = "browser-correlation";
    let initial = authenticator
        .authenticate(OAuthResourceOwnerAuthenticationRequest {
            headers: axum::http::HeaderMap::new(),
            transaction_id: transaction_id.to_owned(),
            continuation_url: format!("{local_origin}/oauth/authorize?transaction_id=ignored"),
            browser_correlation: McpOAuthSecret::new(correlation),
        })
        .await;
    let OAuthResourceOwnerAuthentication::LoginRequired { login_url } = initial else {
        panic!("initial request must require login")
    };
    assert!(!login_url.contains(correlation));
    assert!(!login_url.contains(transaction_id));
    let oidc_state = Url::parse(&login_url)
        .unwrap()
        .query_pairs()
        .find(|(name, _)| name == "state")
        .unwrap()
        .1
        .into_owned();
    let cookie_name = format!(
        "mcp-oidc-{}",
        &URL_SAFE_NO_PAD.encode(Sha256::digest(transaction_id.as_bytes()))[..22]
    );
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let login = client
        .get(&login_url)
        .header(
            "cookie",
            format!("{cookie_name}={correlation}; {cookie_name}=attacker"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), reqwest::StatusCode::BAD_REQUEST);
    let login = client
        .get(&login_url)
        .header("cookie", format!("{cookie_name}={correlation}"))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), reqwest::StatusCode::SEE_OTHER);
    assert_eq!(login.headers()["cache-control"], "no-store");
    assert_eq!(login.headers()["referrer-policy"], "no-referrer");
    let upstream = Url::parse(login.headers()["location"].to_str().unwrap()).unwrap();
    let parameters = upstream
        .query_pairs()
        .into_owned()
        .collect::<HashMap<_, _>>();
    assert_eq!(parameters["state"], oidc_state);
    assert_eq!(parameters["client_id"], "mcp-host");
    assert_eq!(
        parameters["redirect_uri"],
        format!("{local_origin}/oauth/oidc/callback")
    );
    assert_eq!(parameters["code_challenge_method"], "S256");
    *provider_state.nonce.lock().await = Some(parameters["nonce"].clone());
    *provider_state.challenge.lock().await = Some(parameters["code_challenge"].clone());

    let callback = client
        .get(format!(
            "{local_origin}/oauth/oidc/callback?code=provider-code&state={oidc_state}"
        ))
        .header("cookie", format!("{cookie_name}={correlation}"))
        .send()
        .await
        .unwrap();
    assert_eq!(callback.status(), reqwest::StatusCode::SEE_OTHER);
    let continuation = Url::parse(callback.headers()["location"].to_str().unwrap()).unwrap();
    assert_eq!(continuation.path(), "/oauth/authorize");
    assert_eq!(
        continuation.query_pairs().collect::<Vec<_>>(),
        [("transaction_id".into(), transaction_id.into())]
    );
    let replay = client
        .get(format!(
            "{local_origin}/oauth/oidc/callback?code=provider-code&state={oidc_state}"
        ))
        .header("cookie", format!("{cookie_name}={correlation}"))
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), reqwest::StatusCode::BAD_REQUEST);

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::COOKIE,
        format!("{cookie_name}={correlation}").parse().unwrap(),
    );
    let resume_request = || OAuthResourceOwnerAuthenticationRequest {
        headers: headers.clone(),
        transaction_id: transaction_id.to_owned(),
        continuation_url: continuation.to_string(),
        browser_correlation: McpOAuthSecret::new(""),
    };
    assert!(matches!(
        authenticator.authenticate(resume_request()).await,
        OAuthResourceOwnerAuthentication::Authenticated {
            principal_id,
            browser_correlation: Some(value),
        } if principal_id.as_str() == "verified-subject" && value.expose_secret() == correlation
    ));
    assert!(matches!(
        authenticator.authenticate(resume_request()).await,
        OAuthResourceOwnerAuthentication::Authenticated { .. }
    ));
    authenticator
        .finalize_authentication(transaction_id.to_owned())
        .await;
    assert!(matches!(
        authenticator.authenticate(resume_request()).await,
        OAuthResourceOwnerAuthentication::Denied
    ));

    local_task.abort();
    provider_task.abort();
}

struct TestProvider {
    issuer: String,
    encoding: EncodingKey,
    jwk: Value,
    nonce: Mutex<Option<String>>,
    challenge: Mutex<Option<String>>,
}

async fn test_metadata(State(state): State<Arc<TestProvider>>) -> Json<Value> {
    Json(json!({
        "issuer": state.issuer,
        "authorization_endpoint": format!("{}/authorize", state.issuer),
        "token_endpoint": format!("{}/token", state.issuer),
        "jwks_uri": format!("{}/jwks", state.issuer),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "id_token_signing_alg_values_supported": ["ES256"]
    }))
}

async fn test_jwks(State(state): State<Arc<TestProvider>>) -> Json<Value> {
    Json(json!({"keys": [state.jwk.clone()]}))
}

async fn test_token(
    State(state): State<Arc<TestProvider>>,
    axum::extract::Form(form): axum::extract::Form<HashMap<String, String>>,
) -> (axum::http::StatusCode, Json<Value>) {
    let expected_challenge = state.challenge.lock().await.clone();
    if form.get("grant_type").map(String::as_str) != Some("authorization_code")
        || form.get("code").map(String::as_str) != Some("provider-code")
        || form.get("client_id").map(String::as_str) != Some("mcp-host")
        || form
            .get("redirect_uri")
            .is_none_or(|value| !value.ends_with("/oauth/oidc/callback"))
        || form
            .get("code_verifier")
            .is_none_or(|value| value.len() != 43)
        || form.get("code_verifier").is_none_or(|value| {
            URL_SAFE_NO_PAD.encode(Sha256::digest(value.as_bytes()))
                != expected_challenge.as_deref().unwrap_or_default()
        })
    {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_grant"})),
        );
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let access_token = "provider-access-token";
    let at_hash = URL_SAFE_NO_PAD.encode(&Sha256::digest(access_token.as_bytes())[..16]);
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some("test-key".to_owned());
    let token = encode(
        &header,
        &json!({
            "iss": state.issuer,
            "sub": "verified-subject",
            "aud": "mcp-host",
            "exp": now + 300,
            "iat": now,
            "nonce": state.nonce.lock().await.clone().unwrap(),
            "at_hash": at_hash,
            "email": "person@example.test"
        }),
        &state.encoding,
    )
    .unwrap();
    (
        axum::http::StatusCode::OK,
        Json(json!({"access_token": access_token, "id_token": token, "token_type": "Bearer"})),
    )
}

fn test_signing_key() -> (EncodingKey, Value) {
    let secret = SecretKey::from_slice(&[7; 32]).unwrap();
    let pem = secret.to_pkcs8_pem(LineEnding::LF).unwrap();
    let point = secret.public_key().to_encoded_point(false);
    let jwk = json!({
        "kty": "EC", "crv": "P-256", "kid": "test-key", "use": "sig", "alg": "ES256",
        "x": URL_SAFE_NO_PAD.encode(point.x().unwrap()),
        "y": URL_SAFE_NO_PAD.encode(point.y().unwrap())
    });
    (EncodingKey::from_ec_pem(pem.as_bytes()).unwrap(), jwk)
}

#[derive(Default)]
struct CounterEntropy(AtomicU8);
impl McpOAuthEntropy for CounterEntropy {
    fn fill_bytes(&self, output: &mut [u8]) -> mcp::Result<()> {
        output.fill(self.0.fetch_add(1, Ordering::Relaxed).wrapping_add(1));
        Ok(())
    }
}

#[cfg(feature = "oauth-resource-owner-oidc-sqlite")]
#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "migration, reopen, secret-at-rest, and replay assertions form one store contract"
)]
async fn sqlite_store_reopens_without_plaintext_state_or_correlation_and_rejects_replay() {
    use mcp::oauth::server::{
        OAuthAuthorizationStore, OAuthAuthorizationStoreConfig, OAuthAuthorizationTransaction,
        SqliteOAuthAuthorizationStore,
    };
    use sqlx::{Row as _, sqlite::SqlitePoolOptions};

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mcp.sqlite3");
    std::fs::File::create(&path).unwrap();
    let url = format!("sqlite://{}", path.display());
    let pool = SqlitePoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    let hosted = SqliteOAuthAuthorizationStore::from_pool_with_config(
        pool.clone(),
        OAuthAuthorizationStoreConfig::default(),
    )
    .await
    .unwrap();
    hosted
        .register_client(OAuthClient {
            client_id: "client".to_owned(),
            registration_method: OAuthClientRegistrationMethod::Preconfigured,
            display_name: None,
            client_secret_digest: None,
            redirect_uris: vec!["https://client.example/callback".to_owned()],
            created_at_ms: 1_000,
            expires_at_ms: None,
            disabled_at_ms: None,
        })
        .await
        .unwrap();
    hosted
        .create_authorization_transaction(OAuthAuthorizationTransaction {
            transaction_id: "transaction-1".to_owned(),
            client_id: "client".to_owned(),
            client_metadata_snapshot_id: None,
            redirect_uri: "https://client.example/callback".to_owned(),
            resource: "https://resource.example/mcp".to_owned(),
            scopes: vec!["read".to_owned()],
            pkce_challenge: "challenge".to_owned(),
            browser_correlation_digest: vec![8; 32],
            state_reference: "downstream-state".to_owned(),
            principal_id: None,
            decision: None,
            created_at_ms: 1_000,
            expires_at_ms: 61_000,
        })
        .await
        .unwrap();
    let oidc = SqliteOidcResourceOwnerStore::from_pool(pool.clone())
        .await
        .unwrap();
    assert!(oidc.create(attempt()).await.unwrap());

    let row = sqlx::query(
        "SELECT state_digest,browser_correlation_digest FROM mcp_oauth_oidc_resource_owner_attempts",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<Vec<u8>, _>("state_digest"), vec![1; 32]);
    assert_eq!(
        row.get::<Vec<u8>, _>("browser_correlation_digest"),
        vec![2; 32]
    );
    drop(oidc);
    drop(hosted);
    drop(pool);

    let reopened = SqliteOidcResourceOwnerStore::connect(&url).await.unwrap();
    assert!(
        reopened
            .claim_callback("transaction-1".to_owned(), 2_000)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        reopened
            .claim_callback("transaction-1".to_owned(), 2_001)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        reopened
            .complete(
                "transaction-1".to_owned(),
                McpPrincipalId::new("sqlite-subject").unwrap(),
                2_002,
            )
            .await
            .unwrap()
    );
    for _ in 0..2 {
        assert_eq!(
            reopened
                .completed("transaction-1".to_owned(), vec![2; 32], 2_003)
                .await
                .unwrap()
                .unwrap()
                .as_str(),
            "sqlite-subject"
        );
    }
    assert!(
        reopened
            .finalize_completed("transaction-1".to_owned(), 2_004)
            .await
            .unwrap()
    );
    assert!(
        reopened
            .completed("transaction-1".to_owned(), vec![2; 32], 2_005)
            .await
            .unwrap()
            .is_none()
    );
    sqlx::query("DELETE FROM mcp_oauth_authorization_transactions WHERE transaction_id = ?")
        .bind("transaction-1")
        .execute(reopened.pool())
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM mcp_oauth_oidc_resource_owner_attempts")
            .fetch_one(reopened.pool())
            .await
            .unwrap(),
        0
    );
}

#[cfg(feature = "oauth-resource-owner-oidc-postgres")]
#[tokio::test]
#[ignore = "requires Docker"]
#[allow(
    clippy::too_many_lines,
    reason = "container startup, replica races, and cascade assertions form one backend contract"
)]
async fn postgres_store_reopens_and_claims_callback_once_across_replicas() {
    use mcp::oauth::server::{
        OAuthAuthorizationStore, OAuthAuthorizationTransaction, OAuthResumeAuthenticationRequest,
        PostgresOAuthAuthorizationStore,
    };
    use sqlx::PgPool;
    use testcontainers::{
        GenericImage, ImageExt,
        core::{IntoContainerPort as _, WaitFor},
        runners::AsyncRunner as _,
    };

    let image = GenericImage::new("postgres", "16-alpine")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_USER", "mcp")
        .with_env_var("POSTGRES_PASSWORD", "mcp")
        .with_env_var("POSTGRES_DB", "mcp");
    let container = image.start().await.unwrap();
    let port = container.get_host_port_ipv4(5432.tcp()).await.unwrap();
    let url = format!("postgres://mcp:mcp@127.0.0.1:{port}/mcp");
    let pool = PgPool::connect(&url).await.unwrap();
    let hosted = PostgresOAuthAuthorizationStore::from_pool(pool.clone())
        .await
        .unwrap();
    hosted
        .register_client(OAuthClient {
            client_id: "client".to_owned(),
            registration_method: OAuthClientRegistrationMethod::Preconfigured,
            display_name: None,
            client_secret_digest: None,
            redirect_uris: vec!["https://client.example/callback".to_owned()],
            created_at_ms: 1_000,
            expires_at_ms: None,
            disabled_at_ms: None,
        })
        .await
        .unwrap();
    hosted
        .create_authorization_transaction(OAuthAuthorizationTransaction {
            transaction_id: "transaction-1".to_owned(),
            client_id: "client".to_owned(),
            client_metadata_snapshot_id: None,
            redirect_uri: "https://client.example/callback".to_owned(),
            resource: "https://resource.example/mcp".to_owned(),
            scopes: vec!["read".to_owned()],
            pkce_challenge: "challenge".to_owned(),
            browser_correlation_digest: vec![8; 32],
            state_reference: "downstream-state".to_owned(),
            principal_id: None,
            decision: None,
            created_at_ms: 1_000,
            expires_at_ms: 61_000,
        })
        .await
        .unwrap();
    let first = PostgresOidcResourceOwnerStore::from_pool(pool.clone())
        .await
        .unwrap();
    let second = PostgresOidcResourceOwnerStore::from_pool(pool.clone())
        .await
        .unwrap();
    assert!(first.create(attempt()).await.unwrap());
    let (left, right) = tokio::join!(
        first.claim_callback("transaction-1".to_owned(), 2_000),
        second.claim_callback("transaction-1".to_owned(), 2_000),
    );
    assert_eq!(
        usize::from(left.unwrap().is_some()) + usize::from(right.unwrap().is_some()),
        1
    );
    let resume = OAuthResumeAuthenticationRequest {
        transaction_id: "transaction-1".to_owned(),
        principal_id: McpPrincipalId::new("postgres-subject").unwrap(),
        browser_correlation_digest: vec![8; 32],
        now_ms: 2_001,
    };
    let (left, right) = tokio::join!(
        hosted.resume_authorization_transaction(resume.clone()),
        hosted.resume_authorization_transaction(resume),
    );
    assert_eq!(usize::from(left.unwrap()) + usize::from(right.unwrap()), 1);
    hosted.cleanup(61_000, 10).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM mcp.oauth_oidc_resource_owner_attempts")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}
