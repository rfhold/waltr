use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use axum::http::{HeaderMap, Request, Response};
use futures_util::StreamExt;
use http_body::{Body as _, Frame};
use opentelemetry::{
    propagation::{Extractor, Injector, TextMapPropagator},
    trace::TraceContextExt as _,
};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use prost::Message;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tonic::{
    Status, Streaming,
    body::Body,
    codegen::BoxStream,
    metadata::AsciiMetadataValue,
    server::{Grpc, ServerStreamingService, StreamingService, UnaryService},
    transport::{Channel, ClientTlsConfig, Endpoint},
};
use tracing::Instrument as _;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;
use waltr::{
    destination,
    grpc::waltr::{
        auth::v1 as auth, catalog::v1 as catalog, resource::v1 as resource, routine::v2 as routine,
        session::v1 as session, status::v1 as status,
    },
};

use crate::{
    access_auth::{LeaseMonitor, guard_response, lease_end_response},
    agent_definition::AgentDefinitionState,
    auth::{AuthService, RemoteCredentialLease, now_ms, remote_route_id},
    resource_catalog::PublishedState,
};

#[derive(Clone, Debug)]
pub(crate) struct RemoteRoutes {
    catalog: AgentDefinitionState,
    auth: Arc<AuthService>,
}

impl RemoteRoutes {
    pub(crate) const fn new(catalog: AgentDefinitionState, auth: Arc<AuthService>) -> Self {
        Self { catalog, auth }
    }

    pub(crate) async fn forward(
        self,
        selector: String,
        request: Request<Body>,
        entry_lifetime: Option<Arc<CancellationToken>>,
        mut monitor: Option<LeaseMonitor>,
    ) -> Response<Body> {
        let Some((service, method, shape)) = descriptor(request.uri().path()) else {
            return Status::unimplemented("RPC does not support a Remote destination").into_http();
        };
        let mut telemetry = RelayTelemetry::new(service, method, shape, request.headers());
        let span = telemetry.span.clone();
        async {
            let result = tokio::select! {
                biased;
                () = async {
                    match &entry_lifetime {
                        Some(lifetime) => lifetime.cancelled().await,
                        None => std::future::pending().await,
                    }
                } => {
                    telemetry.stage("entry_guard");
                    Err(Status::cancelled("entry server stopped"))
                },
                end = async {
                    match monitor.as_mut() {
                        Some(monitor) => std::future::poll_fn(|cx| monitor.poll(cx)).await,
                        None => std::future::pending().await,
                    }
                } => {
                    telemetry.stage("entry_guard");
                    Ok(lease_end_response(end))
                },
                response = self.route(&selector, request, entry_lifetime.clone(), &mut telemetry) => response,
            };
            match result {
                Ok(response) => {
                    let response = guard_response(response, monitor, Some(telemetry.span.clone()));
                    match grpc_code(response.headers()) {
                        Some(code) => telemetry.finish(code),
                        None if response.body().is_end_stream() => {
                            telemetry.finish(tonic::Code::Unknown)
                        }
                        None => {}
                    }
                    let (parts, body) = response.into_parts();
                    Response::from_parts(
                        parts,
                        Body::new(ObservedBody {
                            body,
                            telemetry,
                            first: true,
                        }),
                    )
                }
                Err(status) => {
                    telemetry.finish(status.code());
                    status.into_http()
                }
            }
        }
        .instrument(span)
        .await
    }

