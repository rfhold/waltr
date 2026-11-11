use super::*;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use tracing_subscriber::layer::SubscriberExt as _;

struct PanicHost;
impl HostDispatcher for PanicHost {
    fn dispatch(&self, _: HostRequest, _: HostContext) -> HostFuture {
        panic!("SECRET_HOST_PANIC")
    }
    fn cleanup(&self, _: InvocationId) -> CleanupFuture {
        Box::pin(async { Ok(()) })
    }
}

#[test]
fn diagnostics_host_early_returns_and_panics_are_classified() {
    if !crate::routine::telemetry::isolated_exporter_test(
        "routine::service::tests::diagnostics::diagnostics_host_early_returns_and_panics_are_classified",
    ) {
        return;
    }
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
    tracing::subscriber::with_default(subscriber, || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let f = Fixture::new().await;
                let waiting = f.invoke("wait").await;
                f.flags.entered.notified().await;
                let trace = tracing::info_span!("test.parent");
                for (cancelled, failed, method, category, outcome) in [
                    (true, false, "echo", "cancelled", "cancelled"),
                    (false, true, "echo", "cleanup", "uncertain"),
                    (false, false, "SECRET_METHOD", "denied", "failure"),
                    (false, false, "echo", "panic", "failure"),
                ] {
                    let cancel = CancellationToken::new();
                    if cancelled {
                        cancel.cancel();
                    }
                    let host = JournalHost {
                        trace: trace.clone(),
                        id: waiting.id.clone(),
                        store: f.store.clone(),
                        clock: Arc::new(SystemClock),
                        inner: Arc::new(PanicHost),
                        policy: Arc::new(TestPolicy),
                        failed: Arc::new(AtomicBool::new(failed)),
                        cancel,
                    };
                    let context = HostContext::new(Limits::default()).unwrap();
                    let request = HostRequest {
                        operation: method.into(),
                        arguments: StructuredValue::new(
                            Value::String("SECRET_ARGUMENT".into()),
                            &Limits::default(),
                        )
                        .unwrap(),
                        bytes: None,
                        callback: None,
                        handle: None,
                    };
                    let result = AssertUnwindSafe(host.dispatch(request, context.clone()))
                        .catch_unwind()
                        .await;
                    assert!(result.is_err() || result.unwrap().is_err());
                    context.close().unwrap();
                    let spans = exporter.get_finished_spans().unwrap();
                    let span = spans
                        .iter()
                        .rev()
                        .find(|span| span.name == "routine.host_call")
                        .unwrap();
                    for (key, value) in [
                        ("error_category", category),
                        ("outcome", outcome),
                        ("host_method", "unknown"),
                    ] {
                        assert!(
                            span.attributes
                                .iter()
                                .any(|field| field.key.as_str() == key
                                    && field.value.as_str() == value),
                            "missing {key}={value}: {span:?}"
                        );
                    }
                    assert!(!format!("{span:?}").contains("SECRET_"));
                }
                waiting.cancel();
                waiting.join().await;
                f.service.shutdown().await.unwrap();
            });
    });
    provider.shutdown().unwrap();
}
