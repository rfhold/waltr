#![allow(clippy::wildcard_imports)]
use futures_util::Stream;
use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::oneshot;
use tonic::{Request, Response, Status};
use waltr_audio::{client::Client, config::Config, stt, tts};

#[derive(Clone, Copy)]
#[allow(dead_code)] // Shared with server media adapter tests.
pub enum Mode {
    Ok,
    Fail,
    #[allow(dead_code)]
    TerminalFail,
    Stall,
    EarlyEof,
    Media,
    Diarized,
    DiarizedPortions,
    DiarizedPortionsEarlyEof,
    DiarizedPortionsFail,
    OddPcm,
    IncompletePcm,
}
pub struct Fake {
    mode: Mode,
    backpressure: Arc<Mutex<Option<BackpressureService>>>,
    speech_capture: Option<SpeechCapture>,
    synthesis_capture: SynthesisCapture,
}
struct BackpressureService {
    release: oneshot::Receiver<()>,
    stopped: Option<oneshot::Sender<()>>,
}
impl Drop for BackpressureService {
    fn drop(&mut self) {
        if let Some(stopped) = self.stopped.take() {
            let _sent = stopped.send(());
        }
    }
}
pub struct Backpressure {
    pub release: oneshot::Sender<()>,
    pub stopped: oneshot::Receiver<()>,
}
pub async fn start_backpressure(timeout: Duration) -> (Client, Server, Backpressure) {
    let (release, released) = oneshot::channel();
    let (stopped, stopping) = oneshot::channel();
    let (client, server) = start_inner(
        Mode::Ok,
        timeout,
        |_| {},
        Some(BackpressureService {
            release: released,
            stopped: Some(stopped),
        }),
        None,
    )
    .await;
    (
        client,
        server,
        Backpressure {
            release,
            stopped: stopping,
        },
    )
}
pub struct Server {
    #[allow(dead_code)] // Shared with server media adapter tests.
    pub synthesis_capture: SynthesisCapture,
    #[allow(dead_code)] // Shared with server media adapter tests.
    pub endpoint: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub async fn start(mode: Mode, timeout: Duration) -> (Client, Server) {
    start_with(mode, timeout, |_| {}).await
}
pub async fn start_with(
    mode: Mode,
    timeout: Duration,
    adjust: impl FnOnce(&mut Config),
) -> (Client, Server) {
    start_inner(mode, timeout, adjust, None, None).await
}
pub type SpeechCapture = Arc<Mutex<Vec<stt::StreamingRecognizeRequest>>>;
pub type SynthesisCapture = Arc<Mutex<Vec<tts::StreamingSynthesizeConfig>>>;
pub async fn start_speech_capture(
    adjust: impl FnOnce(&mut Config),
) -> (Client, Server, SpeechCapture) {
    let capture = SpeechCapture::default();
    let (client, server) = start_inner(
        Mode::Ok,
        Duration::from_secs(5),
        adjust,
        None,
        Some(capture.clone()),
    )
    .await;
    (client, server, capture)
}
async fn start_inner(
    mode: Mode,
    timeout: Duration,
    adjust: impl FnOnce(&mut Config),
    backpressure: Option<BackpressureService>,
    speech_capture: Option<SpeechCapture>,
) -> (Client, Server) {
    start_inner_traced(mode, timeout, adjust, (backpressure, speech_capture, None)).await
}
pub type MetadataCapture = Arc<Mutex<Vec<tonic::metadata::MetadataMap>>>;
#[allow(dead_code)]
pub async fn start_trace_capture(mode: Mode) -> (Client, Server, MetadataCapture) {
    let metadata = MetadataCapture::default();
    let (client, server) = start_inner_traced(
        mode,
        Duration::from_secs(5),
        |_| {},
        (None, None, Some(metadata.clone())),
    )
    .await;
    (client, server, metadata)
}
async fn start_inner_traced(
    mode: Mode,
    timeout: Duration,
    adjust: impl FnOnce(&mut Config),
    captures: (
        Option<BackpressureService>,
        Option<SpeechCapture>,
        Option<MetadataCapture>,
    ),
) -> (Client, Server) {
    let (backpressure, speech_capture, metadata) = captures;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = Config::new(format!("http://{}", listener.local_addr().unwrap()));
    config.request_timeout = timeout;
    config.queue_chunks = 1;
    config.queue_bytes = 128;
    adjust(&mut config);
    let backpressure = Arc::new(Mutex::new(backpressure));
    let synthesis_capture = SynthesisCapture::default();
    let captured_synthesis = synthesis_capture.clone();
    let task = tokio::spawn(async move {
        let intercept = move |request: Request<()>| {
            if let Some(metadata) = &metadata {
                metadata.lock().unwrap().push(request.metadata().clone());
            }
            Ok(request)
        };
        tonic::transport::Server::builder()
            .add_service(stt::speech_server::SpeechServer::with_interceptor(
                Fake {
                    mode,
                    backpressure: backpressure.clone(),
                    speech_capture: speech_capture.clone(),
                    synthesis_capture: captured_synthesis.clone(),
                },
                intercept.clone(),
            ))
            .add_service(
                tts::text_to_speech_server::TextToSpeechServer::with_interceptor(
                    Fake {
                        mode,
                        backpressure,
                        speech_capture,
                        synthesis_capture: captured_synthesis,
                    },
                    intercept,
                ),
            )
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    let endpoint = config.uri.clone();
    (
        Client::connect(config).await.unwrap(),
        Server {
            endpoint,
            task,
            synthesis_capture,
        },
    )
}
async fn behavior(mode: Mode) -> Result<(), Status> {
    match mode {
        Mode::Ok
        | Mode::EarlyEof
        | Mode::TerminalFail
        | Mode::Media
        | Mode::Diarized
        | Mode::DiarizedPortions
        | Mode::DiarizedPortionsEarlyEof
        | Mode::DiarizedPortionsFail
        | Mode::OddPcm
        | Mode::IncompletePcm => Ok(()),
        Mode::Fail => Err(Status::with_details(
            tonic::Code::ResourceExhausted,
            "fake quota",
            vec![1, 2, 3].into(),
        )),
        Mode::Stall => std::future::pending().await,
    }
}
type Output<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;
fn diarized_portions(
    input: tonic::Streaming<stt::StreamingRecognizeRequest>,
    mode: Mode,
) -> Output<stt::StreamingRecognizeResponse> {
    Box::pin(futures_util::stream::try_unfold(
        (input, 0),
        move |(mut input, step)| async move {
            if step == 4 || (step == 3 && matches!(mode, Mode::DiarizedPortionsEarlyEof)) {
                return Ok(None);
            }
            if step == 3 {
                while input.message().await?.is_some() {}
                if matches!(mode, Mode::DiarizedPortionsFail) {
                    return Err(Status::unavailable("after portion finals"));
                }
            }
            let (transcript, final_result, words): (&str, bool, Vec<(&str, &str)>) = match step {
                0 => ("hello", true, vec![("hello", "S1")]),
                1 => ("wor...", false, vec![("hello", "S2"), ("world", "S2")]),
                2 => ("world", true, vec![("hello", "S2"), ("world", "S3")]),
                _ => (
                    "again",
                    true,
                    vec![("hello", "S2"), ("world", "S3"), ("again", "S3")],
                ),
            };
            let response = stt::StreamingRecognizeResponse {
                results: vec![stt::StreamingRecognitionResult {
                    is_final: final_result,
                    channel_tag: 7,
                    alternatives: vec![stt::SpeechRecognitionAlternative {
                        transcript: transcript.into(),
                        words: words
                            .into_iter()
                            .map(|(word, speaker)| stt::WordInfo {
                                word: word.into(),
                                speaker_label: speaker.into(),
                                ..Default::default()
                            })
                            .collect(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            };
            Ok(Some((response, (input, step + 1))))
        },
    ))
}
fn media_transcripts(
    input: tonic::Streaming<stt::StreamingRecognizeRequest>,
    diarized: bool,
) -> Output<stt::StreamingRecognizeResponse> {
    Box::pin(futures_util::stream::try_unfold(
        (input, false),
        move |(mut input, done)| async move {
            if done {
                return Ok(None);
            }
            let final_result = input.message().await?.is_none();
            let response = stt::StreamingRecognizeResponse {
                results: vec![stt::StreamingRecognitionResult {
                    is_final: final_result,
                    channel_tag: 2,
                    alternatives: vec![stt::SpeechRecognitionAlternative {
                        transcript: "hello world".into(),
                        words: if diarized {
                            vec![
                                stt::WordInfo {
                                    word: "hello".into(),
                                    speaker_label: "S1".into(),
                                    ..Default::default()
                                },
                                stt::WordInfo {
                                    word: "world".into(),
                                    speaker_label: "S2".into(),
                                    ..Default::default()
                                },
                            ]
                        } else {
                            vec![]
                        },
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            };
            Ok(Some((response, (input, final_result))))
        },
    ))
}
fn media_synthesis(
    input: tonic::Streaming<tts::StreamingSynthesizeRequest>,
    mode: Mode,
) -> Output<tts::StreamingSynthesizeResponse> {
    Box::pin(futures_util::stream::try_unfold(
        (input, false),
        move |(mut input, done)| async move {
            if done {
                return Ok(None);
            }
            if input.message().await?.is_some() {
                let bytes = if matches!(mode, Mode::OddPcm | Mode::IncompletePcm) {
                    vec![1]
                } else {
                    vec![1, 0]
                };
                Ok(Some((
                    tts::StreamingSynthesizeResponse {
                        audio_content: bytes,
                    },
                    (input, false),
                )))
            } else if matches!(mode, Mode::OddPcm) {
                Ok(Some((
                    tts::StreamingSynthesizeResponse {
                        audio_content: vec![0],
                    },
                    (input, true),
                )))
            } else {
                Ok(None)
            }
        },
    ))
}
#[tonic::async_trait]
impl stt::speech_server::Speech for Fake {
    async fn recognize(
        &self,
        request: Request<stt::RecognizeRequest>,
    ) -> Result<Response<stt::RecognizeResponse>, Status> {
        behavior(self.mode).await?;
        let request = request.into_inner();
        assert_eq!(request.recognizer, "recognizer");
        assert!(
            matches!(request.audio_source, Some(stt::recognize_request::AudioSource::Content(ref bytes)) if bytes == &[0, 0, 1, 0])
        );
        assert!(
            matches!(request.config.unwrap().decoding_config, Some(stt::recognition_config::DecodingConfig::ExplicitDecodingConfig(config)) if config.sample_rate_hertz == 16000 && config.audio_channel_count == 1)
        );
        Ok(Response::new(stt::RecognizeResponse {
            results: vec![stt::SpeechRecognitionResult {
                channel_tag: 1,
                language_code: "en-US".into(),
                alternatives: vec![stt::SpeechRecognitionAlternative {
                    transcript: "hello".into(),
                    words: vec![stt::WordInfo {
                        speaker_label: "request-local".into(),
                        word: "hello".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }))
    }
    type StreamingRecognizeStream = Output<stt::StreamingRecognizeResponse>;
    async fn streaming_recognize(
        &self,
        request: Request<tonic::Streaming<stt::StreamingRecognizeRequest>>,
    ) -> Result<Response<Self::StreamingRecognizeStream>, Status> {
        let mut input = request.into_inner();
        let first = input.message().await?.unwrap();
        if let Some(capture) = &self.speech_capture {
            capture.lock().unwrap().push(first);
            let capture = capture.clone();
            let output = futures_util::stream::once(async move {
                while let Some(request) = input.message().await? {
                    match &request.streaming_request {
                        Some(stt::streaming_recognize_request::StreamingRequest::Audio(bytes))
                            if bytes.len() <= 15_360 && request.recognizer.is_empty() => {}
                        _ => {
                            return Err(Status::resource_exhausted(
                                "invalid streaming audio frame",
                            ));
                        }
                    }
                    capture.lock().unwrap().push(request);
                }
                Ok(stt::StreamingRecognizeResponse::default())
            });
            return Ok(Response::new(Box::pin(output)));
        }
        assert_eq!(first.recognizer, "recognizer");
        assert!(
            matches!(first.streaming_request, Some(stt::streaming_recognize_request::StreamingRequest::StreamingConfig(config)) if config.config.is_some())
        );
        behavior(self.mode).await?;
        if matches!(self.mode, Mode::EarlyEof) {
            return Ok(Response::new(Box::pin(futures_util::stream::empty())));
        }
        if matches!(
            self.mode,
            Mode::DiarizedPortions | Mode::DiarizedPortionsEarlyEof | Mode::DiarizedPortionsFail
        ) {
            return Ok(Response::new(diarized_portions(input, self.mode)));
        }
        if matches!(self.mode, Mode::Media | Mode::Diarized) {
            return Ok(Response::new(media_transcripts(
                input,
                matches!(self.mode, Mode::Diarized),
            )));
        }
        let output = futures_util::stream::unfold((input, 0), |(mut input, step)| async move {
            if step == 2 {
                return None;
            }
            if step == 1 {
                while let Some(next) = input.message().await.unwrap() {
                    assert!(next.recognizer.is_empty());
                    assert!(
                        matches!(next.streaming_request, Some(stt::streaming_recognize_request::StreamingRequest::Audio(bytes)) if bytes.len() == 4)
                    );
                }
            }
            Some((
                Ok(stt::StreamingRecognizeResponse {
                    results: vec![stt::StreamingRecognitionResult {
                        is_final: step == 1,
                        channel_tag: 2,
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                (input, step + 1),
            ))
        });
        Ok(Response::new(Box::pin(output)))
    }
    async fn create_recognizer(
        &self,
        _request: tonic::Request<stt::CreateRecognizerRequest>,
    ) -> std::result::Result<
        tonic::Response<waltr::grpc::google::longrunning::Operation>,
        tonic::Status,
    > {
        Err(Status::unimplemented("unused test RPC"))
    }
    async fn list_recognizers(
        &self,
        _request: tonic::Request<stt::ListRecognizersRequest>,
    ) -> std::result::Result<tonic::Response<stt::ListRecognizersResponse>, tonic::Status> {
        Err(Status::unimplemented("unused test RPC"))
    }
    async fn get_recognizer(
        &self,
        _request: tonic::Request<stt::GetRecognizerRequest>,
    ) -> std::result::Result<tonic::Response<stt::Recognizer>, tonic::Status> {
        Err(Status::unimplemented("unused test RPC"))
    }
    async fn update_recognizer(
        &self,
        _request: tonic::Request<stt::UpdateRecognizerRequest>,
    ) -> std::result::Result<
        tonic::Response<waltr::grpc::google::longrunning::Operation>,
        tonic::Status,
    > {
        Err(Status::unimplemented("unused test RPC"))
    }
    async fn delete_recognizer(
        &self,
        _request: tonic::Request<stt::DeleteRecognizerRequest>,
    ) -> std::result::Result<
        tonic::Response<waltr::grpc::google::longrunning::Operation>,
        tonic::Status,
    > {
        Err(Status::unimplemented("unused test RPC"))
    }
    async fn undelete_recognizer(
        &self,
        _request: tonic::Request<stt::UndeleteRecognizerRequest>,
    ) -> std::result::Result<
        tonic::Response<waltr::grpc::google::longrunning::Operation>,
        tonic::Status,
    > {
        Err(Status::unimplemented("unused test RPC"))
    }
    async fn batch_recognize(
        &self,
        _request: tonic::Request<stt::BatchRecognizeRequest>,
    ) -> std::result::Result<
        tonic::Response<waltr::grpc::google::longrunning::Operation>,
        tonic::Status,
    > {
        Err(Status::unimplemented("unused test RPC"))
    }
    async fn get_config(
        &self,
        _request: tonic::Request<stt::GetConfigRequest>,
    ) -> std::result::Result<tonic::Response<stt::Config>, tonic::Status> {
        Err(Status::unimplemented("unused test RPC"))
    }
    async fn update_config(
        &self,
        _request: tonic::Request<stt::UpdateConfigRequest>,
    ) -> std::result::Result<tonic::Response<stt::Config>, tonic::Status> {
        Err(Status::unimplemented("unused test RPC"))
    }
    async fn create_custom_class(
        &self,
        _request: tonic::Request<stt::CreateCustomClassRequest>,
    ) -> std::result::Result<
        tonic::Response<waltr::grpc::google::longrunning::Operation>,
        tonic::Status,
    > {
        Err(Status::unimplemented("unused test RPC"))
    }
    async fn list_custom_classes(
        &self,
        _request: tonic::Request<stt::ListCustomClassesRequest>,
    ) -> std::result::Result<tonic::Response<stt::ListCustomClassesResponse>, tonic::Status> {
        Err(Status::unimplemented("unused test RPC"))
    }
    async fn get_custom_class(
        &self,
        _request: tonic::Request<stt::GetCustomClassRequest>,
    ) -> std::result::Result<tonic::Response<stt::CustomClass>, tonic::Status> {
        Err(Status::unimplemented("unused test RPC"))
    }
    async fn update_custom_class(
        &self,
        _request: tonic::Request<stt::UpdateCustomClassRequest>,
    ) -> std::result::Result<
        tonic::Response<waltr::grpc::google::longrunning::Operation>,
        tonic::Status,
    > {
        Err(Status::unimplemented("unused test RPC"))
    }
    async fn delete_custom_class(
        &self,
        _request: tonic::Request<stt::DeleteCustomClassRequest>,
    ) -> std::result::Result<
        tonic::Response<waltr::grpc::google::longrunning::Operation>,
        tonic::Status,
    > {
        Err(Status::unimplemented("unused test RPC"))
    }
    async fn undelete_custom_class(
        &self,
        _request: tonic::Request<stt::UndeleteCustomClassRequest>,
    ) -> std::result::Result<
        tonic::Response<waltr::grpc::google::longrunning::Operation>,
        tonic::Status,
    > {
        Err(Status::unimplemented("unused test RPC"))
    }
    async fn create_phrase_set(
        &self,
        _request: tonic::Request<stt::CreatePhraseSetRequest>,
    ) -> std::result::Result<
        tonic::Response<waltr::grpc::google::longrunning::Operation>,
        tonic::Status,
    > {
        Err(Status::unimplemented("unused test RPC"))
    }
    async fn list_phrase_sets(
        &self,
        _request: tonic::Request<stt::ListPhraseSetsRequest>,
    ) -> std::result::Result<tonic::Response<stt::ListPhraseSetsResponse>, tonic::Status> {
        Err(Status::unimplemented("unused test RPC"))
    }
    async fn get_phrase_set(
        &self,
        _request: tonic::Request<stt::GetPhraseSetRequest>,
    ) -> std::result::Result<tonic::Response<stt::PhraseSet>, tonic::Status> {
        Err(Status::unimplemented("unused test RPC"))
    }
    async fn update_phrase_set(
        &self,
        _request: tonic::Request<stt::UpdatePhraseSetRequest>,
    ) -> std::result::Result<
        tonic::Response<waltr::grpc::google::longrunning::Operation>,
        tonic::Status,
    > {
        Err(Status::unimplemented("unused test RPC"))
    }
    async fn delete_phrase_set(
        &self,
        _request: tonic::Request<stt::DeletePhraseSetRequest>,
    ) -> std::result::Result<
        tonic::Response<waltr::grpc::google::longrunning::Operation>,
        tonic::Status,
    > {
        Err(Status::unimplemented("unused test RPC"))
    }
    async fn undelete_phrase_set(
        &self,
        _request: tonic::Request<stt::UndeletePhraseSetRequest>,
    ) -> std::result::Result<
        tonic::Response<waltr::grpc::google::longrunning::Operation>,
        tonic::Status,
    > {
        Err(Status::unimplemented("unused test RPC"))
    }
}
#[tonic::async_trait]
impl tts::text_to_speech_server::TextToSpeech for Fake {
    async fn list_voices(
        &self,
        _request: Request<tts::ListVoicesRequest>,
    ) -> Result<Response<tts::ListVoicesResponse>, Status> {
        Err(Status::unimplemented("unused test RPC"))
    }
    async fn synthesize_speech(
        &self,
        request: Request<tts::SynthesizeSpeechRequest>,
    ) -> Result<Response<tts::SynthesizeSpeechResponse>, Status> {
        behavior(self.mode).await?;
        assert!(
            matches!(request.into_inner().input.unwrap().input_source, Some(tts::synthesis_input::InputSource::Text(text)) if text == "hello")
        );
        Ok(Response::new(tts::SynthesizeSpeechResponse {
            audio_content: b"RIFFfake WAV".to_vec(),
        }))
    }
    type StreamingSynthesizeStream = Output<tts::StreamingSynthesizeResponse>;
    async fn streaming_synthesize(
        &self,
        request: Request<tonic::Streaming<tts::StreamingSynthesizeRequest>>,
    ) -> Result<Response<Self::StreamingSynthesizeStream>, Status> {
        let mut input = request.into_inner();
        let Some(tts::streaming_synthesize_request::StreamingRequest::StreamingConfig(config)) =
            input.message().await?.unwrap().streaming_request
        else {
            panic!("synthesis configuration required")
        };
        self.synthesis_capture.lock().unwrap().push(config);
        behavior(self.mode).await?;
        let backpressure = self.backpressure.lock().unwrap().take();
        if matches!(self.mode, Mode::EarlyEof) {
            return Ok(Response::new(Box::pin(futures_util::stream::empty())));
        }
        if matches!(self.mode, Mode::Media | Mode::OddPcm | Mode::IncompletePcm) {
            return Ok(Response::new(media_synthesis(input, self.mode)));
        }
        if let Some(service) = backpressure {
            let output = futures_util::stream::unfold(
                (input, service, 0),
                |(input, mut service, step)| async move {
                    if step == 0 {
                        (&mut service.release).await.unwrap();
                    }
                    if step == 2 {
                        std::future::pending::<()>().await;
                    }
                    Some((
                        Ok(tts::StreamingSynthesizeResponse {
                            audio_content: vec![step, 0],
                        }),
                        (input, service, step + 1),
                    ))
                },
            );
            return Ok(Response::new(Box::pin(output)));
        }
        let output = futures_util::stream::unfold((input, 0), |(mut input, step)| async move {
            if step == 2 {
                return None;
            }
            if step == 1 {
                while let Some(next) = input.message().await.unwrap() {
                    assert!(matches!(
                        next.streaming_request,
                        Some(tts::streaming_synthesize_request::StreamingRequest::Input(
                            _
                        ))
                    ));
                }
            }
            Some((
                Ok(tts::StreamingSynthesizeResponse {
                    audio_content: vec![step, 0],
                }),
                (input, step + 1),
            ))
        });
        if matches!(self.mode, Mode::TerminalFail) {
            let output = futures_util::StreamExt::chain(
                output,
                futures_util::stream::once(async {
                    Err(Status::resource_exhausted("SECRET_TRAILER_FAILURE"))
                }),
            );
            return Ok(Response::new(Box::pin(output)));
        }
        Ok(Response::new(Box::pin(output)))
    }
}
