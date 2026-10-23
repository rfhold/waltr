#![allow(
    clippy::significant_drop_tightening,
    reason = "tokio::join consumes each job and its permit in the owned future"
)]
use super::*;
use waltr_desktop::{capture::CaptureWorker, host::Stage};

struct Grant;
impl PermissionBackend for Grant {
    fn request(&self, _ticket: u64, reply: PermissionReply) {
        reply.complete(PermissionResult::Granted);
    }
}
#[derive(Default)]
struct Deferred(Mutex<Option<PermissionReply>>);
impl PermissionBackend for Deferred {
    fn request(&self, _ticket: u64, reply: PermissionReply) {
        *self.0.lock().unwrap() = Some(reply);
    }
}
fn selection(name: &str) -> Selection {
    Selection {
        routine: name.into(),
        trigger: "dictate".into(),
        input: "audio".into(),
        output: "clean-text".into(),
    }
}
fn binding() -> Arc<waltr_desktop::config::Keybind> {
    let mut config = waltr_desktop::config::parse(
        br"version: 1
keybinds:
 - id: test
   key: Ctrl+Alt+Space
   mode: hold
   routine: { name: test, trigger: dictate, input: audio, output: clean-text }
   audio: { device: default, sampleRateHz: 24000, channels: 1 }
   command: { argv: [/usr/bin/true], stdin: '{{ text }}', timeoutSeconds: 10 }
",
    )
    .unwrap();
    Arc::new(config.keybinds.remove(0))
}
struct NeverTyping;
impl waltr_desktop::typing::Factory for NeverTyping {
    fn create(
        &self,
    ) -> Result<Box<dyn waltr_desktop::typing::Session>, waltr_desktop::typing::Error> {
        panic!("command bindings must not construct typing sessions")
    }
}

fn host() -> Arc<Host> {
    let host = Arc::new(Host::default());
    host.bind(1);
    host.access(true);
    host
}
fn job(host: &Arc<Host>, operation: u64, name: &str) -> Job {
    host.start(1, operation, selection(name), "local-draft")
        .unwrap()
        .1
        .unwrap()
}
fn phase(host: &Host, operation: u64) -> String {
    host.control(1, operation, "status").unwrap().phase.into()
}
#[allow(
    clippy::future_not_send,
    reason = "synthetic local predicates need not be Send"
)]
async fn until(condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[test]
fn native_audio_lifecycle_telemetry_uses_only_fixed_categories() {
    if !crate::native_audio_telemetry::isolated_exporter_test(
        "dictation::tests::cases::native_audio_lifecycle_telemetry_uses_only_fixed_categories",
    ) {
        return;
    }
    use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
    use opentelemetry_sdk::logs::{InMemoryLogExporter, SdkLoggerProvider};
    use tracing_subscriber::prelude::*;
    struct Deny;
    impl PermissionBackend for Deny {
        fn request(&self, _ticket: u64, reply: PermissionReply) {
            reply.complete(PermissionResult::Denied);
        }
    }
    struct SecretDevice;
    impl Factory for SecretDevice {
        fn open(&self, _config: waltr_audio_io::Config) -> Result<Box<dyn Source>, capture::Error> {
            Ok(Box::new(SecretDevice))
        }
    }
    impl Source for SecretDevice {
        fn frame(&mut self) -> Option<Vec<u8>> {
            None
        }
        fn terminal(&self) -> Terminal {
            Terminal::Failed(waltr_audio_io::Error::Backend(
                "SECRET_BACKEND_SENTINEL".into(),
            ))
        }
        fn stop(&self) {}
        fn shutdown(self: Box<Self>) {}
    }
    let logs = InMemoryLogExporter::default();
    let logger = SdkLoggerProvider::builder()
        .with_simple_exporter(logs.clone())
        .build();
    let spans = opentelemetry_sdk::trace::InMemorySpanExporter::default();
    let tracer = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_simple_exporter(spans.clone())
        .build();
    use opentelemetry::trace::TracerProvider as _;
    let subscriber = tracing_subscriber::registry()
        .with(
            tracing_opentelemetry::layer()
                .with_tracer(tracer.tracer("test"))
                .with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
                    *metadata.level() <= tracing::Level::INFO
                        && (metadata.target().starts_with("waltr_app_lib::")
                            || metadata.target() == "waltr_client::transport::native_trace")
                })),
        )
        .with(OpenTelemetryTracingBridge::new(&logger).with_filter(
            tracing_subscriber::filter::filter_fn(|metadata| {
                *metadata.level() <= tracing::Level::INFO
                    && metadata.target().starts_with("waltr_app_lib::")
            }),
        ));
    tracing::subscriber::with_default(subscriber, || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let (transport, _fake, stop, server) = fixture().await;
                for case in 0..9 {
                    let host = host();
                    let backend: Arc<dyn PermissionBackend> = if case == 3 {
                        Arc::new(Deny)
                    } else {
                        Arc::new(Grant)
                    };
                    let permissions = AudioPermissions::new(1, true, backend);
                    let source = Arc::new(Mock::default());
                    source.burst.store(case == 1, Ordering::SeqCst);
                    source.empty.store(case == 8, Ordering::SeqCst);
                    let name = match case {
                        1 => "pending",
                        2 => "SECRET_CONFIG_SENTINEL_reject",
                        5 => "remote-failed",
                        6 => "remote-interrupted",
                        7 => "remote-rejected",
                        _ => "SECRET_CONFIG_SENTINEL",
                    };
                    let operation = job(&host, 1, name);
                    if case == 0 || case == 8 {
                        host.control(1, 1, "finish").unwrap();
                    }
                    if case == 4 {
                        host.control(1, 1, "cancel").unwrap();
                    }
                    operation
                        .run(
                            &transport,
                            &permissions,
                            Arc::new(MockFactory(source.clone())),
                        )
                        .await;
                    assert!(host.audio.gate.reserve().is_ok());
                    if case < 3 {
                        assert!(source.joined.load(Ordering::SeqCst));
                    }
                }
                let diagnostics = Operation::new("hold");
                let config = capture::native_config(&waltr_desktop::config::Audio {
                    device: waltr_desktop::config::DeviceSelector::Name(
                        "SECRET_DEVICE_SENTINEL".into(),
                    ),
                    sample_rate_hz: 24_000,
                    channels: 1,
                })
                .unwrap();
                diagnostics.stage(Stage::CaptureRequested);
                let mut worker = CaptureWorker::start_observed(
                    Arc::new(SecretDevice),
                    config,
                    diagnostics.observer(),
                );
                let result = worker.join().await;
                diagnostics.primary(result.map_err(waltr_desktop::host::Error::Capture));
                diagnostics.capture_cleanup(result);
                diagnostics.finish();
                for error in [
                    capture::Error::CallbackPoolExhausted,
                    capture::Error::CallbackQueueFull,
                ] {
                    let diagnostics = Operation::new("hold");
                    diagnostics.primary(Err(waltr_desktop::host::Error::Capture(error)));
                    diagnostics.capture_cleanup(Err(error));
                    diagnostics.finish();
                }
                stop.cancel();
                server.await.unwrap();
            });
    });
    logger.force_flush().unwrap();
    let emitted = logs.get_emitted_logs().unwrap();
    tracer.force_flush().unwrap();
    let exported = spans.get_finished_spans().unwrap();
    let operations: Vec<_> = exported
        .iter()
        .filter(|span| span.name == "native.audio.operation")
        .collect();
    assert_eq!(operations.len(), 12);
    for category in [
        "capture_callback_pool_exhausted",
        "capture_callback_queue_full",
    ] {
        assert!(
            operations
                .iter()
                .any(|span| span.attributes.iter().any(|attribute| {
                    attribute.key.as_str() == "category" && attribute.value.to_string() == category
                }))
        );
        assert!(
            emitted
                .iter()
                .any(|log| log.record.attributes_iter().any(|(key, value)| {
                    key.as_str() == "category" && format!("{value:?}").contains(category)
                }))
        );
    }
    let no_input = operations
        .iter()
        .find(|span| {
            span.attributes.iter().any(|attribute| {
                attribute.key.as_str() == "category" && attribute.value.to_string() == "no_input"
            })
        })
        .expect("clean zero-input operation");
    assert!(
        no_input
            .attributes
            .iter()
            .any(|attribute| attribute.key.as_str() == "outcome"
                && attribute.value.to_string() == "no_input")
    );
    assert!(
        !exported
            .iter()
            .any(|span| span.parent_span_id == no_input.span_context.span_id())
    );
    let rpc: Vec<_> = exported
        .iter()
        .filter(|span| {
            span.attributes
                .iter()
                .any(|attribute| attribute.key.as_str() == "rpc.method")
        })
        .collect();
    assert!(!rpc.is_empty());
    for child in rpc {
        assert!(operations.iter().any(|parent| child.parent_span_id
            == parent.span_context.span_id()
            && child.span_context.trace_id() == parent.span_context.trace_id()));
    }
    let diagnostic = format!("{emitted:?} {exported:?}");
    for expected in [
        "succeeded",
        "capture_host_overflow",
        "rpc_open",
        "rpc_remote_failed",
        "rpc_remote_interrupted",
        "rpc_rejected",
        "permission_denied",
        "cancelled",
        "capture_ready",
    ] {
        assert!(diagnostic.contains(expected), "missing {expected}");
    }
    for secret in [
        "SECRET_CONFIG_SENTINEL",
        "SECRET_TRANSCRIPT_SENTINEL",
        "SECRET_ERROR_SENTINEL",
        "SECRET_DEVICE_SENTINEL",
        "SECRET_BACKEND_SENTINEL",
        "SECRET_INVOCATION_SENTINEL",
        "SECRET_CODE_SENTINEL",
        "SECRET_MESSAGE_SENTINEL",
    ] {
        assert!(!diagnostic.contains(secret));
    }
    assert_eq!(
        emitted
            .iter()
            .filter(|log| format!("{log:?}").contains("native.audio.finished"))
            .count(),
        12
    );
    drop(logger.shutdown());
    drop(tracer.shutdown());
}