    async fn route(
        &self,
        selector: &str,
        request: Request<Body>,
        entry_lifetime: Option<Arc<CancellationToken>>,
        telemetry: &mut RelayTelemetry,
    ) -> Result<Response<Body>, Status> {
        let (name, expected) = destination::pinned(selector).ok_or_else(|| {
            Status::failed_precondition("Resolve the Remote authorization before routing")
        })?;
        let publications = self.catalog.subscribe();
        let snapshot = self.catalog.snapshot();
        let remote = snapshot
            .remotes()
            .get(name)
            .ok_or_else(|| Status::not_found("Remote destination not found"))?;
        let endpoint = remote.endpoint().clone();
        let incarnation = snapshot.remote_incarnation(name);
        drop(snapshot);

        telemetry.stage("grant");
        let (lease, lifetime) = self
            .auth
            .remote_route_lease(name, &endpoint, expected)
            .await?;
        if remote_route_id(&lease.authority_id).as_deref() != Some(expected) {
            return Err(Status::failed_precondition(
                "Remote authorization changed; reopen the destination",
            ));
        }
        let mut guard = RouteGuard {
            catalog: self.catalog.clone(),
            name: name.to_owned(),
            incarnation,
            publications,
            lifetime,
            entry_lifetime,
            expiry: tokio::time::Instant::now()
                + Duration::from_millis(
                    u64::try_from(lease.expires_at_ms.saturating_sub(now_ms())).unwrap_or(0),
                ),
        };
        let mut endpoint_builder = Endpoint::from_shared(endpoint.to_string())
            .map_err(|_| Status::internal("Remote endpoint is invalid"))?
            .connect_timeout(Duration::from_secs(5));
        if endpoint.scheme() == "https" {
            endpoint_builder = endpoint_builder
                .tls_config(ClientTlsConfig::new().with_webpki_roots())
                .map_err(|_| Status::internal("Remote TLS could not be configured"))?;
        }
        let outbound = Outbound::new(
            ObservedChannel {
                channel: endpoint_builder.connect_lazy(),
                span: telemetry.span.clone(),
                started: telemetry.started,
            },
            lease,
            request.headers(),
            request.uri().path(),
        )?;
        telemetry.stage("dispatch");
        let dispatch_started = Instant::now();
        let response = tokio::select! {
            biased;
            status = guard.ended() => {
                telemetry.stage("route_guard");
                return Err(status);
            },
            response = dispatch(outbound, request) => response,
        };
        telemetry.span.record(
            "dispatch_ms",
            u64::try_from(dispatch_started.elapsed().as_millis()).unwrap_or(u64::MAX),
        );
        if response.body().is_end_stream() {
            return Ok(response);
        }
        let (parts, body) = response.into_parts();
        Ok(Response::from_parts(
            parts,
            Body::new(GuardedBody {
                source: Some(body),
                ended: Box::pin(async move { guard.ended().await }),
                span: telemetry.span.clone(),
            }),
        ))
    }
}

pub(crate) fn take_destination(headers: &mut HeaderMap) -> Result<Option<String>, Status> {
    let mut values = headers.get_all(destination::HEADER).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| Status::invalid_argument("invalid destination"))?;
    let name = match (value, values.next()) {
        (destination::LOCAL, None) => None,
        (selector, None)
            if destination::pinned(selector).is_some() || destination::is_remote_name(selector) =>
        {
            Some(selector.to_owned())
        }
        _ => return Err(Status::invalid_argument("invalid destination")),
    };
    headers.remove(destination::HEADER);
    Ok(name)
}

struct RouteGuard {
    catalog: AgentDefinitionState,
    name: String,
    incarnation: Option<uuid::Uuid>,
    publications: broadcast::Receiver<PublishedState>,
    lifetime: Arc<CancellationToken>,
    entry_lifetime: Option<Arc<CancellationToken>>,
    expiry: tokio::time::Instant,
}

impl RouteGuard {
    async fn ended(&mut self) -> Status {
        let changed = async {
            loop {
                let snapshot = self.catalog.snapshot();
                if !snapshot.remotes().contains_key(&self.name)
                    || snapshot.remote_incarnation(&self.name) != self.incarnation
                {
                    break;
                }
                if self.publications.recv().await.is_err() {
                    break;
                }
            }
        };
        tokio::select! {
            biased;
            () = async {
                match &self.entry_lifetime {
                    Some(lifetime) => lifetime.cancelled().await,
                    None => std::future::pending().await,
                }
            } => Status::cancelled("entry server stopped"),
            () = self.lifetime.cancelled() => Status::failed_precondition("Remote authorization ended"),
            () = tokio::time::sleep_until(self.expiry) => Status::failed_precondition("Remote authorization expired"),
            () = changed => Status::aborted("Remote destination changed"),
        }
    }
}

struct GuardedBody {
    source: Option<Body>,
    ended: Pin<Box<dyn Future<Output = Status> + Send>>,
    span: tracing::Span,
}

struct TraceHeader<'a>(&'a HeaderMap);

impl Extractor for TraceHeader<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        (key == "traceparent")
            .then(|| self.0.get(key)?.to_str().ok())
            .flatten()
    }

    fn keys(&self) -> Vec<&str> {
        vec!["traceparent"]
    }
}

#[derive(Default)]
struct OutboundTrace(Option<AsciiMetadataValue>);

impl Injector for OutboundTrace {
    fn set(&mut self, key: &str, value: String) {
        if key == "traceparent" {
            self.0 = value.parse().ok();
        }
    }
}

struct RelayTelemetry {
    span: tracing::Span,
    started: Instant,
    finished: bool,
}

