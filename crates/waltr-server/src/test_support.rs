//! Shared integration test server setup.

use crate::{
    ServerApplication,
    access_auth::{
        AccessAuthConfig, AccessAuthHttpClient, AccessAuthStore, AccessAuthenticator,
        BrowserSession, BrowserSessionTokenDigest, OidcConfig,
    },
    app::{
        build_app_with_catalog_loader_and_access_authenticator,
        build_app_with_catalog_loader_and_access_authenticator_and_mcp_oauth,
    },
    build_app_with_catalog_loader,
    catalog::CatalogLoader,
    config::DefaultConfigBuilder,
    config_service::ConfigService,
    mcp_oauth::{McpOAuthEnabledConfig, McpOAuthServer},
    persistence::{DatabaseConfig, Persistence},
    session,
};
use axum::{
    Form, Json, Router,
    extract::{Query, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use waltr_agent::{MemorySessionStore, SessionCoordinator};
use mcp::oauth::{
    McpJwtAccessTokenHeader, OAuthEncryptedPrivateKey, OAuthKeyEncryption,
    VersionedOAuthWrappingKeyring,
};
use p256::{
    SecretKey,
    ecdsa::{Signature, SigningKey, signature::Signer as _},
    pkcs8::DecodePrivateKey as _,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    collections::HashMap,
    net::{SocketAddr, TcpListener},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, SystemTime},
};
use tokio::sync::{Mutex, Notify, oneshot};
use tokio_util::sync::CancellationToken;
use url::Url;

static TEMP_ROOT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Return the deterministic catalog loader used by server tests.
#[must_use]
pub fn default_catalog_loader() -> CatalogLoader {
    CatalogLoader::from_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/models-dev-defaults.json"),
    )
}

/// Return deterministic catalog-backed defaults used by server tests.
#[must_use]
pub fn default_config_builder() -> DefaultConfigBuilder {
    DefaultConfigBuilder::new(default_catalog_loader(), "http://127.0.0.1:14333/mcp")
}

fn config_service_with_loader(loader: &CatalogLoader) -> Arc<ConfigService> {
    ConfigService::new(
        temp_root(),
        DefaultConfigBuilder::new(loader.clone(), "http://127.0.0.1:14333/mcp"),
    )
    .expect("config service should start")
}

/// Observability and join controls for a server application with blocked cleanup.
#[derive(Debug)]
pub struct BlockedBackgroundTaskProbe {
    /// Notified after the blocked cleanup future starts.
    pub cleanup_started: Arc<Notify>,
    /// Set when cancellation drops the blocked cleanup future.
    pub cleanup_dropped: Arc<AtomicBool>,
    /// Notified after cancellation is observed but before cleanup can finish.
    pub shutdown_observed: Arc<Notify>,
    /// Allows the cleanup task to finish so shutdown can join it.
    pub release_cleanup: Arc<Notify>,
}

struct BackgroundTaskDropGuard(Arc<AtomicBool>);

impl Drop for BackgroundTaskDropGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Construct an application whose background task remains blocked until explicitly released.
#[must_use]
pub fn blocked_background_application() -> (ServerApplication, BlockedBackgroundTaskProbe) {
    let cancellation = CancellationToken::new();
    let cleanup_started = Arc::new(Notify::new());
    let cleanup_dropped = Arc::new(AtomicBool::new(false));
    let shutdown_observed = Arc::new(Notify::new());
    let release_cleanup = Arc::new(Notify::new());
    let task = {
        let cancellation = cancellation.clone();
        let cleanup_started = Arc::clone(&cleanup_started);
        let cleanup_dropped = Arc::clone(&cleanup_dropped);
        let shutdown_observed = Arc::clone(&shutdown_observed);
        let release_cleanup = Arc::clone(&release_cleanup);
        tokio::spawn(async move {
            let guard = BackgroundTaskDropGuard(cleanup_dropped);
            cleanup_started.notify_one();
            cancellation.cancelled().await;
            drop(guard);
            shutdown_observed.notify_one();
            release_cleanup.notified().await;
        })
    };
    let app = ServerApplication::with_background_task_for_test(cancellation, task);
    (
        app,
        BlockedBackgroundTaskProbe {
            cleanup_started,
            cleanup_dropped,
            shutdown_observed,
            release_cleanup,
        },
    )
}

