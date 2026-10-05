use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Instant,
};

use http_body::{Body, Frame};
use opentelemetry::trace::TraceContextExt as _;
use pin_project_lite::pin_project;
use tower::{Layer, Service};
use tracing::{Span, field::Empty};
use tracing_opentelemetry::OpenTelemetrySpanExt;

const UNKNOWN_STATUS: u16 = tonic::Code::Unknown as u16;
const CANCELLED_STATUS: u16 = tonic::Code::Cancelled as u16;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct NativeTraceLayer;

impl<S> Layer<S> for NativeTraceLayer {
    type Service = NativeTraceService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        NativeTraceService { inner }
    }
}

#[derive(Clone, Debug)]
pub(super) struct NativeTraceService<S> {
    inner: S,
}

impl<S, RequestBody, ResponseBody> Service<http::Request<RequestBody>> for NativeTraceService<S>
where
    S: Service<http::Request<RequestBody>, Response = http::Response<ResponseBody>>,
    ResponseBody: Body,
    ResponseBody::Error: 'static,
{
    type Response = http::Response<TraceBody<ResponseBody>>;
    type Error = S::Error;
    type Future = TraceResponseFuture<S::Future, ResponseBody>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, mut request: http::Request<RequestBody>) -> Self::Future {
        let method = method_attributes(request.uri().path());
        let span = tracing::info_span!(
            "waltr.grpc.client",
            otel.name = method.operation,
            otel.kind = "client",
            otel.status_code = Empty,
            rpc.system = "grpc",
            rpc.service = method.service,
            rpc.method = method.method,
            rpc.grpc.status_code = Empty,
            waltr.operation = method.operation,
            waltr.grpc.duration_ms = Empty,
        );
        inject_w3c_context(&span, request.headers_mut());
        let started = Instant::now();
        let future = {
            let _entered = span.enter();
            self.inner.call(request)
        };
        TraceResponseFuture {
            inner: future,
            span: Some(span),
            started,
            response_body: std::marker::PhantomData,
        }
    }
}

pin_project! {
    pub(super) struct TraceResponseFuture<F, B> {
        #[pin]
        inner: F,
        span: Option<Span>,
        started: Instant,
        response_body: std::marker::PhantomData<fn() -> B>,
    }

    impl<F, B> PinnedDrop for TraceResponseFuture<F, B> {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            if let Some(span) = this.span.take() {
                finish_span(&span, CANCELLED_STATUS, *this.started);
            }
        }
    }
}

impl<F, ResponseBody, Error> Future for TraceResponseFuture<F, ResponseBody>
where
    F: Future<Output = Result<http::Response<ResponseBody>, Error>>,
    ResponseBody: Body,
    ResponseBody::Error: 'static,
{
    type Output = Result<http::Response<TraceBody<ResponseBody>>, Error>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        let active_span = this
            .span
            .as_ref()
            .expect("trace future polled after completion")
            .clone();
        let _entered = active_span.enter();
        match this.inner.as_mut().poll(context) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => {
                finish_span(&active_span, UNKNOWN_STATUS, *this.started);
                this.span.take();
                Poll::Ready(Err(error))
            }
            Poll::Ready(Ok(response)) => {
                let header_status = grpc_status(response.headers());
                let transport_status =
                    header_status.or_else(|| grpc_status_from_http(response.status()));
                let (parts, body) = response.into_parts();
                let span = this.span.take().expect("trace span is present");
                let mut traced_body = TraceBody {
                    inner: body,
                    span: Some(span),
                    started: *this.started,
                    terminal_status_observed: header_status.is_some(),
                };
                if let Some(status) = transport_status {
                    traced_body.finish(status);
                }
                Poll::Ready(Ok(http::Response::from_parts(parts, traced_body)))
            }
        }
    }
}

pin_project! {
    pub(super) struct TraceBody<B> {
        #[pin]
        inner: B,
        span: Option<Span>,
        started: Instant,
        terminal_status_observed: bool,
    }

    impl<B> PinnedDrop for TraceBody<B> {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            if let Some(span) = this.span.take() {
                finish_span(&span, CANCELLED_STATUS, *this.started);
            }
        }
    }
}

