use super::oauth::now_ms;
use super::*;

fn test_factory(
    _root: std::path::PathBuf,
    credentials: FileCredentialStore,
) -> AgentSessionRuntimeFactory {
    AgentSessionRuntimeFactory::new(credentials)
}
use crate::{
    FileCredentialStore, OAuthCredentialStore, OAuthTokenSet, SessionOAuthJwtMetadata,
    SessionOAuthJwtToken, SessionOAuthProviderSettings, SessionOAuthTokenRequestStyle,
};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::post,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject as _};
use tokio::sync::Mutex;

struct StoredCredential;

impl StoredCredential {
    #[allow(non_snake_case)]
    fn OAuth(credential: OAuthCredential) -> OAuthCredential {
        credential
    }
}

trait TestCredentialStoreExt {
    async fn modify<F>(&self, target_key: &str, update: F) -> Result<(), String>
    where
        F: FnOnce(Option<OAuthCredential>) -> Option<OAuthCredential>;
}

impl TestCredentialStoreExt for CredentialStore {
    async fn modify<F>(&self, target_key: &str, update: F) -> Result<(), String>
    where
        F: FnOnce(Option<OAuthCredential>) -> Option<OAuthCredential>,
    {
        match update(self.read(target_key).await?.map(|record| record.credential)) {
            Some(credential) => self.write(target_key, credential).await,
            None => self.delete(target_key).await,
        }
    }
}

impl TestCredentialStoreExt for FileCredentialStore {
    async fn modify<F>(&self, target_key: &str, update: F) -> Result<(), String>
    where
        F: FnOnce(Option<OAuthCredential>) -> Option<OAuthCredential>,
    {
        let current = OAuthCredentialStore::read(self, target_key.to_owned())
            .await?
            .map(|record| record.credential);
        match update(current) {
            Some(credential) => {
                OAuthCredentialStore::write(self, target_key.to_owned(), credential).await
            }
            None => OAuthCredentialStore::delete(self, target_key.to_owned()).await,
        }
    }
}

fn make_provider_credential(
    access: &str,
    refresh: Option<&str>,
    expires_at_ms: Option<i64>,
    metadata: BTreeMap<String, String>,
) -> OAuthCredential {
    OAuthCredential::provider(
        OAuthTokenSet::bearer(access, refresh.map(ToOwned::to_owned), expires_at_ms),
        metadata,
    )
}

const TEST_TOKEN_CERT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../waltr-audio/tests/fixtures/tls/localhost-test-only.cert.pem"
));
const TEST_TOKEN_KEY: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../waltr-audio/tests/fixtures/tls/localhost-test-only.key.pem"
));

struct TestTokenResponse {
    status: StatusCode,
    location: Option<&'static str>,
    body: String,
}

fn serve_token_request(
    socket: std::net::TcpStream,
    config: Arc<rustls::ServerConfig>,
    handler: impl FnOnce(&str, &str) -> TestTokenResponse,
) -> std::io::Result<()> {
    socket.set_nonblocking(false)?;
    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    let connection = rustls::ServerConnection::new(config).expect("TLS server connection");
    let mut stream = BufReader::new(rustls::StreamOwned::new(connection, socket));
    let mut request_line = String::new();
    stream.read_line(&mut request_line)?;
    let path = request_line
        .split_whitespace()
        .nth(1)
        .expect("request path");
    let mut length = 0;
    loop {
        let mut header = String::new();
        if stream.read_line(&mut header)? == 0 || header == "\r\n" {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().expect("request body length");
        }
    }
    let mut body = vec![0; length];
    stream.read_exact(&mut body)?;
    let response = handler(path, std::str::from_utf8(&body).expect("UTF-8 token body"));
    let stream = stream.get_mut();
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status.as_u16(),
        response.status.canonical_reason().expect("response status"),
        response.body.len(),
    )?;
    if let Some(location) = response.location {
        write!(stream, "Location: {location}\r\n")?;
    }
    write!(stream, "\r\n{}", response.body)?;
    stream.flush()
}