impl RelayTelemetry {
    fn new(
        service: &'static str,
        method: &'static str,
        shape: &'static str,
        headers: &HeaderMap,
    ) -> Self {
        let span = tracing::info_span!(
            "waltr.remote.relay",
            rpc.service = service,
            rpc.method = method,
            rpc.shape = shape,
            owner.scope = "remote",
            stage = "resolve",
            dispatch_ms = tracing::field::Empty,
            headers_ms = tracing::field::Empty,
            first_data_ms = tracing::field::Empty,
            elapsed_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            rpc.grpc.status_code = tracing::field::Empty
        );
        if !tracing::Span::current()
            .context()
            .span()
            .span_context()
            .is_valid()
        {
            let parent = TraceContextPropagator::new().extract(&TraceHeader(headers));
            if parent.span().span_context().is_valid() {
                let _ = span.set_parent(parent);
            }
        }
        Self {
            span,
            started: Instant::now(),
            finished: false,
        }
    }

    fn stage(&self, stage: &'static str) {
        self.span.record("stage", stage);
    }

    fn elapsed(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn finish(&mut self, code: tonic::Code) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.span.record("elapsed_ms", self.elapsed());
        self.span.record("rpc.grpc.status_code", code as i64);
        let outcome = match code {
            tonic::Code::Ok => "ok",
            tonic::Code::Cancelled => "cancelled",
            _ => "failed",
        };
        self.span.record("outcome", outcome);
        if code != tonic::Code::Ok && code != tonic::Code::Cancelled {
            self.span
                .set_status(opentelemetry::trace::Status::error("Remote RPC failed"));
        }
    }
}

impl Drop for RelayTelemetry {
    fn drop(&mut self) {
        self.finish(tonic::Code::Cancelled);
    }
}

struct ObservedBody {
    body: Body,
    telemetry: RelayTelemetry,
    first: bool,
}

fn grpc_code(headers: &HeaderMap) -> Option<tonic::Code> {
    headers
        .get("grpc-status")
        .map(|value| tonic::Code::from_bytes(value.as_bytes()))
}

#[cfg(test)]
mod telemetry_tests {
    use super::*;
    use opentelemetry::{KeyValue, trace::TracerProvider as _};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
    use tracing_subscriber::prelude::*;

    #[tokio::test]
    async fn remote_relay_generates_child_context_and_records_stream_outcomes() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let _subscriber = tracing::subscriber::set_default(
            tracing_subscriber::registry()
                .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("remote-test"))),
        );
        let trace_id = "0123456789abcdef0123456789abcdef";
        let incoming = format!("00-{trace_id}-0123456789abcdef-01");
        let mut headers = HeaderMap::new();
        headers.insert("traceparent", incoming.parse().unwrap());
        for key in [
            "tracestate",
            "baggage",
            "authorization",
            "cookie",
            destination::HEADER,
        ] {
            headers.insert(key, "private-input".parse().unwrap());
        }
        let observed = RelayTelemetry::new(
            "waltr.StatusService",
            "WatchStatus",
            "server_stream",
            &headers,
        );
        let outgoing = observed.span.in_scope(|| {
            Outbound::new(
                ObservedChannel {
                    channel: Endpoint::from_static("http://127.0.0.1:1").connect_lazy(),
                    span: observed.span.clone(),
                    started: observed.started,
                },
                RemoteCredentialLease {
                    access_token: "test-bearer".into(),
                    expires_at_ms: 0,
                    authority_id: "private-grant".into(),
                },
                &headers,
                "/waltr.StatusService/WatchStatus",
            )
            .unwrap()
            .request(())
        });
        let metadata = outgoing.metadata();
        let child = metadata.get("traceparent").unwrap().to_str().unwrap();
        assert!(child.starts_with(&format!("00-{trace_id}-")));
        assert_ne!(child, incoming);
        assert_eq!(metadata.get("authorization").unwrap(), "Bearer test-bearer");
        for key in ["tracestate", "baggage", "cookie", destination::HEADER] {
            assert!(!metadata.contains_key(key), "{key}");
        }
        let serving_id = child.split('-').nth(2).unwrap().to_owned();
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", "0".parse().unwrap());
        struct Frames(std::vec::IntoIter<Frame<axum::body::Bytes>>);
        impl http_body::Body for Frames {
            type Data = axum::body::Bytes;
            type Error = Status;
            fn poll_frame(
                mut self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<Option<Result<Frame<Self::Data>, Status>>> {
                Poll::Ready(self.0.next().map(Ok))
            }
        }
        let mut response = ObservedBody {
            body: Body::new(Frames(
                vec![Frame::data("test".into()), Frame::trailers(trailers)].into_iter(),
            )),
            telemetry: observed,
            first: true,
        };
        while std::future::poll_fn(|cx| Pin::new(&mut response).poll_frame(cx))
            .await
            .is_some()
        {}
        drop(response);

        headers.insert("traceparent", "invalid".parse().unwrap());
        drop(RelayTelemetry::new(
            "waltr.StatusService",
            "WatchStatus",
            "server_stream",
            &headers,
        ));
        let revoked = RelayTelemetry::new(
            "waltr.StatusService",
            "WatchStatus",
            "server_stream",
            &headers,
        );
        let response = guard_response(
            Response::new(Body::new(Frames(
                vec![Frame::data("not sent".into())].into_iter(),
            ))),
            Some(LeaseMonitor::bearer(std::time::SystemTime::UNIX_EPOCH)),
            Some(revoked.span.clone()),
        );
        let mut response = ObservedBody {
            body: response.into_body(),
            telemetry: revoked,
            first: true,
        };
        let frame = std::future::poll_fn(|cx| Pin::new(&mut response).poll_frame(cx))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            frame.trailers_ref().and_then(grpc_code),
            Some(tonic::Code::Unauthenticated)
        );
        drop(response);
        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].name, "waltr.remote.relay");
        assert_eq!(spans[0].span_context.span_id().to_string(), serving_id);
        assert_eq!(spans[0].parent_span_id.to_string(), "0123456789abcdef");
        assert!(
            spans[0]
                .attributes
                .iter()
                .any(|field| field.key.as_str() == "first_data_ms")
        );
        assert!(
            spans[0]
                .attributes
                .contains(&KeyValue::new("outcome", "ok"))
        );
        assert!(
            spans[1]
                .attributes
                .contains(&KeyValue::new("outcome", "cancelled"))
        );
        assert_ne!(spans[1].span_context.trace_id().to_string(), trace_id);
        assert!(
            spans[2]
                .attributes
                .contains(&KeyValue::new("stage", "entry_guard"))
        );
        assert!(spans[2].attributes.contains(&KeyValue::new(
            "rpc.grpc.status_code",
            tonic::Code::Unauthenticated as i64
        )));
        assert!(!format!("{spans:?}").contains("private-"));
    }
}

