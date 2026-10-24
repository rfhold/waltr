//! Content-free interaction tracing, independent of host exporter ownership.
use opentelemetry::trace::{
    SpanContext, SpanId, TraceContextExt as _, TraceFlags, TraceId, TraceState,
};
use std::sync::atomic::{AtomicBool, Ordering};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

pub(crate) struct Operation {
    pub(crate) span: tracing::Span,
    finished: AtomicBool,
}

impl Drop for Operation {
    fn drop(&mut self) {
        if !self.finished.load(Ordering::Relaxed) {
            self.span.record("outcome", "incomplete");
        }
        // End at owner release even if a transport or database retains a tracing handle.
        self.span.context().span().end();
    }
}

impl Operation {
    pub(crate) fn new(span: tracing::Span) -> Self {
        Self {
            span,
            finished: AtomicBool::new(false),
        }
    }

    pub(crate) fn outcome(&self, outcome: &'static str) {
        self.finished.store(true, Ordering::Relaxed);
        self.span.record("outcome", outcome);
    }

    pub(crate) fn result<T>(&self, result: &Result<T, super::interaction::Error>) {
        self.outcome(match result {
            Ok(_) => "success",
            Err(error) => match error.code {
                "cancelled" | "task_interrupted" => "cancelled",
                "deadline_exceeded" => "deadline",
                _ => "failure",
            },
        });
    }
}

/// Fixed, content-free stages shared by access authentication and interaction admission.
#[derive(Clone, Copy)]
pub(crate) enum AdmissionStage {
    NativeAuth,
    BrowserAuth,
    JwksLookup,
    JwksStateWait,
    JwksRefreshWait,
    JwksFetch,
    OpenRead,
    Prepare,
    EngineHealthWait,
    EngineLifecycleWait,
    DurableAdmission,
    DurableStart,
    AcceptedEnqueue,
}

impl AdmissionStage {
    fn name(self) -> &'static str {
        match self {
            Self::NativeAuth => "native_auth",
            Self::BrowserAuth => "browser_auth",
            Self::JwksLookup => "jwks_lookup",
            Self::JwksStateWait => "jwks_state_wait",
            Self::JwksRefreshWait => "jwks_refresh_wait",
            Self::JwksFetch => "jwks_fetch",
            Self::OpenRead => "open_read",
            Self::Prepare => "prepare",
            Self::EngineHealthWait => "engine_health_wait",
            Self::EngineLifecycleWait => "engine_lifecycle_wait",
            Self::DurableAdmission => "durable_admission",
            Self::DurableStart => "durable_start",
            Self::AcceptedEnqueue => "accepted_enqueue",
        }
    }
}

pub(crate) struct AdmissionTiming {
    operation: Operation,
    started: std::time::Instant,
}

impl AdmissionTiming {
    pub(crate) fn new(stage: AdmissionStage) -> Self {
        Self {
            operation: Operation::new(
                tracing::info_span!(target: "waltr_server::routine::telemetry", "server.admission.stage", stage = stage.name(), outcome = tracing::field::Empty, duration_ms = tracing::field::Empty),
            ),
            started: std::time::Instant::now(),
        }
    }

    pub(crate) fn result<T, E>(&self, result: &Result<T, E>) {
        self.operation
            .outcome(if result.is_ok() { "success" } else { "failure" });
    }
}

impl Drop for AdmissionTiming {
    fn drop(&mut self) {
        self.operation
            .span
            .record("duration_ms", self.started.elapsed().as_secs_f64() * 1000.0);
    }
}