const TEST_KEY_ID: &str = "waltr-access-test-key";
const TEST_PRIVATE_KEY: &str = r"-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCNNdGQvErjmqYo
wx04OKzHiIwdhBvAj9adzAH726wXfUhR7ACBxW1Exalb1glGQDvXc2w/5x+zg3K6
vOWgn3nmLA0YmbMEXr8TnXlz5AXIAATLIWm2SkWpyBkEVRkYeyXPwCWBTKxLKyGM
4I0bXlUSD6CN0vAMlbVGu9BAL7DVgePeyBlX9dZ9hbLjTVPkZzs53Gy1UrW4iPYu
DnG6y8oYIQG4N1hmZOODO5CoXasYVZULuvhqDcLzHEF/AClFm2Tte9lpzeQC1gMJ
WUfIVu2SZqk43Ju4OKMlfPBV9L3rHQjs+hdz5zq1I9/7tak20gOgvEdwjlC/rq1+
w5188JzzAgMBAAECggEAAq1rU1etZHJ3mUt9eZJYS7SKUlgGccKNWX8NhQnCoMxP
x83dlnDuyP30mri0DJ9BAzcWHtCtXB9giatriaUymboveuvGg8hjziqi+m3b7FtU
9wMtewWoQvJ32+Lc3n0xHKbXQHA1ut3nkRnwP0wjDMrs6D6UGf8jc2LPo5HD64EG
Ye973DBz/tqIiRYviBYOgD1dgQR0ZqM6QHbVkQ0IJbB3wQNR8zDlubShCHk1ccwz
O1k3CNYI41+r6OQ46yW9KmYyztgzMiep0/lE4C9Qqq+OQyyPG8/F0+qvUKXsJjwx
F/ddPM6VN4iaP8QzwS8ajtm5enVc5Mo0lGZDrQkJcQKBgQDBVaD3Kd+UdpympajV
YcfPru66ZqIm+FUcIjIEyWINCSg90z4w+zzz4ajnB9YktMGRlKgHz95mUm72W2iZ
SAd13HixLlvVWWKYTQH9mdJ+bWzUhzP3XzhO94s3S3oS2Unj3vewKnvwLVKbm54x
VyqF0yldNFbMJYrb/J9T+BXqRwKBgQC6+w9vpXsKm8yd1J2NagyFF1U7uJ9nWc8y
EDOMcJOdPyXrHOpYH5xidubh4Vvt+4iU2QOt15yDOifAfAaAjuP2ZCcCTORXQ+nV
Lb3GUQaGYGvlT2zm9RuAlavV1KDKmvY+F8WueFU5/lSPgu0w2FB6hCRTtcCKuC6h
JOE8ovnh9QKBgQCwhz8yb0l2hdcDUr+WFnIbb98K1Cu66YTM62xAcqDUxke4K+nd
T4mYJbPTP5SIb4NPKi1C9MpdI2euCki3rM0jEOwi2/F+HN+psKbgqFNKnR+B7vHE
LW18rQqwsnoEO+L77dq7QA9qffevnagT5UMubmCWefPG84CW3VOdmChqgQKBgEpe
JKuRM7jqcp2rvOZL8aLVvu675Zqjm8cbroUDGZMHVLuIiqR2HisDg5z/lyK2UoJx
NgdkpDmFXd4u9h8Xtg9/kgxWLNOQjWZehQZln6S+oOtZhcTuY/vmi6vGoBU1QQ89
QUC9V5hzUYqq1lABGTFBcXlh5qzFyLYLBxKo954pAoGAUtBAanvamVMGl0x3HYa9
BHysmfq4utptmwJp6bCV0oqunuJqhSwIyGPlTxkoJKDwfXgSsWoEDePd6nHrm4TG
HpmPp186M8HC6SSmdVh3CSIuSzO3DzHyDCV9k8UzzVZiCqBVm5Vv3m5w2SAQPduP
ZbAq2L8jXEetO4oBq+h/bLE=
-----END PRIVATE KEY-----";

/// Shared test server for integration tests in one test binary.
#[derive(Debug)]
pub struct TestServer {
    addr: SocketAddr,
    _thread: thread::JoinHandle<()>,
}

impl TestServer {
    /// Build an absolute URL for a path on the test server.
    #[must_use]
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url(), path)
    }

    /// Return the base URL for tonic clients.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

/// Auth-enabled integration server and credentials issued by its local fixture.
#[derive(Debug)]
pub struct AccessAuthTestServer {
    /// Running Waltr server.
    pub server: &'static TestServer,
    /// Valid API bearer token.
    pub access_token: String,
    /// Seeded browser session cookie header value.
    pub session_cookie: String,
    /// Issuer identifier used by the OIDC fixture.
    pub issuer: String,
    _issuer_thread: thread::JoinHandle<()>,
}

impl AccessAuthTestServer {
    /// Sign a valid API token with a bounded lifetime.
    #[must_use]
    pub fn access_token_with_ttl(&self, ttl: Duration) -> String {
        sign_access_token_with_ttl(&self.issuer, "waltr:read", ttl)
    }
}

/// File-backed access-auth fixture that can reconstruct the application.
#[derive(Debug)]
pub struct AccessAuthBrowserFixture {
    issuer: String,
    database_url: String,
    _root: PathBuf,
    _issuer_thread: thread::JoinHandle<()>,
}

/// Restartable auth-enabled server for browser flow tests.
#[derive(Debug)]
pub struct RestartableAccessAuthServer {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl RestartableAccessAuthServer {
    /// Build an absolute URL for a server path.
    #[must_use]
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    /// Return the base URL for tonic clients.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Stop this server and wait for its listener to close.
    pub fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().expect("auth test server should stop cleanly");
        }
    }
}

impl Drop for RestartableAccessAuthServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().expect("auth test server should stop cleanly");
        }
    }
}

impl AccessAuthBrowserFixture {
    /// Sign a bearer token from this fixture's issuer with a short lifetime.
    #[must_use]
    pub fn access_token(&self, ttl: Duration) -> String {
        sign_access_token_with_ttl(&self.issuer, "waltr:read", ttl)
    }

    /// Start a newly constructed app over this fixture's persistent database.
    #[must_use]
    pub fn start_server(&self, session_ttl: Duration) -> RestartableAccessAuthServer {
        start_restartable_auth_server(
            &self.issuer,
            "waltr-test-client",
            None,
            &self.database_url,
            session_ttl,
            false,
            false,
            false,
            false,
        )
    }