impl http_body::Body for ObservedBody {
    type Data = axum::body::Bytes;
    type Error = Status;

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Status>>> {
        let frame = Pin::new(&mut self.body).poll_frame(cx);
        match &frame {
            Poll::Ready(Some(Ok(frame))) => {
                if self.first && frame.is_data() {
                    self.first = false;
                    self.telemetry
                        .span
                        .record("first_data_ms", self.telemetry.elapsed());
                    self.telemetry.stage("response_body");
                }
                if let Some(trailers) = frame.trailers_ref() {
                    let code = grpc_code(trailers).unwrap_or(tonic::Code::Unknown);
                    self.telemetry.finish(code);
                }
            }
            Poll::Ready(Some(Err(status))) => self.telemetry.finish(status.code()),
            Poll::Ready(None) => self.telemetry.finish(tonic::Code::Unknown),
            Poll::Pending => {}
        }
        frame
    }
}

impl http_body::Body for GuardedBody {
    type Data = axum::body::Bytes;
    type Error = Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Status>>> {
        if self.source.is_none() {
            return Poll::Ready(None);
        }
        if let Poll::Ready(status) = self.ended.as_mut().poll(cx) {
            self.source = None;
            self.span.record("stage", "route_guard");
            let mut headers = status.into_http::<()>().into_parts().0.headers;
            headers.remove(axum::http::header::CONTENT_TYPE);
            return Poll::Ready(Some(Ok(Frame::trailers(headers))));
        }
        Pin::new(self.source.as_mut().expect("checked source")).poll_frame(cx)
    }
}

#[derive(Clone)]
struct ObservedChannel {
    channel: Channel,
    span: tracing::Span,
    started: Instant,
}

impl tower::Service<Request<Body>> for ObservedChannel {
    type Response = Response<Body>;
    type Error = tonic::transport::Error;
    type Future = futures_util::future::BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.channel.poll_ready(cx)
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let response = self.channel.call(request);
        let span = self.span.clone();
        let started = self.started;
        Box::pin(async move {
            let response = response.await?;
            span.record(
                "headers_ms",
                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            );
            Ok(response)
        })
    }
}

#[derive(Clone)]
struct Outbound {
    channel: ObservedChannel,
    authorization: AsciiMetadataValue,
    image_route: Option<AsciiMetadataValue>,
    grpc_timeout: Option<AsciiMetadataValue>,
    traceparent: Option<AsciiMetadataValue>,
}

// Correlates upload operations after authentication; it never grants access.
pub(crate) const IMAGE_UPLOAD_ROUTE_HEADER: &str = "waltr-image-upload-route";

fn image_upload_route(authority_id: &str) -> Option<String> {
    use sha2::{Digest as _, Sha256};
    static SECRET: std::sync::LazyLock<[uuid::Uuid; 2]> =
        std::sync::LazyLock::new(|| [uuid::Uuid::new_v4(), uuid::Uuid::new_v4()]);
    let grant = uuid::Uuid::parse_str(authority_id).ok()?;
    let mut digest = Sha256::new();
    digest.update(grant.as_bytes());
    digest.update(SECRET[0].as_bytes());
    digest.update(SECRET[1].as_bytes());
    let mut tag = String::with_capacity(64);
    for byte in digest.finalize() {
        use std::fmt::Write as _;
        write!(tag, "{byte:02x}").expect("String writes cannot fail");
    }
    Some(tag)
}