#[test]
fn preview_publication_is_identity_access_cancel_and_credential_fenced() {
    for action in [
        "cancel",
        "access",
        "credentials",
        "error",
        "generation",
        "operation",
    ] {
        let host = host();
        let credentials = waltr_client::NativeCredentialProvider::new();
        host.credentials(credentials.clone());
        let job = job(&host, 1, "test");
        job.preview("owned".into());
        assert_eq!(host.control(1, 1, "status").unwrap().preview, "owned");
        match action {
            "cancel" => {
                host.control(1, 1, "cancel").unwrap();
            }
            "access" => host.access(false),
            "credentials" => {
                let _lease = credentials.clear_bearer_token();
                host.check_credentials();
            }
            "error" => host.publish(1, 1, "error", None),
            "generation" => host.bind(2),
            "operation" => {
                host.control(1, 2, "cancel").unwrap();
            }
            _ => unreachable!(),
        }
        job.preview("stale".into());
        job.publish_current("succeeded", Some("stale final".into()));
        let state = host.lock();
        assert!(
            state
                .latest
                .as_ref()
                .is_none_or(|record| record.status.final_text.is_none())
        );
        assert!(
            state
                .latest
                .as_ref()
                .is_none_or(|record| record.status.preview.is_empty())
        );
    }
}
#[test]
fn final_publication_checks_credential_revision_without_monitor_poll() {
    let host = host();
    let credentials = waltr_client::NativeCredentialProvider::new();
    host.credentials(credentials.clone());
    let job = job(&host, 1, "test");
    let _lease = credentials.clear_bearer_token();
    job.publish_current("succeeded", Some("must not publish".into()));
    let status = host.control(1, 1, "status").unwrap();
    assert_eq!(status.phase, "starting");
    assert!(status.final_text.is_none());
}