    /// Start a reconstructed app with a different browser client ID.
    #[must_use]
    pub fn start_server_with_client_id(
        &self,
        session_ttl: Duration,
        client_id: &str,
    ) -> RestartableAccessAuthServer {
        start_restartable_auth_server(
            &self.issuer,
            client_id,
            None,
            &self.database_url,
            session_ttl,
            false,
            false,
            false,
            false,
        )
    }

    /// Start a reconstructed app with a confidential browser client secret.
    #[must_use]
    pub fn start_server_with_client_secret(
        &self,
        session_ttl: Duration,
        client_secret: &str,
    ) -> RestartableAccessAuthServer {
        start_restartable_auth_server(
            &self.issuer,
            "waltr-test-client",
            Some(client_secret),
            &self.database_url,
            session_ttl,
            false,
            false,
            false,
            false,
        )
    }

    /// Start a reconstructed app against a different discovered issuer.
    #[must_use]
    pub fn start_server_with_new_issuer(
        &self,
        session_ttl: Duration,
    ) -> RestartableAccessAuthServer {
        let issuer = start_fake_issuer();
        start_restartable_auth_server(
            &issuer.url,
            "waltr-test-client",
            None,
            &self.database_url,
            session_ttl,
            false,
            false,
            false,
            false,
        )
    }

    /// Count browser sessions revoked in this fixture's durable database.
    pub async fn revoked_browser_session_count(&self) -> i64 {
        let pool = sqlx::SqlitePool::connect(&self.database_url)
            .await
            .expect("browser fixture database should connect");
        sqlx::query_scalar(
            "SELECT count(*) FROM waltr_browser_sessions WHERE revoked_at_ms IS NOT NULL",
        )
        .fetch_one(&pool)
        .await
        .expect("browser fixture revocation count should load")
    }

    /// Load the persisted expiry for an issued browser-session cookie.
    pub async fn browser_session_expiry(&self, session_cookie: &str) -> SystemTime {
        use std::fmt::Write as _;

        let token = session_cookie
            .split_once('=')
            .map(|(_, token)| token)
            .expect("session cookie should contain a token");
        let mut digest = String::with_capacity(64);
        for byte in Sha256::digest(token.as_bytes()) {
            write!(&mut digest, "{byte:02x}").expect("writing to a String cannot fail");
        }
        let pool = sqlx::SqlitePool::connect(&self.database_url)
            .await
            .expect("browser fixture database should connect");
        let expires_at_ms: i64 = sqlx::query_scalar(
            "SELECT expires_at_ms FROM waltr_browser_sessions WHERE token_digest = ?",
        )
        .bind(digest)
        .fetch_one(&pool)
        .await
        .expect("browser session expiry should load");
        SystemTime::UNIX_EPOCH
            + Duration::from_millis(
                u64::try_from(expires_at_ms).expect("browser session expiry should be positive"),
            )
    }

    /// Make subsequent browser-session revalidation fail at the store boundary.
    pub async fn break_browser_session_revalidation(&self) {
        let pool = sqlx::SqlitePool::connect(&self.database_url)
            .await
            .expect("browser fixture database should connect");
        sqlx::query("DROP TABLE waltr_browser_sessions")
            .execute(&pool)
            .await
            .expect("browser session table should be removable for failure testing");
        pool.close().await;
    }

    /// Start an app whose post-session-insert advisory cleanup is forced to fail.
    #[must_use]
    pub fn start_server_with_callback_cleanup_failure(
        &self,
        session_ttl: Duration,
    ) -> RestartableAccessAuthServer {
        start_restartable_auth_server(
            &self.issuer,
            "waltr-test-client",
            None,
            &self.database_url,
            session_ttl,
            true,
            false,
            false,
            false,
        )
    }

    /// Start a reconstructed app with the local hosted MCP OAuth server enabled.
    #[must_use]
    pub fn start_server_with_mcp_oauth(
        &self,
        session_ttl: Duration,
    ) -> RestartableAccessAuthServer {
        start_restartable_auth_server(
            &self.issuer,
            "waltr-test-client",
            None,
            &self.database_url,
            session_ttl,
            false,
            true,
            false,
            false,
        )
    }

    /// Start an app with DCR and an explicit HTTP loopback redirect policy.
    #[must_use]
    pub fn start_server_with_mcp_oauth_dcr(
        &self,
        session_ttl: Duration,
        allow_loopback_redirects: bool,
    ) -> RestartableAccessAuthServer {
        start_restartable_auth_server(
            &self.issuer,
            "waltr-test-client",
            None,
            &self.database_url,
            session_ttl,
            false,
            true,
            true,
            allow_loopback_redirects,
        )
    }