impl Outbound {
    fn new(
        channel: ObservedChannel,
        lease: RemoteCredentialLease,
        headers: &HeaderMap,
        path: &str,
    ) -> Result<Self, Status> {
        let mut authorization: AsciiMetadataValue = format!("Bearer {}", lease.access_token)
            .parse()
            .map_err(|_| Status::internal("invalid Remote credential"))?;
        authorization.set_sensitive(true);
        let grpc_timeout = headers
            .get("grpc-timeout")
            .and_then(|header| AsciiMetadataValue::try_from(header.as_bytes()).ok());
        let mut trace = OutboundTrace::default();
        TraceContextPropagator::new()
            .inject_context(&tracing::Span::current().context(), &mut trace);
        let image_route = match descriptor(path) {
            Some((
                "waltr.session.v1.SessionService",
                "BeginMessageImageUpload"
                | "WriteMessageImageChunk"
                | "SealMessageImageUpload"
                | "AbortMessageImageUpload"
                | "SubmitUserMessage",
                _,
            )) => {
                let mut route: AsciiMetadataValue = image_upload_route(&lease.authority_id)
                    .and_then(|id| id.parse().ok())
                    .ok_or_else(|| Status::failed_precondition("Remote authorization changed"))?;
                route.set_sensitive(true);
                Some(route)
            }
            _ => None,
        };
        Ok(Self {
            channel,
            authorization,
            image_route,
            grpc_timeout,
            traceparent: trace.0,
        })
    }

    fn request<T>(&self, body: T) -> tonic::Request<T> {
        let mut request = tonic::Request::new(body);
        request
            .metadata_mut()
            .insert("authorization", self.authorization.clone());
        if let Some(route) = &self.image_route {
            request
                .metadata_mut()
                .insert(IMAGE_UPLOAD_ROUTE_HEADER, route.clone());
        }
        if let Some(timeout) = &self.grpc_timeout {
            request
                .metadata_mut()
                .insert("grpc-timeout", timeout.clone());
        }
        if let Some(traceparent) = &self.traceparent {
            request
                .metadata_mut()
                .insert("traceparent", traceparent.clone());
        }
        request
    }
}

#[cfg(test)]
#[tokio::test]
async fn image_upload_relay_replaces_untrusted_correlation_with_checked_grant() {
    let grant = uuid::Uuid::new_v4();
    let mut headers = HeaderMap::new();
    headers.insert(IMAGE_UPLOAD_ROUTE_HEADER, "untrusted".parse().unwrap());
    let span = tracing::Span::none();
    let outbound = Outbound::new(
        ObservedChannel {
            channel: Endpoint::from_static("http://127.0.0.1:1").connect_lazy(),
            span,
            started: Instant::now(),
        },
        RemoteCredentialLease {
            access_token: "unused-test-token".into(),
            expires_at_ms: 0,
            authority_id: grant.to_string(),
        },
        &headers,
        "/waltr.session.v1.SessionService/BeginMessageImageUpload",
    )
    .unwrap();
    let request = outbound.request(());
    let route = request.metadata().get(IMAGE_UPLOAD_ROUTE_HEADER).unwrap();
    assert_eq!(
        route.to_str().unwrap(),
        image_upload_route(&grant.to_string()).unwrap()
    );
    assert_ne!(route.to_str().unwrap(), grant.simple().to_string());
    assert_ne!(
        route.to_str().unwrap(),
        image_upload_route(&uuid::Uuid::new_v4().to_string()).unwrap()
    );
    assert!(route.is_sensitive());
}

fn map_status(status: Status) -> Status {
    // Neither a downstream access error nor its challenge is evidence about entry login.
    if auth::AuthenticationFailure::decode(status.details())
        .is_ok_and(|failure| failure.domain == auth::AuthenticationDomain::WaltrAccess as i32)
    {
        return Status::failed_precondition("Remote access rejected");
    }
    if status.code() == tonic::Code::Unauthenticated && status.details().is_empty() {
        return Status::failed_precondition("Remote access rejected");
    }
    Status::with_details(
        status.code(),
        status.message(),
        status.details().to_vec().into(),
    )
}

struct Call<F>(F);

