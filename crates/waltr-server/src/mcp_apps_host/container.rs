//! Dedicated asset-only Apps listener. Never merge this router into the API router.
use crate::http_lifecycle::serve_owned_connections;
use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const MAX_HTML: usize = 1024 * 1024;
const MAX_VIEWS: usize = 64;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const TTL: Duration = Duration::from_secs(600);
const RATE_WINDOW: Duration = Duration::from_secs(60);
const MAX_UPLOADS: usize = 60;
const PERMISSIONS: &str = "camera=(), microphone=(), geolocation=(), clipboard-write=(), fullscreen=(), payment=(), usb=()";
const VIEW_CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; media-src data:; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'; navigate-to 'none'; sandbox allow-scripts";
const CONTAINER_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; connect-src 'self'; frame-src 'self'; object-src 'none'; base-uri 'none'; form-action 'none'";

/// Explicit asset listener policy. External binds and container origins are operator choices.
#[derive(Clone, Debug)]
pub struct AppsContainerConfig {
    /// Defaults to ephemeral IPv4 loopback.
    pub bind: SocketAddr,
    /// Exact public HTTP(S) origin; absent only for loopback listeners.
    pub public_origin: Option<String>,
    /// Exact trusted parent origins; no wildcard, path, or opaque origins.
    pub allowed_host_origins: Vec<String>,
}

impl Default for AppsContainerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:0".parse().unwrap(),
            public_origin: None,
            allowed_host_origins: Vec::new(),
        }
    }
}

struct View {
    html: String,
    expires: Instant,
}
#[derive(Default)]
struct Store {
    views: HashMap<String, View>,
    uploads: VecDeque<Instant>,
}
impl Store {
    fn expire(&mut self) {
        let now = Instant::now();
        self.views.retain(|_, view| view.expires > now);
        while self
            .uploads
            .front()
            .is_some_and(|at| *at + RATE_WINDOW <= now)
        {
            self.uploads.pop_front();
        }
    }
}

struct Inner {
    origin: String,
    container_csp: String,
    view_csp: String,
    store: Mutex<Store>,
    cancel: CancellationToken,
    expiry_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Reusable isolated router and bounded, expiring HTML store.
#[derive(Clone)]
pub struct AppsContainer(Arc<Inner>);

impl std::fmt::Debug for AppsContainer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppsContainer")
            .finish_non_exhaustive()
    }
}

fn origin(value: &str, native: bool) -> Result<(), &'static str> {
    if native && value == "tauri://localhost" {
        return Ok(());
    }
    let url = url::Url::parse(value).map_err(|_| "invalid Apps origin")?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.origin().ascii_serialization() != value
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Apps origins must be exact HTTP(S) or packaged native origins");
    }
    Ok(())
}

impl AppsContainer {
    /// Construct using prebuilt SDK assets. Rejects missing assets and unsafe origin policies.
    pub fn new(
        public_origin: String,
        allowed_host_origins: Vec<String>,
    ) -> Result<Self, &'static str> {
        if super::assets::CONTAINER_HTML.is_none() || super::assets::CONTAINER_JS.is_none() {
            return Err(
                "MCP Apps assets are disabled; compile with default features or apps-host-assets",
            );
        }
        origin(&public_origin, false)?;
        if allowed_host_origins.is_empty() || allowed_host_origins.len() > 20 {
            return Err("Apps requires 1..20 allowed host origins");
        }
        for host in &allowed_host_origins {
            origin(host, true)?;
            if host == &public_origin {
                return Err("Apps container must have a separate origin");
            }
        }
        let hosts = allowed_host_origins.join(" ");
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| "Apps container requires a Tokio runtime")?;
        let inner = Arc::new(Inner {
            container_csp: format!("{CONTAINER_CSP}; frame-ancestors {hosts}"),
            view_csp: format!("{VIEW_CSP}; frame-ancestors {public_origin} {hosts}"),
            origin: public_origin,
            store: Mutex::new(Store::default()),
            cancel: CancellationToken::new(),
            expiry_task: Mutex::new(None),
        });
        let weak = Arc::downgrade(&inner);
        let cancel = inner.cancel.clone();
        let task = runtime.spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(10)) => {
                        let Some(inner) = weak.upgrade() else { break };
                        inner.store.lock().unwrap().expire();
                    }
                }
            }
        });
        *inner.expiry_task.lock().unwrap() = Some(task);
        Ok(Self(inner))
    }

    /// Build only asset routes; no Waltr APIs, CORS, auth, cookies, or filesystem access.
    pub fn router(&self) -> Router {
        let state = self.clone();
        Router::new()
            .route("/container.html", get(container_html))
            .route("/container.js", get(container_js))
            .route("/views", post(upload))
            .route("/views/{token}", get(view).delete(remove))
            .layer(DefaultBodyLimit::max(MAX_HTML))
            .layer(axum::middleware::from_fn(
                move |request, next: axum::middleware::Next| {
                    let state = state.clone();
                    async move {
                        let mut response = next.run(request).await;
                        secure(&mut response, &state.0.container_csp);
                        response
                    }
                },
            ))
            .with_state(self.clone())
    }
}

