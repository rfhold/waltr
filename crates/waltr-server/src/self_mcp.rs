//! Private, application-generation self-MCP transport. Not an OAuth resource or public listener.

use crate::{
    agent_host::runtime_factory::McpResolvedConnection,
    local_admission::{LocalBearer, ValidatedLocalOrigin, require_local_protocol_bearer},
};
use std::{fmt, sync::Arc};
use tokio_util::sync::CancellationToken;

/// Opaque host-composed binding; callers cannot manufacture local admission from metadata.
#[derive(Clone)]
pub struct SelfMcpBinding {
    logical: McpResolvedConnection,
    endpoint: String,
    bearer: LocalBearer,
    authority: String,
    lifetime: CancellationToken,
    cleanup_finished: CancellationToken,
}

impl fmt::Debug for SelfMcpBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SelfMcpBinding(<private>)")
    }
}

impl SelfMcpBinding {
    pub(crate) fn matches(&self, connection: &McpResolvedConnection) -> bool {
        &self.logical == connection
    }

    pub(crate) fn logical_url(&self) -> &str {
        &self.logical.url
    }
    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }
    pub(crate) fn active(&self) -> bool {
        !self.lifetime.is_cancelled()
    }
    pub(crate) fn authority(&self) -> &str {
        &self.authority
    }

    #[cfg(test)]
    pub(crate) async fn cleanup_finished(&self) {
        self.cleanup_finished.cancelled().await;
    }

    pub(crate) fn token(&self) -> Result<String, mcp::client::McpBearerProviderError> {
        if !self.active() {
            return Err(mcp::client::McpBearerProviderError::Authorization);
        }
        Ok(self.bearer.secret().to_owned())
    }
}

pub(crate) struct PrivateSelfMcp {
    binding: SelfMcpBinding,
    listener: Option<tokio::net::TcpListener>,
    task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
    runtime: Option<tokio::runtime::Handle>,
    drop_cleanup: Option<(
        Arc<dyn crate::session::SessionHost>,
        Arc<crate::routine::production::ProductionRuntime>,
    )>,
}

impl PrivateSelfMcp {
    pub(crate) async fn bind(logical: McpResolvedConnection) -> std::io::Result<Self> {
        use rand::TryRngCore as _;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let endpoint = format!("http://{}/mcp", listener.local_addr()?);
        let mut bytes = [0_u8; 32];
        rand::rngs::OsRng
            .try_fill_bytes(&mut bytes)
            .map_err(|_| std::io::Error::other("self-MCP entropy unavailable"))?;
        let secret = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Ok(Self {
            binding: SelfMcpBinding {
                logical,
                endpoint,
                bearer: LocalBearer::new(secret).expect("hex is a bearer token"),
                authority: uuid::Uuid::new_v4().to_string(),
                lifetime: CancellationToken::new(),
                cleanup_finished: CancellationToken::new(),
            },
            listener: Some(listener),
            task: None,
            runtime: None,
            drop_cleanup: None,
        })
    }

    pub(crate) fn binding(&self) -> SelfMcpBinding {
        self.binding.clone()
    }

    pub(crate) fn start(
        &mut self,
        server: Arc<crate::mcp_service::WaltrMcpServer>,
        host: Arc<dyn crate::session::SessionHost>,
        routines: Arc<crate::routine::production::ProductionRuntime>,
    ) {
        let router = mcp::server::streamable_http_router_with_options(
            server,
            mcp::server::StreamableHttpOptions::default()
                .without_root_protected_resource_metadata(),
        )
        .layer(axum::middleware::from_fn_with_state(
            (
                self.binding.bearer.clone(),
                ValidatedLocalOrigin(Arc::new(self.binding.lifetime.clone())),
            ),
            require_local_protocol_bearer,
        ));
        self.start_router(router);
        self.drop_cleanup = Some((host, routines));
    }

    fn start_router(&mut self, router: axum::Router) {
        let listener = self.listener.take().expect("private self-MCP starts once");
        let lifetime = self.binding.lifetime.clone();
        self.runtime = Some(tokio::runtime::Handle::current());
        self.task = Some(tokio::spawn(async move {
            let _revoke = lifetime.clone().drop_guard();
            crate::http_lifecycle::serve_owned_connections(
                listener,
                router,
                lifetime.cancelled_owned(),
            )
            .await
        }));
    }

    pub(crate) async fn shutdown(mut self) {
        self.binding.lifetime.cancel();
        self.listener.take();
        if let Some(task) = self.task.as_mut() {
            // Connection tasks, including HTTP/2 service futures, join inside the owned server.
            // Retain the handle and fallback cleanup in self if this await is canceled.
            if !matches!(task.await, Ok(Ok(()))) {
                tracing::warn!("private self-MCP listener failed to join cleanly");
            }
        }
        self.task.take();
        self.drop_cleanup.take();
        self.binding.cleanup_finished.cancel();
    }
}

impl Drop for PrivateSelfMcp {
    fn drop(&mut self) {
        // The owned server observes cancellation and joins its connections even on owner drop.
        self.binding.lifetime.cancel();
        if let Some(task) = self.task.take() {
            let cleanup = self.drop_cleanup.take();
            let finished = self.binding.cleanup_finished.clone();
            // Async drop cannot wait; retain the join and host cleanup in one completion task.
            self.runtime
                .take()
                .expect("started listener owns a runtime")
                .spawn(async move {
                    drop(task.await);
                    if let Some((host, routines)) = cleanup {
                        let _ = routines.shutdown().await;
                        host.shutdown().await;
                    }
                    finished.cancel();
                });
        } else {
            self.binding.cleanup_finished.cancel();
        }
    }
}