impl<I, O, F, Fut> UnaryService<I> for Call<F>
where
    F: FnMut(tonic::Request<I>) -> Fut,
    Fut: Future<Output = Result<tonic::Response<O>, Status>>,
{
    type Response = O;
    type Future = Fut;
    fn call(&mut self, request: tonic::Request<I>) -> Self::Future {
        (self.0)(request)
    }
}

impl<I, O, F, Fut> ServerStreamingService<I> for Call<F>
where
    F: FnMut(tonic::Request<I>) -> Fut,
    Fut: Future<Output = Result<tonic::Response<OutputStream<O>>, Status>>,
{
    type Response = O;
    type ResponseStream = OutputStream<O>;
    type Future = Fut;
    fn call(&mut self, request: tonic::Request<I>) -> Self::Future {
        (self.0)(request)
    }
}

impl<I, O, F, Fut> StreamingService<I> for Call<F>
where
    F: FnMut(tonic::Request<Streaming<I>>) -> Fut,
    Fut: Future<Output = Result<tonic::Response<OutputStream<O>>, Status>>,
{
    type Response = O;
    type ResponseStream = OutputStream<O>;
    type Future = Fut;
    fn call(&mut self, request: tonic::Request<Streaming<I>>) -> Self::Future {
        (self.0)(request)
    }
}

struct OutputStream<T>(BoxStream<T>);

impl<T> futures_util::Stream for OutputStream<T> {
    type Item = Result<T, Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.as_mut().poll_next(cx)
    }
}

fn stream_response<T: prost::Message + Default + Send + 'static>(
    response: tonic::Response<Streaming<T>>,
) -> tonic::Response<OutputStream<T>> {
    tonic::Response::new(OutputStream(Box::pin(
        response.into_inner().map(|item| item.map_err(map_status)),
    )))
}

type StreamCall<T> =
    Pin<Box<dyn Future<Output = Result<tonic::Response<OutputStream<T>>, Status>> + Send>>;

fn codec<I: Message + Default + Send + 'static, O: Message + Send + 'static>(
    service: &str,
) -> Grpc<tonic_prost::ProstCodec<O, I>> {
    let grpc = Grpc::new(tonic_prost::ProstCodec::default());
    if service == "waltr.routine.v2.RoutineService" {
        grpc.max_decoding_message_size(131_072)
            .max_encoding_message_size(16_777_216)
    } else {
        grpc
    }
}

fn entry_control_plane(path: &str) -> Result<(), Status> {
    if path.split('/').next() == Some("remotes") {
        return Err(Status::failed_precondition(
            "Remote administration belongs to the entry server",
        ));
    }
    Ok(())
}

fn auth_control_plane(target: &Option<auth::AuthTarget>) -> Result<(), Status> {
    if matches!(
        target.as_ref().and_then(|target| target.target.as_ref()),
        Some(auth::auth_target::Target::RemoteKey(_))
    ) {
        return entry_control_plane("remotes");
    }
    Ok(())
}

fn attempt_control_plane(id: &str) -> Result<(), Status> {
    if id.starts_with("remote-") {
        return entry_control_plane("remotes");
    }
    Ok(())
}

macro_rules! check_unary {
    (replace_resource, $request:ident) => {
        entry_control_plane(&$request.path)
    };
    (reset_resource, $request:ident) => {
        entry_control_plane(&$request.path)
    };
    (get_auth_status, $request:ident) => {
        auth_control_plane(&$request.target)
    };
    (begin_auth, $request:ident) => {
        auth_control_plane(&$request.target)
    };
    (refresh_auth, $request:ident) => {
        auth_control_plane(&$request.target)
    };
    (logout, $request:ident) => {
        auth_control_plane(&$request.target)
    };
    (complete_auth, $request:ident) => {
        attempt_control_plane(&$request.attempt_id)
    };
    (get_auth_attempt_status, $request:ident) => {
        attempt_control_plane(&$request.attempt_id)
    };
    (cancel_auth, $request:ident) => {
        attempt_control_plane(&$request.attempt_id)
    };
    ($method:ident, $request:ident) => {
        Ok::<(), Status>(())
    };
}

