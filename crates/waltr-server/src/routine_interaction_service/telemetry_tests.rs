use super::*;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use tokio::sync::{Semaphore, mpsc};
use tracing_subscriber::{Layer as _, layer::SubscriberExt as _};
use waltr_audio::{client::Client, config::Config, tts};

fn headers<T>(mut request: Request<T>, parent: Option<&str>, duplicate: bool) -> Request<T> {
    if let Some(parent) = parent {
        request
            .metadata_mut()
            .insert("traceparent", parent.parse().unwrap());
    }
    if duplicate {
        request.metadata_mut().append(
            "traceparent",
            "00-22222222222222222222222222222222-3333333333333333-01"
                .parse()
                .unwrap(),
        );
    }
    request
        .metadata_mut()
        .insert("baggage", "SECRET_BAGGAGE_MARKER".parse().unwrap());
    request
        .metadata_mut()
        .insert("tracestate", "secret=value".parse().unwrap());
    request
}

async fn traced_loopback(service: RoutineInteractionService) -> Loopback {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stop = tokio_util::sync::CancellationToken::new();
    let shutdown = stop.clone();
    let task = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .layer(tonic_tracing_opentelemetry::middleware::server::OtelGrpcLayer::default())
            .add_service(RoutineInteractionServiceServer::new(service))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), shutdown.cancelled())
            .await
            .unwrap();
    });
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let client = RoutineInteractionServiceClient::new(channel);
    Loopback { client, stop, task }
}

struct Speech {
    metadata: mpsc::UnboundedSender<tonic::metadata::MetadataMap>,
    release: Arc<Semaphore>,
}
#[tonic::async_trait]
impl tts::text_to_speech_server::TextToSpeech for Speech {
    async fn list_voices(
        &self,
        _: Request<tts::ListVoicesRequest>,
    ) -> std::result::Result<tonic::Response<tts::ListVoicesResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused"))
    }
    async fn synthesize_speech(
        &self,
        request: Request<tts::SynthesizeSpeechRequest>,
    ) -> std::result::Result<tonic::Response<tts::SynthesizeSpeechResponse>, tonic::Status> {
        self.metadata.send(request.metadata().clone()).unwrap();
        self.release.acquire().await.unwrap().forget();
        Ok(tonic::Response::new(
            tts::SynthesizeSpeechResponse::default(),
        ))
    }
    type StreamingSynthesizeStream =
        tokio_stream::Empty<std::result::Result<tts::StreamingSynthesizeResponse, tonic::Status>>;
    async fn streaming_synthesize(
        &self,
        _: Request<tonic::Streaming<tts::StreamingSynthesizeRequest>>,
    ) -> std::result::Result<tonic::Response<Self::StreamingSynthesizeStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused"))
    }
}
struct AudioAdapter(Client);
impl interaction::Adapters for AudioAdapter {
    fn execute<'a>(
        &'a self,
        task: &'a crate::resource_catalog::routine::RoutineTask,
        context: &'a mut interaction::TaskContext,
    ) -> interaction::AdapterFuture<'a> {
        Box::pin(async move {
            let result = session_fixture::adapters().execute(task, context).await?;
            self.0
                .synthesize_speech(
                    tts::SynthesizeSpeechRequest {
                        input: Some(tts::SynthesisInput {
                            input_source: Some(tts::synthesis_input::InputSource::Text(
                                "SECRET_CONTENT_MARKER".into(),
                            )),
                            ..Default::default()
                        }),
                        audio_config: Some(tts::AudioConfig::default()),
                        ..Default::default()
                    },
                    context.cancellation.clone(),
                )
                .await
                .map_err(|_| interaction::Error::new("cancelled", "cancelled"))?;
            Ok(result)
        })
    }
}

#[tokio::test]
async fn admission_telemetry_preserves_numeric_status_and_finite_rejections() {
    if !super::super::telemetry::isolated_exporter_test(
        "routine::interaction_service_tests::telemetry_tests::admission_telemetry_preserves_numeric_status_and_finite_rejections",
    ) {
        return;
    }
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry().with(
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("test"))
            .with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
                metadata.is_span() && metadata.target() == "waltr_server::routine::telemetry"
            })),
    );
    let _guard = tracing::subscriber::set_default(subscriber);
    let limits = InteractionLimits {
        complete_request_bytes: 1024,
        chunk_bytes: 1024,
        ..Default::default()
    };
    let (service, engine, _bus, _persistence, root) = setup(TEXT, limits).await;
    for (open, inputs, expected, status) in [
        (None, vec![], "rejected", 0),
        (
            Some(open()),
            vec![text_input(&"x".repeat(2048))],
            "failure",
            8,
        ),
    ] {
        exporter.reset();
        drop(
            service
                .run(Request::new(wire::RunInteractionRequest { open, inputs }))
                .await,
        );
        let spans = exporter.get_finished_spans().unwrap();
        let span = spans
            .iter()
            .find(|span| span.name == "routine.run")
            .unwrap();
        assert!(
            span.attributes
                .iter()
                .any(|field| field.key.as_str() == "outcome" && field.value.as_str() == expected)
        );
        assert!(
            span.attributes
                .iter()
                .any(|field| field.key.as_str() == "grpc_status"
                    && field.value.as_str() == status.to_string())
        );
    }
    engine.shutdown().await;
    exporter.reset();
    assert_eq!(
        service
            .run(Request::new(wire::RunInteractionRequest {
                open: Some(open()),
                inputs: vec![text_input("hello")]
            }))
            .await
            .err()
            .unwrap()
            .code(),
        tonic::Code::Unavailable
    );
    let spans = exporter.get_finished_spans().unwrap();
    let span = spans
        .iter()
        .find(|span| span.name == "routine.run")
        .unwrap();
    assert!(
        span.attributes
            .iter()
            .any(|field| field.key.as_str() == "grpc_status" && field.value.as_str() == "14")
    );
    std::fs::remove_dir_all(root).unwrap();
    provider.shutdown().unwrap();
}

