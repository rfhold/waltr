use std::{
    convert::Infallible,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use axum::{
    body::Body,
    extract::State,
    http::{Request, Response, StatusCode, header},
    middleware::Next,
    response::IntoResponse,
};
use http_body::{Body as HttpBody, Frame, SizeHint};
use waltr::grpc::waltr::auth::v1::{
    AuthenticationDomain, AuthenticationFailure, AuthenticationFailureReason,
};
use prost::Message;
use tower::{Layer, Service};

use super::{
    AccessAuthenticator, RequestAuthFailure,
    lease::{LeaseEnd, LeaseMonitor},
};

const ACCESS_AUTH_REQUIRED: &str = "Waltr access authentication required";

#[derive(Clone, Copy)]
enum GrpcTransport {
    Native,
    Web,
}

/// Classify native gRPC and gRPC-web before tonic-web normalizes the request.
#[derive(Clone, Copy, Debug, Default)]
pub struct GrpcTransportLayer;

impl<S> Layer<S> for GrpcTransportLayer {
    type Service = GrpcTransportService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        GrpcTransportService { inner }
    }
}

/// Tower service produced by [`GrpcTransportLayer`].
#[derive(Clone, Debug)]
pub struct GrpcTransportService<S> {
    inner: S,
}

impl<S, B> Service<Request<B>> for GrpcTransportService<S>
where
    S: Service<Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, mut request: Request<B>) -> Self::Future {
        let transport = if request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                matches!(
                    value,
                    "application/grpc-web"
                        | "application/grpc-web+proto"
                        | "application/grpc-web-text"
                        | "application/grpc-web-text+proto"
                )
            }) {
            GrpcTransport::Web
        } else {
            GrpcTransport::Native
        };
        request.extensions_mut().insert(transport);
        self.inner.call(request)
    }
}

/// Authenticate every tonic request after tonic-web request decoding.
#[derive(Clone, Debug)]
pub struct GrpcAccessAuthLayer {
    authenticator: AccessAuthenticator,
}

impl GrpcAccessAuthLayer {
    /// Create a gRPC access-authentication layer.
    #[must_use]
    pub const fn new(authenticator: AccessAuthenticator) -> Self {
        Self { authenticator }
    }
}

impl<S> Layer<S> for GrpcAccessAuthLayer {
    type Service = GrpcAccessAuthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        GrpcAccessAuthService {
            inner,
            authenticator: self.authenticator.clone(),
        }
    }
}

/// Tower service produced by [`GrpcAccessAuthLayer`].
#[derive(Clone, Debug)]
pub struct GrpcAccessAuthService<S> {
    inner: S,
    authenticator: AccessAuthenticator,
}

impl<S> Service<Request<tonic::body::Body>> for GrpcAccessAuthService<S>
where
    S: Service<
            Request<tonic::body::Body>,
            Response = Response<tonic::body::Body>,
            Error = Infallible,
        > + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<tonic::body::Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, mut request: Request<tonic::body::Body>) -> Self::Future {
        let authenticator = self.authenticator.clone();
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(async move {
            let result = match request
                .extensions()
                .get::<GrpcTransport>()
                .copied()
                .unwrap_or(GrpcTransport::Native)
            {
                GrpcTransport::Native => {
                    let required_scopes = authenticator.api_required_scopes().to_vec();
                    authenticator
                        .authenticate_bearer_headers(request.headers(), &required_scopes)
                        .await
                }
                GrpcTransport::Web => {
                    authenticator
                        .authenticate_browser_session_headers(request.headers())
                        .await
                }
            };
            match result {
                Ok(principal) => {
                    let monitor = principal.lease_monitor();
                    request.extensions_mut().insert(principal);
                    let response = inner.call(request).await?;
                    Ok(guard_response(response, monitor))
                }
                Err(RequestAuthFailure::Internal) => Ok(access_auth_status(
                    tonic::Code::Unavailable,
                    AuthenticationFailureReason::NetworkFailed,
                    "Waltr access authentication unavailable",
                )
                .into_http()),
                Err(RequestAuthFailure::Unauthenticated) => Ok(access_auth_status(
                    tonic::Code::Unauthenticated,
                    AuthenticationFailureReason::CredentialRequired,
                    ACCESS_AUTH_REQUIRED,
                )
                .into_http()),
                Err(RequestAuthFailure::InsufficientScope) => Ok(access_auth_status(
                    tonic::Code::Unauthenticated,
                    AuthenticationFailureReason::CredentialRejected,
                    ACCESS_AUTH_REQUIRED,
                )
                .into_http()),
            }
        })
    }
}