macro_rules! forward_call {
    (unary, $out:ident, $req:ident, $service:literal, $client:path, $input:path, $method:ident) => {
        codec($service).unary(Call(move |request: tonic::Request<$input>| {
            let outbound = $out.clone();
            async move {
                let request = request.into_inner();
                check_unary!($method, request)?;
                let mut client = <$client>::new(outbound.channel.clone());
                let response = client.$method(outbound.request(request)).await.map_err(map_status)?;
                Ok(tonic::Response::new(response.into_inner()))
            }
        }), $req).await
    };
    (stream, $out:ident, $req:ident, $service:literal, $client:path, $input:path, $method:ident) => {
        codec($service).server_streaming(Call(move |request: tonic::Request<$input>| -> StreamCall<_> {
            let outbound = $out.clone();
            Box::pin(async move {
                let mut client = <$client>::new(outbound.channel.clone()).max_decoding_message_size(16_777_216);
                let response = client.$method(outbound.request(request.into_inner())).await.map_err(map_status)?;
                Ok(stream_response(response))
            })
        }), $req).await
    };
    (duplex, $out:ident, $req:ident, $service:literal, $client:path, $input:path, $method:ident) => {
        codec($service).streaming(Call(move |request: tonic::Request<Streaming<$input>>| -> StreamCall<_> {
            let outbound = $out.clone();
            Box::pin(async move {
                let mut client = <$client>::new(outbound.channel.clone()).max_decoding_message_size(16_777_216);
                let failed = CancellationToken::new();
                let input = request.into_inner().scan(failed.clone(), |failed, item| {
                    let value = item.map_err(|_| failed.cancel()).ok();
                    std::future::ready(value)
                });
                let response = tokio::select! {
                    biased;
                    () = failed.cancelled() => return Err(Status::invalid_argument("invalid Remote stream input")),
                    response = client.$method(outbound.request(input)) => response.map_err(map_status)?,
                };
                let stream = futures_util::stream::unfold(Some((response.into_inner(), failed)), |state| async {
                    let (mut output, failed) = state?;
                    match tokio::select! {
                            biased;
                            () = failed.cancelled() => Err(Status::invalid_argument("invalid Remote stream input")),
                            item = output.message() => item.map_err(map_status),
                    } {
                        Ok(Some(item)) => Some((Ok(item), Some((output, failed)))),
                        Ok(None) => None,
                        Err(error) => Some((Err(error), None)),
                    }
                });
                Ok(tonic::Response::new(OutputStream(Box::pin(stream))))
            })
        }), $req).await
    };
}

macro_rules! routes {
    ($($service:literal => $client:path {
        $($shape:ident $rpc:literal => $method:ident($input:path);)*
    })*) => {
        fn descriptor(path: &str) -> Option<(&'static str, &'static str, &'static str)> {
            match path {
                $($(concat!("/", $service, "/", $rpc) => Some(($service, $rpc, stringify!($shape))),)*)*
                _ => None,
            }
        }

        async fn dispatch(outbound: Outbound, request: Request<Body>) -> Response<Body> {
            match request.uri().path() {
                $($(concat!("/", $service, "/", $rpc) =>
                    forward_call!($shape, outbound, request, $service, $client, $input, $method),)*)*
                _ => Status::unimplemented("RPC does not support a Remote destination").into_http(),
            }
        }
    };
}