    /// Seed a browser session for an OIDC subject and return its cookie header value.
    pub async fn browser_session_for_subject(&self, subject: &str) -> String {
        let token = format!(
            "test-browser-session-{}",
            TEMP_ROOT_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let now = SystemTime::now();
        let persistence = Persistence::initialize(
            DatabaseConfig::from_url(&self.database_url).expect("fixture database URL is valid"),
        )
        .await
        .expect("fixture persistence should initialize");
        AccessAuthStore::new(&persistence)
            .insert_browser_session(&BrowserSession {
                token_digest: BrowserSessionTokenDigest::from_token(&token),
                issuer: self.issuer.clone(),
                client_id: "waltr-test-client".to_owned(),
                subject: subject.to_owned(),
                created_at: now,
                expires_at: now + Duration::from_hours(1),
                last_seen_at: now,
            })
            .await
            .expect("browser session should seed");
        format!("waltr-session-local={token}")
    }

    /// Re-sign an issued hosted MCP token with a bounded lifetime.
    pub async fn mcp_access_token_with_ttl(&self, access_token: &str, ttl: Duration) -> String {
        let mut pieces = access_token.split('.');
        let encoded_header = pieces.next().expect("access token should contain a header");
        let encoded_claims = pieces.next().expect("access token should contain claims");
        assert!(
            pieces.next().is_some(),
            "access token should contain a signature"
        );
        assert!(
            pieces.next().is_none(),
            "access token should have three parts"
        );
        let header: McpJwtAccessTokenHeader = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(encoded_header)
                .expect("access token header should be base64url"),
        )
        .expect("access token header should decode");
        let mut claims: Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(encoded_claims)
                .expect("access token claims should be base64url"),
        )
        .expect("access token claims should decode");
        let now = jsonwebtoken::get_current_timestamp();
        claims["iat"] = json!(now);
        claims["nbf"] = json!(now.saturating_sub(30));
        claims["exp"] = json!(now + ttl.as_secs());

        let pool = sqlx::SqlitePool::connect(&self.mcp_database_url())
            .await
            .expect("MCP fixture database should connect");
        let (ciphertext, wrapping_key_id, nonce): (Vec<u8>, String, Vec<u8>) = sqlx::query_as(
            "SELECT encrypted_private_material, wrapping_key_id, encryption_nonce FROM mcp_oauth_signing_keys WHERE key_id = ? AND state = 'active'",
        )
        .bind(&header.kid)
        .fetch_one(&pool)
        .await
        .expect("active MCP signing key should load");
        pool.close().await;
        let wrapping_key = URL_SAFE_NO_PAD
            .decode("BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc")
            .expect("test wrapping key should decode");
        let keyring =
            VersionedOAuthWrappingKeyring::new("test", [("test".to_owned(), wrapping_key)])
                .expect("test wrapping keyring should be valid");
        let private = keyring
            .decrypt(
                "ES256",
                &header.kid,
                &OAuthEncryptedPrivateKey {
                    wrapping_key_id,
                    nonce,
                    ciphertext,
                },
            )
            .expect("MCP signing key should decrypt");
        let secret = SecretKey::from_pkcs8_der(&private).expect("MCP signing key should parse");
        let encoded_claims = URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).expect("MCP access token claims should encode"));
        let message = format!("{encoded_header}.{encoded_claims}");
        let signature: Signature = SigningKey::from(secret).sign(message.as_bytes());
        format!("{message}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()))
    }

    fn mcp_database_url(&self) -> String {
        let waltr_path = PathBuf::from(
            self.database_url
                .strip_prefix("sqlite://")
                .expect("test database uses sqlite://"),
        );
        format!(
            "sqlite://{}",
            waltr_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("mcp.sqlite3")
                .display()
        )
    }

    /// Corrupt the active hosted OAuth public JWK while preserving its encrypted private key.
    pub async fn corrupt_mcp_signing_public_key(&self) {
        let waltr_path = PathBuf::from(
            self.database_url
                .strip_prefix("sqlite://")
                .expect("test database uses sqlite://"),
        );
        let mcp_url = format!(
            "sqlite://{}",
            waltr_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("mcp.sqlite3")
                .display()
        );
        let pool = sqlx::SqlitePool::connect(&mcp_url)
            .await
            .expect("MCP fixture database should connect");
        sqlx::query("UPDATE mcp_oauth_signing_keys SET public_jwk = json_set(public_jwk, '$.x', 'invalid') WHERE state = 'active'")
            .execute(&pool)
            .await
            .expect("active signing JWK should be corruptible");
        pool.close().await;
    }

    /// Rotate the hosted OAuth signing key through a separate replica composition.
    pub async fn rotate_mcp_signing_key(&self) {
        let persistence = Persistence::initialize(
            DatabaseConfig::from_url(&self.database_url).expect("fixture database URL is valid"),
        )
        .await
        .expect("replica persistence should initialize");
        let authenticator = AccessAuthenticator::initialize(
            AccessAuthConfig::oidc(
                OidcConfig::new(
                    self.issuer.clone(),
                    "waltr-test-client",
                    None,
                    "http://127.0.0.1/oidc/callback",
                    "waltr-api",
                    vec!["openid".to_owned()],
                    Duration::from_hours(1),
                    true,
                    true,
                )
                .expect("fixture OIDC config is valid"),
            ),
            &persistence,
            AccessAuthHttpClient::build(reqwest::Client::builder().timeout(Duration::from_secs(5)))
                .expect("fixture OIDC client should build"),
        )
        .await
        .expect("replica access authenticator should initialize");
        let key_file = PathBuf::from(
            self.database_url
                .strip_prefix("sqlite://")
                .expect("test database uses sqlite://"),
        )
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("mcp-wrapping-keys.json");
        let oauth = McpOAuthServer::initialize(
            McpOAuthEnabledConfig::new(
                "https://waltr.example",
                "https://waltr.example/oauth",
                vec!["mcp:use".to_owned()],
                key_file,
            )
            .expect("fixture MCP OAuth config is valid")
            .with_access_token_ttl(Duration::from_hours(1)),
            &persistence,
            authenticator,
        )
        .await
        .expect("replica MCP OAuth should initialize");
        oauth
            .maintain(true)
            .await
            .expect("replica should rotate the signing key");
    }
}

