//! Fake upstream only: trace identity, metadata privacy, and owned RPC lifetimes.
#[allow(dead_code)]
mod support;
use futures_util::{StreamExt as _, stream};
use waltr_audio::{CancellationToken, audio::PcmS16Le, stt, tts};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use std::time::Duration;
use tracing::Instrument as _;
use tracing_subscriber::{Layer as _, layer::SubscriberExt as _};

fn safe_target(metadata: &tracing::Metadata<'_>) -> bool {
    metadata.is_span()
        && (metadata.target() == "waltr_audio::telemetry" || metadata.name() == "test.parent")
}

fn synthesis() -> tts::SynthesizeSpeechRequest {
    tts::SynthesizeSpeechRequest {
        input: Some(tts::SynthesisInput {
            input_source: Some(tts::synthesis_input::InputSource::Text("hello".into())),
            ..Default::default()
        }),
        audio_config: Some(tts::AudioConfig::default()),
        ..Default::default()
    }
}
fn config() -> tts::StreamingSynthesizeConfig {
    tts::StreamingSynthesizeConfig {
        streaming_audio_config: Some(tts::StreamingAudioConfig::default()),
        ..Default::default()
    }
}
async fn wait(
    exporter: &InMemorySpanExporter,
    count: usize,
) -> Vec<opentelemetry_sdk::trace::SpanData> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let spans = exporter.get_finished_spans().unwrap();
            if spans.len() >= count {
                return spans;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}
fn outcome(span: &opentelemetry_sdk::trace::SpanData) -> String {
    span.attributes
        .iter()
        .rev()
        .find(|field| field.key.as_str() == "outcome")
        .unwrap_or_else(|| panic!("missing outcome: {span:?}"))
        .value
        .as_str()
        .into_owned()
}

#[tokio::test]
async fn all_speech_methods_propagate_only_traceparent_and_stream_spans_outlive_headers() {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry().with(
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("test"))
            .with_filter(tracing_subscriber::filter::filter_fn(safe_target)),
    );
    let _guard = tracing::subscriber::set_default(subscriber);
    let (client, _server, metadata) = support::start_trace_capture(support::Mode::Ok).await;
    let root = tracing::info_span!("test.parent");
    async {
        client
            .recognize(
                "recognizer".into(),
                stt::RecognitionConfig::default(),
                PcmS16Le::new(16000, 1).unwrap(),
                vec![0, 0, 1, 0],
                CancellationToken::new(),
            )
            .await
            .unwrap();
        client
            .synthesize_speech(synthesis(), CancellationToken::new())
            .await
            .unwrap();
        let (sender, input) = tokio::sync::mpsc::channel(1);
        let mut output = client
            .streaming_recognize(
                "recognizer".into(),
                stt::StreamingRecognitionConfig::default(),
                PcmS16Le::new(16000, 1).unwrap(),
                tokio_stream::wrappers::ReceiverStream::new(input),
                CancellationToken::new(),
            )
            .unwrap();
        assert!(output.next().await.unwrap().is_ok());
        assert!(
            !exporter
                .get_finished_spans()
                .unwrap()
                .iter()
                .any(|span| span.name == "speech.streaming_recognize")
        );
        drop(sender);
        while output.next().await.is_some() {}
        drop(output);
        let (sender, input) = tokio::sync::mpsc::channel(1);
        let mut output = client
            .streaming_synthesize(
                config(),
                tokio_stream::wrappers::ReceiverStream::new(input),
                CancellationToken::new(),
            )
            .unwrap();
        assert!(output.next().await.unwrap().is_ok());
        assert!(
            !exporter
                .get_finished_spans()
                .unwrap()
                .iter()
                .any(|span| span.name == "speech.streaming_synthesize")
        );
        drop(sender);
        while output.next().await.is_some() {}
    }
    .instrument(root.clone())
    .await;
    drop(root);
    let spans = wait(&exporter, 5).await;
    let parent = spans
        .iter()
        .find(|span| span.name == "test.parent")
        .unwrap();
    for span in spans.iter().filter(|span| span.name.starts_with("speech.")) {
        assert_eq!(outcome(span), "success");
        assert_eq!(span.parent_span_id, parent.span_context.span_id());
        assert_eq!(span.span_context.trace_id(), parent.span_context.trace_id());
        let expected = format!(
            "00-{}-{}-01",
            span.span_context.trace_id(),
            span.span_context.span_id()
        );
        assert!(
            metadata.lock().unwrap().iter().any(|metadata| metadata
                .get("traceparent")
                .unwrap()
                .to_str()
                .unwrap()
                == expected)
        );
    }
    assert_eq!(metadata.lock().unwrap().len(), 4);
    for metadata in metadata.lock().unwrap().iter() {
        assert!(!metadata.contains_key("baggage"));
        assert!(!metadata.contains_key("tracestate"));
    }
    let diagnostic = format!("{spans:?}");
    for secret in ["recognizer", "hello", "RIFF", "127.0.0.1"] {
        assert!(!diagnostic.contains(secret));
    }
    provider.shutdown().unwrap();
}