pub(crate) async fn admission_result<T, E>(
    stage: AdmissionStage,
    future: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, E> {
    use tracing::Instrument as _;
    let timing = AdmissionTiming::new(stage);
    let result = future.instrument(timing.operation.span.clone()).await;
    timing.result(&result);
    result
}

pub(crate) async fn admission_wait<T>(
    stage: AdmissionStage,
    future: impl std::future::Future<Output = T>,
) -> T {
    use tracing::Instrument as _;
    let timing = AdmissionTiming::new(stage);
    let result = future.instrument(timing.operation.span.clone()).await;
    timing.operation.outcome("success");
    result
}

pub(crate) fn admission_sync<T, E>(
    stage: AdmissionStage,
    work: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    let timing = AdmissionTiming::new(stage);
    let result = timing.operation.span.in_scope(work);
    timing.result(&result);
    result
}

pub(crate) fn rpc(metadata: &tonic::metadata::MetadataMap, run: bool) -> Operation {
    let span = if run {
        tracing::info_span!(target: "waltr_server::routine::telemetry", "routine.run", outcome = tracing::field::Empty, grpc_status = tracing::field::Empty)
    } else {
        tracing::info_span!(target: "waltr_server::routine::telemetry", "routine.interact", outcome = tracing::field::Empty, grpc_status = tracing::field::Empty)
    };
    let mut values = metadata.get_all("traceparent").iter();
    let parsed = values
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(parent)
        .filter(|_| values.next().is_none());
    let context = if let Some(parent) = parsed {
        let current = tracing::Span::current().context();
        let current_span = current.span();
        let current = current_span.span_context();
        let chosen = if current.is_valid() && current.trace_id() == parent.trace_id() {
            // Keep middleware identity, but never inherit its remotely supplied tracestate.
            SpanContext::new(
                current.trace_id(),
                current.span_id(),
                current.trace_flags(),
                current.is_remote(),
                TraceState::default(),
            )
        } else {
            parent
        };
        opentelemetry::Context::new().with_remote_span_context(chosen)
    } else {
        // Explicitly root absent or rejected input; SDK extraction cannot bypass our parser.
        opentelemetry::Context::new()
    };
    let _ = span.set_parent(context);
    Operation::new(span)
}

fn parent(value: &str) -> Option<SpanContext> {
    let bytes = value.as_bytes();
    if bytes.len() != 55
        || &bytes[..3] != b"00-"
        || bytes[35] != b'-'
        || bytes[52] != b'-'
        || bytes.iter().enumerate().any(|(index, byte)| {
            !matches!(index, 2 | 35 | 52) && !matches!(byte, b'0'..=b'9' | b'a'..=b'f')
        })
    {
        return None;
    }
    let trace = TraceId::from_hex(&value[3..35]).ok()?;
    let span = SpanId::from_hex(&value[36..52]).ok()?;
    let flags = u8::from_str_radix(&value[53..55], 16).ok()?;
    let parent = SpanContext::new(
        trace,
        span,
        TraceFlags::new(flags & 1),
        true,
        TraceState::default(),
    );
    parent.is_valid().then_some(parent)
}

#[cfg(test)]
pub(crate) fn isolated_exporter_test(name: &str) -> bool {
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    const CHILD: &str = "WALTR_ROUTINE_TRACE_TEST_CHILD";
    match std::env::var(CHILD) {
        Ok(child) => {
            assert_eq!(child, name, "isolated child must run only its exact test");
            return true;
        }
        Err(std::env::VarError::NotPresent) => {}
        Err(error) => panic!("invalid isolated child marker: {error}"),
    }
    fn drain(mut pipe: impl std::io::Read) -> Vec<u8> {
        let mut retained = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let count = pipe.read(&mut buffer).expect("read isolated test output");
            if count == 0 {
                return retained;
            }
            let keep = count.min((64 * 1024_usize).saturating_sub(retained.len()));
            retained.extend_from_slice(&buffer[..keep]);
        }
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--test-threads=1", "--nocapture"])
        .env(CHILD, name)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start isolated telemetry test");
    let stdout = child.stdout.take().expect("child stdout");
    let stderr = child.stderr.take().expect("child stderr");
    let stdout = std::thread::spawn(move || drain(stdout));
    let stderr = std::thread::spawn(move || drain(stderr));
    let started = Instant::now();
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait().expect("poll isolated test") {
            break (status, false);
        }
        if started.elapsed() >= Duration::from_secs(30) {
            let _killed = child.kill();
            break (child.wait().expect("reap timed-out isolated test"), true);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout.join().expect("join stdout reader");
    let stderr = stderr.join().expect("join stderr reader");
    assert!(
        !timed_out && status.success(),
        "{name}: timed_out={timed_out}\n{}\n{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    assert!(
        String::from_utf8_lossy(&stdout)
            .lines()
            .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed;")),
        "isolated telemetry test must execute exactly one successful test: {name}\n{}\n{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_stage_exports_success_failure_and_dropped_wait() {
        if !isolated_exporter_test(
            "routine::telemetry::tests::admission_stage_exports_success_failure_and_dropped_wait",
        ) {
            return;
        }
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
        use tracing_subscriber::layer::SubscriberExt as _;
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
        tracing::subscriber::with_default(subscriber, || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(async {
                    assert_eq!(
                        admission_result(AdmissionStage::NativeAuth, async { Ok::<_, ()>(7) })
                            .await,
                        Ok(7)
                    );
                    assert_eq!(
                        admission_sync(AdmissionStage::Prepare, || Err::<(), _>("SECRET_ERROR")),
                        Err("SECRET_ERROR")
                    );
                    let mutex = tokio::sync::Mutex::new(());
                    let _held = mutex.lock().await;
                    let mut wait = Box::pin(admission_wait(
                        AdmissionStage::JwksRefreshWait,
                        mutex.lock(),
                    ));
                    assert!(futures_util::poll!(wait.as_mut()).is_pending());
                    assert_eq!(exporter.get_finished_spans().unwrap().len(), 2);
                    drop(wait);
                });
        });
        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 3);
        for (stage, outcome) in [
            ("native_auth", "success"),
            ("prepare", "failure"),
            ("jwks_refresh_wait", "incomplete"),
        ] {
            let span = spans
                .iter()
                .find(|span| {
                    span.attributes
                        .iter()
                        .any(|field| field.key.as_str() == "stage" && field.value.as_str() == stage)
                })
                .unwrap();
            assert_eq!(span.name, "server.admission.stage");
            assert!(span.attributes.iter().any(|field| field.key.as_str() == "outcome" && field.value.as_str() == outcome));
            assert!(
                span.attributes
                    .iter()
                    .any(|field| field.key.as_str() == "duration_ms"
                        && field.value.as_str().parse::<f64>().unwrap().is_finite())
            );
            assert!(!format!("{span:?}").contains("SECRET_ERROR"));
        }
        provider.shutdown().unwrap();
    }

    #[test]
    fn rpc_reuses_an_existing_extracted_middleware_parent() {
        if !isolated_exporter_test(
            "routine::telemetry::tests::rpc_reuses_an_existing_extracted_middleware_parent",
        ) {
            return;
        }
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
        use tracing_subscriber::layer::SubscriberExt as _;
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
        tracing::subscriber::with_default(subscriber, || {
            let middleware = tracing::info_span!("test.middleware");
            let parsed = parent("00-0123456789abcdef0123456789abcdef-0123456789abcdef-01").unwrap();
            let parent = SpanContext::new(
                parsed.trace_id(),
                parsed.span_id(),
                parsed.trace_flags(),
                true,
                TraceState::from_key_value([("secret", "value")]).unwrap(),
            );
            middleware
                .set_parent(opentelemetry::Context::new().with_remote_span_context(parent))
                .unwrap();
            middleware.in_scope(|| {
                let mut metadata = tonic::metadata::MetadataMap::new();
                metadata.insert(
                    "traceparent",
                    "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01"
                        .parse()
                        .unwrap(),
                );
                let operation = rpc(&metadata, false);
                operation.outcome("accepted");
                metadata.insert(
                    "traceparent",
                    "00-22222222222222222222222222222222-3333333333333333-01"
                        .parse()
                        .unwrap(),
                );
                let unrelated = rpc(&metadata, true);
                unrelated.outcome("accepted");
            });
        });
        let spans = exporter.get_finished_spans().unwrap();
        let middleware = spans
            .iter()
            .find(|span| span.name == "test.middleware")
            .unwrap();
        let rpc = spans
            .iter()
            .find(|span| span.name == "routine.interact")
            .unwrap();
        assert_eq!(
            middleware.span_context.trace_state().header(),
            "secret=value"
        );
        assert_eq!(rpc.span_context.trace_state().header(), "");
        assert_eq!(
            rpc.span_context.trace_flags(),
            middleware.span_context.trace_flags()
        );
        assert_eq!(rpc.parent_span_id, middleware.span_context.span_id());
        assert_eq!(
            rpc.span_context.trace_id(),
            middleware.span_context.trace_id()
        );
        let unrelated = spans
            .iter()
            .find(|span| span.name == "routine.run")
            .unwrap();
        assert_eq!(
            unrelated.span_context.trace_id().to_string(),
            "22222222222222222222222222222222"
        );
        assert_eq!(unrelated.parent_span_id.to_string(), "3333333333333333");
        provider.shutdown().unwrap();
    }

    #[test]
    fn operation_ends_when_owner_drops_with_retained_tracing_handle() {
        if !isolated_exporter_test(
            "routine::telemetry::tests::operation_ends_when_owner_drops_with_retained_tracing_handle",
        ) {
            return;
        }
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
        use tracing_subscriber::layer::SubscriberExt as _;
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
        tracing::subscriber::with_default(subscriber, || {
            let operation = rpc(&tonic::metadata::MetadataMap::new(), true);
            let retained = operation.span.clone();
            operation.outcome("accepted");
            assert!(exporter.get_finished_spans().unwrap().is_empty());
            drop(operation);
            let spans = exporter.get_finished_spans().unwrap();
            assert_eq!(spans.len(), 1);
            assert_eq!(spans[0].name, "routine.run");
            assert!(spans[0].attributes.iter().any(|field| {
                field.key.as_str() == "outcome" && field.value.as_str() == "accepted"
            }));
            drop(retained);
            assert_eq!(exporter.get_finished_spans().unwrap().len(), 1);
        });
        provider.shutdown().unwrap();
    }

    #[test]
    fn isolated_exporter_rejects_empty_selection() {
        assert!(
            std::panic::catch_unwind(|| isolated_exporter_test(
                "routine::telemetry::tests::missing_test_must_not_pass"
            ))
            .is_err()
        );
    }

    #[test]
    fn isolated_exporter_rejects_failed_child() {
        const NAME: &str = "routine::telemetry::tests::isolated_exporter_rejects_failed_child";
        if std::env::var("WALTR_ROUTINE_TRACE_TEST_CHILD").as_deref() == Ok(NAME) {
            panic!("intentional isolated child failure");
        }
        assert!(std::panic::catch_unwind(|| isolated_exporter_test(NAME)).is_err());
    }

    #[test]
    fn only_valid_version_zero_parents_are_accepted() {
        let valid = "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01";
        assert!(parent(valid).is_some());
        for invalid in [
            "",
            "secret",
            "00-00000000000000000000000000000000-0123456789abcdef-01",
            "00-0123456789abcdef0123456789abcdef-0000000000000000-01",
            "01-0123456789abcdef0123456789abcdef-0123456789abcdef-01",
            "00-0123456789ABCDEF0123456789abcdef-0123456789abcdef-01",
        ] {
            assert!(parent(invalid).is_none());
        }
    }
}