fn access_auth_status(
    code: tonic::Code,
    reason: AuthenticationFailureReason,
    message: &'static str,
) -> tonic::Status {
    let failure = AuthenticationFailure {
        version: 1,
        domain: AuthenticationDomain::WaltrAccess as i32,
        reason: reason as i32,
    };
    tonic::Status::with_details(code, message, failure.encode_to_vec().into())
}

fn guard_response(
    response: Response<tonic::body::Body>,
    monitor: Option<LeaseMonitor>,
) -> Response<tonic::body::Body> {
    let Some(monitor) = monitor else {
        return response;
    };
    if response.body().is_end_stream() {
        return response;
    }
    let (parts, body) = response.into_parts();
    Response::from_parts(
        parts,
        tonic::body::Body::new(AuthenticatedGrpcBody::new(body, monitor)),
    )
}

#[derive(Debug)]
struct AuthenticatedGrpcBody {
    source: Option<Pin<Box<tonic::body::Body>>>,
    monitor: LeaseMonitor,
    terminal_sent: bool,
}

impl AuthenticatedGrpcBody {
    fn new(source: tonic::body::Body, monitor: LeaseMonitor) -> Self {
        Self {
            source: Some(Box::pin(source)),
            monitor,
            terminal_sent: false,
        }
    }

    fn terminal_trailers(end: LeaseEnd) -> axum::http::HeaderMap {
        let status = match end {
            LeaseEnd::Unauthenticated => access_auth_status(
                tonic::Code::Unauthenticated,
                AuthenticationFailureReason::CredentialRejected,
                ACCESS_AUTH_REQUIRED,
            ),
            LeaseEnd::Unavailable => access_auth_status(
                tonic::Code::Unavailable,
                AuthenticationFailureReason::NetworkFailed,
                "Waltr access authentication unavailable",
            ),
        };
        let response: Response<()> = status.into_http();
        let mut headers = response.into_parts().0.headers;
        headers.remove(header::CONTENT_TYPE);
        headers
    }
}

impl HttpBody for AuthenticatedGrpcBody {
    type Data = <tonic::body::Body as HttpBody>::Data;
    type Error = <tonic::body::Body as HttpBody>::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if self.terminal_sent {
            return Poll::Ready(None);
        }
        if let Poll::Ready(end) = self.monitor.poll(context) {
            self.source = None;
            self.terminal_sent = true;
            return Poll::Ready(Some(Ok(Frame::trailers(Self::terminal_trailers(end)))));
        }
        let source_result = {
            let Some(source) = self.source.as_mut() else {
                return Poll::Ready(None);
            };
            source.as_mut().poll_frame(context)
        };
        if let Poll::Ready(end) = self.monitor.poll(context) {
            self.source = None;
            self.terminal_sent = true;
            return Poll::Ready(Some(Ok(Frame::trailers(Self::terminal_trailers(end)))));
        }
        match source_result {
            Poll::Ready(None) => {
                self.source = None;
                Poll::Ready(None)
            }
            result => result,
        }
    }

    fn size_hint(&self) -> SizeHint {
        self.source
            .as_ref()
            .map_or_else(|| SizeHint::with_exact(0), http_body::Body::size_hint)
    }

    fn is_end_stream(&self) -> bool {
        self.terminal_sent
            || self
                .source
                .as_ref()
                .is_none_or(http_body::Body::is_end_stream)
    }
}