/// Create a full browser-flow fixture with durable `SQLite` persistence.
#[must_use]
pub fn setup_access_auth_browser_fixture() -> AccessAuthBrowserFixture {
    let issuer = start_fake_issuer();
    let root = temp_root();
    let database_url = format!("sqlite://{}", root.join("access-auth.sqlite3").display());
    AccessAuthBrowserFixture {
        issuer: issuer.url,
        database_url,
        _root: root,
        _issuer_thread: issuer.thread,
    }
}

/// Start an auth-enabled server backed by a deterministic loopback OIDC issuer.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "the fixture owns one complete server lifecycle"
)]
pub fn setup_access_auth_server() -> AccessAuthTestServer {
    let fake_issuer = start_fake_issuer();
    let issuer = fake_issuer.url;
    let issuer_thread = fake_issuer.thread;
    let access_token = sign_access_token(&issuer, "waltr:read");
    let browser_token = "deterministic-browser-session";
    let session_cookie = format!("waltr-session-local={browser_token}");
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let issuer_for_server = issuer.clone();
    let server_thread = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("auth test server runtime should start");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("auth test server should bind");
            let addr = listener.local_addr().expect("bound address should exist");
            let persistence = test_persistence().await;
            let config = OidcConfig::new(
                &issuer_for_server,
                "waltr-test-client",
                None,
                format!("http://{addr}/oidc/callback"),
                "waltr-api",
                vec!["openid".to_owned()],
                Duration::from_hours(1),
                true,
                true,
            )
            .expect("auth test config should be valid");
            let authenticator = AccessAuthenticator::initialize(
                AccessAuthConfig::oidc(config),
                &persistence,
                AccessAuthHttpClient::build(
                    reqwest::Client::builder().timeout(Duration::from_secs(5)),
                )
                .expect("auth test client should build"),
            )
            .await
            .expect("auth test provider should initialize");
            let now = SystemTime::now();
            AccessAuthStore::new(&persistence)
                .insert_browser_session(&BrowserSession {
                    token_digest: BrowserSessionTokenDigest::from_token(browser_token),
                    issuer: issuer_for_server,
                    client_id: "waltr-test-client".to_owned(),
                    subject: "browser-subject".to_owned(),
                    created_at: now,
                    expires_at: now + Duration::from_hours(1),
                    last_seen_at: now,
                })
                .await
                .expect("browser session should seed");
            let catalog_loader = default_catalog_loader();
            let config_service = config_service_with_loader(&catalog_loader);
            let app = build_app_with_catalog_loader_and_access_authenticator(
                config_service,
                persistence,
                catalog_loader,
                authenticator,
                |routes, config, credential_store| {
                    session::add_sessions_service(
                        routes,
                        SessionCoordinator::new(MemorySessionStore::new()),
                        config,
                        credential_store,
                        None,
                    )
                },
            )
            .await
            .expect("test server config should load");
            ready_tx
                .send(addr)
                .expect("auth test server should report ready");
            app.serve_with_shutdown(listener, std::future::pending())
                .await
                .expect("auth test server should run");
        });
    });
    let addr = ready_rx
        .recv()
        .expect("auth test server should initialize before serving");
    let server = Box::leak(Box::new(TestServer {
        addr,
        _thread: server_thread,
    }));
    AccessAuthTestServer {
        server,
        access_token,
        session_cookie,
        issuer,
        _issuer_thread: issuer_thread,
    }
}

/// Start an isolated integration test server.
#[must_use]
pub fn setup_server() -> &'static TestServer {
    let catalog_loader = default_catalog_loader();
    let config_service = config_service_with_loader(&catalog_loader);
    let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
    setup_server_with_listener_and_catalog_loader(config_service, listener, catalog_loader)
}

/// Start an isolated integration test server at a fixed address.
#[must_use]
pub fn setup_server_at(addr: SocketAddr) -> &'static TestServer {
    let catalog_loader = default_catalog_loader();
    let config_service = config_service_with_loader(&catalog_loader);
    let listener = TcpListener::bind(addr).expect("test server should bind fixed address");
    setup_server_with_listener_and_catalog_loader(config_service, listener, catalog_loader)
}

/// Start an isolated integration test server with an explicit config service.
#[must_use]
pub fn setup_server_with_services(config_service: Arc<ConfigService>) -> &'static TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
    setup_server_with_listener(config_service, listener)
}

/// Start an isolated integration test server with an explicit catalog loader.
#[must_use]
pub fn setup_server_with_catalog_loader(catalog_loader: CatalogLoader) -> &'static TestServer {
    let config_service = config_service_with_loader(&catalog_loader);
    let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
    setup_server_with_listener_and_catalog_loader(config_service, listener, catalog_loader)
}

