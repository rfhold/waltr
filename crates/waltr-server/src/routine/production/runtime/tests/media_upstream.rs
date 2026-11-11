use futures_util::Stream;
use prost::Message;
use std::{
    marker::PhantomData,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};
use tokio_util::sync::CancellationToken;
use tonic::{
    Request, Response, Status,
    codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder},
    codegen::{Body, BoxFuture, Service, StdError, http},
};
use waltr_audio::{stt, tts};

#[derive(Default)]
pub(super) struct Metrics {
    pub bytes: AtomicU64,
    pub largest_frame: AtomicUsize,
    pub live: AtomicUsize,
    pub speech_calls: AtomicUsize,
    pub recognition_configs: std::sync::Mutex<Vec<(String, bool)>>,
    pub synthesis_calls: AtomicUsize,
    pub synthesis_models: std::sync::Mutex<Vec<String>>,
    pub synthesis_text: std::sync::Mutex<Vec<String>>,
    pub expected_synthesis_text: std::sync::Mutex<Option<String>>,
    pub dropped: tokio::sync::Notify,
    pub pair: std::sync::Mutex<Option<Arc<tokio::sync::Barrier>>>,
    pub peak_live: AtomicUsize,
    pub fail_final: AtomicBool,
}
struct Live(Arc<Metrics>);
impl Drop for Live {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::SeqCst);
        self.0.dropped.notify_one();
    }
}
pub(super) struct Upstream {
    pub endpoint: String,
    pub metrics: Arc<Metrics>,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}