/// Require a browser session for embedded web routes.
pub async fn require_browser_session(
    State(authenticator): State<AccessAuthenticator>,
    mut request: Request<Body>,
    next: Next,
) -> axum::response::Response {
    match authenticator
        .authenticate_browser_session_headers(request.headers())
        .await
    {
        Ok(principal) => {
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        Err(RequestAuthFailure::Internal) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        Err(RequestAuthFailure::Unauthenticated | RequestAuthFailure::InsufficientScope) => {
            let return_to = request
                .uri()
                .path_and_query()
                .map_or("/", |value| value.as_str());
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("return_to", return_to)
                .finish();
            (
                StatusCode::SEE_OTHER,
                [(header::LOCATION, format!("/oidc/login?{query}"))],
            )
                .into_response()
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::duration_suboptimal_units,
    reason = "middleware tests use short expiry fixtures"
)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, SystemTime},
    };

    use futures_util::future::poll_fn;

    use super::*;

    use crate::access_auth::{
        BrowserSessionTokenDigest,
        lease::{
            BrowserSessionRevalidator, RevalidationFuture, RevocationRegistry, RevocationSignal,
        },
    };

    #[derive(Debug)]
    struct ReadyBody {
        polls: Arc<AtomicUsize>,
    }

    impl HttpBody for ReadyBody {
        type Data = axum::body::Bytes;
        type Error = tonic::Status;

        fn poll_frame(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(Some(Ok(Frame::data(axum::body::Bytes::from_static(
                b"protected",
            )))))
        }

        fn is_end_stream(&self) -> bool {
            false
        }
    }

    #[derive(Debug)]
    struct RevokingReadyBody {
        polls: Arc<AtomicUsize>,
        registry: Arc<RevocationRegistry>,
        token_digest: BrowserSessionTokenDigest,
    }

    impl HttpBody for RevokingReadyBody {
        type Data = axum::body::Bytes;
        type Error = tonic::Status;

        fn poll_frame(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            self.registry.cancel(&self.token_digest);
            Poll::Ready(Some(Ok(Frame::data(axum::body::Bytes::from_static(
                b"must be discarded",
            )))))
        }

        fn is_end_stream(&self) -> bool {
            false
        }
    }

    #[derive(Debug)]
    struct PanicRevalidator;

    impl BrowserSessionRevalidator for PanicRevalidator {
        fn is_active(&self, _token_digest: BrowserSessionTokenDigest) -> RevalidationFuture {
            panic!("expired browser lease must not revalidate")
        }
    }

    #[tokio::test]
    async fn already_expired_bearer_emits_status_before_ready_source() {
        assert_already_expired(LeaseMonitor::bearer(SystemTime::UNIX_EPOCH)).await;
    }

    #[tokio::test]
    async fn already_expired_browser_session_emits_status_before_ready_source() {
        assert_already_expired(LeaseMonitor::browser(
            SystemTime::UNIX_EPOCH,
            BrowserSessionTokenDigest::from_token("expired"),
            Arc::new(PanicRevalidator),
            Arc::new(RevocationSignal::new()),
        ))
        .await;
    }

    #[tokio::test]
    async fn revocation_during_source_poll_discards_ready_data_and_terminates_once() {
        let registry = Arc::new(RevocationRegistry::default());
        let token_digest = BrowserSessionTokenDigest::from_token("source-poll-race");
        let revocation_signal = registry.signal(&token_digest);
        let monitor = LeaseMonitor::browser(
            SystemTime::now() + Duration::from_secs(60),
            token_digest.clone(),
            Arc::new(PanicRevalidator),
            revocation_signal,
        );
        let polls = Arc::new(AtomicUsize::new(0));
        let source = tonic::body::Body::new(RevokingReadyBody {
            polls: Arc::clone(&polls),
            registry,
            token_digest,
        });
        let mut body = Box::pin(AuthenticatedGrpcBody::new(source, monitor));

        assert_unauthenticated_terminal(&mut body).await;
        assert_eq!(polls.load(Ordering::SeqCst), 1);
    }

    async fn assert_already_expired(monitor: LeaseMonitor) {
        let polls = Arc::new(AtomicUsize::new(0));
        let source = tonic::body::Body::new(ReadyBody {
            polls: Arc::clone(&polls),
        });
        let mut body = Box::pin(AuthenticatedGrpcBody::new(source, monitor));

        assert_unauthenticated_terminal(&mut body).await;
        assert_eq!(polls.load(Ordering::SeqCst), 0);
    }

    async fn assert_unauthenticated_terminal(body: &mut Pin<Box<AuthenticatedGrpcBody>>) {
        let frame = poll_fn(|context| body.as_mut().poll_frame(context))
            .await
            .expect("terminal frame")
            .expect("terminal frame should not be a body error");
        let trailers = frame
            .into_trailers()
            .expect("terminal frame should be trailers");
        assert_eq!(trailers["grpc-status"], "16");
        assert_eq!(
            trailers["grpc-message"],
            "Waltr%20access%20authentication%20required"
        );
        assert!(trailers.contains_key("grpc-status-details-bin"));
        assert!(!trailers.contains_key(header::CONTENT_TYPE));
        assert!(
            poll_fn(|context| body.as_mut().poll_frame(context))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn completed_body_passes_through_without_a_guard() {
        let response = Response::new(tonic::body::Body::empty());
        let monitor = LeaseMonitor::bearer(SystemTime::now() + Duration::from_secs(1));

        assert!(
            guard_response(response, Some(monitor))
                .body()
                .is_end_stream()
        );
    }
}
