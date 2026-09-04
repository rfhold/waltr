//! Native application observability with bounded, fail-open OTLP exporters.

use std::{
    io::Write as _,
    panic::PanicHookInfo,
    sync::{Arc, Once},
    time::Duration,
};

use opentelemetry::{KeyValue, metrics::MeterProvider as _, trace::TracerProvider as _};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{WithExportConfig as _, WithHttpConfig as _};
use opentelemetry_sdk::{
    Resource,
    logs::{LogExporter, SdkLoggerProvider},
    metrics::{SdkMeterProvider, exporter::PushMetricExporter},
    trace::SdkTracerProvider,
};
use tracing::Span;

#[cfg(any(target_os = "android", test))]
use crate::server_controller::ServerStartError;
use tracing_subscriber::{
    Layer as _, filter::filter_fn, layer::SubscriberExt as _, util::SubscriberInitExt as _,
};

const SERVICE_NAME: &str = "waltr-app-native";
const OTLP_ENDPOINT: &str = "https://telemetry.holdenitdown.net:4318";
const EXPORT_TIMEOUT: Duration = Duration::from_secs(2);
const TRACE_LOG_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const STARTUP_METRIC: &str = "waltr.app.startups";
const STARTUP_ATTRIBUTE_KEY: &str = "app.lifecycle";
const STARTUP_ATTRIBUTE_VALUE: &str = "started";
const APP_TARGET: &str = "waltr_app_lib";
const NATIVE_TRACE_TARGET: &str = "waltr_client::transport::native_trace";
const INIT_FAILURE_MESSAGE: &str =
    "waltr native telemetry initialization failed; continuing without telemetry";
static PANIC_HOOK: Once = Once::new();

/// Process-lifetime owner of all native telemetry providers.
#[derive(Debug, Default)]
pub(crate) struct ObservabilityGuard {
    tracer: Option<SdkTracerProvider>,
    logger: Option<SdkLoggerProvider>,
    meter: Option<SdkMeterProvider>,
}

impl Drop for ObservabilityGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.tracer.take() {
            drop(provider.shutdown_with_timeout(TRACE_LOG_SHUTDOWN_TIMEOUT));
        }
        if let Some(provider) = self.logger.take() {
            drop(provider.shutdown_with_timeout(TRACE_LOG_SHUTDOWN_TIMEOUT));
        }
        if let Some(provider) = self.meter.take() {
            shutdown_meter(&provider);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExportMode {
    Disabled,
    Enabled,
}

const fn export_enabled(is_test: bool, _debug_assertions: bool) -> bool {
    !is_test
}

/// Initialize native telemetry without allowing telemetry failure to escape.
pub(crate) fn init() -> ObservabilityGuard {
    let mode = if export_enabled(cfg!(test), cfg!(debug_assertions)) {
        ExportMode::Enabled
    } else {
        ExportMode::Disabled
    };
    init_for_mode(mode)
}

fn init_for_mode(mode: ExportMode) -> ObservabilityGuard {
    if mode == ExportMode::Disabled {
        return ObservabilityGuard::default();
    }

    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(try_init)) {
        Ok(Ok(guard)) => guard,
        Ok(Err(_)) | Err(_) => {
            report_init_failure();
            ObservabilityGuard::default()
        }
    }
}

fn report_init_failure() {
    drop(writeln!(std::io::stderr().lock(), "{INIT_FAILURE_MESSAGE}"));
}

fn try_init() -> Result<ObservabilityGuard, Box<dyn std::error::Error + Send + Sync>> {
    let http_client = PrivacyHttpClient::new()?;
    let span_exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_http_client(http_client.clone())
        .with_endpoint(signal_endpoint("traces"))
        .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
        .with_timeout(EXPORT_TIMEOUT)
        .build()?;
    let log_exporter = opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .with_http_client(http_client.clone())
        .with_endpoint(signal_endpoint("logs"))
        .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
        .with_timeout(EXPORT_TIMEOUT)
        .build()?;
    let metric_exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .with_http_client(http_client)
        .with_endpoint(signal_endpoint("metrics"))
        .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
        .with_timeout(EXPORT_TIMEOUT)
        .build()?;
    let providers = build_providers(resource(), span_exporter, log_exporter, metric_exporter);
    if let Err(error) = install_subscriber(&providers) {
        shutdown_providers(&providers);
        return Err(error);
    }
    emit_startup_metric(&providers.meter);
    install_panic_hook();
    Ok(providers.into())
}

fn signal_endpoint(signal: &str) -> String {
    format!("{OTLP_ENDPOINT}/v1/{signal}")
}