#[test]
fn cancel_before_start_is_bounded_monotonic_tombstone() {
    let host = host();
    assert_eq!(host.control(1, 42, "cancel").unwrap().phase, "cancelled");
    assert!(
        host.start(1, 42, selection("test"), "draft")
            .unwrap()
            .1
            .is_none()
    );
    assert!(host.start(1, 41, selection("test"), "draft").is_err());
    assert!(
        host.start(1, MAX_ID + 1, selection("test"), "draft")
            .is_err()
    );
    assert!(host.control(0, 1, "cancel").is_err());
    assert!(host.control(1, 0, "status").is_err());
    host.bind(2);
    host.access(true);
    assert!(host.control(1, 42, "status").is_err());
    assert!(
        host.start(2, 1, selection("test"), "draft")
            .unwrap()
            .1
            .is_some()
    );
}
#[tokio::test]
async fn permission_failures_flash_for_chat_and_holds_without_capture() {
    struct Deny(PermissionResult);
    impl PermissionBackend for Deny {
        fn request(&self, _ticket: u64, reply: PermissionReply) {
            reply.complete(self.0);
        }
    }
    let (transport, _fake, stop, server) = fixture().await;
    for result in [PermissionResult::Denied, PermissionResult::Unavailable] {
        for held in [false, true] {
            let host = host();
            let permission = AudioPermissions::new(1, true, Arc::new(Deny(result)));
            let mock = Arc::new(Mock::default());
            let factory = Arc::new(MockFactory(mock.clone()));
            if held {
                host.audio
                    .hold(binding())
                    .unwrap()
                    .0
                    .run(&transport, &permission, factory)
                    .await;
            } else {
                job(&host, 1, "test")
                    .run(&transport, &permission, factory)
                    .await;
            }
            assert!(!mock.started.load(Ordering::SeqCst));
            assert!(host.audio.gate.reserve().is_ok());
            assert_eq!(
                host.audio.activity.phase(std::time::Instant::now()),
                crate::activity::Phase::Error
            );
        }
    }
    stop.cancel();
    server.await.unwrap();
}
#[test]
fn gate_is_shared_with_hold_and_busy_ids_cannot_retry() {
    let host = host();
    let chat = job(&host, 1, "test");
    assert!(host.audio.hold(binding()).is_err());
    assert!(host.start(1, 2, selection("test"), "draft").is_err());
    drop(chat);
    assert_eq!(
        host.audio.activity.phase(std::time::Instant::now()),
        crate::activity::Phase::Idle
    );
    assert!(host.start(1, 2, selection("test"), "draft").is_err());
    let hold = host.audio.hold(binding()).unwrap();
    assert!(host.start(1, 3, selection("test"), "draft").is_err());
    drop(hold);
    assert!(
        host.start(1, 4, selection("test"), "draft")
            .unwrap()
            .1
            .is_some()
    );
}
#[test]
fn selection_and_status_serialization_are_bounded_and_secret_free() {
    let host = host();
    assert!(host.start(1, 1, selection(""), "draft").is_err());
    assert!(
        host.start(1, 1, selection(&"x".repeat(257)), "draft")
            .is_err()
    );
    assert!(host.start(1, 1, selection("test"), "").is_err());
    let (status, job) = host.start(1, 1, selection("test"), "draft").unwrap();
    let json = serde_json::to_value(status).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"generation":1,"operationId":1,"phase":"starting","preview":""})
    );
    drop(job);
}
#[tokio::test]
async fn live_preview_revisions_and_commit_precede_release_but_final_requires_clean_eof() {
    let (transport, fake, stop, server) = fixture().await;
    let host = host();
    let job = job(&host, 1, "live");
    let mock = Arc::new(Mock::default());
    let permission = AudioPermissions::new(1, true, Arc::new(Grant));
    let drive = async {
        until(|| host.control(1, 1, "status").unwrap().preview == "hello").await;
        assert!(host.control(1, 1, "status").unwrap().final_text.is_none());
        fake.preview_step.store(1, Ordering::SeqCst);
        until(|| host.control(1, 1, "status").unwrap().preview == "hello world").await;
        fake.preview_step.store(2, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!mock.stopped.load(Ordering::SeqCst));
        assert!(host.control(1, 1, "status").unwrap().final_text.is_none());
        host.control(1, 1, "finish").unwrap();
    };
    tokio::join!(
        job.run(&transport, &permission, Arc::new(MockFactory(mock.clone()))),
        drive
    );
    let status = host.control(1, 1, "status").unwrap();
    assert_eq!(status.phase, "succeeded");
    assert_eq!(status.final_text.as_deref(), Some("hello world"));
    assert!(status.preview.is_empty());
    assert!(mock.joined.load(Ordering::SeqCst));
    stop.cancel();
    server.await.unwrap();
}
#[tokio::test]
async fn accepted_capture_finish_final_text_and_clean_join() {
    let (transport, fake, stop, server) = fixture().await;
    let host = host();
    let job = job(&host, 1, "test");
    let mock = Arc::new(Mock::default());
    let permission = AudioPermissions::new(1, true, Arc::new(Grant));
    let drive = async {
        until(|| phase(&host, 1) == "recording").await;
        assert_eq!(fake.uploads.load(Ordering::SeqCst), 0);
        host.control(1, 1, "finish").unwrap();
        host.control(1, 1, "finish").unwrap();
    };
    tokio::join!(
        job.run(&transport, &permission, Arc::new(MockFactory(mock.clone()))),
        drive
    );
    let result = host.control(1, 1, "status").unwrap();
    assert_eq!(result.phase, "succeeded");
    assert_eq!(
        result.final_text.as_deref(),
        Some("literal $(touch nope); '{{ invocationId }}'")
    );
    assert_eq!(fake.uploads.load(Ordering::SeqCst), 100);
    assert!(mock.joined.load(Ordering::SeqCst));
    assert!(host.audio.gate.reserve().is_ok());
    stop.cancel();
    server.await.unwrap();
}
#[tokio::test]
async fn failed_uncommitted_incomplete_and_oversized_output_never_publish_final() {
    let (transport, _fake, stop, server) = fixture().await;
    for name in ["failed", "uncommitted", "incomplete", "large", "ambiguous"] {
        let host = host();
        let job = job(&host, 1, name);
        host.control(1, 1, "finish").unwrap();
        let permission = AudioPermissions::new(1, true, Arc::new(Grant));
        job.run(
            &transport,
            &permission,
            Arc::new(MockFactory(Arc::new(Mock::default()))),
        )
        .await;
        let status = host.control(1, 1, "status").unwrap();
        assert_eq!(status.phase, "error");
        assert!(status.preview.is_empty());
        assert!(status.final_text.is_none());
    }
    stop.cancel();
    server.await.unwrap();
}
#[tokio::test]
async fn another_output_port_does_not_make_selected_text_ambiguous() {
    let (transport, _fake, stop, server) = fixture().await;
    let host = host();
    let job = job(&host, 1, "other-port");
    host.control(1, 1, "finish").unwrap();
    job.run(
        &transport,
        &AudioPermissions::new(1, true, Arc::new(Grant)),
        Arc::new(MockFactory(Arc::new(Mock::default()))),
    )
    .await;
    assert_eq!(phase(&host, 1), "succeeded");
    stop.cancel();
    server.await.unwrap();
}
#[tokio::test]
async fn cancel_pending_permission_does_not_capture_or_wait_for_os_dialog() {
    let (transport, _fake, stop, server) = fixture().await;
    let host = host();
    let job = job(&host, 1, "test");
    let deferred = Arc::new(Deferred::default());
    let permission = AudioPermissions::new(1, true, deferred.clone());
    let mock = Arc::new(Mock::default());
    let cancel = async {
        until(|| deferred.0.lock().unwrap().is_some()).await;
        assert_eq!(
            host.audio.activity.phase(std::time::Instant::now()),
            crate::activity::Phase::Working
        );
        host.control(1, 1, "cancel").unwrap();
    };
    tokio::join!(
        job.run(&transport, &permission, Arc::new(MockFactory(mock.clone()))),
        cancel
    );
    assert_eq!(phase(&host, 1), "cancelled");
    assert!(!mock.started.load(Ordering::SeqCst));
    deferred
        .0
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .complete(PermissionResult::Granted);
    assert!(!mock.started.load(Ordering::SeqCst));
    assert!(host.audio.gate.reserve().is_ok());
    stop.cancel();
    server.await.unwrap();
}
#[tokio::test]
async fn zero_callbacks_or_empty_frames_on_permission_release_never_open_rpc() {
    for zero_bytes in [false, true] {
        let (transport, fake, stop, server) = fixture().await;
        let host = host();
        let task = job(&host, 1, "test");
        let deferred = Arc::new(Deferred::default());
        let permission = AudioPermissions::new(1, true, deferred.clone());
        let mock = Arc::new(Mock {
            empty: AtomicBool::new(!zero_bytes),
            zero_bytes: AtomicBool::new(zero_bytes),
            ..Default::default()
        });
        let release = async {
            until(|| deferred.0.lock().unwrap().is_some()).await;
            host.control(1, 1, "finish").unwrap();
            deferred
                .0
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .complete(PermissionResult::Granted);
        };
        tokio::join!(
            task.run(&transport, &permission, Arc::new(MockFactory(mock.clone()))),
            release
        );
        assert_eq!(phase(&host, 1), "succeeded");
        assert!(host.control(1, 1, "status").unwrap().final_text.is_none());
        assert_eq!(fake.admissions.load(Ordering::SeqCst), 0);
        assert_eq!(fake.uploads.load(Ordering::SeqCst), 0);
        assert!(mock.joined.load(Ordering::SeqCst));
        assert_eq!(
            host.audio.activity.phase(std::time::Instant::now()),
            crate::activity::Phase::Idle
        );
        // Admission is reusable and the ordinary successful path still publishes text.
        let next = job(&host, 2, "test");
        let permission = AudioPermissions::new(1, true, Arc::new(Grant));
        host.control(1, 2, "finish").unwrap();
        next.run(
            &transport,
            &permission,
            Arc::new(MockFactory(Arc::new(Mock::default()))),
        )
        .await;
        assert!(host.control(1, 2, "status").unwrap().final_text.is_some());
        stop.cancel();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn release_preserves_nonzero_converter_tail_before_first_rpc() {
    let (transport, fake, stop, server) = fixture().await;
    let host = host();
    let task = job(&host, 1, "test");
    let permission = AudioPermissions::new(1, true, Arc::new(Grant));
    let mut converter = waltr_audio_io::Converter::new(
        waltr_audio_io::Config::default(),
        waltr_audio_io::InputFormat {
            rate: 48_000,
            channels: 1,
        },
    )
    .unwrap();
    assert!(
        converter
            .push(waltr_audio_io::Samples::I16(&[1234; 64]))
            .unwrap()
            .is_empty()
    );
    let tail = converter.finish().unwrap();
    assert_eq!(tail.len(), 64);
    let mock = Arc::new(Mock {
        tail_only: AtomicBool::new(true),
        tail: Mutex::new(Some(tail)),
        ..Default::default()
    });
    let release = async {
        until(|| mock.started.load(Ordering::SeqCst)).await;
        assert_eq!(fake.admissions.load(Ordering::SeqCst), 0);
        host.control(1, 1, "finish").unwrap();
    };
    tokio::join!(
        task.run(&transport, &permission, Arc::new(MockFactory(mock.clone()))),
        release
    );
    assert_eq!(phase(&host, 1), "succeeded");
    assert_eq!(fake.uploads.load(Ordering::SeqCst), 64);
    assert!(host.control(1, 1, "status").unwrap().final_text.is_some());
    assert!(mock.joined.load(Ordering::SeqCst));
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn startup_release_zero_capture_is_clean_but_zero_capture_failure_is_not() {
    for failure in [None, Some(waltr_audio_io::Error::CallbackOverflow)] {
        let (transport, fake, stop, server) = fixture().await;
        let host = host();
        let job = job(&host, 1, "test");
        let permission = AudioPermissions::new(1, true, Arc::new(Grant));
        let (tx, rx) = std::sync::mpsc::channel();
        let mock = Arc::new(Mock {
            empty: AtomicBool::new(true),
            startup: Mutex::new(Some(rx)),
            failure: Mutex::new(failure.clone()),
            ..Default::default()
        });
        let release = async {
            until(|| mock.started.load(Ordering::SeqCst)).await;
            assert_eq!(fake.admissions.load(Ordering::SeqCst), 0);
            host.control(1, 1, "finish").unwrap();
            tx.send(()).unwrap();
        };
        tokio::join!(
            job.run(&transport, &permission, Arc::new(MockFactory(mock.clone()))),
            release
        );
        assert_eq!(
            phase(&host, 1),
            if failure.is_some() {
                "error"
            } else {
                "succeeded"
            }
        );
        assert!(host.control(1, 1, "status").unwrap().final_text.is_none());
        assert_eq!(fake.admissions.load(Ordering::SeqCst), 0);
        assert!(mock.joined.load(Ordering::SeqCst));
        assert_eq!(
            host.audio.activity.phase(std::time::Instant::now()),
            if failure.is_some() {
                crate::activity::Phase::Error
            } else {
                crate::activity::Phase::Idle
            }
        );
        stop.cancel();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn release_during_permission_is_retained_without_automatic_cancel() {
    let (transport, _fake, stop, server) = fixture().await;
    let host = host();
    let job = job(&host, 1, "test");
    let deferred = Arc::new(Deferred::default());
    let permission = AudioPermissions::new(1, true, deferred.clone());
    let mock = Arc::new(Mock::default());
    let release = async {
        until(|| deferred.0.lock().unwrap().is_some()).await;
        host.control(1, 1, "finish").unwrap();
        assert!(!mock.started.load(Ordering::SeqCst));
        deferred
            .0
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .complete(PermissionResult::Granted);
    };
    tokio::join!(
        job.run(&transport, &permission, Arc::new(MockFactory(mock.clone()))),
        release
    );
    assert_eq!(phase(&host, 1), "succeeded");
    assert!(mock.stopped.load(Ordering::SeqCst));
    assert!(mock.joined.load(Ordering::SeqCst));
    stop.cancel();
    server.await.unwrap();
}
#[tokio::test]
async fn cancellation_retains_permit_until_blocked_late_start_worker_is_joined() {
    let (transport, _fake, stop, server) = fixture().await;
    let host = host();
    let job = job(&host, 1, "pending");
    let (tx, rx) = std::sync::mpsc::channel();
    let mock = Arc::new(Mock {
        startup: Mutex::new(Some(rx)),
        ..Default::default()
    });
    let permission = AudioPermissions::new(1, true, Arc::new(Grant));
    let cancel = async {
        until(|| mock.started.load(Ordering::SeqCst)).await;
        host.cancel_all();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(host.audio.gate.reserve().is_err());
        assert!(!mock.joined.load(Ordering::SeqCst));
        assert_eq!(
            host.audio.activity.phase(std::time::Instant::now()),
            crate::activity::Phase::Working
        );
        tx.send(()).unwrap();
    };
    tokio::join!(
        job.run(&transport, &permission, Arc::new(MockFactory(mock.clone()))),
        cancel
    );
    assert!(mock.joined.load(Ordering::SeqCst));
    assert_eq!(phase(&host, 1), "cancelled");
    assert_eq!(
        host.audio.activity.phase(std::time::Instant::now()),
        crate::activity::Phase::Idle
    );
    assert!(host.audio.gate.reserve().is_ok());
    stop.cancel();
    server.await.unwrap();
}
#[tokio::test]
async fn early_successful_eof_cancels_chat_capture_without_finish_or_final_publication() {
    let (transport, _, stop, server) = fixture().await;
    let host = host();
    let job = job(&host, 1, "early-success");
    let mock = Arc::new(Mock::default());
    tokio::time::timeout(
        Duration::from_secs(2),
        job.run(
            &transport,
            &AudioPermissions::new(1, true, Arc::new(Grant)),
            Arc::new(MockFactory(mock.clone())),
        ),
    )
    .await
    .unwrap();
    let status = host.control(1, 1, "status").unwrap();
    assert_eq!(status.phase, "error");
    assert!(status.preview.is_empty());
    assert!(status.final_text.is_none());
    assert!(mock.joined.load(Ordering::SeqCst));
    assert!(host.audio.gate.reserve().is_ok());
    assert_eq!(
        host.audio.activity.phase(std::time::Instant::now()),
        crate::activity::Phase::Error
    );
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn cancelled_chat_retains_cleanup_failure_and_admission_until_join() {
    let (transport, fake, stop, server) = fixture().await;
    let host = host();
    let job = job(&host, 1, "pending");
    let (allow, blocked) = std::sync::mpsc::channel();
    let closing = Arc::new(AtomicBool::new(false));
    let entered = closing.clone();
    let mock = Arc::new(Mock::default());
    *mock.shutdown.lock().unwrap() = Some(Box::new(move || {
        entered.store(true, Ordering::SeqCst);
        blocked.recv().unwrap();
        panic!("synthetic capture cleanup failure");
    }));
    let permission = AudioPermissions::new(1, true, Arc::new(Grant));
    let cancel = async {
        until(|| fake.admissions.load(Ordering::SeqCst) == 1).await;
        host.control(1, 1, "cancel").unwrap();
        until(|| closing.load(Ordering::SeqCst)).await;
        assert!(host.audio.gate.reserve().is_err());
        assert_eq!(
            host.audio.activity.phase(std::time::Instant::now()),
            crate::activity::Phase::Working
        );
        allow.send(()).unwrap();
    };
    tokio::join!(
        job.run(&transport, &permission, Arc::new(MockFactory(mock))),
        cancel
    );
    let status = host.control(1, 1, "status").unwrap();
    assert_eq!(status.phase, "cancelled");
    assert!(status.preview.is_empty());
    assert!(status.final_text.is_none());
    assert_eq!(
        host.audio.activity.phase(std::time::Instant::now()),
        crate::activity::Phase::Error
    );
    assert!(host.audio.gate.reserve().is_ok());
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn replacement_and_access_loss_suppress_late_success() {
    let (transport, fake, stop, server) = fixture().await;
    for replace in [false, true] {
        fake.completed.store(false, Ordering::SeqCst);
        let host = host();
        let job = job(&host, 1, "test");
        host.control(1, 1, "finish").unwrap();
        let permission = AudioPermissions::new(1, true, Arc::new(Grant));
        let invalidate = async {
            until(|| fake.completed.load(Ordering::SeqCst)).await;
            if replace {
                host.bind(2);
            } else {
                host.cancel_all();
            }
        };
        let mock = Arc::new(Mock::default());
        tokio::join!(
            job.run(&transport, &permission, Arc::new(MockFactory(mock.clone()))),
            invalidate
        );
        assert!(mock.joined.load(Ordering::SeqCst));
        if replace {
            assert!(host.control(1, 1, "status").is_err());
        } else {
            assert_eq!(phase(&host, 1), "cancelled");
        }
    }
    stop.cancel();
    server.await.unwrap();
}
#[tokio::test]
async fn credential_revision_changed_during_permission_fails_before_capture() {
    let (transport, _fake, stop, server) = fixture().await;
    let host = host();
    let credentials = waltr_client::NativeCredentialProvider::new();
    host.credentials(credentials.clone());
    let job = job(&host, 1, "test");
    let deferred = Arc::new(Deferred::default());
    let permission = AudioPermissions::new(1, true, deferred.clone());
    let mock = Arc::new(Mock::default());
    let mutate = async {
        until(|| deferred.0.lock().unwrap().is_some()).await;
        let _lease = credentials.clear_bearer_token();
        deferred
            .0
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .complete(PermissionResult::Granted);
    };
    tokio::join!(
        job.run(&transport, &permission, Arc::new(MockFactory(mock.clone()))),
        mutate
    );
    assert!(!mock.started.load(Ordering::SeqCst));
    assert!(host.audio.gate.reserve().is_ok());
    stop.cancel();
    server.await.unwrap();
}
#[tokio::test]
async fn foreground_pause_cancels_audio_without_retiring_authenticated_generation_or_auto_resume() {
    let (transport, _fake, stop, server) = fixture().await;
    let host = host();
    let job = job(&host, 1, "pending");
    let permission = AudioPermissions::new(1, true, Arc::new(Grant));
    let mock = Arc::new(Mock::default());
    let pause = async {
        until(|| mock.started.load(Ordering::SeqCst)).await;
        permission.pause();
        host.cancel_all();
        permission.resume();
    };
    tokio::join!(
        job.run(&transport, &permission, Arc::new(MockFactory(mock.clone()))),
        pause
    );
    assert_eq!(phase(&host, 1), "cancelled");
    assert!(mock.joined.load(Ordering::SeqCst));
    assert_eq!(host.audio.lock().generation, 1);
    assert!(host.audio.lock().allowed);
    assert!(host.audio.gate.reserve().is_ok());
    assert!(
        host.start(1, 1, selection("test"), "draft")
            .unwrap()
            .1
            .is_none()
    );
    assert!(
        host.start(1, 2, selection("test"), "draft")
            .unwrap()
            .1
            .is_some()
    );
    stop.cancel();
    server.await.unwrap();
}
#[tokio::test]
async fn hold_keyup_during_permission_and_capture_start_finishes_the_pinned_command() {
    let (transport, fake, stop, server) = fixture().await;
    let host = host();
    let (held, control) = host.audio.hold(binding()).unwrap();
    let status = control.status();
    let deferred = Arc::new(Deferred::default());
    let permission = AudioPermissions::new(1, true, deferred.clone());
    let (tx, rx) = std::sync::mpsc::channel();
    let mock = Arc::new(Mock {
        startup: Mutex::new(Some(rx)),
        ..Default::default()
    });
    let release = async {
        until(|| deferred.0.lock().unwrap().is_some()).await;
        control.release();
        deferred
            .0
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .complete(PermissionResult::Granted);
        until(|| mock.started.load(Ordering::SeqCst)).await;
        control.release();
        assert!(host.audio.gate.reserve().is_err());
        tx.send(()).unwrap();
    };
    tokio::join!(
        held.run_with_typing(
            &transport,
            &permission,
            Arc::new(MockFactory(mock.clone())),
            &NeverTyping
        ),
        release
    );
    assert_eq!(
        *status.borrow(),
        waltr_desktop::host::Status::Finished(Ok(()))
    );
    assert_eq!(fake.uploads.load(Ordering::SeqCst), 100);
    assert!(mock.joined.load(Ordering::SeqCst));
    assert!(host.audio.gate.reserve().is_ok());
    stop.cancel();
    server.await.unwrap();
}
#[tokio::test]
async fn hold_backend_loss_cancels_permission_and_never_opens_capture() {
    let (transport, _fake, stop, server) = fixture().await;
    let host = host();
    let (held, control) = host.audio.hold(binding()).unwrap();
    let token = held.token.clone();
    let deferred = Arc::new(Deferred::default());
    let permission = AudioPermissions::new(1, true, deferred.clone());
    let mock = Arc::new(Mock::default());
    let cancel = async {
        until(|| deferred.0.lock().unwrap().is_some()).await;
        token.cancel();
        control.cancel();
    };
    tokio::join!(
        held.run(&transport, &permission, Arc::new(MockFactory(mock.clone()))),
        cancel
    );
    assert!(!mock.started.load(Ordering::SeqCst));
    assert!(host.audio.gate.reserve().is_ok());
    deferred
        .0
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .complete(PermissionResult::Granted);
    stop.cancel();
    server.await.unwrap();
}
#[tokio::test]
async fn held_credential_change_at_capture_join_suppresses_command_without_monitor() {
    let (transport, _fake, stop, server) = fixture().await;
    let host = host();
    let credentials = waltr_client::NativeCredentialProvider::new();
    host.credentials(credentials.clone());
    let (held, control) = host.audio.hold(binding()).unwrap();
    let status = control.status();
    control.release();
    let mock = Arc::new(Mock {
        shutdown: Mutex::new(Some(Box::new(move || {
            let _lease = credentials.clear_bearer_token();
        }))),
        ..Default::default()
    });
    held.run(
        &transport,
        &AudioPermissions::new(1, true, Arc::new(Grant)),
        Arc::new(MockFactory(mock.clone())),
    )
    .await;
    assert_eq!(
        *status.borrow(),
        waltr_desktop::host::Status::Finished(Err(waltr_desktop::host::Error::Cancelled))
    );
    assert!(mock.joined.load(Ordering::SeqCst));
    assert!(host.audio.gate.reserve().is_ok());
    stop.cancel();
    server.await.unwrap();
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn generic_bridge_activity_outlives_audio_generation_and_does_not_reserve_microphone() {
    use crate::activity::Phase;
    let bridge = crate::bridge::RuntimeBridge::new(
        crate::server_controller::EmbeddedServerController::default(),
    );
    let tracker = bridge.activity();
    let generic = tracker.begin();
    let mut receive = bridge.synthetic_shortcut_generation(1);
    let (router, mut receiver) =
        waltr_desktop::shortcut::Router::channel_with_epoch(bridge.shortcut_ingress());
    router
        .effective(vec![waltr_desktop::shortcut::Effective {
            id: "test".into(),
            trigger: "Space".into(),
        }])
        .unwrap();
    let mut active = None;
    router.edge("s", "s", "test", true);
    next_shortcut(&bridge, &[binding()], &mut active, &mut receiver).await;
    let held = receive().expect("generic activity must not reserve the microphone");
    bridge.cancel_audio();
    drop(held);
    assert_eq!(tracker.phase(std::time::Instant::now()), Phase::Working);
    drop(generic);
    assert_eq!(tracker.phase(std::time::Instant::now()), Phase::Idle);
}

#[tokio::test]
async fn lifecycle_retirement_returns_while_capture_startup_still_owns_admission() {
    let (transport, _fake, stop, server) = fixture().await;
    let host = host();
    let job = job(&host, 1, "pending");
    let (release, startup) = std::sync::mpsc::channel();
    let mock = Arc::new(Mock {
        startup: Mutex::new(Some(startup)),
        ..Default::default()
    });
    let capture = mock.clone();
    let worker = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(job.run(
                &transport,
                &AudioPermissions::new(1, true, Arc::new(Grant)),
                Arc::new(MockFactory(capture)),
            ));
    });
    until(|| mock.started.load(Ordering::SeqCst)).await;
    host.cancel_all();
    let bridge = Arc::new(crate::bridge::RuntimeBridge::new(
        crate::server_controller::EmbeddedServerController::default(),
    ));
    bridge.retire_shortcut_backend(worker);
    let (returned, response) = tokio::sync::oneshot::channel();
    let callback_bridge = bridge.clone();
    let callback = std::thread::spawn(move || {
        crate::stop_client_bridge(&callback_bridge);
        let _sent = returned.send(());
    });
    let result = tokio::time::timeout(Duration::from_secs(2), response).await;
    assert!(host.audio.gate.reserve().is_err());
    assert!(!mock.joined.load(Ordering::SeqCst));
    release.send(()).unwrap();
    callback.join().unwrap();
    assert!(
        result.is_ok(),
        "lifecycle must not await native startup/retirement"
    );
    until(|| mock.joined.load(Ordering::SeqCst)).await;
    until(|| host.audio.gate.reserve().is_ok()).await;
    drop(bridge);
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn logout_drain_precedes_credential_teardown_and_reply() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (transport, _fake, stop, server) = fixture().await;
            let host = host();
            let credentials = waltr_client::NativeCredentialProvider::new();
            host.credentials(credentials.clone());
            let revision = credentials.credential_revision();
            let job = job(&host, 1, "pending");
            let (release, startup) = std::sync::mpsc::channel();
            let mock = Arc::new(Mock {
                startup: Mutex::new(Some(startup)),
                ..Default::default()
            });
            let capture = mock.clone();
            let mut tasks = tokio::task::JoinSet::new();
            tasks.spawn_local(async move {
                job.run(
                    &transport,
                    &AudioPermissions::new(1, true, Arc::new(Grant)),
                    Arc::new(MockFactory(capture)),
                )
                .await;
            });
            until(|| mock.started.load(Ordering::SeqCst)).await;
            let replied = Arc::new(AtomicBool::new(false));
            let reply = replied.clone();
            let logout = async {
                crate::bridge::drain_audio(&host, &mut tasks).await;
                assert!(mock.joined.load(Ordering::SeqCst));
                let _lease = credentials.clear_bearer_token();
                reply.store(true, Ordering::SeqCst);
            };
            let inspect = async {
                until(|| !host.audio.lock().allowed).await;
                assert!(!replied.load(Ordering::SeqCst));
                assert_eq!(credentials.credential_revision(), revision);
                assert!(host.audio.gate.reserve().is_err());
                assert!(host.start(1, 2, selection("test"), "draft").is_err());
                release.send(()).unwrap();
            };
            tokio::join!(logout, inspect);
            assert!(replied.load(Ordering::SeqCst));
            assert_ne!(credentials.credential_revision(), revision);
            assert!(host.audio.gate.reserve().is_ok());
            let source = include_str!("bridge.rs");
            let logout = source
                .split("Some(HostRequest::LogoutAccess { reply }) => {")
                .nth(2)
                .unwrap()
                .split("Some(HostRequest::Access(action))")
                .next()
                .unwrap();
            assert!(
                logout.find("drain_audio").unwrap() < logout.find("coordinator.stop()").unwrap()
            );
            assert!(
                logout.find("drain_audio").unwrap()
                    < logout.find("CredentialStore::delete_server").unwrap()
            );
            assert!(
                logout.find("drain_audio").unwrap() < logout.find("reply.send(deleted)").unwrap()
            );
            stop.cancel();
            server.await.unwrap();
        })
        .await;
}

async fn next_shortcut(
    bridge: &crate::bridge::RuntimeBridge,
    bindings: &[Arc<waltr_desktop::config::Keybind>],
    active: &mut Option<(
        String,
        u64,
        waltr_desktop::host::HoldControl,
        CancellationToken,
    )>,
    receiver: &mut waltr_desktop::shortcut::Receiver,
) {
    crate::shortcuts::dispatch_edge(
        Some(bridge),
        bindings,
        active,
        receiver.next_epoch().await.unwrap(),
    );
}
#[tokio::test]
async fn shortcut_ingress_epoch_rejects_queued_profile_and_access_replay() {
    use crate::{
        bridge::{RuntimeBridge, RuntimeTarget},
        server_controller::EmbeddedServerController,
        shortcuts::dispatch_edge,
    };
    use waltr_desktop::shortcut::{Effective, Router};
    for replace in [true, false] {
        let (transport, fake, stop, server) = fixture().await;
        let bridge = RuntimeBridge::new(EmbeddedServerController::default());
        let mut take_b = bridge.synthetic_shortcut_generation(1);
        let (router, mut receiver) = Router::channel_with_epoch(bridge.shortcut_ingress());
        router
            .effective(vec![Effective {
                id: "test".into(),
                trigger: "Space".into(),
            }])
            .unwrap();
        let bindings = vec![binding()];
        let mut active = None;
        // Consumer is paused: both physical A edges are already queued before replacement.
        router.edge("s", "s", "test", true);
        router.edge("s", "s", "test", false);
        if replace {
            bridge.select(RuntimeTarget {
                id: "B".into(),
                ..RuntimeTarget::default()
            });
            take_b = bridge.synthetic_shortcut_generation(2);
        } else {
            bridge.synthetic_shortcut_access(1, false);
            // Presses made while denied must not replay after same-generation reauthentication.
            router.edge("s", "s", "test", true);
            router.edge("s", "s", "test", false);
            bridge.synthetic_shortcut_access(1, true);
        }
        next_shortcut(&bridge, &bindings, &mut active, &mut receiver).await;
        let old_release = receiver.next_epoch().await.unwrap();
        assert!(take_b().is_none());
        assert!(active.is_none());
        assert_eq!(fake.uploads.load(Ordering::SeqCst), 0);
        if !replace {
            for _ in 0..2 {
                next_shortcut(&bridge, &bindings, &mut active, &mut receiver).await;
            }
            assert!(take_b().is_none());
        }
        router.edge("s", "s", "test", true);
        next_shortcut(&bridge, &bindings, &mut active, &mut receiver).await;
        let held = take_b().expect("fresh epoch press must enqueue a hold");
        let status = active.as_ref().unwrap().2.status();
        // Deliver the retained A key-up after B admission: matching id alone is insufficient.
        dispatch_edge(Some(&bridge), &bindings, &mut active, old_release);
        let mock = Arc::new(Mock {
            emit: AtomicBool::new(true),
            ..Mock::default()
        });
        let permission = AudioPermissions::new(if replace { 2 } else { 1 }, true, Arc::new(Grant));
        let release = async {
            until(|| fake.uploads.load(Ordering::SeqCst) == 100).await;
            assert!(
                !mock.stopped.load(Ordering::SeqCst),
                "old key-up must not finish B"
            );
            router.edge("s", "s", "test", false);
            next_shortcut(&bridge, &bindings, &mut active, &mut receiver).await;
        };
        tokio::join!(
            held.run(&transport, &permission, Arc::new(MockFactory(mock.clone()))),
            release
        );
        assert_eq!(
            *status.borrow(),
            waltr_desktop::host::Status::Finished(Ok(()))
        );
        assert!(mock.joined.load(Ordering::SeqCst));
        bridge.stop();
        stop.cancel();
        server.await.unwrap();
    }
}
#[tokio::test]
async fn typing_hold_uses_guarded_factory_and_remains_active_through_owned_close() {
    use waltr_desktop::typing::{Error, Factory as TypingFactory, Guard, LocalFuture, Session};
    #[derive(Clone)]
    struct Sink {
        text: Arc<Mutex<String>>,
        closing: Arc<AtomicBool>,
        closed: CancellationToken,
        revoke: Option<waltr_client::NativeCredentialProvider>,
    }
    impl TypingFactory for Sink {
        fn create(&self) -> Result<Box<dyn Session>, Error> {
            Ok(Box::new(self.clone()))
        }
    }
    impl Session for Sink {
        fn open<'a>(&'a mut self, guard: &'a Guard<'_>) -> LocalFuture<'a, Result<(), Error>> {
            Box::pin(async move { guard.check() })
        }
        fn write<'a>(
            &'a mut self,
            text: &'a str,
            guard: &'a Guard<'_>,
        ) -> LocalFuture<'a, Result<(), Error>> {
            Box::pin(async move {
                tokio::task::yield_now().await;
                if let Some(provider) = self.revoke.take() {
                    let _lease = provider.clear_bearer_token();
                }
                guard.check()?;
                self.text.lock().unwrap().push_str(text);
                Ok(())
            })
        }
        fn finish<'a>(&'a mut self, guard: &'a Guard<'_>) -> LocalFuture<'a, Result<(), Error>> {
            Box::pin(async move { guard.check() })
        }
        fn close(&mut self) -> LocalFuture<'_, Result<(), Error>> {
            Box::pin(async move {
                self.closing.store(true, Ordering::SeqCst);
                self.closed.cancelled().await;
                Ok(())
            })
        }
    }
    let (transport, _fake, stop, server) = fixture().await;
    for revoke in [false, true] {
        let host = host();
        let provider = waltr_client::NativeCredentialProvider::new();
        host.credentials(provider.clone());
        let mut binding = Arc::try_unwrap(binding()).unwrap();
        binding.command = None;
        binding.typing = Some(waltr_desktop::config::TypingSpec {});
        let (held, control) = host.audio.hold(Arc::new(binding)).unwrap();
        control.release();
        let mock = Arc::new(Mock::default());
        let sink = Sink {
            text: Arc::default(),
            closing: Arc::default(),
            closed: CancellationToken::new(),
            revoke: revoke.then_some(provider),
        };
        let permission = AudioPermissions::new(1, true, Arc::new(Grant));
        let cleanup = async {
            until(|| sink.closing.load(Ordering::SeqCst)).await;
            assert!(mock.joined.load(Ordering::SeqCst));
            assert!(host.audio.gate.reserve().is_err());
            assert_eq!(
                host.audio.activity.phase(std::time::Instant::now()),
                crate::activity::Phase::Working
            );
            sink.closed.cancel();
        };
        tokio::join!(
            held.run_with_typing(
                &transport,
                &permission,
                Arc::new(MockFactory(mock.clone())),
                &sink
            ),
            cleanup
        );
        assert_eq!(sink.text.lock().unwrap().is_empty(), revoke);
        assert!(host.audio.gate.reserve().is_ok());
        assert_eq!(
            host.audio.activity.phase(std::time::Instant::now()),
            crate::activity::Phase::Idle
        );
    }
    stop.cancel();
    server.await.unwrap();
}

