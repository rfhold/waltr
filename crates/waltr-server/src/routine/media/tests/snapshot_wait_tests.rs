use super::*;
use futures_util::Stream;
use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tonic::{Request, Response, Status};
use waltr_audio::stt;

#[derive(Clone)]
struct DelayedSpeech {
    finish: bool,
    packets: Arc<AtomicUsize>,
}
type Responses =
    Pin<Box<dyn Stream<Item = Result<stt::StreamingRecognizeResponse, Status>> + Send>>;

fn response(labelled: bool) -> stt::StreamingRecognizeResponse {
    stt::StreamingRecognizeResponse {
        results: vec![stt::StreamingRecognitionResult {
            is_final: labelled,
            alternatives: vec![stt::SpeechRecognitionAlternative {
                transcript: "not an authoritative snapshot".into(),
                words: if labelled {
                    vec![stt::WordInfo {
                        word: "hello".into(),
                        speaker_label: "S".into(),
                        ..Default::default()
                    }]
                } else {
                    Vec::new()
                },
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    }
}

impl tonic::server::NamedService for DelayedSpeech {
    const NAME: &'static str = "google.cloud.speech.v2.Speech";
}
impl<B> tonic::codegen::Service<tonic::codegen::http::Request<B>> for DelayedSpeech
where
    B: tonic::codegen::Body + Send + 'static,
    B::Error: Into<tonic::codegen::StdError> + Send + 'static,
{
    type Response = tonic::codegen::http::Response<tonic::body::Body>;
    type Error = std::convert::Infallible;
    type Future = tonic::codegen::BoxFuture<Self::Response, Self::Error>;
    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn call(&mut self, request: tonic::codegen::http::Request<B>) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            Ok(tonic::server::Grpc::new(SpeechCodec)
                .streaming(this, request)
                .await)
        })
    }
}
impl tonic::server::StreamingService<stt::StreamingRecognizeRequest> for DelayedSpeech {
    type Response = stt::StreamingRecognizeResponse;
    type ResponseStream = Responses;
    type Future = tonic::codegen::BoxFuture<Response<Responses>, Status>;
    fn call(
        &mut self,
        request: Request<tonic::Streaming<stt::StreamingRecognizeRequest>>,
    ) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            let stream = futures_util::stream::try_unfold(
                (request.into_inner(), this, false),
                |(mut input, this, done)| async move {
                    if done {
                        return Ok(None);
                    }
                    while let Some(packet) = input.message().await? {
                        if matches!(
                            packet.streaming_request,
                            Some(stt::streaming_recognize_request::StreamingRequest::Audio(_))
                        ) {
                            this.packets.fetch_add(1, Ordering::SeqCst);
                            return Ok(Some((response(false), (input, this, false))));
                        }
                    }
                    if !this.finish {
                        return std::future::pending().await;
                    }
                    tokio::time::sleep(Duration::from_millis(450)).await;
                    Ok(Some((response(true), (input, this, true))))
                },
            );
            Ok(Response::new(Box::pin(stream) as Responses))
        })
    }
}