/// Start an isolated integration test server with an explicit session coordinator.
#[must_use]
pub fn setup_server_with_sessions() -> (&'static TestServer, SessionCoordinator<MemorySessionStore>)
{
    let catalog_loader = default_catalog_loader();
    let config_service = config_service_with_loader(&catalog_loader);
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
    let server = setup_server_with_listener_sessions_and_catalog_loader(
        config_service,
        listener,
        sessions.clone(),
        catalog_loader,
    );
    (server, sessions)
}

/// Start an isolated integration test server with a caller-seeded session coordinator.
#[must_use]
pub fn setup_server_with_seeded_sessions(
    sessions: SessionCoordinator<MemorySessionStore>,
) -> &'static TestServer {
    let catalog_loader = default_catalog_loader();
    let config_service = config_service_with_loader(&catalog_loader);
    let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
    setup_server_with_listener_sessions_and_catalog_loader(
        config_service,
        listener,
        sessions,
        catalog_loader,
    )
}

fn setup_server_with_listener(
    config_service: Arc<ConfigService>,
    listener: TcpListener,
) -> &'static TestServer {
    setup_server_with_listener_and_sessions(
        config_service,
        listener,
        SessionCoordinator::new(MemorySessionStore::new()),
    )
}

fn setup_server_with_listener_and_catalog_loader(
    config_service: Arc<ConfigService>,
    listener: TcpListener,
    catalog_loader: CatalogLoader,
) -> &'static TestServer {
    setup_server_with_listener_sessions_and_catalog_loader(
        config_service,
        listener,
        SessionCoordinator::new(MemorySessionStore::new()),
        catalog_loader,
    )
}

fn setup_server_with_listener_and_sessions(
    config_service: Arc<ConfigService>,
    listener: TcpListener,
    sessions: SessionCoordinator<MemorySessionStore>,
) -> &'static TestServer {
    let catalog_loader = config_service.catalog_loader();
    setup_server_with_listener_sessions_and_catalog_loader(
        config_service,
        listener,
        sessions,
        catalog_loader,
    )
}