impl<B> TraceBody<B> {
    fn finish(&mut self, status: u16) {
        if let Some(span) = self.span.take() {
            finish_span(&span, status, self.started);
        }
    }
}

impl<B> Default for TraceBody<B>
where
    B: Default,
{
    fn default() -> Self {
        Self {
            inner: B::default(),
            span: None,
            started: Instant::now(),
            terminal_status_observed: false,
        }
    }
}

impl<B> Body for TraceBody<B>
where
    B: Body,
    B::Error: 'static,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        let active_span = this.span.clone();
        let _entered = active_span.as_ref().map(Span::enter);
        match this.inner.as_mut().poll_frame(context) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(trailers) = frame.trailers_ref()
                    && let Some(status) = grpc_status(trailers)
                {
                    *this.terminal_status_observed = true;
                    if let Some(span) = this.span.take() {
                        finish_span(&span, status, *this.started);
                    }
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                if let Some(span) = this.span.take() {
                    finish_span(&span, body_error_status(&error), *this.started);
                }
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                if let Some(span) = this.span.take() {
                    let status = if *this.terminal_status_observed {
                        tonic::Code::Ok as u16
                    } else {
                        UNKNOWN_STATUS
                    };
                    finish_span(&span, status, *this.started);
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

fn body_error_status<Error: 'static>(error: &Error) -> u16 {
    (error as &dyn std::any::Any)
        .downcast_ref::<tonic::Status>()
        .map_or(UNKNOWN_STATUS, |status| status.code() as u16)
}

fn inject_w3c_context(span: &Span, headers: &mut http::HeaderMap) {
    let context = span.context();
    let context_span = context.span();
    let span_context = context_span.span_context();
    if !span_context.is_valid() {
        return;
    }
    let traceparent = format!(
        "00-{}-{}-{:02x}",
        span_context.trace_id(),
        span_context.span_id(),
        span_context.trace_flags().to_u8(),
    );
    if let Ok(traceparent) = http::HeaderValue::from_str(&traceparent) {
        headers.insert("traceparent", traceparent);
    }
    let tracestate = span_context.trace_state().header();
    if !tracestate.is_empty()
        && let Ok(tracestate) = http::HeaderValue::from_str(&tracestate)
    {
        headers.insert("tracestate", tracestate);
    }
}

pub(super) fn connection_span() -> Span {
    tracing::info_span!(
        "waltr.grpc.connect",
        otel.name = "waltr.grpc.connect",
        otel.kind = "client",
        otel.status_code = Empty,
        waltr.operation = "connect",
        waltr.grpc.duration_ms = Empty,
    )
}

pub(super) fn finish_connection(span: &Span, succeeded: bool, started: Instant) {
    span.record("otel.status_code", if succeeded { "OK" } else { "ERROR" });
    record_duration(span, started);
}

fn finish_span(span: &Span, status: u16, started: Instant) {
    span.record("rpc.grpc.status_code", i64::from(status));
    span.record("otel.status_code", if status == 0 { "OK" } else { "ERROR" });
    record_duration(span, started);
}

fn record_duration(span: &Span, started: Instant) {
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    span.record("waltr.grpc.duration_ms", duration_ms);
}

fn grpc_status(headers: &http::HeaderMap) -> Option<u16> {
    headers
        .get("grpc-status")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|status| *status <= 16)
}

const fn grpc_status_from_http(status: http::StatusCode) -> Option<u16> {
    match status.as_u16() {
        200 => None,
        400 => Some(tonic::Code::Internal as u16),
        401 => Some(tonic::Code::Unauthenticated as u16),
        403 => Some(tonic::Code::PermissionDenied as u16),
        404 => Some(tonic::Code::Unimplemented as u16),
        429 | 502..=504 => Some(tonic::Code::Unavailable as u16),
        _ => Some(UNKNOWN_STATUS),
    }
}

#[derive(Clone, Copy)]
struct MethodAttributes {
    service: &'static str,
    method: &'static str,
    operation: &'static str,
}

fn method_attributes(path: &str) -> MethodAttributes {
    const UNKNOWN: MethodAttributes = MethodAttributes {
        service: "unknown",
        method: "unknown",
        operation: "grpc.unknown",
    };

    let Some((service, method)) = path.strip_prefix('/').and_then(|path| path.split_once('/'))
    else {
        return UNKNOWN;
    };
    known_method(service, method).unwrap_or(UNKNOWN)
}