routes! {
    "waltr.session.v1.SessionService" => session::session_service_client::SessionServiceClient<ObservedChannel> {
        unary "OpenSessionMcpApp" => open_session_mcp_app(session::OpenSessionMcpAppRequest);
        unary "CallSessionMcpAppTool" => call_session_mcp_app_tool(session::CallSessionMcpAppToolRequest);
        unary "CloseSessionMcpApp" => close_session_mcp_app(session::CloseSessionMcpAppRequest);
        unary "ListAgents" => list_agents(session::ListAgentsRequest);
        unary "CreateSession" => create_session(session::CreateSessionRequest);
        unary "ListSessions" => list_sessions(session::ListSessionsRequest);
        unary "DeleteSession" => delete_session(session::DeleteSessionRequest);
        unary "WipeSessions" => wipe_sessions(session::WipeSessionsRequest);
        unary "UpdateSessionInferenceSettings" => update_session_inference_settings(session::UpdateSessionInferenceSettingsRequest);
        unary "UpdateSessionAgent" => update_session_agent(session::UpdateSessionAgentRequest);
        stream "WatchSessions" => watch_sessions(session::WatchSessionsRequest);
        unary "GetTimeline" => get_timeline(session::GetTimelineRequest);
        stream "WatchTimeline" => watch_timeline(session::WatchTimelineRequest);
        unary "SubmitUserMessage" => submit_user_message(session::SubmitUserMessageRequest);
        unary "BeginMessageImageUpload" => begin_message_image_upload(session::BeginMessageImageUploadRequest);
        unary "WriteMessageImageChunk" => write_message_image_chunk(session::WriteMessageImageChunkRequest);
        unary "SealMessageImageUpload" => seal_message_image_upload(session::SealMessageImageUploadRequest);
        unary "AbortMessageImageUpload" => abort_message_image_upload(session::AbortMessageImageUploadRequest);
        stream "ReadMessageImage" => read_message_image(session::ReadMessageImageRequest);
        unary "InterruptSession" => interrupt_session(session::InterruptSessionRequest);
        unary "RollbackSession" => rollback_session(session::RollbackSessionRequest);
        unary "ContinueSession" => continue_session(session::ContinueSessionRequest);
        unary "SearchSessionTemplates" => search_session_templates(session::SearchSessionTemplatesRequest);
    }
    "waltr.session.v1.InteractionService" => session::interaction_service_client::InteractionServiceClient<ObservedChannel> {
        unary "GetInteractions" => get_interactions(session::GetInteractionsRequest);
        stream "WatchInteractions" => watch_interactions(session::WatchInteractionsRequest);
        unary "RespondInteraction" => respond_interaction(session::RespondInteractionRequest);
    }
    "waltr.routine.v2.RoutineService" => routine::routine_service_client::RoutineServiceClient<ObservedChannel> {
        stream "WatchDefinitions" => watch_definitions(routine::WatchDefinitionsRequest);
        unary "StartInvocation" => start_invocation(routine::StartInvocationRequest);
        unary "ListInvocations" => list_invocations(routine::ListInvocationsRequest);
        unary "GetInvocation" => get_invocation(routine::GetInvocationRequest);
        stream "WatchInvocations" => watch_invocations(routine::WatchInvocationsRequest);
        unary "ListOperations" => list_operations(routine::ListOperationsRequest);
        unary "CancelInvocation" => cancel_invocation(routine::CancelInvocationRequest);
        unary "ReadTextResult" => read_text_result(routine::ReadTextResultRequest);
        duplex "Interact" => interact(routine::InteractRequest);
        unary "OpenAttachment" => open_attachment(routine::OpenAttachmentRequest);
        unary "SendInputBatch" => send_input_batch(routine::SendInputBatchRequest);
        stream "WatchLiveOutput" => watch_live_output(routine::WatchLiveOutputRequest);
        unary "RenewAttachment" => renew_attachment(routine::RenewAttachmentRequest);
        unary "CloseAttachment" => close_attachment(routine::CloseAttachmentRequest);
        unary "BeginBlobUpload" => begin_blob_upload(routine::BeginBlobUploadRequest);
        unary "AppendBlobUpload" => append_blob_upload(routine::AppendBlobUploadRequest);
        unary "CommitBlobUpload" => commit_blob_upload(routine::CommitBlobUploadRequest);
        unary "CancelBlobUpload" => cancel_blob_upload(routine::CancelBlobUploadRequest);
        unary "OpenBlobRead" => open_blob_read(routine::OpenBlobReadRequest);
        stream "ReadBlob" => read_blob(routine::ReadBlobRequest);
        unary "CloseBlobRead" => close_blob_read(routine::CloseBlobReadRequest);
    }
    "waltr.resource.v1.ResourceService" => resource::resource_service_client::ResourceServiceClient<ObservedChannel> {
        unary "GetResourceCatalog" => get_resource_catalog(resource::GetResourceCatalogRequest);
        stream "WatchResourceCatalog" => watch_resource_catalog(resource::WatchResourceCatalogRequest);
        unary "InspectResource" => inspect_resource(resource::InspectResourceRequest);
        unary "ReplaceResource" => replace_resource(resource::ReplaceResourceRequest);
        unary "ResetResource" => reset_resource(resource::ResetResourceRequest);
    }
    "waltr.catalog.v1.CatalogService" => catalog::catalog_service_client::CatalogServiceClient<ObservedChannel> {
        unary "SearchCatalogModels" => search_catalog_models(catalog::SearchCatalogModelsRequest);
        unary "SuggestCatalogMatches" => suggest_catalog_matches(catalog::SuggestCatalogMatchesRequest);
        unary "GetModelCatalogDrift" => get_model_catalog_drift(catalog::GetModelCatalogDriftRequest);
    }
    "waltr.auth.v1.AuthService" => auth::auth_service_client::AuthServiceClient<ObservedChannel> {
        unary "ListAuthStatuses" => list_auth_statuses(auth::ListAuthStatusesRequest);
        unary "GetAuthStatus" => get_auth_status(auth::GetAuthStatusRequest);
        unary "BeginAuth" => begin_auth(auth::BeginAuthRequest);
        unary "CompleteAuth" => complete_auth(auth::CompleteAuthRequest);
        unary "GetAuthAttemptStatus" => get_auth_attempt_status(auth::GetAuthAttemptStatusRequest);
        unary "CancelAuth" => cancel_auth(auth::CancelAuthRequest);
        unary "RefreshAuth" => refresh_auth(auth::RefreshAuthRequest);
        unary "Logout" => logout(auth::LogoutRequest);
    }
    "waltr.status.v1.StatusService" => status::status_service_client::StatusServiceClient<ObservedChannel> {
        stream "WatchStatus" => watch_status(status::WatchStatusRequest);
        unary "GetFeatureStatus" => get_feature_status(status::GetFeatureStatusRequest);
    }
}