pub(crate) fn public_resource_from_env() -> Option<url::Url> {
    let public = std::env::var("WALTR_PUBLIC_URL").ok()?;
    let mut url = url::Url::parse(&public).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    url.set_path(&format!("{}/mcp", url.path().trim_end_matches('/')));
    Some(url)
}

pub(crate) fn private_client() -> Result<reqwest::Client, reqwest::Error> {
    crate::outbound_http::client_builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_host::runtime_factory::McpConnectionKind;
    use axum::{Router, http::StatusCode, routing::get};
    use std::{
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    };

    fn logical() -> McpResolvedConnection {
        McpResolvedConnection {
            key: "waltr".into(),
            kind: McpConnectionKind::StreamableHttp,
            command: String::new(),
            url: "https://logical.example/mcp".into(),
        }
    }
    fn admitted(binding: &SelfMcpBinding, router: Router) -> Router {
        router.layer(axum::middleware::from_fn_with_state(
            (
                binding.bearer.clone(),
                ValidatedLocalOrigin(Arc::new(binding.lifetime.clone())),
            ),
            require_local_protocol_bearer,
        ))
    }
    async fn get_status(binding: &SelfMcpBinding, token: Option<String>) -> StatusCode {
        let mut request = private_client().unwrap().get(binding.endpoint());
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        tokio::time::timeout(Duration::from_secs(2), request.send())
            .await
            .unwrap()
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn self_mcp_generations_isolate_and_retire() {
        let mut first = PrivateSelfMcp::bind(logical()).await.unwrap();
        let mut second = PrivateSelfMcp::bind(logical()).await.unwrap();
        let a = first.binding();
        let b = second.binding();
        let router = || {
            Router::new().route(
                "/mcp",
                get(|origin: axum::Extension<ValidatedLocalOrigin>| async move {
                    assert!(!origin.0.0.is_cancelled());
                    StatusCode::OK
                }),
            )
        };
        first.start_router(admitted(&a, router()));
        second.start_router(admitted(&b, router()));
        assert!(a.token().unwrap() != b.token().unwrap());
        assert_eq!(get_status(&a, None).await, StatusCode::UNAUTHORIZED);
        assert_eq!(
            get_status(&a, b.token().ok()).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            get_status(&b, a.token().ok()).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(get_status(&a, a.token().ok()).await, StatusCode::OK);
        first.shutdown().await;
        assert!(!a.active());
        assert!(a.token().is_err());
        assert_eq!(get_status(&b, b.token().ok()).await, StatusCode::OK);
        second.shutdown().await;
        let pending = PrivateSelfMcp::bind(logical()).await.unwrap();
        let address = pending.listener.as_ref().unwrap().local_addr().unwrap();
        let binding = pending.binding();
        drop(pending);
        assert!(!binding.active());
        assert!(
            tokio::net::TcpStream::connect(address).await.is_err(),
            "unstarted rollback closes listener"
        );
    }

    #[tokio::test]
    async fn self_mcp_shutdown_joins_pending_service_and_drop_cleanup() {
        struct Cleanup(Arc<AtomicBool>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        for explicit in [true, false] {
            let mut private = PrivateSelfMcp::bind(logical()).await.unwrap();
            let binding = private.binding();
            let entered = Arc::new(tokio::sync::Notify::new());
            let cleaned = Arc::new(AtomicBool::new(false));
            let notify = entered.clone();
            let cleanup = cleaned.clone();
            let router = Router::new().route(
                "/mcp",
                get(move || {
                    let notify = notify.clone();
                    let cleanup = cleanup.clone();
                    async move {
                        let _guard = Cleanup(cleanup);
                        notify.notify_one();
                        std::future::pending::<StatusCode>().await
                    }
                }),
            );
            private.start_router(admitted(&binding, router));
            let url = binding.endpoint().to_owned();
            let token = binding.token().unwrap();
            let request = tokio::spawn(async move {
                private_client()
                    .unwrap()
                    .get(url)
                    .bearer_auth(token)
                    .send()
                    .await
            });
            tokio::time::timeout(Duration::from_secs(2), entered.notified())
                .await
                .unwrap();
            if explicit {
                tokio::time::timeout(Duration::from_secs(8), private.shutdown())
                    .await
                    .unwrap();
                assert!(
                    cleaned.load(Ordering::SeqCst),
                    "service cleanup must join before shutdown returns"
                );
            } else {
                std::thread::spawn(move || drop(private)).join().unwrap();
                tokio::time::timeout(Duration::from_secs(8), async {
                    while !cleaned.load(Ordering::SeqCst) {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
            }
            assert!(!binding.active());
            assert!(
                tokio::time::timeout(Duration::from_secs(2), request)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err()
            );
            let address = url::Url::parse(binding.endpoint())
                .unwrap()
                .socket_addrs(|| None)
                .unwrap()[0];
            assert!(tokio::net::TcpStream::connect(address).await.is_err());
        }
    }
}