macro_rules! service_methods {
    ($method_value:expr, $service:literal, $prefix:literal, [$($method:literal),+ $(,)?]) => {
        match $method_value {
            $($method => Some(MethodAttributes {
                service: $service,
                method: $method,
                operation: concat!($prefix, ".", $method),
            }),)+
            _ => None,
        }
    };
}

#[allow(clippy::too_many_lines)]
fn known_method(service: &str, method: &str) -> Option<MethodAttributes> {
    match service {
        "waltr.resource.v1.ResourceService" => service_methods!(
            method,
            "waltr.resource.v1.ResourceService",
            "resource",
            [
                "GetResourceCatalog",
                "WatchResourceCatalog",
                "InspectResource",
                "ReplaceResource",
                "ResetResource",
            ]
        ),
        "waltr.auth.v1.AuthService" => service_methods!(
            method,
            "waltr.auth.v1.AuthService",
            "auth",
            [
                "ListAuthStatuses",
                "GetAuthStatus",
                "BeginAuth",
                "CompleteAuth",
                "GetAuthAttemptStatus",
                "CancelAuth",
                "RefreshAuth",
                "Logout",
            ]
        ),
        "waltr.catalog.v1.CatalogService" => service_methods!(
            method,
            "waltr.catalog.v1.CatalogService",
            "catalog",
            [
                "SearchCatalogModels",
                "SuggestCatalogMatches",
                "GetModelCatalogDrift",
            ]
        ),
        "waltr.status.v1.StatusService" => service_methods!(
            method,
            "waltr.status.v1.StatusService",
            "status",
            ["WatchStatus"]
        ),
        "waltr.session.v1.SessionService" => service_methods!(
            method,
            "waltr.session.v1.SessionService",
            "session",
            [
                "ListAgents",
                "CreateSession",
                "ListSessions",
                "DeleteSession",
                "WipeSessions",
                "UpdateSessionInferenceSettings",
                "UpdateSessionAgent",
                "WatchSessions",
                "GetTimeline",
                "WatchTimeline",
                "SubmitUserMessage",
                "RespondExternalRequest",
                "InterruptSession",
                "RollbackSession",
                "ContinueSession",
                "SearchSessionTemplates",
                "OpenSessionMcpApp",
                "CallSessionMcpAppTool",
                "CloseSessionMcpApp",
            ]
        ),
        "waltr.routine.v1.RoutineService" => service_methods!(
            method,
            "waltr.routine.v1.RoutineService",
            "routine",
            [
                "WatchManualRoutines",
                "StartManualRoutine",
                "ListRoutineInvocations",
                "GetRoutineInvocation",
                "WatchRoutineInvocations",
            ]
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, convert::Infallible, task::Waker};

    use http_body::Frame;
    use opentelemetry::{
        Context as OtelContext,
        trace::{
            SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState,
            TracerProvider as _,
        },
    };
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    use tracing_subscriber::{Registry, layer::SubscriberExt as _};

    use super::*;

    struct ReadyService {
        response: Option<http::Response<TestBody>>,
    }

    impl Service<http::Request<()>> for ReadyService {
        type Response = http::Response<TestBody>;
        type Error = Infallible;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: http::Request<()>) -> Self::Future {
            std::future::ready(Ok(self.response.take().expect("one response")))
        }
    }

    struct PendingService;

    impl Service<http::Request<()>> for PendingService {
        type Response = http::Response<TestBody>;
        type Error = Infallible;
        type Future = std::future::Pending<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: http::Request<()>) -> Self::Future {
            std::future::pending()
        }
    }

    struct TestBody {
        frames: VecDeque<Result<Frame<tonic::codegen::Bytes>, tonic::Status>>,
        pending_at_end: bool,
    }

    impl TestBody {
        fn empty() -> Self {
            Self {
                frames: VecDeque::new(),
                pending_at_end: false,
            }
        }

        fn pending() -> Self {
            Self {
                frames: VecDeque::new(),
                pending_at_end: true,
            }
        }

        fn trailers(status: &'static str) -> Self {
            let mut trailers = http::HeaderMap::new();
            trailers.insert("grpc-status", http::HeaderValue::from_static(status));
            Self {
                frames: VecDeque::from([Ok(Frame::trailers(trailers))]),
                pending_at_end: false,
            }
        }

        fn error(status: tonic::Status) -> Self {
            Self {
                frames: VecDeque::from([Err(status)]),
                pending_at_end: false,
            }
        }
    }

    impl Body for TestBody {
        type Data = tonic::codegen::Bytes;
        type Error = tonic::Status;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            self.frames.pop_front().map_or_else(
                || {
                    if self.pending_at_end {
                        Poll::Pending
                    } else {
                        Poll::Ready(None)
                    }
                },
                |frame| Poll::Ready(Some(frame)),
            )
        }
    }

    fn test_waker() -> &'static Waker {
        Waker::noop()
    }

    fn capture_spans(run: impl FnOnce()) -> Vec<SpanData> {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = Registry::default().with(
            tracing_opentelemetry::layer()
                .with_tracer(provider.tracer("waltr-client-lifecycle-test")),
        );
        tracing::subscriber::with_default(subscriber, run);
        exporter.get_finished_spans().unwrap()
    }

    fn request() -> http::Request<()> {
        http::Request::builder()
            .uri("/waltr.status.v1.StatusService/WatchStatus")
            .body(())
            .unwrap()
    }

    fn response(
        status: http::StatusCode,
        grpc_status: Option<&'static str>,
        body: TestBody,
    ) -> http::Response<TestBody> {
        let mut response = http::Response::builder().status(status).body(body).unwrap();
        if let Some(status) = grpc_status {
            response
                .headers_mut()
                .insert("grpc-status", http::HeaderValue::from_static(status));
        }
        response
    }

    fn poll_response_body(response: http::Response<TraceBody<TestBody>>) {
        let mut body = response.into_body();
        let mut context = Context::from_waker(test_waker());
        loop {
            match Pin::new(&mut body).poll_frame(&mut context) {
                Poll::Ready(Some(_)) => {}
                Poll::Ready(None) | Poll::Pending => break,
            }
        }
    }

    fn run_response(response: http::Response<TestBody>, poll_body: bool) -> Vec<SpanData> {
        capture_spans(|| {
            let mut service = NativeTraceLayer.layer(ReadyService {
                response: Some(response),
            });
            let mut context = Context::from_waker(test_waker());
            assert!(matches!(
                service.poll_ready(&mut context),
                Poll::Ready(Ok(()))
            ));
            let mut future = service.call(request());
            let Poll::Ready(Ok(response)) = Pin::new(&mut future).poll(&mut context) else {
                panic!("response should be ready");
            };
            if poll_body {
                poll_response_body(response);
            } else {
                drop(response);
            }
        })
    }

    fn recorded_status(spans: &[SpanData]) -> i64 {
        assert_eq!(spans.len(), 1, "one RPC must finalize exactly once");
        assert!(
            spans[0]
                .attributes
                .iter()
                .any(|attribute| attribute.key.as_str() == "waltr.grpc.duration_ms"),
            "RPC span must have an elapsed duration"
        );
        spans[0]
            .attributes
            .iter()
            .find_map(|attribute| {
                (attribute.key.as_str() == "rpc.grpc.status_code").then(|| match attribute.value {
                    opentelemetry::Value::I64(value) => value,
                    _ => panic!("gRPC status must be an integer"),
                })
            })
            .expect("RPC span must have a terminal status")
    }

    #[test]
    fn dropping_pending_response_future_records_cancelled_once() {
        let spans = capture_spans(|| {
            let mut service = NativeTraceLayer.layer(PendingService);
            let mut context = Context::from_waker(test_waker());
            assert!(matches!(
                service.poll_ready(&mut context),
                Poll::Ready(Ok(()))
            ));
            let mut future = service.call(request());
            assert!(Pin::new(&mut future).poll(&mut context).is_pending());
            drop(future);
        });

        assert_eq!(recorded_status(&spans), i64::from(CANCELLED_STATUS));
    }

    #[test]
    fn response_lifecycle_classifies_terminal_outcomes_without_duplicates() {
        let cases = [
            (
                "trailers-only OK",
                response(http::StatusCode::OK, None, TestBody::trailers("0")),
                true,
                tonic::Code::Ok,
            ),
            (
                "valid header status",
                response(http::StatusCode::OK, Some("7"), TestBody::pending()),
                false,
                tonic::Code::PermissionDenied,
            ),
            (
                "missing terminal status",
                response(http::StatusCode::OK, None, TestBody::empty()),
                true,
                tonic::Code::Unknown,
            ),
            (
                "malformed terminal status",
                response(http::StatusCode::OK, Some("999"), TestBody::empty()),
                true,
                tonic::Code::Unknown,
            ),
            (
                "non-200 transport response",
                response(
                    http::StatusCode::SERVICE_UNAVAILABLE,
                    None,
                    TestBody::pending(),
                ),
                false,
                tonic::Code::Unavailable,
            ),
            (
                "body error",
                response(
                    http::StatusCode::OK,
                    None,
                    TestBody::error(tonic::Status::resource_exhausted(
                        "sensitive status message",
                    )),
                ),
                true,
                tonic::Code::ResourceExhausted,
            ),
            (
                "body cancellation",
                response(http::StatusCode::OK, None, TestBody::pending()),
                false,
                tonic::Code::Cancelled,
            ),
        ];

        for (name, response, poll_body, expected) in cases {
            let spans = run_response(response, poll_body);
            assert!(
                spans[0]
                    .attributes
                    .iter()
                    .all(|attribute| !attribute.value.to_string().contains("sensitive"))
            );
            assert_eq!(
                recorded_status(&spans),
                i64::from(expected as u16),
                "{name}"
            );
        }
    }

    #[test]
    fn sampled_context_injects_valid_w3c_headers_and_preserves_authorization() {
        let parent = OtelContext::new().with_remote_span_context(SpanContext::new(
            TraceId::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").unwrap(),
            SpanId::from_hex("00f067aa0ba902b7").unwrap(),
            TraceFlags::SAMPLED,
            true,
            TraceState::from_key_value([("vendor", "state")]).unwrap(),
        ));
        let provider = SdkTracerProvider::builder().build();
        let subscriber = Registry::default()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("waltr-client-test")));
        let (headers, span_debug) = tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("propagation-test");
            span.set_parent(parent).unwrap();
            let mut headers = http::HeaderMap::new();
            headers.insert(
                "authorization",
                http::HeaderValue::from_static("Bearer secret"),
            );
            inject_w3c_context(&span, &mut headers);
            (headers, format!("{span:?}"))
        });

        let traceparent = headers.get("traceparent").unwrap().to_str().unwrap();
        assert!(traceparent.starts_with("00-4bf92f3577b34da6a3ce929d0e0e4736-"));
        assert!(traceparent.ends_with("-01"));
        assert_eq!(headers.get("tracestate").unwrap(), "vendor=state");
        assert_eq!(headers.get("authorization").unwrap(), "Bearer secret");
        assert!(!span_debug.contains("secret"));
    }

    #[test]
    fn absent_context_does_not_inject_headers() {
        let mut headers = http::HeaderMap::new();
        inject_w3c_context(&Span::none(), &mut headers);
        assert!(headers.get("traceparent").is_none());
        assert!(headers.get("tracestate").is_none());
    }

    #[test]
    fn status_and_method_classification_are_bounded() {
        let mut headers = http::HeaderMap::new();
        for status in 0_u16..=16 {
            headers.insert("grpc-status", status.into());
            assert_eq!(grpc_status(&headers), Some(status));
        }
        headers.insert("grpc-status", http::HeaderValue::from_static("999"));
        assert_eq!(grpc_status(&headers), None);
        assert_eq!(
            grpc_status_from_http(http::StatusCode::BAD_REQUEST),
            Some(tonic::Code::Internal as u16)
        );
        assert_eq!(grpc_status_from_http(http::StatusCode::OK), None);

        let known = method_attributes("/waltr.session.v1.SessionService/ListSessions");
        assert_eq!(known.operation, "session.ListSessions");
        let unknown = method_attributes("/private.UserService/GetUser");
        assert_eq!(unknown.operation, "grpc.unknown");
        assert_eq!(unknown.service, "unknown");
    }
}