#[test]
fn native_ipc_and_lifecycle_source_contracts() {
    let chat = include_str!("dictation.rs");
    let hold = include_str!("../../waltr-desktop/src/host.rs");
    let driver = include_str!("../../waltr-desktop/src/interaction.rs");
    assert!(chat.contains("waltr_desktop::interaction::run("));
    assert!(hold.contains("crate::interaction::run("));
    for consumer in [chat, hold] {
        assert!(!consumer.contains("Event::InputChunk("));
        assert!(!consumer.contains("CaptureWorker::start_observed("));
        assert!(!consumer.contains("async fn duplex"));
    }
    assert!(driver.contains("tokio::try_join!(receive, send)"));
    assert!(driver.contains("ready.await"));
    let lib = include_str!("lib.rs");
    let lifecycle = lib
        .split("fn stop_client_bridge(")
        .nth(1)
        .unwrap()
        .split("#[tauri::command]")
        .next()
        .unwrap();
    assert!(lifecycle.contains("bridge.stop_nonblocking()"));
    assert!(!lifecycle.contains("bridge.stop()"));
    let build = include_str!("../build.rs");
    let capability = include_str!("../capabilities/main.json");
    for command in [
        "dictation_start",
        "dictation_finish",
        "dictation_status",
        "dictation_cancel",
    ] {
        assert!(lib.contains(command));
        assert!(build.contains(command));
        assert!(capability.contains(&format!("allow-{}", command.replace('_', "-"))));
    }
    let suspend = lib
        .split("pub fn nativeClientSuspend")
        .nth(1)
        .unwrap()
        .split("pub fn nativeClientResume")
        .next()
        .unwrap();
    assert!(suspend.find("cancel_audio()").unwrap() < suspend.find("is_remote()").unwrap());
    assert!(
        include_str!("shortcuts.rs")
            .contains("REGISTRATION.with(|slot| drop(slot.borrow_mut().take()))")
    );
    let bridge = include_str!("bridge.rs");
    assert!(bridge.contains("let audio_transport = transport.clone()"));
    assert!(bridge.contains("drain_audio(&audio, &mut audio_tasks).await"));
    assert!(bridge.contains("while tasks.join_next().await.is_some()"));
}