async fn tls_token_fixture(
    handler: impl Fn(&str, &str) -> TestTokenResponse + Send + Sync + 'static,
) -> String {
    let certificate = CertificateDer::from_pem_slice(TEST_TOKEN_CERT).expect("TLS certificate");
    let key = PrivateKeyDer::from_pem_slice(TEST_TOKEN_KEY).expect("TLS key");
    let config = Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("TLS protocol versions")
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .expect("TLS server configuration"),
    );
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind token server");
    let port = listener.local_addr().expect("token server address").port();
    let handler = Arc::new(handler);
    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.expect("accept token connection");
            let socket = socket.into_std().expect("standard TCP stream");
            let config = Arc::clone(&config);
            let handler = Arc::clone(&handler);
            tokio::task::spawn_blocking(move || {
                serve_token_request(socket, config, |path, body| handler(path, body))
                    .expect("serve token request");
            });
        }
    });
    format!("https://localhost:{port}/token")
}

async fn token_server(response: serde_json::Value, delay: Duration) -> (String, Arc<AtomicUsize>) {
    token_server_with_status(StatusCode::OK, response, delay).await
}

async fn token_server_with_status(
    status: StatusCode,
    response: serde_json::Value,
    delay: Duration,
) -> (String, Arc<AtomicUsize>) {
    let requests = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&requests);
    let url = tls_token_fixture(move |path, body| {
        assert_eq!(path, "/token");
        assert!(body.contains("refresh_token=old-provider-refresh"));
        seen.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(delay);
        TestTokenResponse {
            status,
            location: None,
            body: response.to_string(),
        }
    })
    .await;
    (url, requests)
}

async fn token_error_server(
    status: StatusCode,
    response: serde_json::Value,
) -> (String, Arc<AtomicUsize>) {
    token_server_with_status(status, response, Duration::ZERO).await
}

#[derive(Clone, Default)]
struct MessagesServerState {
    requests: Arc<Mutex<Vec<(HeaderMap, serde_json::Value)>>>,
}

async fn messages_handler(
    State(state): State<MessagesServerState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    state.requests.lock().await.push((headers, body));
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    )
}

async fn messages_server() -> (String, MessagesServerState) {
    let state = MessagesServerState::default();
    let app = Router::new()
        .route("/v1/messages", post(messages_handler))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind messages server");
    let address = listener.local_addr().expect("messages server address");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve messages endpoint");
    });
    (format!("http://{address}/v1/messages?beta=true"), state)
}

#[tokio::test]
async fn provider_refresh_connect_failure_keeps_credential_retryable() {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let (coordinator, _, settings) = provider_coordinator(
        format!("https://{address}/token"),
        provider_credential(now_ms() - 1),
    )
    .await;
    for _ in 0..2 {
        let error = coordinator
            .refresh_provider("test", &settings, RefreshMode::Normal)
            .await
            .unwrap_err();
        assert_eq!(
            error.reason(),
            crate::AuthenticationFailureReason::NetworkFailed
        );
    }
}