fn resource() -> Resource {
    let revision = sanitize_revision(revision()).unwrap_or_else(|| "unknown".to_owned());
    Resource::builder()
        .with_service_name(SERVICE_NAME)
        .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
        .with_attribute(KeyValue::new("waltr.app.revision", revision))
        .with_attribute(KeyValue::new("waltr.app.target", target()))
        .build()
}

#[derive(Debug)]
struct Providers {
    tracer: SdkTracerProvider,
    logger: SdkLoggerProvider,
    meter: SdkMeterProvider,
}

fn build_providers<S, L, M>(resource: Resource, spans: S, logs: L, metrics: M) -> Providers
where
    S: opentelemetry_sdk::trace::SpanExporter + 'static,
    L: LogExporter + 'static,
    M: PushMetricExporter,
{
    let tracer = SdkTracerProvider::builder()
        .with_resource(resource.clone())
        .with_batch_exporter(spans)
        .build();
    let logger = SdkLoggerProvider::builder()
        .with_resource(resource.clone())
        .with_batch_exporter(logs)
        .build();
    let meter = SdkMeterProvider::builder()
        .with_resource(resource)
        .with_periodic_exporter(metrics)
        .build();
    Providers {
        tracer,
        logger,
        meter,
    }
}

fn install_subscriber(
    providers: &Providers,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let tracer = providers.tracer.tracer(SERVICE_NAME);
    let safe_spans = filter_fn(safe_trace_target);
    let safe_events = filter_fn(safe_log_target);
    tracing_subscriber::registry()
        .with(
            tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_filter(safe_spans),
        )
        .with(OpenTelemetryTracingBridge::new(&providers.logger).with_filter(safe_events.clone()))
        .with(
            tracing_subscriber::fmt::layer()
                .compact()
                .with_ansi(false)
                .with_target(false)
                .with_filter(safe_events),
        )
        .try_init()?;
    Ok(())
}

fn safe_trace_target(metadata: &tracing::Metadata<'_>) -> bool {
    useful_level(metadata)
        && (target_in_module(metadata.target(), APP_TARGET)
            || target_in_module(metadata.target(), NATIVE_TRACE_TARGET))
}

fn safe_log_target(metadata: &tracing::Metadata<'_>) -> bool {
    useful_level(metadata) && target_in_module(metadata.target(), APP_TARGET)
}

fn useful_level(metadata: &tracing::Metadata<'_>) -> bool {
    *metadata.level() <= tracing::Level::INFO
}

fn target_in_module(target: &str, module: &str) -> bool {
    target == module
        || target
            .strip_prefix(module)
            .is_some_and(|suffix| suffix.starts_with("::"))
}

fn emit_startup_metric(provider: &SdkMeterProvider) {
    provider
        .meter(SERVICE_NAME)
        .u64_counter(STARTUP_METRIC)
        .build()
        .add(
            1,
            &[KeyValue::new(
                STARTUP_ATTRIBUTE_KEY,
                STARTUP_ATTRIBUTE_VALUE,
            )],
        );
}

fn shutdown_providers(providers: &Providers) {
    drop(
        providers
            .tracer
            .shutdown_with_timeout(TRACE_LOG_SHUTDOWN_TIMEOUT),
    );
    drop(
        providers
            .logger
            .shutdown_with_timeout(TRACE_LOG_SHUTDOWN_TIMEOUT),
    );
    shutdown_meter(&providers.meter);
}

fn shutdown_meter(provider: &SdkMeterProvider) {
    // SDK 0.32 ignores caller-provided meter timeouts and uses its fixed five-second bound.
    drop(provider.shutdown());
}

#[derive(Clone, Debug)]
struct PrivacyHttpClient(reqwest::blocking::Client);

impl PrivacyHttpClient {
    fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let tls = tls_client_config()?;
        reqwest::blocking::Client::builder()
            .timeout(EXPORT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .tls_backend_preconfigured(tls)
            .build()
            .map(Self)
            .map_err(Into::into)
    }
}

fn mozilla_root_store() -> rustls::RootCertStore {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    roots
}

fn tls_client_config() -> Result<rustls::ClientConfig, rustls::Error> {
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])?
    .with_root_certificates(mozilla_root_store())
    .with_no_client_auth())
}

#[async_trait::async_trait]
impl opentelemetry_http::HttpClient for PrivacyHttpClient {
    async fn send_bytes(
        &self,
        mut request: opentelemetry_http::Request<opentelemetry_http::Bytes>,
    ) -> Result<
        opentelemetry_http::Response<opentelemetry_http::Bytes>,
        opentelemetry_http::HttpError,
    > {
        sanitize_export_headers(&mut request);
        opentelemetry_http::HttpClient::send_bytes(&self.0, request).await
    }
}