fn secure(response: &mut Response, csp: &str) {
    let headers = response.headers_mut();
    if !headers.contains_key("content-security-policy") {
        headers.insert(
            "content-security-policy",
            HeaderValue::from_str(csp).unwrap(),
        );
    }
    for (name, value) in [
        ("permissions-policy", PERMISSIONS),
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
}
fn asset(text: &'static str, mime: &'static str) -> Response {
    ([("content-type", mime)], text).into_response()
}
async fn container_html() -> Response {
    asset(
        super::assets::CONTAINER_HTML.unwrap(),
        "text/html; charset=utf-8",
    )
}
async fn container_js() -> Response {
    asset(
        super::assets::CONTAINER_JS.unwrap(),
        "text/javascript; charset=utf-8",
    )
}
fn same_origin(headers: &HeaderMap, state: &AppsContainer) -> bool {
    headers.get_all("origin").iter().count() == 1
        && headers.get("origin").and_then(|value| value.to_str().ok())
            == Some(state.0.origin.as_str())
}
fn token_valid(token: &str) -> bool {
    token.len() == 43
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}
async fn upload(State(state): State<AppsContainer>, headers: HeaderMap, body: Bytes) -> Response {
    if !same_origin(&headers, &state) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let content_type = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !matches!(
        content_type.to_ascii_lowercase().as_str(),
        "text/html" | "text/html; charset=utf-8" | "text/html;charset=utf-8"
    ) {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    let Ok(html) = std::str::from_utf8(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mut store = state.0.store.lock().unwrap();
    store.expire();
    if store.uploads.len() >= MAX_UPLOADS {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    store.uploads.push_back(Instant::now());
    if store.views.len() >= MAX_VIEWS
        || store
            .views
            .values()
            .map(|view| view.html.len())
            .sum::<usize>()
            + body.len()
            > MAX_BYTES
    {
        return StatusCode::INSUFFICIENT_STORAGE.into_response();
    }
    let token = loop {
        let token = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
        if !store.views.contains_key(&token) {
            break token;
        }
    };
    store.views.insert(
        token.clone(),
        View {
            html: html.to_owned(),
            expires: Instant::now() + TTL,
        },
    );
    (
        StatusCode::CREATED,
        axum::Json(serde_json::json!({"path":format!("/views/{token}")})),
    )
        .into_response()
}
async fn view(State(state): State<AppsContainer>, Path(token): Path<String>) -> Response {
    if !token_valid(&token) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mut store = state.0.store.lock().unwrap();
    store.expire();
    let Some(view) = store.views.get(&token) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut response = (
        [("content-type", "text/html; charset=utf-8")],
        view.html.clone(),
    )
        .into_response();
    secure(&mut response, &state.0.view_csp);
    response
}
async fn remove(
    State(state): State<AppsContainer>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !same_origin(&headers, &state) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !token_valid(&token) {
        return StatusCode::NOT_FOUND.into_response();
    }
    state.0.store.lock().unwrap().views.remove(&token);
    StatusCode::NO_CONTENT.into_response()
}

/// Owned listener lifecycle. Drop cancels the listener and expiry task.
pub struct SpawnedAppsContainer {
    /// Dedicated public asset origin; contains no API listener or credentials.
    pub public_origin: String,
    cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}
impl std::fmt::Debug for SpawnedAppsContainer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SpawnedAppsContainer")
            .finish_non_exhaustive()
    }
}
impl Drop for SpawnedAppsContainer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
impl SpawnedAppsContainer {
    /// Observe listener termination or requested shutdown; callers must still join with `shutdown`.
    pub fn stopped(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Stop accepting, drain for up to five seconds, then abort and join all connections and expiry.
    pub async fn shutdown(mut self) -> std::io::Result<()> {
        self.cancel.cancel();
        self.task
            .take()
            .unwrap()
            .await
            .map_err(std::io::Error::other)?
    }
}

/// Prebind a dedicated listener, validate its public-origin policy, and start expiry.
pub async fn spawn_app_container(
    config: AppsContainerConfig,
) -> Result<SpawnedAppsContainer, Box<dyn std::error::Error + Send + Sync>> {
    if !config.bind.ip().is_loopback() && config.public_origin.is_none() {
        return Err("external Apps binds require an explicit public origin".into());
    }
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    let public_origin = config
        .public_origin
        .unwrap_or_else(|| format!("http://{}", listener.local_addr().unwrap()));
    let public_url = url::Url::parse(&public_origin)?;
    let public_loopback = match public_url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(host)) => host == "localhost",
        None => false,
    };
    let local_addr = listener.local_addr()?;
    if public_loopback
        && (!local_addr.ip().is_loopback()
            || public_url.port_or_known_default() != Some(local_addr.port())
            || match public_url.host() {
                Some(url::Host::Ipv4(ip)) => std::net::IpAddr::V4(ip) != local_addr.ip(),
                Some(url::Host::Ipv6(ip)) => std::net::IpAddr::V6(ip) != local_addr.ip(),
                _ => false,
            })
    {
        return Err("loopback Apps public origin must match the loopback listener port".into());
    }
    let container = AppsContainer::new(public_origin.clone(), config.allowed_host_origins)?;
    let router = container.router();
    let cancel = CancellationToken::new();
    let shutdown = cancel.clone();
    let task = tokio::spawn(async move {
        let expiry_cancel = container.0.cancel.clone();
        let shutdown_signal = shutdown.clone();
        let result = serve_owned_connections(listener, router, async move {
            shutdown_signal.cancelled().await;
            expiry_cancel.cancel();
        })
        .await;
        shutdown.cancel();
        container.0.cancel.cancel();
        let expiry = container.0.expiry_task.lock().unwrap().take();
        if let Some(expiry) = expiry {
            expiry.await.map_err(std::io::Error::other)?;
        }
        drop(container);
        result
    });
    Ok(SpawnedAppsContainer {
        public_origin,
        cancel,
        task: Some(task),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "apps-host-assets")]
    #[tokio::test]
    async fn shutdown_aborts_and_joins_partial_upload_connections() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let spawned = spawn_app_container(AppsContainerConfig {
            allowed_host_origins: vec!["https://host.example".into()],
            ..Default::default()
        })
        .await
        .unwrap();
        let addr = spawned.public_origin.strip_prefix("http://").unwrap();
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream.write_all(format!("POST /views HTTP/1.1\r\nHost: {addr}\r\nOrigin: {}\r\nContent-Type: text/html\r\nContent-Length: 10\r\n\r\nx", spawned.public_origin).as_bytes()).await.unwrap();
        // A complete preceding request on another connection proves the accept loop is running.
        reqwest::get(format!("{}/container.html", spawned.public_origin))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(7), spawned.shutdown())
            .await
            .unwrap()
            .unwrap();
        drop(stream.write_all(b"123456789").await);
        let mut bytes = [0; 1024];
        let read = tokio::time::timeout(Duration::from_secs(1), stream.read(&mut bytes))
            .await
            .unwrap();
        assert!(
            matches!(read, Ok(0) | Err(_)),
            "upload responded after shutdown: {read:?}"
        );
    }

    #[cfg(feature = "apps-host-assets")]
    #[tokio::test]
    async fn draining_partial_body_never_dispatches_or_inserts_a_view() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let container = AppsContainer::new(
            format!("http://{addr}"),
            vec!["https://host.example".into()],
        )
        .unwrap();
        let accepted = Arc::new(tokio::sync::Notify::new());
        let notify = accepted.clone();
        let router = container.router().layer(axum::middleware::from_fn(
            move |request, next: axum::middleware::Next| {
                let notify = notify.clone();
                async move {
                    notify.notify_one();
                    next.run(request).await
                }
            },
        ));
        let cancel = CancellationToken::new();
        let server = tokio::spawn(serve_owned_connections(
            listener,
            router,
            cancel.clone().cancelled_owned(),
        ));
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream.write_all(format!("POST /views HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://{addr}\r\nContent-Type: text/html\r\nContent-Length: 10\r\n\r\nx").as_bytes()).await.unwrap();
        accepted.notified().await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(7), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        drop(stream.write_all(b"123456789").await);
        let mut bytes = [0; 1024];
        let read = tokio::time::timeout(Duration::from_secs(1), stream.read(&mut bytes))
            .await
            .unwrap();
        assert!(matches!(read, Ok(0) | Err(_)));
        let store = container.0.store.lock().unwrap();
        assert!(store.uploads.is_empty());
        assert!(store.views.is_empty());
    }

    #[test]
    fn exact_origins_reject_injection_credentials_paths_and_opaque_hosts() {
        for invalid in [
            "*",
            "null",
            "https://host.example/",
            "https://host.example/path",
            "https://user:secret@host.example",
            "https://host.example?x=1",
            "https://host.example\nhttps://evil.example",
            "file://localhost",
            "tauri://evil",
        ] {
            assert!(origin(invalid, true).is_err(), "{invalid}");
        }
        for valid in [
            "https://host.example",
            "http://127.0.0.1:8080",
            "https://tauri.localhost",
            "tauri://localhost",
        ] {
            assert!(origin(valid, true).is_ok());
        }
    }

    #[cfg(not(feature = "apps-host-assets"))]
    #[test]
    fn missing_assets_fail_closed() {
        assert!(
            AppsContainer::new(
                "http://127.0.0.1:8080".into(),
                vec!["https://host.example".into()]
            )
            .is_err()
        );
    }

    #[cfg(feature = "apps-host-assets")]
    async fn request(
        router: &Router,
        method: &str,
        path: &str,
        origin: Option<&str>,
        body: Vec<u8>,
    ) -> Response {
        use tower::ServiceExt as _;
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "text/html; charset=utf-8");
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        router
            .clone()
            .oneshot(request.body(axum::body::Body::from(body)).unwrap())
            .await
            .unwrap()
    }

    #[cfg(feature = "apps-host-assets")]
    fn container() -> AppsContainer {
        AppsContainer::new(
            "http://127.0.0.1:8080".into(),
            vec!["https://host.example".into()],
        )
        .unwrap()
    }

    #[cfg(feature = "apps-host-assets")]
    #[tokio::test(start_paused = true)]
    async fn asset_only_contract_origin_headers_random_tokens_expiry_and_delete() {
        let container = container();
        let router = container.router();
        for path in [
            "/mcp",
            "/health",
            "/ready",
            "/waltr.session.v1.SessionService/ListSessions",
            "/",
            "/views/invalid",
        ] {
            let response = request(&router, "GET", path, None, vec![]).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            assert_eq!(response.headers()["cache-control"], "no-store");
            assert!(
                !response
                    .headers()
                    .contains_key("access-control-allow-origin")
            );
            assert!(!response.headers().contains_key("set-cookie"));
        }
        let response = request(&router, "GET", "/container.html", None, vec![]).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["content-security-policy"],
            format!("{CONTAINER_CSP}; frame-ancestors https://host.example")
        );
        for origin in [
            None,
            Some("null"),
            Some("https://host.example"),
            Some("http://127.0.0.1:8081"),
        ] {
            assert_eq!(
                request(&router, "POST", "/views", origin, b"secret-html".to_vec())
                    .await
                    .status(),
                StatusCode::FORBIDDEN
            );
        }
        let html = b"<!doctype html><html><body><script>console.log('app')</script></body></html>";
        let mut paths = Vec::new();
        for _ in 0..2 {
            let response = request(
                &router,
                "POST",
                "/views",
                Some(&container.0.origin),
                html.to_vec(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::CREATED);
            let bytes = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let path = value["path"].as_str().unwrap().to_owned();
            assert!(token_valid(path.strip_prefix("/views/").unwrap()));
            paths.push(path);
        }
        assert_ne!(paths[0], paths[1]);
        let response = request(&router, "GET", &paths[0], None, vec![]).await;
        assert_eq!(
            response.headers()["content-security-policy"],
            container.0.view_csp
        );
        assert_eq!(response.headers()["permissions-policy"], PERMISSIONS);
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(
            axum::body::to_bytes(response.into_body(), MAX_HTML)
                .await
                .unwrap()
                .as_ref(),
            html
        );
        assert_eq!(
            request(&router, "DELETE", &paths[0], None, vec![])
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        for _ in 0..2 {
            assert_eq!(
                request(
                    &router,
                    "DELETE",
                    &paths[0],
                    Some(&container.0.origin),
                    vec![]
                )
                .await
                .status(),
                StatusCode::NO_CONTENT
            );
        }
        tokio::task::yield_now().await;
        tokio::time::advance(TTL).await;
        tokio::task::yield_now().await;
        assert!(container.0.store.lock().unwrap().views.is_empty());
        assert_eq!(
            request(&router, "GET", &paths[1], None, vec![])
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        assert!(container.0.store.lock().unwrap().views.is_empty());
    }

    #[cfg(feature = "apps-host-assets")]
    #[tokio::test(start_paused = true)]
    async fn body_utf8_storage_count_and_rate_are_bounded() {
        let container = container();
        let router = container.router();
        assert_eq!(
            request(
                &router,
                "POST",
                "/views",
                Some(&container.0.origin),
                vec![0; MAX_HTML + 1]
            )
            .await
            .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            request(
                &router,
                "POST",
                "/views",
                Some(&container.0.origin),
                vec![255]
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
        for _ in 0..16 {
            assert_eq!(
                request(
                    &router,
                    "POST",
                    "/views",
                    Some(&container.0.origin),
                    vec![b'x'; MAX_HTML]
                )
                .await
                .status(),
                StatusCode::CREATED
            );
        }
        assert_eq!(
            request(
                &router,
                "POST",
                "/views",
                Some(&container.0.origin),
                b"x".to_vec()
            )
            .await
            .status(),
            StatusCode::INSUFFICIENT_STORAGE
        );
        container.0.store.lock().unwrap().views.clear();
        for _ in 17..MAX_UPLOADS {
            assert_eq!(
                request(
                    &router,
                    "POST",
                    "/views",
                    Some(&container.0.origin),
                    b"x".to_vec()
                )
                .await
                .status(),
                StatusCode::CREATED
            );
        }
        assert_eq!(
            request(
                &router,
                "POST",
                "/views",
                Some(&container.0.origin),
                b"x".to_vec()
            )
            .await
            .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        tokio::time::advance(RATE_WINDOW).await;
        container.0.store.lock().unwrap().views.clear();
        for i in 0..MAX_VIEWS {
            container.0.store.lock().unwrap().views.insert(
                format!("{i}"),
                View {
                    html: String::new(),
                    expires: Instant::now() + TTL,
                },
            );
        }
        assert_eq!(
            request(
                &router,
                "POST",
                "/views",
                Some(&container.0.origin),
                b"x".to_vec()
            )
            .await
            .status(),
            StatusCode::INSUFFICIENT_STORAGE
        );
    }

    #[cfg(feature = "apps-host-assets")]
    #[tokio::test]
    async fn dedicated_listener_defaults_loopback_rejects_implicit_external_origin_and_joins() {
        let external = AppsContainerConfig {
            bind: "0.0.0.0:0".parse().unwrap(),
            allowed_host_origins: vec!["tauri://localhost".into()],
            ..Default::default()
        };
        assert!(spawn_app_container(external).await.is_err());
        let mismatch = AppsContainerConfig {
            public_origin: Some("http://127.0.0.1:1".into()),
            allowed_host_origins: vec!["tauri://localhost".into()],
            ..Default::default()
        };
        assert!(spawn_app_container(mismatch).await.is_err());
        let container = spawn_app_container(AppsContainerConfig {
            allowed_host_origins: vec!["tauri://localhost".into()],
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(container.public_origin.starts_with("http://127.0.0.1:"));
        let client = reqwest::Client::new();
        let response = client
            .get(format!("{}/container.html", container.public_origin))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["content-security-policy"],
            format!("{CONTAINER_CSP}; frame-ancestors tauri://localhost")
        );
        assert_eq!(
            client
                .get(format!("{}/mcp", container.public_origin))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        container.shutdown().await.unwrap();
    }
}