fn setup_server_with_listener_sessions_and_catalog_loader(
    config_service: Arc<ConfigService>,
    listener: TcpListener,
    sessions: SessionCoordinator<MemorySessionStore>,
    catalog_loader: CatalogLoader,
) -> &'static TestServer {
    listener
        .set_nonblocking(true)
        .expect("test server listener should be nonblocking");
    let addr = listener
        .local_addr()
        .expect("test server local address should be available");

    let server_thread = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("test server runtime should start");

        runtime.block_on(async move {
            let persistence = Persistence::initialize(
                DatabaseConfig::for_tests().expect("test persistence config should be valid"),
            )
            .await
            .expect("test persistence should initialize");
            let listener = tokio::net::TcpListener::from_std(listener)
                .expect("test server listener should become async");
            build_app_with_catalog_loader(
                config_service,
                persistence,
                catalog_loader,
                |routes, config, credential_store| {
                    session::add_sessions_service(routes, sessions, config, credential_store, None)
                },
            )
            .await
            .expect("test server config should load")
            .serve_with_shutdown(listener, std::future::pending())
            .await
            .expect("test server should run");
        });
    });

    Box::leak(Box::new(TestServer {
        addr,
        _thread: server_thread,
    }))
}
async fn test_persistence() -> Persistence {
    Persistence::initialize(
        DatabaseConfig::for_tests().expect("test persistence config should be valid"),
    )
    .await
    .expect("test persistence should initialize")
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the fixture exposes independent authentication server settings"
)]
fn start_restartable_auth_server(
    issuer: &str,
    client_id: &str,
    client_secret: Option<&str>,
    database_url: &str,
    session_ttl: Duration,
    fail_callback_cleanup: bool,
    enable_mcp_oauth: bool,
    allow_mcp_oauth_dcr: bool,
    allow_mcp_oauth_loopback_redirects: bool,
) -> RestartableAccessAuthServer {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let issuer = issuer.to_owned();
    let client_id = client_id.to_owned();
    let client_secret = client_secret.map(ToOwned::to_owned);
    let database_url = database_url.to_owned();
    let thread = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("restartable auth test runtime should start");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("restartable auth test server should bind");
            let addr = listener.local_addr().expect("bound address should exist");
            let persistence = Persistence::initialize(
                DatabaseConfig::from_url(&database_url)
                    .expect("file-backed test persistence URL should be valid"),
            )
            .await
            .expect("file-backed test persistence should initialize");
            let config = OidcConfig::new(
                issuer,
                client_id,
                client_secret,
                format!("http://{addr}/oidc/callback"),
                "waltr-api",
                vec!["openid".to_owned()],
                session_ttl,
                true,
                true,
            )
            .expect("browser flow config should be valid");
            let authenticator = AccessAuthenticator::initialize(
                AccessAuthConfig::oidc(config),
                &persistence,
                AccessAuthHttpClient::build(
                    reqwest::Client::builder().timeout(Duration::from_secs(5)),
                )
                .expect("browser flow client should build"),
            )
            .await
            .expect("browser flow authenticator should initialize");
            if fail_callback_cleanup {
                sqlx::query(
                    r"
CREATE TRIGGER fail_callback_cleanup
BEFORE DELETE ON waltr_oidc_login_attempts
WHEN EXISTS (
    SELECT 1 FROM waltr_browser_sessions
    WHERE subject = 'browser-subject' AND revoked_at_ms IS NULL
)
BEGIN
    SELECT RAISE(ABORT, 'cleanup failed');
END
                    ",
                )
                .execute(persistence.sqlite_pool().expect("SQLite test persistence"))
                .await
                .expect("callback cleanup failure trigger should install");
            }
            let catalog_loader = default_catalog_loader();
            let config_service = config_service_with_loader(&catalog_loader);
            let mcp_oauth = if enable_mcp_oauth {
                let key_file = PathBuf::from(
                    database_url
                        .strip_prefix("sqlite://")
                        .expect("test database uses sqlite://"),
                )
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("mcp-wrapping-keys.json");
                if !key_file.exists() {
                    std::fs::write(
                        &key_file,
                        r#"{"schema_version":1,"active":"test","keys":[{"id":"test","key":"BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc"}]}"#,
                    )
                    .expect("test wrapping key should be written");
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt as _;
                        std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o400))
                            .expect("test wrapping key should be read-only");
                    }
                }
                let oauth = McpOAuthServer::initialize(
                    McpOAuthEnabledConfig::new(
                        "https://waltr.example",
                        "https://waltr.example/oauth",
                        vec!["mcp:use".to_owned()],
                        key_file,
                    )
                    .expect("test MCP OAuth config should be valid")
                    .with_client_registration(allow_mcp_oauth_dcr, false)
                    .with_loopback_redirects(allow_mcp_oauth_loopback_redirects)
                    .with_access_token_ttl(Duration::from_hours(1)),
                    &persistence,
                    authenticator.clone(),
                )
                .await
                .expect("test MCP OAuth server should initialize");
                oauth
                    .register_client(mcp::oauth::OAuthClient {
                        client_id: "test-mcp-client".to_owned(),
                        registration_method: mcp::oauth::OAuthClientRegistrationMethod::Preconfigured,
                        display_name: Some("Test MCP Client".to_owned()),
                        client_secret_digest: None,
                        redirect_uris: vec!["https://client.example/callback".to_owned()],
                        created_at_ms: i64::try_from(
                            SystemTime::now()
                                .duration_since(SystemTime::UNIX_EPOCH)
                                .expect("current time")
                                .as_millis(),
                        )
                        .expect("test timestamp should fit"),
                        expires_at_ms: None,
                        disabled_at_ms: None,
                    })
                    .await
                    .expect("test MCP OAuth client should register");
                Some(oauth)
            } else {
                None
            };
            let trusted_mcp_oauth_origin = mcp_oauth.as_ref().map(|oauth| {
                reqwest::Url::parse(oauth.resource()).expect("test hosted MCP resource URL")
            });
            let app = build_app_with_catalog_loader_and_access_authenticator_and_mcp_oauth(
                config_service,
                persistence,
                catalog_loader,
                authenticator,
                mcp_oauth,
                move |routes, config, credential_store| {
                    session::add_sessions_service(
                        routes,
                        SessionCoordinator::new(MemorySessionStore::new()),
                        config,
                        credential_store,
                        trusted_mcp_oauth_origin,
                    )
                },
            )
            .await
            .expect("test server config should load");
            ready_tx.send(addr).expect("server should report ready");
            app.serve_with_shutdown(listener, async move {
                drop(shutdown_rx.await);
            })
            .await
            .expect("restartable auth test server should run");
        });
    });
    RestartableAccessAuthServer {
        addr: ready_rx.recv().expect("server should initialize"),
        shutdown: Some(shutdown_tx),
        thread: Some(thread),
    }
}

struct FakeIssuerHandle {
    url: String,
    thread: thread::JoinHandle<()>,
}

#[derive(Debug)]
struct FakeIssuerState {
    issuer: String,
    pending: Mutex<HashMap<String, PendingAuthorization>>,
}

#[derive(Debug)]
struct PendingAuthorization {
    nonce: String,
    challenge: String,
    redirect_uri: String,
}

fn start_fake_issuer() -> FakeIssuerHandle {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let thread = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("fake issuer runtime should start");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("fake issuer should bind");
            let issuer = format!(
                "http://{}",
                listener.local_addr().expect("issuer address should exist")
            );
            let state = Arc::new(FakeIssuerState {
                issuer: issuer.clone(),
                pending: Mutex::new(HashMap::new()),
            });
            let app = Router::new()
                .route("/.well-known/openid-configuration", get(discovery))
                .route("/jwks", get(jwks))
                .route("/authorize", get(authorize))
                .route("/token", post(token))
                .with_state(state);
            ready_tx
                .send(issuer)
                .expect("fake issuer should report ready");
            axum::serve(listener, app)
                .await
                .expect("fake issuer should run");
        });
    });
    FakeIssuerHandle {
        url: ready_rx.recv().expect("fake issuer should initialize"),
        thread,
    }
}

async fn discovery(State(state): State<Arc<FakeIssuerState>>) -> Json<Value> {
    let issuer = &state.issuer;
    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "jwks_uri": format!("{issuer}/jwks"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none", "client_secret_post"],
        "id_token_signing_alg_values_supported": ["RS256"]
    }))
}

