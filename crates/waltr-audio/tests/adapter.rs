//! Loopback-only upstream contract and lifecycle tests.
use futures_core::Stream;
use futures_util::{StreamExt, stream};
use waltr_audio::{
    CancellationToken,
    audio::{Container, PcmS16Le},
    client::Client,
    config::Config,
    error::Error,
    stt, tts,
};
use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Wake, Waker},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
#[path = "support/streaming_pcm.rs"]
mod streaming_pcm;
mod support;
use support::{Mode, start, start_backpressure, start_with};

fn pcm() -> PcmS16Le {
    PcmS16Le::new(16000, 1).unwrap()
}
fn synthesis() -> tts::SynthesizeSpeechRequest {
    tts::SynthesizeSpeechRequest {
        input: Some(tts::SynthesisInput {
            input_source: Some(tts::synthesis_input::InputSource::Text("hello".into())),
            ..Default::default()
        }),
        audio_config: Some(tts::AudioConfig {
            audio_encoding: tts::AudioEncoding::Linear16 as i32,
            ..Default::default()
        }),
        ..Default::default()
    }
}
fn streaming_config() -> tts::StreamingSynthesizeConfig {
    tts::StreamingSynthesizeConfig {
        streaming_audio_config: Some(tts::StreamingAudioConfig {
            audio_encoding: tts::AudioEncoding::Pcm as i32,
            ..Default::default()
        }),
        ..Default::default()
    }
}
fn text() -> tts::StreamingSynthesisInput {
    tts::StreamingSynthesisInput {
        input_source: Some(tts::streaming_synthesis_input::InputSource::Text(
            "hello".into(),
        )),
        ..Default::default()
    }
}
async fn next<T>(output: &mut (impl Stream<Item = T> + Unpin)) -> T {
    tokio::time::timeout(Duration::from_secs(3), output.next())
        .await
        .unwrap()
        .unwrap()
}
#[tokio::test]
async fn unary_methods_and_containers_preserve_upstream_data() {
    let (client, _server) = start(Mode::Ok, Duration::from_secs(5)).await;
    let result = client
        .recognize(
            "recognizer".into(),
            stt::RecognitionConfig::default(),
            pcm(),
            vec![0, 0, 1, 0],
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        result.results[0].alternatives[0].words[0].speaker_label,
        "request-local"
    );
    let audio = client
        .synthesize_speech(synthesis(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(audio.container, Container::Wav);
    assert_eq!(audio.response.audio_content, b"RIFFfake WAV");
    assert_eq!(
        tts::streaming_container(tts::AudioEncoding::Linear16 as i32),
        Container::RawPcm
    );
}
#[tokio::test]
async fn streaming_recognition_early_interim_and_final_after_input_close() {
    let (client, _server) = start(Mode::Ok, Duration::from_secs(5)).await;
    let (sender, receiver) = mpsc::channel(1);
    let mut output = client
        .streaming_recognize(
            "recognizer".into(),
            stt::StreamingRecognitionConfig::default(),
            pcm(),
            tokio_stream::wrappers::ReceiverStream::new(receiver),
            CancellationToken::new(),
        )
        .unwrap();
    let early = next(&mut output).await.unwrap();
    assert!(!early.results[0].is_final);
    assert_eq!(early.results[0].channel_tag, 2);
    sender.send(Ok(vec![0, 0, 1, 0])).await.unwrap();
    drop(sender);
    assert!(next(&mut output).await.unwrap().results[0].is_final);
    assert!(output.next().await.is_none());
}
#[tokio::test]
async fn streaming_synthesis_early_audio_before_input_close() {
    let (client, _server) = start(Mode::Ok, Duration::from_secs(5)).await;
    let (sender, receiver) = mpsc::channel(1);
    let mut output = client
        .streaming_synthesize(
            streaming_config(),
            tokio_stream::wrappers::ReceiverStream::new(receiver),
            CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(next(&mut output).await.unwrap().audio_content, [0, 0]);
    sender.send(Ok(text())).await.unwrap();
    drop(sender);
    assert_eq!(next(&mut output).await.unwrap().audio_content, [1, 0]);
    assert!(output.next().await.is_none());
}
#[tokio::test]
async fn malformed_pcm_and_input_producer_failure_are_not_eof() {
    assert!(PcmS16Le::new(0, 1).is_err());
    assert!(PcmS16Le::new(16000, 0).is_err());
    assert!(PcmS16Le::new(16000, 2).unwrap().validate(&[0, 0]).is_err());
    let (client, _server) = start(Mode::Stall, Duration::from_secs(5)).await;
    assert!(matches!(
        client
            .recognize(
                "recognizer".into(),
                stt::RecognitionConfig::default(),
                pcm(),
                vec![0],
                CancellationToken::new()
            )
            .await,
        Err(Error::Invalid(_))
    ));
    let mut output = client
        .streaming_recognize(
            "recognizer".into(),
            stt::StreamingRecognitionConfig::default(),
            pcm(),
            stream::iter([Ok(vec![0])]),
            CancellationToken::new(),
        )
        .unwrap();
    assert!(matches!(
        next(&mut output).await,
        Err(Error::Invalid("incomplete PCM frame"))
    ));
    let mut output = client
        .streaming_synthesize(
            streaming_config(),
            stream::iter([Err(Error::Invalid("caller producer failed"))]),
            CancellationToken::new(),
        )
        .unwrap();
    assert!(matches!(
        next(&mut output).await,
        Err(Error::Invalid("caller producer failed"))
    ));
    let mut output = client
        .streaming_synthesize(
            streaming_config(),
            stream::iter([Ok(tts::StreamingSynthesisInput::default())]),
            CancellationToken::new(),
        )
        .unwrap();
    assert!(matches!(next(&mut output).await, Err(Error::Invalid(_))));
}
#[tokio::test]
async fn upstream_status_is_preserved_for_all_methods() {
    let (client, _server) = start(Mode::Fail, Duration::from_secs(5)).await;
    let error = client
        .recognize(
            "recognizer".into(),
            stt::RecognitionConfig::default(),
            pcm(),
            vec![0, 0, 1, 0],
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    check_status(error);
    check_status(
        client
            .synthesize_speech(synthesis(), CancellationToken::new())
            .await
            .unwrap_err(),
    );
    let mut output = client
        .streaming_recognize(
            "recognizer".into(),
            stt::StreamingRecognitionConfig::default(),
            pcm(),
            stream::pending(),
            CancellationToken::new(),
        )
        .unwrap();
    check_status(next(&mut output).await.unwrap_err());
    let mut output = client
        .streaming_synthesize(
            streaming_config(),
            stream::pending(),
            CancellationToken::new(),
        )
        .unwrap();
    check_status(next(&mut output).await.unwrap_err());
}
fn check_status(error: Error) {
    let Error::Status(status) = error else {
        panic!("expected status, got {error:?}");
    };
    assert_eq!(status.code(), tonic::Code::ResourceExhausted);
    assert_eq!(status.message(), "fake quota");
    assert_eq!(status.details(), &[1, 2, 3]);
}

struct TrackedInput<T> {
    input: Pin<Box<dyn Stream<Item = T> + Send>>,
    dropped: Option<oneshot::Sender<()>>,
}
impl<T> Stream for TrackedInput<T> {
    type Item = T;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.input.as_mut().poll_next(cx)
    }
}
impl<T> Drop for TrackedInput<T> {
    fn drop(&mut self) {
        if let Some(sender) = self.dropped.take() {
            let _sent = sender.send(());
        }
    }
}
fn tracked<T: 'static>(
    input: impl Stream<Item = T> + Send + 'static,
) -> (TrackedInput<T>, oneshot::Receiver<()>) {
    let (sender, receiver) = oneshot::channel();
    (
        TrackedInput {
            input: Box::pin(input),
            dropped: Some(sender),
        },
        receiver,
    )
}
#[tokio::test]
async fn cancellation_and_response_drop_destroy_input_producers() {
    let (client, _server) = start(Mode::Ok, Duration::from_secs(5)).await;
    for cancel_explicitly in [true, false] {
        let parent = CancellationToken::new();
        let cancel = parent.child_token();
        let (input, dropped) = tracked(stream::pending());
        let mut output = client
            .streaming_synthesize(streaming_config(), input, cancel.clone())
            .unwrap();
        next(&mut output).await.unwrap();
        if cancel_explicitly {
            cancel.cancel();
            assert!(matches!(next(&mut output).await, Err(Error::Cancelled)));
            assert!(output.next().await.is_none());
        }
        drop(output);
        tokio::time::timeout(Duration::from_secs(3), dropped)
            .await
            .unwrap()
            .unwrap();
        assert!(!parent.is_cancelled());
        assert_eq!(cancel.is_cancelled(), cancel_explicitly);
    }
}

struct OutputReady(Mutex<Option<oneshot::Sender<()>>>);
impl Wake for OutputReady {
    fn wake(self: Arc<Self>) {
        let ready = self.0.lock().unwrap().take();
        if let Some(ready) = ready {
            let _sent = ready.send(());
        }
    }
}

async fn full_output_queue_stops_without_draining(deadline: bool) {
    let timeout = if deadline {
        Duration::from_secs(1)
    } else {
        Duration::from_secs(30)
    };
    let (client, _server, barrier) = start_backpressure(timeout).await;
    let parent = CancellationToken::new();
    let cancel = parent.child_token();
    let (input, dropped) = tracked(stream::pending());
    let mut output = client
        .streaming_synthesize(streaming_config(), input, cancel.clone())
        .unwrap();
    let (ready, queued) = oneshot::channel();
    let waker = Waker::from(Arc::new(OutputReady(Mutex::new(Some(ready)))));
    assert!(
        Pin::new(&mut output)
            .poll_next(&mut Context::from_waker(&waker))
            .is_pending()
    );
    barrier.release.send(()).unwrap();
    // The registered receiver wakes only after enqueueing (or closure). With no
    // further polls, the successful response occupies the one-slot output queue.
    tokio::time::timeout(Duration::from_secs(3), queued)
        .await
        .unwrap()
        .unwrap();
    if !deadline {
        cancel.cancel();
    }
    // Observe both directions stopping before consuming even the queued response.
    tokio::time::timeout(Duration::from_secs(3), dropped)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), barrier.stopped)
        .await
        .unwrap()
        .unwrap();
    assert!(!parent.is_cancelled());
    assert_eq!(cancel.is_cancelled(), !deadline);
    assert_eq!(next(&mut output).await.unwrap().audio_content, [0, 0]);
    let error = next(&mut output).await.unwrap_err();
    if deadline {
        check_deadline(error);
    } else {
        assert!(matches!(error, Error::Cancelled));
    }
    assert!(output.next().await.is_none());
    drop(output);
    assert!(!parent.is_cancelled());

    let mut subsequent = client
        .streaming_synthesize(
            streaming_config(),
            stream::iter([Ok(text())]),
            parent.clone(),
        )
        .unwrap();
    assert_eq!(next(&mut subsequent).await.unwrap().audio_content, [0, 0]);
    assert_eq!(next(&mut subsequent).await.unwrap().audio_content, [1, 0]);
    assert!(next_or_eof(&mut subsequent).await.is_none());
    assert!(!parent.is_cancelled());
}
async fn next_or_eof<T>(output: &mut (impl Stream<Item = T> + Unpin)) -> Option<T> {
    tokio::time::timeout(Duration::from_secs(3), output.next())
        .await
        .unwrap()
}
#[tokio::test]
async fn cancellation_stops_a_full_output_queue_and_channel_is_reusable() {
    full_output_queue_stops_without_draining(false).await;
}
#[tokio::test]
async fn deadline_stops_a_full_output_queue_and_channel_is_reusable() {
    full_output_queue_stops_without_draining(true).await;
}
#[tokio::test]
async fn whole_operation_deadlines_and_unary_cancel() {
    let (client, _server) = start(Mode::Stall, Duration::from_millis(80)).await;
    let error = client
        .synthesize_speech(synthesis(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Deadline | Error::Status(_)));
    let (input, dropped) = tracked(stream::pending());
    let mut output = client
        .streaming_synthesize(streaming_config(), input, CancellationToken::new())
        .unwrap();
    assert!(matches!(
        next(&mut output).await,
        Err(Error::Deadline | Error::Status(_))
    ));
    tokio::time::timeout(Duration::from_secs(3), dropped)
        .await
        .unwrap()
        .unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        client.synthesize_speech(synthesis(), cancel).await,
        Err(Error::Cancelled)
    ));
}
#[tokio::test]
async fn queue_limit_errors_and_cancellation_under_backpressure() {
    let (client, _server) = start(Mode::Stall, Duration::from_secs(5)).await;
    let mut output = client
        .streaming_synthesize(
            streaming_config(),
            stream::iter([Ok(tts::StreamingSynthesisInput {
                input_source: Some(tts::streaming_synthesis_input::InputSource::Text(
                    "x".repeat(200),
                )),
                ..Default::default()
            })]),
            CancellationToken::new(),
        )
        .unwrap();
    assert!(matches!(
        next(&mut output).await,
        Err(Error::Invalid(
            "input chunk exceeds queue or encoding limit"
        ))
    ));
    let cancel = CancellationToken::new();
    let (input, dropped) = tracked(stream::repeat_with(|| Ok(text())));
    let mut output = client
        .streaming_synthesize(streaming_config(), input, cancel.clone())
        .unwrap();
    cancel.cancel();
    assert!(matches!(next(&mut output).await, Err(Error::Cancelled)));
    tokio::time::timeout(Duration::from_secs(3), dropped)
        .await
        .unwrap()
        .unwrap();
}
#[tokio::test]
async fn invalid_resolved_limits_rejected_before_connect() {
    let mut config = Config::new("http://127.0.0.1:1");
    config.queue_chunks = 0;
    assert!(matches!(
        Client::connect(config).await,
        Err(Error::Invalid(_))
    ));
    assert!(
        Client::connect(Config::new("ftp://127.0.0.1:1"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn deadline_includes_finalization_after_early_output() {
    let (client, _server) = start(Mode::Ok, Duration::from_millis(100)).await;
    let mut output = client
        .streaming_recognize(
            "recognizer".into(),
            stt::StreamingRecognitionConfig::default(),
            PcmS16Le::new(16000, 1).unwrap(),
            stream::pending(),
            CancellationToken::new(),
        )
        .unwrap();
    assert!(!output.next().await.unwrap().unwrap().results[0].is_final);
    let error = tokio::time::timeout(Duration::from_secs(3), output.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    check_deadline(error);
}
fn check_deadline(error: Error) {
    match error {
        Error::Deadline => {}
        Error::Status(status) => assert!(matches!(
            status.code(),
            tonic::Code::DeadlineExceeded | tonic::Code::Cancelled
        )),
        error => panic!("expected deadline, got {error:?}"),
    }
}
#[tokio::test]
async fn encoding_decoding_and_configuration_limits() {
    let (client, _server) = start_with(Mode::Ok, Duration::from_secs(5), |config| {
        config.max_encoding_bytes = 1;
    })
    .await;
    let result = client
        .recognize(
            "recognizer".into(),
            stt::RecognitionConfig::default(),
            PcmS16Le::new(16000, 1).unwrap(),
            vec![0, 0, 1, 0],
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(result, Err(Error::Status(_))));
    assert!(matches!(
        client.streaming_recognize(
            "recognizer".into(),
            stt::StreamingRecognitionConfig::default(),
            PcmS16Le::new(16000, 1).unwrap(),
            stream::pending(),
            CancellationToken::new()
        ),
        Err(Error::Invalid("configuration exceeds encoding limit"))
    ));
    let (client, _server) = start_with(Mode::Ok, Duration::from_secs(5), |config| {
        config.max_decoding_bytes = 1;
    })
    .await;
    let result = client
        .synthesize_speech(
            tts::SynthesizeSpeechRequest {
                input: Some(tts::SynthesisInput {
                    input_source: Some(tts::synthesis_input::InputSource::Text("hello".into())),
                    ..Default::default()
                }),
                audio_config: Some(tts::AudioConfig::default()),
                ..Default::default()
            },
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(result, Err(Error::Status(_))));
}
