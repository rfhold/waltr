//! Native application tracing with a bounded, fail-open OTLP exporter.

use std::{panic::PanicHookInfo, time::Duration};

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{WithExportConfig as _, WithHttpConfig as _};
use opentelemetry_sdk::{
    Resource,
    trace::{BatchConfigBuilder, BatchSpanProcessor, SdkTracerProvider},
};
use tracing::Span;
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

const SERVICE_NAME: &str = "waltr-app-native";
const OTLP_ENDPOINT: &str = "https://telemetry.holdenitdown.net:4318";
const EXPORT_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_QUEUE_SIZE: usize = 256;
const MAX_EXPORT_BATCH_SIZE: usize = 64;

/// Process-lifetime owner of the native trace provider.
#[derive(Debug, Default)]
pub(crate) struct ObservabilityGuard {
    provider: Option<SdkTracerProvider>,
}

impl Drop for ObservabilityGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take() {
            drop(provider.shutdown_with_timeout(SHUTDOWN_TIMEOUT));
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExportMode {
    Disabled,
    Enabled,
}

/// Initialize release tracing without allowing telemetry failure to escape.
pub(crate) fn init() -> ObservabilityGuard {
    let mode = if cfg!(any(test, debug_assertions)) {
        ExportMode::Disabled
    } else {
        ExportMode::Enabled
    };
    init_for_mode(mode)
}

fn init_for_mode(mode: ExportMode) -> ObservabilityGuard {
    if mode == ExportMode::Disabled {
        return ObservabilityGuard::default();
    }

    std::panic::catch_unwind(std::panic::AssertUnwindSafe(try_init))
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
}

fn try_init() -> Result<ObservabilityGuard, Box<dyn std::error::Error + Send + Sync>> {
    let revision = sanitize_revision(revision()).unwrap_or_else(|| "unknown".to_owned());
    let http_client = PrivacyHttpClient(
        reqwest::blocking::Client::builder()
            .timeout(EXPORT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
    );
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_http_client(http_client)
        .with_endpoint(OTLP_ENDPOINT)
        .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
        .with_timeout(EXPORT_TIMEOUT)
        .build()?;
    let batch = BatchSpanProcessor::builder(exporter)
        .with_batch_config(
            BatchConfigBuilder::default()
                .with_max_queue_size(MAX_QUEUE_SIZE)
                .with_max_export_batch_size(MAX_EXPORT_BATCH_SIZE)
                .with_scheduled_delay(Duration::from_secs(5))
                .build(),
        )
        .build();
    let resource = Resource::builder()
        .with_service_name(SERVICE_NAME)
        .with_attribute(opentelemetry::KeyValue::new(
            "service.version",
            env!("CARGO_PKG_VERSION"),
        ))
        .with_attribute(opentelemetry::KeyValue::new("waltr.app.revision", revision))
        .with_attribute(opentelemetry::KeyValue::new("waltr.app.target", target()))
        .build();
    let provider = SdkTracerProvider::builder()
        .with_resource(resource)
        .with_span_processor(batch)
        .build();
    let tracer = provider.tracer(SERVICE_NAME);
    if let Err(error) = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(tracer))
        .try_init()
    {
        drop(provider.shutdown_with_timeout(SHUTDOWN_TIMEOUT));
        return Err(error.into());
    }

    install_panic_hook();
    Ok(ObservabilityGuard {
        provider: Some(provider),
    })
}

#[derive(Debug)]
struct PrivacyHttpClient(reqwest::blocking::Client);

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

pub(crate) fn operation_span(name: &'static str, class: &'static str) -> Span {
    tracing::info_span!(
        "waltr.app.operation",
        operation.name = name,
        operation.class = class,
        operation.outcome = tracing::field::Empty,
    )
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
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        emit_panic(info);
        previous(info);
    }));
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
        assert!(guard.provider.is_none());
        assert!(!tracing::dispatcher::has_been_set());
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
}