// Both host compositions use the same scoped exporter; the embedded filter excludes middleware.
#[tokio::test]
async fn trace_chain_survives_handler_return_in_standalone_and_embedded_compositions() {
    if !super::super::telemetry::isolated_exporter_test(
        "routine::interaction_service_tests::telemetry_tests::trace_chain_survives_handler_return_in_standalone_and_embedded_compositions",
    ) {
        return;
    }
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );
    for embedded in [false, true] {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer()
                .with_tracer(provider.tracer("test"))
                .with_filter(tracing_subscriber::filter::filter_fn(move |metadata| {
                    metadata.is_span()
                        && (*metadata.level() <= tracing::Level::INFO)
                        && (!embedded
                            || matches!(
                                metadata.target(),
                                "waltr_server::routine::telemetry" | "waltr_audio::telemetry"
                            ))
                })),
        );
        let _guard = tracing::subscriber::set_default(subscriber);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (metadata, mut captured) = mpsc::unbounded_channel();
        let release = Arc::new(Semaphore::new(0));
        let speech = Speech {
            metadata,
            release: release.clone(),
        };
        let upstream = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(tts::text_to_speech_server::TextToSpeechServer::new(speech))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        let client = Client::connect(Config::new(format!("http://{address}")))
            .await
            .unwrap();
        let (service, engine, _bus, _persistence, root, _) = setup_full(
            TEXT,
            InteractionLimits::default(),
            Arc::new(AudioAdapter(client)),
        )
        .await;
        let mut server = traced_loopback(service).await;
        let valid = Some("00-0123456789abcdef0123456789abcdef-0123456789abcdef-01");
        for (parent, duplicate, live, cancel) in [
            (valid, false, false, false),
            (None, false, false, false),
            (Some("malformed"), false, false, false),
            (
                Some("01-0123456789abcdef0123456789abcdef-0123456789abcdef-01"),
                false,
                false,
                false,
            ),
            (valid, true, false, false),
            (valid, false, true, false),
            (valid, false, true, true),
            (
                Some("01-0123456789abcdef0123456789abcdef-0123456789abcdef-01"),
                false,
                true,
                false,
            ),
            (valid, true, true, false),
        ] {
            exporter.reset();
            let (mut stream, live_tx) = if live {
                let (tx, rx) = mpsc::channel(8);
                for event in [
                    In::Open(open()),
                    opened(),
                    chunk(true),
                    closed(),
                    In::InputCompleted(wire::InteractionInputCompleted {}),
                ] {
                    tx.send(request(event)).await.unwrap();
                }
                (
                    server
                        .client
                        .interact(headers(
                            Request::new(ReceiverStream::new(rx)),
                            parent,
                            duplicate,
                        ))
                        .await
                        .unwrap()
                        .into_inner(),
                    Some(tx),
                )
            } else {
                (
                    server
                        .client
                        .run(headers(
                            Request::new(wire::RunInteractionRequest {
                                open: Some(open()),
                                inputs: vec![text_input("SECRET_INPUT_MARKER")],
                            }),
                            parent,
                            duplicate,
                        ))
                        .await
                        .unwrap()
                        .into_inner(),
                    None,
                )
            };
            let metadata = tokio::time::timeout(Duration::from_secs(5), captured.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(!metadata.contains_key("baggage"));
            assert!(!metadata.contains_key("tracestate"));
            assert!(
                !exporter
                    .get_finished_spans()
                    .unwrap()
                    .iter()
                    .any(|span| span.name == "routine.interaction"
                        || span.name == "speech.synthesize")
            );
            if cancel {
                live_tx
                    .as_ref()
                    .unwrap()
                    .send(request(In::Cancel(wire::InteractionCancel {})))
                    .await
                    .unwrap();
            } else {
                release.add_permits(1);
            }
            while tokio::time::timeout(Duration::from_secs(5), stream.message())
                .await
                .unwrap()
                .unwrap()
                .is_some()
            {}
            drop(live_tx);
            // The transport can deliver terminal output before engine-owned cleanup finishes.
            let spans = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let spans = exporter.get_finished_spans().unwrap();
                    let rpc = spans.iter().find(|span| {
                        span.name
                            == if live {
                                "routine.interact"
                            } else {
                                "routine.run"
                            }
                    });
                    let middleware_finished = embedded
                        || parent != valid
                        || duplicate
                        || rpc.is_some_and(|rpc| {
                            spans
                                .iter()
                                .any(|span| span.span_context.span_id() == rpc.parent_span_id)
                        });
                    if middleware_finished
                        && [
                            "routine.interaction",
                            "routine.interaction.owner",
                            "routine.interaction.task",
                            "speech.synthesize",
                        ]
                        .iter()
                        .all(|name| spans.iter().any(|span| span.name == *name))
                    {
                        break spans;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "embedded={embedded} live={live} cancel={cancel} finished={:?}",
                    exporter
                        .get_finished_spans()
                        .unwrap()
                        .iter()
                        .map(|span| span.name.as_ref())
                        .collect::<Vec<_>>()
                )
            });
            let named = |name: &str| spans.iter().find(|span| span.name == name).unwrap();
            let rpc = named(if live {
                "routine.interact"
            } else {
                "routine.run"
            });
            let interaction = named("routine.interaction");
            let owner = named("routine.interaction.owner");
            let task = named("routine.interaction.task");
            let speech = named("speech.synthesize");
            for (child, parent) in [
                (interaction, rpc),
                (owner, interaction),
                (task, owner),
                (speech, task),
            ] {
                assert_eq!(
                    child.span_context.trace_id(),
                    parent.span_context.trace_id()
                );
                assert_eq!(child.parent_span_id, parent.span_context.span_id());
            }
            assert_eq!(
                metadata.get("traceparent").unwrap().to_str().unwrap(),
                format!(
                    "00-{}-{}-01",
                    speech.span_context.trace_id(),
                    speech.span_context.span_id()
                )
            );
            if parent == valid && !duplicate {
                assert_eq!(
                    rpc.span_context.trace_id().to_string(),
                    "0123456789abcdef0123456789abcdef"
                );
                if embedded {
                    assert_eq!(rpc.parent_span_id.to_string(), "0123456789abcdef");
                } else {
                    let middleware = spans
                        .iter()
                        .find(|span| span.span_context.span_id() == rpc.parent_span_id)
                        .unwrap();
                    assert_eq!(middleware.parent_span_id.to_string(), "0123456789abcdef");
                    assert_eq!(
                        middleware.span_context.trace_state().header(),
                        "secret=value"
                    );
                }
            } else {
                assert_eq!(rpc.parent_span_id, opentelemetry::trace::SpanId::INVALID);
                for rejected in [
                    "0123456789abcdef0123456789abcdef",
                    "22222222222222222222222222222222",
                ] {
                    assert_ne!(rpc.span_context.trace_id().to_string(), rejected);
                }
            }
            let audited = [rpc, interaction, owner, task, speech];
            for span in audited {
                assert_eq!(
                    span.span_context.trace_state().header(),
                    "",
                    "{}",
                    span.name
                );
            }
            let stages: Vec<_> = spans
                .iter()
                .filter(|span| span.name == "server.admission.stage")
                .collect();
            for stage in [
                "prepare",
                "engine_health_wait",
                "engine_lifecycle_wait",
                "durable_admission",
                "durable_start",
                "accepted_enqueue",
            ] {
                let span = stages
                    .iter()
                    .find(|span| {
                        span.attributes.iter().any(|field| {
                            field.key.as_str() == "stage" && field.value.as_str() == stage
                        })
                    })
                    .unwrap();
                assert!(
                    span.attributes
                        .iter()
                        .any(|field| field.key.as_str() == "outcome"
                            && field.value.as_str() == "success")
                );
                assert!(
                    span.attributes
                        .iter()
                        .any(|field| field.key.as_str() == "duration_ms")
                );
                assert_eq!(span.span_context.trace_id(), rpc.span_context.trace_id());
            }
            assert_eq!(
                stages.iter().any(|span| span
                    .attributes
                    .iter()
                    .any(|field| field.key.as_str() == "stage"
                        && field.value.as_str() == "open_read")),
                live
            );
            let diagnostic = format!("{audited:?} {stages:?}");
            for secret in [
                "SECRET_CONTENT_MARKER",
                "SECRET_INPUT_MARKER",
                "SECRET_BAGGAGE_MARKER",
                "secret=value",
                "correlation",
                "recognizer",
            ] {
                assert!(!diagnostic.contains(secret), "leaked {secret}");
            }
            for span in [interaction, owner, task, speech] {
                assert!(
                    span.attributes
                        .iter()
                        .any(|field| field.key.as_str() == "outcome"
                            && field.value.as_str()
                                == if cancel { "cancelled" } else { "success" }),
                    "{span:?}"
                );
            }
        }
        server.shutdown().await;
        engine.shutdown().await;
        upstream.abort();
        drop(upstream.await);
        std::fs::remove_dir_all(root).unwrap();
        provider.shutdown().unwrap();
    }
}