#[tokio::test]
async fn speech_failures_cancellation_and_drop_are_not_success() {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry().with(
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("test"))
            .with_filter(tracing_subscriber::filter::filter_fn(safe_target)),
    );
    let _guard = tracing::subscriber::set_default(subscriber);
    let (client, _server, _) = support::start_trace_capture(support::Mode::Fail).await;
    assert!(
        client
            .synthesize_speech(synthesis(), CancellationToken::new())
            .await
            .is_err()
    );
    let mut output = client
        .streaming_synthesize(config(), stream::pending(), CancellationToken::new())
        .unwrap();
    assert!(output.next().await.unwrap().is_err());
    drop(output);
    let spans = wait(&exporter, 2).await;
    for span in spans.iter().filter(|span| span.name.starts_with("speech.")) {
        assert_eq!(outcome(span), "failure", "{span:?}");
        assert!(
            span.attributes
                .iter()
                .any(|field| field.key.as_str() == "grpc_status" && field.value.as_str() == "8")
        );
        assert!(!format!("{span:?}").contains("fake quota"));
    }
    exporter.reset();
    let (client, _server, _) = support::start_trace_capture(support::Mode::TerminalFail).await;
    let (sender, receiver) = tokio::sync::mpsc::channel::<
        Result<tts::StreamingSynthesisInput, waltr_audio::error::Error>,
    >(1);
    let mut output = client
        .streaming_synthesize(
            config(),
            tokio_stream::wrappers::ReceiverStream::new(receiver),
            CancellationToken::new(),
        )
        .unwrap();
    assert!(output.next().await.unwrap().is_ok());
    assert!(exporter.get_finished_spans().unwrap().is_empty());
    drop(sender);
    while output.next().await.is_some() {}
    drop(output);
    let spans = wait(&exporter, 1).await;
    assert_eq!(outcome(&spans[0]), "failure");
    assert!(
        spans[0]
            .attributes
            .iter()
            .any(|field| field.key.as_str() == "grpc_status" && field.value.as_str() == "8")
    );
    assert!(!format!("{spans:?}").contains("SECRET_TRAILER_FAILURE"));
    exporter.reset();
    let (client, _server, _) = support::start_trace_capture(support::Mode::Ok).await;
    let cancel = CancellationToken::new();
    let mut output = client
        .streaming_synthesize(config(), stream::pending(), cancel.clone())
        .unwrap();
    assert!(output.next().await.unwrap().is_ok());
    cancel.cancel();
    while output.next().await.is_some() {}
    drop(output);
    assert_eq!(outcome(&wait(&exporter, 1).await[0]), "cancelled");
    exporter.reset();
    let mut output = client
        .streaming_synthesize(config(), stream::pending(), CancellationToken::new())
        .unwrap();
    assert!(output.next().await.unwrap().is_ok());
    assert!(exporter.get_finished_spans().unwrap().is_empty());
    drop(output);
    assert_eq!(outcome(&wait(&exporter, 1).await[0]), "incomplete");
    exporter.reset();
    let (client, _server) =
        support::start_with(support::Mode::Stall, Duration::from_millis(20), |_| {}).await;
    let mut output = client
        .streaming_synthesize(config(), stream::pending(), CancellationToken::new())
        .unwrap();
    assert!(output.next().await.unwrap().is_err());
    drop(output);
    let spans = wait(&exporter, 1).await;
    assert!(matches!(
        outcome(&spans[0]).as_str(),
        "deadline" | "failure"
    ));
    provider.shutdown().unwrap();
}
