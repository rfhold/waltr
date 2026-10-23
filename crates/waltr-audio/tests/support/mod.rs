#![allow(clippy::wildcard_imports)]
use futures_core::Stream;
use waltr_audio::{client::Client, config::Config, stt, tts};
use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::oneshot;
use tonic::{Request, Response, Status};

#[derive(Clone, Copy)]
pub enum Mode {
    Ok,
    Fail,
    #[allow(dead_code)]
    TerminalFail,
    Stall,
    EarlyEof,
}
pub struct Fake {
    mode: Mode,
    backpressure: Arc<Mutex<Option<BackpressureService>>>,
    speech_capture: Option<SpeechCapture>,
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
    start_inner_traced(mode, timeout, adjust, backpressure, speech_capture, None).await
}
pub type MetadataCapture = Arc<Mutex<Vec<tonic::metadata::MetadataMap>>>;
#[allow(dead_code)]
pub async fn start_trace_capture(mode: Mode) -> (Client, Server, MetadataCapture) {
    let metadata = MetadataCapture::default();
    let (client, server) = start_inner_traced(
        mode,
        Duration::from_secs(5),
        |_| {},
        None,
        None,
        Some(metadata.clone()),
    )
    .await;
    (client, server, metadata)
}
async fn start_inner_traced(
    mode: Mode,
    timeout: Duration,
    adjust: impl FnOnce(&mut Config),
    backpressure: Option<BackpressureService>,
    speech_capture: Option<SpeechCapture>,
    metadata: Option<MetadataCapture>,
) -> (Client, Server) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = Config::new(format!("http://{}", listener.local_addr().unwrap()));
    config.request_timeout = timeout;
    config.queue_chunks = 1;
    config.queue_bytes = 128;
    adjust(&mut config);
    let backpressure = Arc::new(Mutex::new(backpressure));
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
                },
                intercept.clone(),
            ))
            .add_service(
                tts::text_to_speech_server::TextToSpeechServer::with_interceptor(
                    Fake {
                        mode,
                        backpressure,
                        speech_capture,
                    },
                    intercept,
                ),
            )
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    (Client::connect(config).await.unwrap(), Server { task })
}
async fn behavior(mode: Mode) -> Result<(), Status> {
    match mode {
        Mode::Ok | Mode::EarlyEof | Mode::TerminalFail => Ok(()),
        Mode::Fail => Err(Status::with_details(
            tonic::Code::ResourceExhausted,
            "fake quota",
            vec![1, 2, 3].into(),
        )),
        Mode::Stall => std::future::pending().await,
    }
}
type Output<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;
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
        assert!(matches!(
            input.message().await?.unwrap().streaming_request,
            Some(tts::streaming_synthesize_request::StreamingRequest::StreamingConfig(_))
        ));
        behavior(self.mode).await?;
        let backpressure = self.backpressure.lock().unwrap().take();
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