fn sanitize_export_headers(request: &mut opentelemetry_http::Request<opentelemetry_http::Bytes>) {
    let headers = request.headers_mut();
    headers.clear();
    headers.insert(
        "content-type",
        "application/x-protobuf"
            .parse()
            .expect("static OTLP content type"),
    );
}

impl From<Providers> for ObservabilityGuard {
    fn from(providers: Providers) -> Self {
        Self {
            tracer: Some(providers.tracer),
            logger: Some(providers.logger),
            meter: Some(providers.meter),
        }
    }
}

pub(crate) fn operation_span(name: &'static str, class: &'static str) -> Span {
    tracing::info_span!(
        "waltr.app.operation",
        operation.name = name,
        operation.class = class,
        operation.outcome = tracing::field::Empty,
        failure.category = tracing::field::Empty,
    )
}

#[derive(Clone, Copy, Debug)]
#[cfg(any(target_os = "android", test))]
pub(crate) enum AndroidStartupFailure {
    Jni,
    Server(ServerStartError),
}

#[cfg(any(target_os = "android", test))]
impl AndroidStartupFailure {
    const fn category(self) -> &'static str {
        match self {
            Self::Jni => "jni",
            Self::Server(error) => error.telemetry_category(),
        }
    }
}

#[cfg(any(target_os = "android", test))]
pub(crate) fn record_android_startup_panic(span: &Span) {
    span.record("operation.outcome", "error");
    span.record("failure.category", "panic");
}

#[cfg(any(target_os = "android", test))]
pub(crate) fn record_android_startup_failure(span: &Span, failure: AndroidStartupFailure) {
    let category = failure.category();
    span.record("operation.outcome", "error");
    span.record("failure.category", category);
    span.in_scope(|| {
        tracing::error!(
            event.name = "android.server.start_failed",
            outcome = "error",
            failure.category = category,
        );
    });
}

pub(crate) fn record_result<T, E>(span: &Span, result: &Result<T, E>) {
    span.record("operation.outcome", result_outcome(result));
}

const fn result_outcome<T, E>(result: &Result<T, E>) -> &'static str {
    if result.is_ok() { "ok" } else { "error" }
}

pub(crate) fn record_outcome(span: &Span, outcome: &'static str) {
    span.record("operation.outcome", outcome);
}

pub(crate) fn lifecycle_event(name: &'static str, class: &'static str, outcome: &'static str) {
    let span = operation_span(name, class);
    record_outcome(&span, outcome);
    span.in_scope(|| tracing::info!(event.name = name, outcome));
}

pub(crate) fn error_event(name: &'static str, class: &'static str, outcome: &'static str) {
    let span = operation_span(name, class);
    record_outcome(&span, outcome);
    span.in_scope(|| tracing::error!(event.name = name, outcome));
}

pub(crate) const fn revision() -> &'static str {
    env!("WALTR_APP_REVISION")
}

pub(crate) const fn target() -> &'static str {
    env!("WALTR_APP_TARGET")
}

fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            emit_panic(info);
            previous(info);
        }));
    });
}

fn emit_panic(info: &PanicHookInfo<'_>) {
    let metadata = PanicMetadata::from_hook(info);
    let span = tracing::error_span!(
        "waltr.app.panic",
        panic.class = metadata.class,
        code.filepath = metadata.file.as_deref().unwrap_or("unknown"),
        code.lineno = metadata.line.unwrap_or(0),
        code.column = metadata.column.unwrap_or(0),
        thread.name = metadata.thread.as_deref().unwrap_or("unnamed"),
        service.version = env!("CARGO_PKG_VERSION"),
        waltr.app.revision = revision(),
        waltr.app.target = target(),
    );
    span.in_scope(|| tracing::error!(event.name = "panic", outcome = "panicked"));
}

#[derive(Debug, Eq, PartialEq)]
struct PanicMetadata {
    class: &'static str,
    file: Option<String>,
    line: Option<u32>,
    column: Option<u32>,
    thread: Option<String>,
}

impl PanicMetadata {
    fn from_hook(info: &PanicHookInfo<'_>) -> Self {
        let location = info.location();
        Self {
            class: "rust_panic",
            file: location.map(|location| location.file().to_owned()),
            line: location.map(std::panic::Location::line),
            column: location.map(std::panic::Location::column),
            thread: std::thread::current().name().map(str::to_owned),
        }
    }