struct Server(tokio::task::JoinHandle<()>);
struct SpeechCodec;
struct SpeechEncoder;
struct SpeechDecoder;
impl tonic::codec::Encoder for SpeechEncoder {
    type Item = stt::StreamingRecognizeResponse;
    type Error = Status;
    fn encode(
        &mut self,
        value: Self::Item,
        buffer: &mut tonic::codec::EncodeBuf<'_>,
    ) -> Result<(), Status> {
        prost::Message::encode(&value, buffer).map_err(|_| Status::internal("fixture response"))
    }
}
impl tonic::codec::Decoder for SpeechDecoder {
    type Item = stt::StreamingRecognizeRequest;
    type Error = Status;
    fn decode(
        &mut self,
        buffer: &mut tonic::codec::DecodeBuf<'_>,
    ) -> Result<Option<Self::Item>, Status> {
        prost::Message::decode(buffer)
            .map(Some)
            .map_err(|_| Status::invalid_argument("fixture request"))
    }
}
impl tonic::codec::Codec for SpeechCodec {
    type Encode = stt::StreamingRecognizeResponse;
    type Decode = stt::StreamingRecognizeRequest;
    type Encoder = SpeechEncoder;
    type Decoder = SpeechDecoder;
    fn encoder(&mut self) -> SpeechEncoder {
        SpeechEncoder
    }
    fn decoder(&mut self) -> SpeechDecoder {
        SpeechDecoder
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn start(finish: bool) -> (String, Arc<AtomicUsize>, Server) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let packets = Arc::new(AtomicUsize::new(0));
    let service = DelayedSpeech {
        finish,
        packets: packets.clone(),
    };
    let task = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    (endpoint, packets, Server(task))
}
fn limits() -> Limits {
    Limits {
        idle: Duration::from_millis(150),
        input_duration: Duration::from_secs(2),
        total_duration: Duration::from_secs(3),
        ..Limits::default()
    }
}

#[tokio::test]
async fn snapshot_wait_wordless_interims_and_silent_finalization_collect_after_idle() {
    use crate::routine::media::snapshot::{Workspace, WorkspacePool};
    let (endpoint, packets, _server) = start(true).await;
    let s = streams(limits());
    let (mut writer, input) = s.open(Direction::Input, pcm(), false).unwrap();
    let workspace = Workspace::new(&input, WorkspacePool::principal(), WorkspacePool::process());
    let (owner, reader) = backend(&endpoint, true)
        .transcribe_snapshot(input, workspace.clone())
        .unwrap();
    let feed = async move {
        for _ in 0..10 {
            writer
                .write(writer.key(), Chunk::Audio(vec![0, 0]))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        writer.close(writer.key()).await.unwrap();
    };
    let ((), result, note) = tokio::join!(feed, joined(owner), reader.collect(1024));
    result.unwrap();
    let note = note.unwrap();
    assert!(note.committed());
    assert_eq!(note.generation(), 1);
    assert_eq!(note.chunks().collect::<String>(), "S: hello");
    assert_eq!(packets.load(Ordering::SeqCst), 10);
    drop(note);
    assert_eq!(workspace.retained(), 0);
}

#[tokio::test]
async fn snapshot_wait_silent_finalization_without_interims_succeeds() {
    use crate::routine::media::snapshot::{Workspace, WorkspacePool};
    let (endpoint, _, _server) = start(true).await;
    let s = streams(limits());
    let (mut writer, input) = s.open(Direction::Input, pcm(), false).unwrap();
    let workspace = Workspace::new(&input, WorkspacePool::principal(), WorkspacePool::process());
    let (owner, reader) = backend(&endpoint, true)
        .transcribe_snapshot(input, workspace)
        .unwrap();
    writer.close(writer.key()).await.unwrap();
    let (result, note) = tokio::join!(joined(owner), reader.collect(1024));
    result.unwrap();
    assert!(note.unwrap().committed());
}

#[tokio::test]
async fn snapshot_wait_never_finishes_uses_original_absolute_deadline() {
    use crate::routine::media::snapshot::{Workspace, WorkspacePool};
    let (endpoint, _, _server) = start(false).await;
    let s = streams(Limits {
        input_duration: Duration::from_millis(300),
        total_duration: Duration::from_millis(700),
        ..limits()
    });
    let began = tokio::time::Instant::now();
    let (mut writer, input) = s.open(Direction::Input, pcm(), false).unwrap();
    let workspace = Workspace::new(&input, WorkspacePool::principal(), WorkspacePool::process());
    let (owner, reader) = backend(&endpoint, true)
        .transcribe_snapshot(input, workspace.clone())
        .unwrap();
    writer.close(writer.key()).await.unwrap();
    let (result, note) = tokio::join!(joined(owner), reader.collect(1024));
    assert!(result.is_err());
    assert!(matches!(note, Err(Error::Deadline | Error::Cancelled)));
    assert!(
        began.elapsed() >= Duration::from_millis(650),
        "must not expire at idle"
    );
    assert!(began.elapsed() < Duration::from_secs(2));
    assert_eq!(workspace.retained(), 0);
}

#[tokio::test]
async fn snapshot_wait_cancel_and_consumer_drop_join_before_final() {
    use crate::routine::media::snapshot::{Workspace, WorkspacePool};
    for consumer_drop in [false, true] {
        let (endpoint, packets, _server) = start(false).await;
        let s = streams(limits());
        let (mut writer, input) = s.open(Direction::Input, pcm(), false).unwrap();
        let workspace =
            Workspace::new(&input, WorkspacePool::principal(), WorkspacePool::process());
        let (owner, reader) = backend(&endpoint, true)
            .transcribe_snapshot(input, workspace.clone())
            .unwrap();
        writer
            .write(writer.key(), Chunk::Audio(vec![0, 0]))
            .await
            .unwrap();
        writer.close(writer.key()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while packets.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if consumer_drop {
            drop(reader);
        } else {
            owner.cancel();
            assert!(reader.collect(1024).await.is_err());
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(500), owner.join())
                .await
                .unwrap()
                .is_err()
        );
        assert_eq!(workspace.retained(), 0);
    }
}

#[tokio::test]
async fn snapshot_wait_preserves_input_idle() {
    use crate::routine::media::snapshot::{Workspace, WorkspacePool};
    let (endpoint, _, _server) = start(false).await;
    let s = streams(limits());
    let (_writer, input) = s.open(Direction::Input, pcm(), false).unwrap();
    let workspace = Workspace::new(&input, WorkspacePool::principal(), WorkspacePool::process());
    let (owner, reader) = backend(&endpoint, true)
        .transcribe_snapshot(input, workspace.clone())
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), owner.join())
            .await
            .unwrap(),
        Err(Error::Deadline)
    );
    assert!(reader.collect(1024).await.is_err());
    assert_eq!(workspace.retained(), 0);
}

#[tokio::test]
async fn snapshot_wait_does_not_relax_ordinary_stt_or_tts_response_idle() {
    let (_, server) = fixtures::start(fixtures::Mode::Stall, Duration::from_secs(5)).await;
    for synthesis in [false, true] {
        let s = streams(limits());
        let (mut writer, input) = s
            .open(
                Direction::Input,
                if synthesis { Format::Text } else { pcm() },
                false,
            )
            .unwrap();
        let (output, _reader) = s
            .open(
                Direction::Output,
                if synthesis { pcm() } else { Format::Text },
                false,
            )
            .unwrap();
        writer.close(writer.key()).await.unwrap();
        let speech = backend(&server.endpoint, false);
        let owner = if synthesis {
            speech.synthesize(input, output)
        } else {
            speech.transcribe(input, output)
        }
        .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), owner.join())
                .await
                .unwrap(),
            Err(Error::Deadline)
        );
    }
}