async fn jwks() -> Json<Value> {
    Json(json!({"keys": [{
        "kty": "RSA",
        "kid": TEST_KEY_ID,
        "use": "sig",
        "alg": "RS256",
        "n": "jTXRkLxK45qmKMMdODisx4iMHYQbwI_WncwB-9usF31IUewAgcVtRMWpW9YJRkA713NsP-cfs4NyurzloJ955iwNGJmzBF6_E515c-QFyAAEyyFptkpFqcgZBFUZGHslz8AlgUysSyshjOCNG15VEg-gjdLwDJW1RrvQQC-w1YHj3sgZV_XWfYWy401T5Gc7OdxstVK1uIj2Lg5xusvKGCEBuDdYZmTjgzuQqF2rGFWVC7r4ag3C8xxBfwApRZtk7XvZac3kAtYDCVlHyFbtkmapONybuDijJXzwVfS96x0I7PoXc-c6tSPf-7WpNtIDoLxHcI5Qv66tfsOdfPCc8w",
        "e": "AQAB"
    }]}))
}

#[derive(Deserialize)]
struct AuthorizationQuery {
    response_type: String,
    client_id: String,
    redirect_uri: String,
    scope: String,
    state: String,
    nonce: String,
    code_challenge: String,
    code_challenge_method: String,
}

async fn authorize(
    State(issuer): State<Arc<FakeIssuerState>>,
    Query(query): Query<AuthorizationQuery>,
) -> Response {
    if query.response_type != "code"
        || query.client_id != "waltr-test-client"
        || query.scope != "openid"
        || query.code_challenge_method != "S256"
        || query.state.is_empty()
        || query.nonce.is_empty()
        || query.code_challenge.is_empty()
        || Url::parse(&query.redirect_uri).is_err()
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let code = "deterministic-authorization-code".to_owned();
    issuer.pending.lock().await.insert(
        code.clone(),
        PendingAuthorization {
            nonce: query.nonce,
            challenge: query.code_challenge,
            redirect_uri: query.redirect_uri.clone(),
        },
    );
    let mut callback =
        Url::parse(&query.redirect_uri).expect("validated redirect URI should parse");
    callback
        .query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &query.state);
    let mut response = StatusCode::SEE_OTHER.into_response();
    response.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(callback.as_str()).expect("callback location should be valid"),
    );
    response
}

#[derive(Deserialize)]
struct TokenRequest {
    grant_type: String,
    code: String,
    redirect_uri: String,
    code_verifier: String,
    client_id: String,
    client_secret: Option<String>,
}

async fn token(
    State(issuer): State<Arc<FakeIssuerState>>,
    Form(request): Form<TokenRequest>,
) -> Response {
    let Some(pending) = issuer.pending.lock().await.remove(&request.code) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(request.code_verifier.as_bytes()));
    if request.grant_type != "authorization_code"
        || request.client_id != "waltr-test-client"
        || request.client_secret.as_deref().is_some_and(str::is_empty)
        || request.redirect_uri != pending.redirect_uri
        || challenge != pending.challenge
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let access_token = sign_access_token(&issuer.issuer, "waltr:read");
    let id_token = sign_id_token(&issuer.issuer, &pending.nonce, &access_token);
    Json(json!({
        "access_token": access_token,
        "id_token": id_token,
        "token_type": "Bearer",
        "expires_in": 3600
    }))
    .into_response()
}

fn sign_access_token(issuer: &str, scope: &str) -> String {
    sign_access_token_with_ttl(issuer, scope, Duration::from_hours(1))
}

fn sign_access_token_with_ttl(issuer: &str, scope: &str, ttl: Duration) -> String {
    sign_access_token_for_subject_with_ttl(issuer, "api-subject", scope, ttl)
}

fn sign_access_token_for_subject_with_ttl(
    issuer: &str,
    subject: &str,
    scope: &str,
    ttl: Duration,
) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(TEST_KEY_ID.to_owned());
    header.typ = Some("at+jwt".to_owned());
    sign_token(
        header,
        json!({
            "iss": issuer,
            "sub": subject,
            "aud": "waltr-api",
            "iat": jsonwebtoken::get_current_timestamp(),
            "nbf": jsonwebtoken::get_current_timestamp(),
            "exp": jsonwebtoken::get_current_timestamp() + ttl.as_secs(),
            "scope": scope
        }),
    )
}

fn sign_id_token(issuer: &str, nonce: &str, access_token: &str) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(TEST_KEY_ID.to_owned());
    header.typ = Some("JWT".to_owned());
    let digest = Sha256::digest(access_token.as_bytes());
    sign_token(
        header,
        json!({
            "iss": issuer,
            "sub": "browser-subject",
            "aud": "waltr-test-client",
            "iat": jsonwebtoken::get_current_timestamp(),
            "nbf": jsonwebtoken::get_current_timestamp(),
            "exp": jsonwebtoken::get_current_timestamp() + 3600,
            "nonce": nonce,
            "at_hash": URL_SAFE_NO_PAD.encode(&digest[..digest.len() / 2])
        }),
    )
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "test token components are consumed as complete signing inputs"
)]
fn sign_token(header: Header, claims: Value) -> String {
    encode(
        &header,
        &claims,
        &EncodingKey::from_rsa_pem(TEST_PRIVATE_KEY.as_bytes())
            .expect("test private key should parse"),
    )
    .expect("test access token should encode")
}

#[must_use]
pub fn temp_root() -> PathBuf {
    let counter = TEMP_ROOT_COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "waltr-config-test-{}-{counter}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("temp root should be created");
    root
}