    #[cfg(test)]
    fn from_parts(
        file: Option<&str>,
        line: Option<u32>,
        column: Option<u32>,
        thread: Option<&str>,
    ) -> Self {
        Self {
            class: "rust_panic",
            file: file.map(str::to_owned),
            line,
            column,
            thread: thread.map(str::to_owned),
        }
    }
}

pub(crate) fn sanitize_revision(value: &str) -> Option<String> {
    let value = value.trim();
    (value.len() >= 7 && value.len() <= 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use std::{
        future::Future,
        io::{Read, Write},
        net::TcpListener,
        process::Command,
        task::{Context, Poll, Waker},
        thread,
        time::Instant,
    };

    use opentelemetry::{
        Key,
        logs::AnyValue,
        trace::{Span as _, Tracer as _},
    };
    use opentelemetry_sdk::{
        error::OTelSdkResult,
        logs::InMemoryLogExporter,
        metrics::InMemoryMetricExporter,
        trace::{InMemorySpanExporter, SpanData, SpanExporter},
    };

    use super::*;

    #[test]
    fn revision_sanitization_accepts_only_bounded_hex() {
        assert_eq!(sanitize_revision(" A1b2C3d\n").as_deref(), Some("a1b2c3d"));
        assert_eq!(
            sanitize_revision("0123456789abcdef0123456789abcdef01234567").as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        for rejected in [
            "",
            "123456",
            "0123456789abcdef0123456789abcdef012345678",
            "main",
            "abcdefg",
            "1234567;cargo:rustc-env=LEAK=value",
        ] {
            assert_eq!(sanitize_revision(rejected), None, "{rejected}");
        }
    }

    #[test]
    fn disabled_initialization_is_fail_open_and_global_free() {
        let guard = init_for_mode(ExportMode::Disabled);
        assert!(guard.tracer.is_none());
        assert!(guard.logger.is_none());
        assert!(guard.meter.is_none());
        assert!(!tracing::dispatcher::has_been_set());
    }

    #[test]
    fn only_tests_disable_export_even_for_debug_builds() {
        assert!(!export_enabled(true, true));
        assert!(!export_enabled(true, false));
        assert!(export_enabled(false, true));
        assert!(export_enabled(false, false));
    }

    #[test]
    fn panic_metadata_has_only_safe_fields_and_no_payload() {
        let metadata =
            PanicMetadata::from_parts(Some("src/lib.rs"), Some(12), Some(4), Some("main"));
        assert_eq!(metadata.class, "rust_panic");
        assert_eq!(metadata.file.as_deref(), Some("src/lib.rs"));
        assert_eq!(metadata.line, Some(12));
        assert_eq!(metadata.column, Some(4));
        assert_eq!(metadata.thread.as_deref(), Some("main"));
        let source = include_str!("observability.rs");
        let production_source = source.split("#[cfg(test)]").next().unwrap();
        assert!(!production_source.contains(".payload("));
    }

    #[test]
    fn result_classification_is_bounded() {
        let success: Result<(), ()> = Ok(());
        let failure: Result<(), &str> = Err("SECRET_ERROR_MARKER");
        assert_eq!(result_outcome(&success), "ok");
        assert_eq!(result_outcome(&failure), "error");
        assert!(!result_outcome(&failure).contains("SECRET_ERROR_MARKER"));
    }

    #[test]
    fn android_startup_failure_classification_is_closed_and_categorical() {
        use waltr_server::embedded::EmbeddedBuildError;

        let categories = [
            AndroidStartupFailure::Jni,
            AndroidStartupFailure::Server(ServerStartError::Paths),
            AndroidStartupFailure::Server(ServerStartError::Entropy),
            AndroidStartupFailure::Server(ServerStartError::Thread),
            AndroidStartupFailure::Server(ServerStartError::Readiness),
            AndroidStartupFailure::Server(ServerStartError::Build(EmbeddedBuildError::Paths)),
            AndroidStartupFailure::Server(ServerStartError::Build(EmbeddedBuildError::Listener)),
            AndroidStartupFailure::Server(ServerStartError::Build(EmbeddedBuildError::Persistence)),
            AndroidStartupFailure::Server(ServerStartError::Build(EmbeddedBuildError::Catalog)),
            AndroidStartupFailure::Server(ServerStartError::Build(EmbeddedBuildError::Resources)),
            AndroidStartupFailure::Server(ServerStartError::Build(EmbeddedBuildError::Application)),
        ]
        .map(AndroidStartupFailure::category);

        assert_eq!(
            categories,
            [
                "jni",
                "paths",
                "entropy",
                "thread",
                "readiness",
                "build_paths",
                "build_listener",
                "build_persistence",
                "build_catalog",
                "build_resources",
                "build_application",
            ]
        );
        assert!(categories.iter().all(|category| {
            category.len() <= 17
                && category
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        }));

        let production_source = include_str!("observability.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(production_source.contains("android.server.start_failed"));
        assert!(!production_source.contains("error.to_string()"));
        assert!(!production_source.contains("format!(\"{error}"));

        let spans = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(spans.clone())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
        tracing::subscriber::with_default(subscriber, || {
            let span = operation_span("android.server.start", "lifecycle");
            record_android_startup_failure(
                &span,
                AndroidStartupFailure::Server(ServerStartError::Build(EmbeddedBuildError::Catalog)),
            );
        });
        provider.force_flush().unwrap();
        let exported = spans.get_finished_spans().unwrap();
        let startup = exported
            .iter()
            .find(|span| span.name == "waltr.app.operation")
            .unwrap();
        for (key, value) in [
            ("operation.name", "android.server.start"),
            ("operation.class", "lifecycle"),
            ("operation.outcome", "error"),
            ("failure.category", "build_catalog"),
        ] {
            assert!(startup.attributes.iter().any(|attribute| {
                attribute.key.as_str() == key && attribute.value.to_string() == value
            }));
        }
        let event = startup.events.events.first().unwrap();
        assert_eq!(startup.events.events.len(), 1);
        for (key, value) in [
            ("event.name", "android.server.start_failed"),
            ("outcome", "error"),
            ("failure.category", "build_catalog"),
        ] {
            assert!(event.attributes.iter().any(|attribute| {
                attribute.key.as_str() == key && attribute.value.to_string() == value
            }));
        }
        let diagnostic = format!("{startup:?}");
        assert!(!diagnostic.contains("embedded models catalog could not be initialized"));
        assert!(!diagnostic.contains("http://"));
        drop(provider.shutdown_with_timeout(TRACE_LOG_SHUTDOWN_TIMEOUT));
    }

    #[test]
    fn android_startup_panic_marks_operation_without_duplicate_failure_event() {
        let spans = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(spans.clone())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
        tracing::subscriber::with_default(subscriber, || {
            let span = operation_span("android.server.start", "lifecycle");
            record_android_startup_panic(&span);
        });
        provider.force_flush().unwrap();
        let exported = spans.get_finished_spans().unwrap();
        let startup = exported
            .iter()
            .find(|span| span.name == "waltr.app.operation")
            .unwrap();
        for (key, value) in [
            ("operation.outcome", "error"),
            ("failure.category", "panic"),
        ] {
            assert!(startup.attributes.iter().any(|attribute| {
                attribute.key.as_str() == key && attribute.value.to_string() == value
            }));
        }
        assert!(startup.events.events.is_empty());
        drop(provider.shutdown_with_timeout(TRACE_LOG_SHUTDOWN_TIMEOUT));
    }

    #[test]
    fn providers_share_resources_and_guard_owns_shutdown() {
        let spans = ResourceCaptureSpanExporter::default();
        let logs = InMemoryLogExporter::default();
        let metrics = InMemoryMetricExporter::default();
        let providers = build_providers(resource(), spans.clone(), logs.clone(), metrics.clone());

        let mut span = providers.tracer.tracer("test").start("resource-test");
        span.end();
        providers.tracer.force_flush().unwrap();

        let subscriber =
            tracing_subscriber::registry().with(OpenTelemetryTracingBridge::new(&providers.logger));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(event.name = "app.startup", outcome = "started");
        });
        providers.logger.force_flush().unwrap();

        emit_startup_metric(&providers.meter);
        providers.meter.force_flush().unwrap();

        assert_resource(spans.resource.lock().unwrap().as_ref().unwrap());
        let emitted_logs = logs.get_emitted_logs().unwrap();
        assert_resource(&emitted_logs[0].resource);
        let log_attributes: Vec<_> = emitted_logs[0].record.attributes_iter().collect();
        assert_eq!(log_attributes.len(), 2);
        for (key, value) in [("event.name", "app.startup"), ("outcome", "started")] {
            assert!(log_attributes.iter().any(|(actual_key, actual_value)| {
                actual_key.as_str() == key
                    && *actual_value == AnyValue::String(value.to_owned().into())
            }));
        }

        let finished_metrics = metrics.get_finished_metrics().unwrap();
        assert_resource(finished_metrics[0].resource());
        let startup = finished_metrics[0]
            .scope_metrics()
            .flat_map(|scope| scope.metrics())
            .find(|metric| metric.name() == STARTUP_METRIC)
            .unwrap();
        let opentelemetry_sdk::metrics::data::AggregatedMetrics::U64(
            opentelemetry_sdk::metrics::data::MetricData::Sum(startup),
        ) = startup.data()
        else {
            panic!("startup metric must be a u64 sum");
        };
        let point = startup.data_points().next().unwrap();
        assert_eq!(point.value(), 1);
        let metric_attributes: Vec<_> = point
            .attributes()
            .map(|attribute| (attribute.key.as_str(), attribute.value.to_string()))
            .collect();
        assert_eq!(
            metric_attributes,
            [(STARTUP_ATTRIBUTE_KEY, STARTUP_ATTRIBUTE_VALUE.to_owned())]
        );

        let guard: ObservabilityGuard = providers.into();
        assert!(guard.tracer.is_some());
        assert!(guard.logger.is_some());
        assert!(guard.meter.is_some());
        drop(guard);
        assert!(spans.shutdown_called.load(Ordering::Relaxed));
        assert!(logs.is_shutdown_called());
    }

    #[test]
    fn trace_filter_exports_only_safe_app_and_native_client_targets() {
        let spans = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(spans.clone())
            .build();
        let tracer = provider.tracer("test");
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_filter(filter_fn(safe_trace_target)),
        );

        tracing::subscriber::with_default(subscriber, || {
            drop(tracing::info_span!(
                target: "tauri::ipc",
                "hostile.tauri",
                ipc.argument = "SECRET_IPC_MARKER",
            ));
            drop(tracing::info_span!(
                target: "dependency::client",
                "hostile.dependency",
                error = "SECRET_ERROR_MARKER",
            ));
            drop(tracing::info_span!(
                target: "waltr_app_lib::previously_unlisted::nested",
                "safe.nested.app",
                operation.outcome = "ok",
            ));
            drop(tracing::info_span!(
                target: "waltr_client::transport::native_trace",
                "safe.native.grpc",
                rpc.system = "grpc",
            ));
            drop(tracing::debug_span!(
                target: "waltr_app_lib::bridge",
                "noisy.app.debug",
            ));
            drop(tracing::info_span!(
                target: "waltr_app_lib_extra::bridge",
                "hostile.app.lookalike",
            ));
            drop(tracing::info_span!(
                target: "waltr_client::transport::native_trace_extra",
                "hostile.native.lookalike",
            ));
        });
        provider.force_flush().unwrap();

        let exported = spans.get_finished_spans().unwrap();
        let mut names: Vec<_> = exported.iter().map(|span| span.name.as_ref()).collect();
        names.sort_unstable();
        assert_eq!(names, ["safe.native.grpc", "safe.nested.app"]);
        let diagnostic = format!("{exported:?}");
        assert!(!diagnostic.contains("SECRET_IPC_MARKER"));
        assert!(!diagnostic.contains("SECRET_ERROR_MARKER"));
        assert!(!diagnostic.contains("noisy.app.debug"));
        assert!(!diagnostic.contains("hostile.app.lookalike"));
        assert!(!diagnostic.contains("hostile.native.lookalike"));
        drop(provider.shutdown_with_timeout(TRACE_LOG_SHUTDOWN_TIMEOUT));
    }

    #[test]
    fn log_filter_exports_all_app_modules_at_info_and_above() {
        let logs = InMemoryLogExporter::default();
        let provider = SdkLoggerProvider::builder()
            .with_simple_exporter(logs.clone())
            .build();
        let subscriber = tracing_subscriber::registry().with(
            OpenTelemetryTracingBridge::new(&provider).with_filter(filter_fn(safe_log_target)),
        );

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "waltr_app_lib::previously_unlisted", "safe.app");
            tracing::error!(target: "waltr_app_lib", "safe.app.error");
            tracing::debug!(target: "waltr_app_lib::bridge", "noisy.app.debug");
            tracing::info!(
                target: "waltr_client::transport::native_trace",
                "excluded.native.log",
            );
            tracing::info!(target: "waltr_app_lib_extra", "excluded.lookalike");
        });
        provider.force_flush().unwrap();

        let emitted = logs.get_emitted_logs().unwrap();
        let diagnostic = format!("{emitted:?}");
        assert_eq!(emitted.len(), 2);
        assert!(diagnostic.contains("safe.app"));
        assert!(diagnostic.contains("safe.app.error"));
        assert!(!diagnostic.contains("noisy.app.debug"));
        assert!(!diagnostic.contains("excluded.native.log"));
        assert!(!diagnostic.contains("excluded.lookalike"));
        drop(provider.shutdown_with_timeout(TRACE_LOG_SHUTDOWN_TIMEOUT));
    }

    #[test]
    fn initialization_failure_diagnostic_is_fixed_and_local() {
        assert_eq!(
            INIT_FAILURE_MESSAGE,
            "waltr native telemetry initialization failed; continuing without telemetry"
        );
        let production_source = include_str!("observability.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(production_source.contains("std::io::stderr().lock()"));
        assert!(!INIT_FAILURE_MESSAGE.contains("http"));
    }

    #[test]
    fn startup_signals_and_endpoints_are_fixed_and_bounded() {
        assert_eq!(
            signal_endpoint("traces"),
            format!("{OTLP_ENDPOINT}/v1/traces")
        );
        assert_eq!(signal_endpoint("logs"), format!("{OTLP_ENDPOINT}/v1/logs"));
        assert_eq!(
            signal_endpoint("metrics"),
            format!("{OTLP_ENDPOINT}/v1/metrics")
        );
        assert_eq!(STARTUP_METRIC, "waltr.app.startups");
        assert_eq!(STARTUP_ATTRIBUTE_KEY, "app.lifecycle");
        assert_eq!(STARTUP_ATTRIBUTE_VALUE, "started");
        let run_source = include_str!("lib.rs");
        assert!(
            run_source
                .contains("tracing::info!(event.name = \"app.startup\", outcome = \"started\")")
        );
    }

    #[test]
    fn exporter_strips_environment_or_injected_credentials() {
        let mut request = opentelemetry_http::Request::builder()
            .header("content-type", "application/x-protobuf")
            .header("authorization", "Bearer SECRET_TOKEN_MARKER")
            .header("x-api-key", "SECRET_KEY_MARKER")
            .body(opentelemetry_http::Bytes::new())
            .unwrap();
        sanitize_export_headers(&mut request);
        assert_eq!(request.headers().len(), 1);
        assert_eq!(request.headers()["content-type"], "application/x-protobuf");
    }

    #[test]
    fn native_tls_uses_complete_mozilla_store_with_isrg_root_x1() {
        const ISRG_ROOT_X1_SUBJECT: &[u8] = b"1\x0b0\t\x06\x03U\x04\x06\x13\x02US1)0'\x06\x03U\x04\n\x13 Internet Security Research Group1\x150\x13\x06\x03U\x04\x03\x13\x0cISRG Root X1";

        let roots = mozilla_root_store();
        assert!(!roots.is_empty());
        assert_eq!(roots.len(), webpki_roots::TLS_SERVER_ROOTS.len());
        assert!(
            roots
                .roots
                .iter()
                .any(|anchor| anchor.subject.as_ref() == ISRG_ROOT_X1_SUBJECT)
        );
        tls_client_config().unwrap();
    }

    #[test]
    fn privacy_http_client_does_not_follow_redirects() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            connection
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut request = [0_u8; 4096];
            let _ = connection.read(&mut request).unwrap();
            write!(
                connection,
                "HTTP/1.1 302 Found\r\nLocation: http://{address}/followed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_millis(250);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok(_) => return true,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("redirect fixture failed: {error}"),
                }
            }
            false
        });
        let request = opentelemetry_http::Request::post(format!("http://{address}/v1/traces"))
            .header("authorization", "Bearer SECRET_TOKEN_MARKER")
            .body(opentelemetry_http::Bytes::new())
            .unwrap();
        let response = poll_blocking_client(opentelemetry_http::HttpClient::send_bytes(
            &PrivacyHttpClient::new().unwrap(),
            request,
        ))
        .unwrap();
        assert_eq!(response.status().as_u16(), 302);
        assert!(!server.join().unwrap());
    }

    #[test]
    fn privacy_http_client_rejects_controlled_environment_root() {
        const CHILD_ENDPOINT_ENV: &str = "WALTR_APP_TLS_CHILD_ENDPOINT";
        if let Ok(endpoint) = std::env::var(CHILD_ENDPOINT_ENV) {
            let request = opentelemetry_http::Request::post(endpoint)
                .body(opentelemetry_http::Bytes::new())
                .unwrap();
            let error = poll_blocking_client(opentelemetry_http::HttpClient::send_bytes(
                &PrivacyHttpClient::new().unwrap(),
                request,
            ))
            .expect_err("a certificate trusted only through SSL_CERT_FILE must be rejected");
            let diagnostic = error_chain(error.as_ref()).to_ascii_lowercase();
            assert!(
                diagnostic.contains("certificate")
                    && (diagnostic.contains("unknownissuer")
                        || diagnostic.contains("unknown issuer")
                        || diagnostic.contains("unknown ca")),
                "expected an untrusted-certificate rejection, got: {diagnostic}"
            );
            return;
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = serve_controlled_tls(listener, 2);
        let endpoint = format!("https://localhost:{}/v1/traces", address.port());

        let response = controlled_root_http_client()
            .post(&endpoint)
            .body(Vec::new())
            .send()
            .expect("the positive-control client must trust the controlled CA");
        assert_eq!(response.status(), reqwest::StatusCode::OK);

        let ca_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../waltr-client/tests/fixtures/native-tls-ca.pem");
        let output = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("observability::tests::privacy_http_client_rejects_controlled_environment_root")
            .arg("--nocapture")
            .env(CHILD_ENDPOINT_ENV, endpoint)
            .env("SSL_CERT_FILE", ca_path)
            .env_remove("SSL_CERT_DIR")
            .output()
            .expect("TLS child test should start");

        server.join().unwrap();
        assert!(
            output.status.success(),
            "TLS child failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    fn shutdown_bounds_match_sdk_behavior() {
        assert_eq!(TRACE_LOG_SHUTDOWN_TIMEOUT, Duration::from_secs(2));
        let production_source = include_str!("observability.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(production_source.contains("fixed five-second bound"));
        assert!(production_source.contains("provider.shutdown()"));
    }

    fn assert_resource(resource: &Resource) {
        assert_eq!(
            resource
                .get(&Key::new("service.name"))
                .map(|value| value.to_string()),
            Some(SERVICE_NAME.to_owned())
        );
        assert_eq!(
            resource
                .get(&Key::new("service.version"))
                .map(|value| value.to_string()),
            Some(env!("CARGO_PKG_VERSION").to_owned())
        );
        assert_eq!(
            resource
                .get(&Key::new("waltr.app.target"))
                .map(|value| value.to_string()),
            Some(target().to_owned())
        );
        assert!(resource.get(&Key::new("waltr.app.revision")).is_some());
    }

    fn poll_blocking_client<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("blocking HTTP client future unexpectedly yielded"),
        }
    }

    fn controlled_root_http_client() -> reqwest::blocking::Client {
        use rustls::pki_types::{CertificateDer, pem::PemObject as _};

        let ca = CertificateDer::from_pem_slice(include_bytes!(
            "../../waltr-client/tests/fixtures/native-tls-ca.pem"
        ))
        .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca).unwrap();
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        reqwest::blocking::Client::builder()
            .tls_backend_preconfigured(tls)
            .build()
            .unwrap()
    }

    fn serve_controlled_tls(listener: TcpListener, connections: usize) -> thread::JoinHandle<()> {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject as _};

        let certificates = CertificateDer::pem_slice_iter(include_bytes!(
            "../../waltr-client/tests/fixtures/native-tls-server.pem"
        ))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
        let key = PrivateKeyDer::from_pem_slice(include_bytes!(
            "../../waltr-client/tests/fixtures/native-tls-server-key.pem"
        ))
        .unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .unwrap();

        thread::spawn(move || {
            for _ in 0..connections {
                let (connection, _) = listener.accept().unwrap();
                connection
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                connection
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let server = rustls::ServerConnection::new(Arc::new(config.clone())).unwrap();
                let mut stream = rustls::StreamOwned::new(server, connection);
                let mut request = [0_u8; 4096];
                match stream.read(&mut request) {
                    Ok(_) => stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK",
                        )
                        .unwrap(),
                    Err(error) => assert!(
                        error
                            .to_string()
                            .to_ascii_lowercase()
                            .contains("certificate")
                            || error.to_string().to_ascii_lowercase().contains("alert"),
                        "expected the rejected client handshake to fail: {error}"
                    ),
                }
            }
        })
    }

    fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
        let mut diagnostic = error.to_string();
        let mut source = error.source();
        while let Some(error) = source {
            diagnostic.push_str(": ");
            diagnostic.push_str(&error.to_string());
            source = error.source();
        }
        diagnostic
    }

    #[derive(Clone, Debug, Default)]
    struct ResourceCaptureSpanExporter {
        resource: Arc<Mutex<Option<Resource>>>,
        shutdown_called: Arc<AtomicBool>,
    }

    impl SpanExporter for ResourceCaptureSpanExporter {
        async fn export(&self, _batch: Vec<SpanData>) -> OTelSdkResult {
            Ok(())
        }

        fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
            self.shutdown_called.store(true, Ordering::Relaxed);
            Ok(())
        }

        fn set_resource(&mut self, resource: &Resource) {
            *self.resource.lock().unwrap() = Some(resource.clone());
        }
    }
}