#[tokio::test]
async fn provider_refresh_does_not_forward_credentials_on_redirect() {
    let redirected_requests = Arc::new(AtomicUsize::new(0));
    let redirected = Arc::clone(&redirected_requests);
    let url = tls_token_fixture(move |path, body| {
        if path == "/token" {
            assert!(body.contains("refresh_token=old-provider-refresh"));
            TestTokenResponse {
                status: StatusCode::TEMPORARY_REDIRECT,
                location: Some("/secret-sink"),
                body: "{}".to_owned(),
            }
        } else {
            redirected.fetch_add(1, Ordering::SeqCst);
            TestTokenResponse {
                status: StatusCode::OK,
                location: None,
                body: r#"{"access_token":"unexpected"}"#.to_owned(),
            }
        }
    })
    .await;
    let (coordinator, _, settings) =
        provider_coordinator(url, provider_credential(now_ms() - 1)).await;
    let error = coordinator
        .refresh_provider("test", &settings, RefreshMode::Normal)
        .await
        .expect_err("redirect must fail");
    assert_eq!(
        error.reason(),
        crate::AuthenticationFailureReason::TokenExchangeFailed
    );
    assert_eq!(redirected_requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn provider_refresh_rejects_oversized_responses() {
    let (url, _) = token_server(serde_json::json!("x".repeat(64 * 1024)), Duration::ZERO).await;
    let (coordinator, _, settings) =
        provider_coordinator(url, provider_credential(now_ms() - 1)).await;
    let error = coordinator
        .refresh_provider("test", &settings, RefreshMode::Normal)
        .await
        .expect_err("oversized token response must fail");
    assert_eq!(
        error.reason(),
        crate::AuthenticationFailureReason::ResponseInvalid
    );
    assert!(error.message().contains("size limit"));
}

fn provider_oauth_settings(token_url: String) -> SessionOAuthProviderSettings {
    SessionOAuthProviderSettings {
        token_url,
        client_id: "provider-client".to_owned(),
        redirect_uri: "http://localhost/callback".to_owned(),
        token_request_style: SessionOAuthTokenRequestStyle::FormUrlencoded,
        token_fields: Vec::new(),
        jwt_metadata: vec![SessionOAuthJwtMetadata {
            token: SessionOAuthJwtToken::Id,
            metadata_key: "account_id".to_owned(),
            claim_names: vec!["account.id".to_owned()],
            required: true,
        }],
    }
}

fn provider_credential(expires_at_ms: i64) -> OAuthCredential {
    make_provider_credential(
        "old-provider-access",
        Some("old-provider-refresh"),
        Some(expires_at_ms),
        BTreeMap::from([
            ("account_id".to_owned(), "old-account".to_owned()),
            ("preserved".to_owned(), "preserved-value".to_owned()),
        ]),
    )
}

fn anthropic_settings(
    key: &str,
    url: String,
    headers: BTreeMap<String, String>,
    oauth: Option<SessionOAuthProviderSettings>,
) -> InferenceSettings {
    InferenceSettings {
        provider: InferenceProviderSettings {
            key: key.to_owned(),
            api: ModelApi::AnthropicMessages,
            url,
            model_id: "claude-test".to_owned(),
            model_cost: None,
            headers,
            oauth,
            reasoning_effort: None,
        },
    }
}

fn jwt(claims: &serde_json::Value) -> String {
    jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(b"test-secret"),
    )
    .expect("encode JWT")
}

async fn provider_coordinator(
    token_url: String,
    credential: OAuthCredential,
) -> (
    CredentialRefreshCoordinator,
    FileCredentialStore,
    SessionOAuthProviderSettings,
) {
    let directory = tempfile::tempdir().expect("temp directory").keep();
    let store = FileCredentialStore::new(directory.join("auth.json"));
    store
        .modify(&AuthTarget::provider("test").credential_key(), |_| {
            Some(StoredCredential::OAuth(credential))
        })
        .await
        .expect("store provider credential");
    (
        tls_provider_coordinator(store.clone()),
        store,
        provider_oauth_settings(token_url),
    )
}

fn tls_provider_coordinator(store: FileCredentialStore) -> CredentialRefreshCoordinator {
    let cert = reqwest::Certificate::from_pem(TEST_TOKEN_CERT).expect("TLS test root");
    CredentialRefreshCoordinator::new(store).with_test_root(cert)
}

#[tokio::test]
#[allow(clippy::literal_string_with_formatting_args)]
async fn runtime_builder_resolves_headers_at_agent_boundary() {
    let factory = AgentSessionRuntimeFactory::new(FileCredentialStore::new(
        std::env::temp_dir().join("waltr-agent-runtime-test-auth.json"),
    ));
    let settings = InferenceSettings {
        provider: InferenceProviderSettings {
            key: "test".to_owned(),
            api: ModelApi::OpenAiResponses,
            url: String::new(),
            model_id: "test-model".to_owned(),
            model_cost: None,
            headers: BTreeMap::from([(
                "Authorization".to_owned(),
                "Bearer ${env:WALTR_AGENT_TEST_MISSING_HEADER_TOKEN_UNSET}".to_owned(),
            )]),
            oauth: None,
            reasoning_effort: None,
        },
    };

    let error = factory
        .provider_builder(&settings)
        .await
        .expect_err("missing env should fail at runtime resolution");

    assert!(matches!(
        error,
        AgentSessionRuntimeError::FailedPrecondition(_)
    ));
    assert!(
        error
            .to_string()
            .contains("provider header 'Authorization' references unset env var")
    );
}

#[test]
fn claude_code_version_accepts_one_safe_case_insensitive_user_agent() {
    assert_eq!(
        configured_claude_code_version(&BTreeMap::from([(
            "User-Agent".to_owned(),
            "claude-code/9.8.7".to_owned(),
        )]))
        .expect("single header is valid"),
        Some("9.8.7")
    );
    let duplicate = BTreeMap::from([
        ("User-Agent".to_owned(), "claude-code/9.8.7".to_owned()),
        ("user-agent".to_owned(), "claude-code/2.1.76".to_owned()),
    ]);
    assert!(matches!(
        configured_claude_code_version(&duplicate),
        Err(AgentSessionRuntimeError::FailedPrecondition(_))
    ));
    assert_eq!(
        configured_claude_code_version(&BTreeMap::from([(
            "uSeR-aGeNt".to_owned(),
            "claude-code/3.4.5-beta_1+local".to_owned(),
        )]))
        .expect("single header is valid"),
        Some("3.4.5-beta_1+local")
    );
    for headers in [
        BTreeMap::new(),
        BTreeMap::from([("user-agent".to_owned(), "waltr/1.0".to_owned())]),
        BTreeMap::from([("user-agent".to_owned(), "claude-code/2.1;bad".to_owned())]),
    ] {
        assert_eq!(
            configured_claude_code_version(&headers).expect("header set is unambiguous"),
            None
        );
    }
}

#[tokio::test]
async fn anthropic_oauth_runtime_sends_its_projected_billing_marker() {
    let directory = tempfile::tempdir().expect("temp directory");
    let (url, server) = messages_server().await;
    let factory = AgentSessionRuntimeFactory::new(FileCredentialStore::new(
        directory.path().join("auth.json"),
    ));
    factory
        .credential_store
        .modify(
            &AuthTarget::provider("anthropic-oauth").credential_key(),
            |_| {
                Some(StoredCredential::OAuth(make_provider_credential(
                    "oauth-access",
                    Some("oauth-refresh"),
                    Some(now_ms() + 60 * 60 * 1000),
                    BTreeMap::new(),
                )))
            },
        )
        .await
        .expect("store OAuth credential");
    let settings = anthropic_settings(
        "anthropic-oauth",
        url,
        BTreeMap::from([
            ("anthropic-beta".to_owned(), "oauth-2025-04-20".to_owned()),
            ("user-agent".to_owned(), "claude-code/2.1.76".to_owned()),
            ("x-app".to_owned(), "cli".to_owned()),
        ]),
        Some(SessionOAuthProviderSettings {
            token_url: "https://example.test/token".to_owned(),
            client_id: "client".to_owned(),
            redirect_uri: "http://localhost/callback".to_owned(),
            token_request_style: SessionOAuthTokenRequestStyle::FormUrlencoded,
            token_fields: Vec::new(),
            jwt_metadata: Vec::new(),
        }),
    );
    let runtime = factory
        .provider_builder(&settings)
        .await
        .expect("build OAuth provider")
        .build();
    let request = CompletionRequest::new("Hello from Claude Code");
    let projected = runtime
        .provider
        .project_request(&request)
        .expect("project request");

    let _stream = runtime
        .provider
        .stream_request(request)
        .await
        .expect("send request");
    let (headers, sent) = server
        .requests
        .lock()
        .await
        .first()
        .cloned()
        .expect("captured request");

    assert_eq!(sent, projected);
    assert_eq!(headers[header::AUTHORIZATION], "Bearer oauth-access");
    assert_eq!(headers["anthropic-beta"], "oauth-2025-04-20");
    assert_eq!(headers[header::USER_AGENT], "claude-code/2.1.76");
    assert_eq!(headers["x-app"], "cli");
    assert_eq!(
        sent["system"][0]["text"],
        "x-anthropic-billing-header: cc_version=2.1.76.ad6; cc_entrypoint=cli; cch=00000;"
    );
}

#[tokio::test]
async fn anthropic_api_key_runtime_sends_projection_without_billing_marker() {
    let directory = tempfile::tempdir().expect("temp directory");
    let (url, server) = messages_server().await;
    let factory = AgentSessionRuntimeFactory::new(FileCredentialStore::new(
        directory.path().join("auth.json"),
    ));
    let settings = anthropic_settings(
        "anthropic-api-key",
        url,
        BTreeMap::from([
            ("user-agent".to_owned(), "claude-code/2.1.76".to_owned()),
            ("x-api-key".to_owned(), "api-key-value".to_owned()),
        ]),
        Some(SessionOAuthProviderSettings {
            token_url: "https://example.test/token".to_owned(),
            client_id: "client".to_owned(),
            redirect_uri: "http://localhost/callback".to_owned(),
            token_request_style: SessionOAuthTokenRequestStyle::FormUrlencoded,
            token_fields: Vec::new(),
            jwt_metadata: Vec::new(),
        }),
    );
    let runtime = factory
        .provider_builder(&settings)
        .await
        .expect("build API-key provider")
        .build();
    let request = CompletionRequest::new("Hello from Claude Code");
    let projected = runtime
        .provider
        .project_request(&request)
        .expect("project request");

    let _stream = runtime
        .provider
        .stream_request(request)
        .await
        .expect("send request");
    let (headers, sent) = server
        .requests
        .lock()
        .await
        .first()
        .cloned()
        .expect("captured request");

    assert_eq!(sent, projected);
    assert_eq!(headers["x-api-key"], "api-key-value");
    assert_eq!(headers[header::USER_AGENT], "claude-code/2.1.76");
    assert!(!headers.contains_key(header::AUTHORIZATION));
    assert!(sent.get("system").is_none());
}

#[tokio::test]
async fn anthropic_oauth_without_claude_code_user_agent_omits_billing_marker() {
    let directory = tempfile::tempdir().expect("temp directory");
    let factory = AgentSessionRuntimeFactory::new(FileCredentialStore::new(
        directory.path().join("auth.json"),
    ));
    factory
        .credential_store
        .modify(
            &AuthTarget::provider("anthropic-oauth").credential_key(),
            |_| {
                Some(StoredCredential::OAuth(make_provider_credential(
                    "oauth-access",
                    Some("oauth-refresh"),
                    Some(now_ms() + 60 * 60 * 1000),
                    BTreeMap::new(),
                )))
            },
        )
        .await
        .expect("store OAuth credential");
    let oauth = SessionOAuthProviderSettings {
        token_url: "https://example.test/token".to_owned(),
        client_id: "client".to_owned(),
        redirect_uri: "http://localhost/callback".to_owned(),
        token_request_style: SessionOAuthTokenRequestStyle::FormUrlencoded,
        token_fields: Vec::new(),
        jwt_metadata: Vec::new(),
    };

    for headers in [
        BTreeMap::new(),
        BTreeMap::from([("user-agent".to_owned(), "waltr/1.0".to_owned())]),
    ] {
        let runtime = factory
            .provider_builder(&anthropic_settings(
                "anthropic-oauth",
                "https://example.test/v1/messages".to_owned(),
                headers,
                Some(oauth.clone()),
            ))
            .await
            .expect("build OAuth provider")
            .build();
        let projected = runtime
            .provider
            .project_request(&CompletionRequest::new("Hello from Claude Code"))
            .expect("project request");

        assert!(projected.get("system").is_none());
    }

    let error = factory
        .provider_builder(&anthropic_settings(
            "anthropic-oauth",
            "https://example.test/v1/messages".to_owned(),
            BTreeMap::from([
                ("User-Agent".to_owned(), "claude-code/9.8.7".to_owned()),
                ("user-agent".to_owned(), "claude-code/2.1.76".to_owned()),
            ]),
            Some(oauth),
        ))
        .await
        .expect_err("ambiguous user-agent configuration must fail");
    assert!(matches!(
        error,
        AgentSessionRuntimeError::FailedPrecondition(_)
    ));
}

#[tokio::test]
async fn provider_oauth_uses_namespaced_credential_key() {
    let auth_dir = std::env::temp_dir().join(format!(
        "waltr-agent-runtime-test-auth-{}",
        uuid::Uuid::new_v4()
    ));
    let auth_path = auth_dir.join("auth.json");
    let factory = test_factory(auth_dir, FileCredentialStore::new(auth_path));
    factory
        .credential_store
        .modify(&AuthTarget::provider("test").credential_key(), |_| {
            Some(StoredCredential::OAuth(make_provider_credential(
                "access-token",
                Some("refresh-token"),
                Some(now_ms() + 60 * 60 * 1000),
                BTreeMap::new(),
            )))
        })
        .await
        .expect("store credential");
    let provider = InferenceProviderSettings {
        key: "test".to_owned(),
        api: ModelApi::OpenAiResponses,
        url: String::new(),
        model_id: "test-model".to_owned(),
        model_cost: None,
        headers: BTreeMap::new(),
        oauth: Some(SessionOAuthProviderSettings {
            token_url: "https://example.test/token".to_owned(),
            client_id: "client".to_owned(),
            redirect_uri: "http://localhost/callback".to_owned(),
            token_request_style: SessionOAuthTokenRequestStyle::FormUrlencoded,
            token_fields: Vec::new(),
            jwt_metadata: Vec::new(),
        }),
        reasoning_effort: None,
    };

    let credential = factory
        .oauth_access(&provider, &BTreeMap::new())
        .await
        .expect("oauth lookup succeeds")
        .expect("credential is found");

    assert_eq!(credential.token.access_token.secret(), "access-token");
}

#[tokio::test]
async fn configured_provider_oauth_without_credential_fails_before_provider_construction() {
    let directory = tempfile::tempdir().expect("temp directory");
    let factory = AgentSessionRuntimeFactory::new(FileCredentialStore::new(
        directory.path().join("auth.json"),
    ));
    let settings = InferenceSettings {
        provider: InferenceProviderSettings {
            key: "configured-provider".to_owned(),
            api: ModelApi::OpenAiResponses,
            url: "https://provider.example/v1/responses".to_owned(),
            model_id: "test-model".to_owned(),
            model_cost: None,
            headers: BTreeMap::new(),
            oauth: Some(provider_oauth_settings(
                "https://provider.example/oauth/token".to_owned(),
            )),
            reasoning_effort: None,
        },
    };

    let error = factory
        .provider_builder(&settings)
        .await
        .expect_err("missing configured OAuth credential must fail");

    assert!(matches!(
        error,
        AgentSessionRuntimeError::Authentication {
            target: AuthTarget::Provider { ref key },
            reason: crate::AuthenticationFailureReason::CredentialRequired,
            ..
        } if key == "configured-provider"
    ));
}

#[test]
fn provider_http_401_is_classified_for_exact_configured_target() {
    let error = provider_authentication_error(
        LlmError::Api {
            provider: "test",
            message: "secret response".to_owned(),
            status: Some(401),
            headers: Vec::new(),
            code: None,
            body: Some("secret response".to_owned()),
        },
        &AuthTarget::provider("configured-provider"),
    );

    assert!(matches!(
        error,
        LlmError::AuthenticationFailure {
            failure: AuthenticationFailure {
                target: AuthTarget::Provider { ref key },
                reason: crate::AuthenticationFailureReason::CredentialRejected,
                ..
            },
            ref message,
        } if key == "configured-provider" && !message.contains("secret response")
    ));
}

#[test]
fn provider_websocket_401_is_classified_for_exact_configured_target() {
    let response = http::Response::builder()
        .status(http::StatusCode::UNAUTHORIZED)
        .body(Some(Vec::new()))
        .expect("websocket handshake response");
    let error = provider_authentication_error(
        LlmError::WebSocket(tokio_tungstenite::tungstenite::Error::Http(Box::new(
            response,
        ))),
        &AuthTarget::provider("configured-websocket-provider"),
    );

    assert!(matches!(
        error,
        LlmError::AuthenticationFailure {
            failure: AuthenticationFailure {
                target: AuthTarget::Provider { ref key },
                reason: crate::AuthenticationFailureReason::CredentialRejected,
                ..
            },
            ..
        } if key == "configured-websocket-provider"
    ));
}

#[tokio::test]
async fn expired_provider_oauth_without_refresh_token_fails_before_unauthenticated_call() {
    let auth_dir = std::env::temp_dir().join(format!(
        "waltr-agent-runtime-test-auth-{}",
        uuid::Uuid::new_v4()
    ));
    let auth_path = auth_dir.join("auth.json");
    let factory = test_factory(auth_dir, FileCredentialStore::new(auth_path));
    factory
        .credential_store
        .modify(&AuthTarget::provider("test").credential_key(), |_| {
            Some(StoredCredential::OAuth(make_provider_credential(
                "stale-token",
                None,
                Some(now_ms() - 1000),
                BTreeMap::new(),
            )))
        })
        .await
        .expect("store credential");
    let provider = InferenceProviderSettings {
        key: "test".to_owned(),
        api: ModelApi::OpenAiResponses,
        url: String::new(),
        model_id: "test-model".to_owned(),
        model_cost: None,
        headers: BTreeMap::new(),
        oauth: Some(SessionOAuthProviderSettings {
            token_url: "https://example.test/token".to_owned(),
            client_id: "client".to_owned(),
            redirect_uri: "http://localhost/callback".to_owned(),
            token_request_style: SessionOAuthTokenRequestStyle::FormUrlencoded,
            token_fields: Vec::new(),
            jwt_metadata: Vec::new(),
        }),
        reasoning_effort: None,
    };

    let error = factory
        .oauth_access(&provider, &BTreeMap::new())
        .await
        .expect_err("expired configured OAuth credential must fail");

    assert!(matches!(
        error,
        AgentSessionRuntimeError::Authentication {
            target: AuthTarget::Provider { ref key },
            reason: crate::AuthenticationFailureReason::CredentialRequired,
            ..
        } if key == "test"
    ));
}

#[tokio::test]
async fn forced_provider_refresh_preserves_and_reextracts_metadata_and_rotates_refresh() {
    let (token_url, server) = token_server(
        serde_json::json!({
            "access_token": "new-provider-access",
            "refresh_token": "rotated-provider-refresh",
            "expires_in": 3600,
            "id_token": jwt(&serde_json::json!({"account": {"id": "new-account"}}))
        }),
        Duration::ZERO,
    )
    .await;
    let (coordinator, store, settings) =
        provider_coordinator(token_url, provider_credential(now_ms() + 3_600_000)).await;

    let refreshed = coordinator
        .refresh_provider("test", &settings, RefreshMode::Forced)
        .await
        .expect("force provider refresh")
        .expect("provider credential");

    assert_eq!(refreshed.token.access_token.secret(), "new-provider-access");
    assert_eq!(
        refreshed
            .token
            .refresh_token
            .as_ref()
            .map(|token| token.secret().as_str()),
        Some("rotated-provider-refresh")
    );
    assert_eq!(
        refreshed
            .provider_metadata()
            .and_then(|metadata| metadata.get("account_id")),
        Some(&"new-account".to_owned())
    );
    assert_eq!(
        refreshed
            .provider_metadata()
            .and_then(|metadata| metadata.get("preserved")),
        Some(&"preserved-value".to_owned())
    );
    assert_eq!(server.load(Ordering::SeqCst), 1);
    let stored = OAuthCredentialStore::read(&store, AuthTarget::provider("test").credential_key())
        .await
        .expect("read provider credential")
        .expect("expected provider credential")
        .credential;
    assert_eq!(
        stored
            .token
            .refresh_token
            .as_ref()
            .map(|token| token.secret().as_str()),
        Some("rotated-provider-refresh")
    );
}

#[tokio::test]
async fn normal_provider_refresh_retains_omitted_refresh_token_and_metadata() {
    let (token_url, _) = token_server(
        serde_json::json!({
            "access_token": "new-provider-access",
            "expires_in": 3600
        }),
        Duration::ZERO,
    )
    .await;
    let (coordinator, _, settings) =
        provider_coordinator(token_url, provider_credential(now_ms() + 1_000)).await;

    let refreshed = coordinator
        .refresh_provider("test", &settings, RefreshMode::Normal)
        .await
        .expect("normal provider refresh")
        .expect("provider credential");

    assert_eq!(
        refreshed
            .token
            .refresh_token
            .as_ref()
            .map(|token| token.secret().as_str()),
        Some("old-provider-refresh")
    );
    assert_eq!(
        refreshed
            .provider_metadata()
            .and_then(|metadata| metadata.get("account_id")),
        Some(&"old-account".to_owned())
    );
}

#[tokio::test]
async fn provider_refresh_rejects_error_and_invalid_responses_without_leaking_body() {
    let (token_url, _) = token_error_server(
        StatusCode::BAD_REQUEST,
        serde_json::json!({"error": "invalid_grant", "detail": "private-response-body"}),
    )
    .await;
    let (coordinator, _, settings) =
        provider_coordinator(token_url, provider_credential(now_ms() - 1)).await;
    let error = coordinator
        .refresh_provider("test", &settings, RefreshMode::Normal)
        .await
        .expect_err("rejected refresh");
    assert_eq!(
        error.reason(),
        crate::AuthenticationFailureReason::TokenExchangeFailed
    );
    assert!(!format!("{error:?}").contains("private-response-body"));
    assert!(!error.to_string().contains("old-provider-refresh"));

    let (token_url, _) =
        token_server(serde_json::json!({"expires_in": 3600}), Duration::ZERO).await;
    let (coordinator, _, settings) =
        provider_coordinator(token_url, provider_credential(now_ms() - 1)).await;
    let error = coordinator
        .refresh_provider("test", &settings, RefreshMode::Normal)
        .await
        .expect_err("invalid refresh response");
    assert_eq!(
        error.reason(),
        crate::AuthenticationFailureReason::ResponseInvalid
    );
}

#[tokio::test]
async fn independently_constructed_provider_stores_share_singleflight() {
    let (token_url, server) = token_server(
        serde_json::json!({
            "access_token": "new-provider-access",
            "expires_in": 3600
        }),
        Duration::from_millis(50),
    )
    .await;
    let (coordinator, store, settings) =
        provider_coordinator(token_url, provider_credential(now_ms() - 1)).await;
    let second = tls_provider_coordinator(FileCredentialStore::new(store.path()));

    let (first, second) = tokio::join!(
        coordinator.refresh_provider("test", &settings, RefreshMode::Normal),
        second.refresh_provider("test", &settings, RefreshMode::Normal),
    );

    assert_eq!(
        first
            .expect("first refresh")
            .expect("credential")
            .token
            .access_token
            .secret(),
        "new-provider-access"
    );
    assert_eq!(
        second
            .expect("second refresh")
            .expect("credential")
            .token
            .access_token
            .secret(),
        "new-provider-access"
    );
    assert_eq!(server.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stale_provider_refresh_cannot_replace_newer_credential() {
    let (token_url, server) = token_server(
        serde_json::json!({
            "access_token": "stale-refreshed-access",
            "expires_in": 3600
        }),
        Duration::from_millis(100),
    )
    .await;
    let (coordinator, store, settings) =
        provider_coordinator(token_url, provider_credential(now_ms() - 1)).await;
    let task = tokio::spawn({
        let coordinator = coordinator.clone();
        let settings = settings.clone();
        async move {
            coordinator
                .refresh_provider("test", &settings, RefreshMode::Normal)
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while server.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("refresh request reached the server");
    let mut replacement = provider_credential(now_ms() + 3_600_000);
    replacement.token.access_token = oauth2::AccessToken::new("newly-authorized-access".to_owned());
    replacement.token.refresh_token = Some(oauth2::RefreshToken::new(
        "newly-authorized-refresh".to_owned(),
    ));
    store
        .modify(&AuthTarget::provider("test").credential_key(), |_| {
            Some(StoredCredential::OAuth(replacement))
        })
        .await
        .expect("replace provider credential");

    let current = task
        .await
        .expect("refresh task")
        .expect("coordinated refresh")
        .expect("provider credential");
    assert_eq!(
        current.token.access_token.secret(),
        "newly-authorized-access"
    );
    assert_eq!(
        current
            .token
            .refresh_token
            .as_ref()
            .map(|token| token.secret().as_str()),
        Some("newly-authorized-refresh")
    );
}