impl Upstream {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let metrics = Arc::new(Metrics::default());
        let stop = CancellationToken::new();
        let end = stop.clone();
        let state = metrics.clone();
        let task = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(Speech(state.clone()))
                .add_service(Synthesis(state))
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(listener),
                    end.cancelled(),
                )
                .await
                .unwrap();
        });
        Self {
            endpoint,
            metrics,
            stop,
            task,
        }
    }
    pub async fn shutdown(self) {
        assert_eq!(self.metrics.live.load(Ordering::SeqCst), 0);
        self.stop.cancel();
        self.task.await.unwrap();
    }
}
struct ProtoCodec<A, B>(PhantomData<(A, B)>);
impl<A, B> Default for ProtoCodec<A, B> {
    fn default() -> Self {
        Self(PhantomData)
    }
}
struct Enc<T>(PhantomData<T>);
struct Dec<T>(PhantomData<T>);
impl<T: Message> Encoder for Enc<T> {
    type Item = T;
    type Error = Status;
    fn encode(&mut self, item: T, buf: &mut EncodeBuf<'_>) -> Result<(), Status> {
        item.encode(buf)
            .map_err(|_| Status::internal("fixture encoding"))
    }
}
impl<T: Message + Default> Decoder for Dec<T> {
    type Item = T;
    type Error = Status;
    fn decode(&mut self, buf: &mut DecodeBuf<'_>) -> Result<Option<T>, Status> {
        T::decode(buf)
            .map(Some)
            .map_err(|_| Status::invalid_argument("fixture decoding"))
    }
}
impl<A: Message + Default + Send + 'static, B: Message + Default + Send + 'static> Codec
    for ProtoCodec<A, B>
{
    type Encode = B;
    type Decode = A;
    type Encoder = Enc<B>;
    type Decoder = Dec<A>;
    fn encoder(&mut self) -> Self::Encoder {
        Enc(PhantomData)
    }
    fn decoder(&mut self) -> Self::Decoder {
        Dec(PhantomData)
    }
}
type Output<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;
#[derive(Clone)]
struct Speech(Arc<Metrics>);
#[derive(Clone)]
struct Synthesis(Arc<Metrics>);
macro_rules! service {
    ($name:ident,$service:literal,$input:ty,$output:ty) => {
        impl tonic::server::NamedService for $name {
            const NAME: &'static str = $service;
        }
        impl<B> Service<http::Request<B>> for $name
        where
            B: Body + Send + 'static,
            B::Error: Into<StdError> + Send + 'static,
        {
            type Response = http::Response<tonic::body::Body>;
            type Error = std::convert::Infallible;
            type Future = BoxFuture<Self::Response, Self::Error>;
            fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }
            fn call(&mut self, request: http::Request<B>) -> Self::Future {
                let this = self.clone();
                Box::pin(async move {
                    Ok(
                        tonic::server::Grpc::new(ProtoCodec::<$input, $output>::default())
                            .streaming(this, request)
                            .await,
                    )
                })
            }
        }
    };
}
service!(
    Speech,
    "google.cloud.speech.v2.Speech",
    stt::StreamingRecognizeRequest,
    stt::StreamingRecognizeResponse
);
service!(
    Synthesis,
    "google.cloud.texttospeech.v1.TextToSpeech",
    tts::StreamingSynthesizeRequest,
    tts::StreamingSynthesizeResponse
);
fn transcript(text: &str, final_text: bool) -> stt::StreamingRecognizeResponse {
    stt::StreamingRecognizeResponse {
        results: vec![stt::StreamingRecognitionResult {
            is_final: final_text,
            alternatives: vec![stt::SpeechRecognitionAlternative {
                transcript: text.into(),
                words: text
                    .split_whitespace()
                    .map(|word| stt::WordInfo {
                        word: word.into(),
                        speaker_label: "voice-1".into(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    }
}
impl tonic::server::StreamingService<stt::StreamingRecognizeRequest> for Speech {
    type Response = stt::StreamingRecognizeResponse;
    type ResponseStream = Output<Self::Response>;
    type Future = BoxFuture<Response<Self::ResponseStream>, Status>;
    fn call(
        &mut self,
        request: Request<tonic::Streaming<stt::StreamingRecognizeRequest>>,
    ) -> Self::Future {
        let metrics = self.0.clone();
        Box::pin(async move {
            let mut input = request.into_inner();
            let first = input
                .message()
                .await?
                .ok_or_else(|| Status::invalid_argument("configuration required"))?;
            assert!(matches!(
                first.streaming_request,
                Some(stt::streaming_recognize_request::StreamingRequest::StreamingConfig(_))
            ));
            if let Some(stt::streaming_recognize_request::StreamingRequest::StreamingConfig(
                config,
            )) = first.streaming_request
            {
                let config = config.config.unwrap();
                metrics.recognition_configs.lock().unwrap().push((
                    config.model,
                    config
                        .features
                        .is_some_and(|features| features.diarization_config.is_some()),
                ));
            }
            metrics.speech_calls.fetch_add(1, Ordering::SeqCst);
            let live_count = metrics.live.fetch_add(1, Ordering::SeqCst) + 1;
            metrics.peak_live.fetch_max(live_count, Ordering::SeqCst);
            let live = Live(metrics.clone());
            let stream = futures_util::stream::try_unfold(
                (input, metrics, live, 0, 0u8),
                |(mut input, metrics, live, step, marker)| async move {
                    if step == 3 {
                        return Ok(None);
                    }
                    if step == 1 {
                        return Ok(Some((
                            transcript(
                                if marker == 1 {
                                    "source-one revised"
                                } else if marker == 2 {
                                    "source-two revised"
                                } else {
                                    "revised hypothesis"
                                },
                                false,
                            ),
                            (input, metrics, live, 2, marker),
                        )));
                    }
                    while let Some(next) = input.message().await? {
                        let Some(stt::streaming_recognize_request::StreamingRequest::Audio(bytes)) =
                            next.streaming_request
                        else {
                            return Err(Status::invalid_argument("audio required"));
                        };
                        assert!(bytes.len() <= 15360);
                        assert_eq!(bytes.len() % 2, 0);
                        metrics
                            .bytes
                            .fetch_add(bytes.len() as u64, Ordering::SeqCst);
                        metrics
                            .largest_frame
                            .fetch_max(bytes.len(), Ordering::SeqCst);
                        if step == 0 {
                            let marker = bytes[0];
                            if marker == 1 || marker == 2 {
                                let pair = metrics.pair.lock().unwrap().clone().unwrap();
                                pair.wait().await;
                            }
                            return Ok(Some((
                                transcript(
                                    if marker == 1 {
                                        "source-one first"
                                    } else if marker == 2 {
                                        "source-two first"
                                    } else {
                                        "first hypothesis"
                                    },
                                    false,
                                ),
                                (input, metrics, live, 1, marker),
                            )));
                        }
                    }
                    if metrics.fail_final.swap(false, Ordering::SeqCst) {
                        return Err(Status::unavailable("explicit fixture final failure"));
                    }
                    Ok(Some((
                        transcript(
                            if marker == 1 {
                                "source-one committed"
                            } else if marker == 2 {
                                "source-two committed"
                            } else {
                                "Committed conversation"
                            },
                            true,
                        ),
                        (input, metrics, live, 3, marker),
                    )))
                },
            );
            Ok(Response::new(Box::pin(stream) as Self::ResponseStream))
        })
    }
}
impl tonic::server::StreamingService<tts::StreamingSynthesizeRequest> for Synthesis {
    type Response = tts::StreamingSynthesizeResponse;
    type ResponseStream = Output<Self::Response>;
    type Future = BoxFuture<Response<Self::ResponseStream>, Status>;
    fn call(
        &mut self,
        request: Request<tonic::Streaming<tts::StreamingSynthesizeRequest>>,
    ) -> Self::Future {
        let metrics = self.0.clone();
        Box::pin(async move {
            let mut input = request.into_inner();
            let first = input.message().await?.unwrap();
            assert!(matches!(
                first.streaming_request,
                Some(tts::streaming_synthesize_request::StreamingRequest::StreamingConfig(_))
            ));
            if let Some(tts::streaming_synthesize_request::StreamingRequest::StreamingConfig(config)) = first.streaming_request {
                metrics.synthesis_models.lock().unwrap().push(config.voice.unwrap().model_name);
            }
            metrics.synthesis_calls.fetch_add(1, Ordering::SeqCst);
            metrics.live.fetch_add(1, Ordering::SeqCst);
            let live = Live(metrics);
            let stream = futures_util::stream::try_unfold(
                (input, live),
                |(mut input, live)| async move {
                    let Some(next) = input.message().await? else {
                        return Ok(None);
                    };
                    let Some(tts::streaming_synthesize_request::StreamingRequest::Input(value)) =
                        next.streaming_request
                    else {
                        return Err(Status::invalid_argument("text required"));
                    };
                    let Some(tts::streaming_synthesis_input::InputSource::Text(text)) = value.input_source else {
                        panic!("text required");
                    };
                    if let Some(expected) = live.0.expected_synthesis_text.lock().unwrap().as_ref() {
                        assert!(expected.contains(&text));
                    } else {
                        assert!(text == "Committed first sentence." || text == " Second sentence.");
                    }
                    live.0.synthesis_text.lock().unwrap().push(text);
                    Ok(Some((
                        tts::StreamingSynthesizeResponse {
                            audio_content: vec![1, 0, 2, 0, 3, 0, 4, 0],
                        },
                        (input, live),
                    )))
                },
            );
            Ok(Response::new(Box::pin(stream) as Self::ResponseStream))
        })
    }
}
