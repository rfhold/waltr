use super::super::*;
use crate::session::SessionHost;
use futures_util::Stream;
use std::result::Result;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use waltr_agent::*;

pub async fn request(source: &str) -> Request {
    request_with_diarization(source, false).await
}

pub async fn request_with_diarization(source: &str, diarization: bool) -> Request {
    let loader = crate::test_support::default_catalog_loader();
    let materializer = crate::catalog::CatalogMaterializer::load(&loader)
        .await
        .unwrap();
    let root = crate::test_support::temp_root();
    let resources = root.join(".waltr/resources");
    for directory in ["mcp-servers", "routines", "providers"] {
        std::fs::create_dir_all(resources.join(directory)).unwrap();
    }
    std::fs::write(resources.join("mcp-servers/waltr.yaml"), "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata: {name: waltr}\nspec: {transport: streamableHttp, url: 'https://unused.example', path: /mcp}\n").unwrap();
    let definition: crate::resource_catalog::routine::RoutineDefinition =
        serde_yaml_ng::from_str(source).unwrap();
    std::fs::write(
        resources.join(format!("routines/{}.yaml", definition.metadata.name)),
        source,
    )
    .unwrap();
    for (name, spec) in [
        (
            "speech",
            "transport: googleSpeechV2\n  url: http://127.0.0.1:1\n  models:\n    dictation:\n      name: Test\n      type: 3\n      speech_capabilities: {unary: true, streaming: true}\n      recognition: {recognizer: recognizer, model: chirp_3, language_codes: [en-US], sample_rate_hz: 24000, channels: 1, interim_results: true, automatic_punctuation: true}",
        ),
        (
            "voice",
            "transport: googleTextToSpeechV1\n  url: http://127.0.0.1:1\n  models:\n    reader:\n      name: Test\n      type: 4\n      speech_capabilities: {unary: true, streaming: true}\n      synthesis: {voice: test-voice, language_code: en-US, sample_rate_hz: 24000, channels: 1}",
        ),
        (
            "inference",
            "transport: openaiChatCompletions\n  url: http://127.0.0.1:1/v1\n  models:\n    cleanup:\n      name: Test\n      type: 1\n      limits: {context: 65536, output: 1024}",
        ),
    ] {
        let spec = if diarization && name == "speech" {
            spec.replace(
                "automatic_punctuation: true",
                "automatic_punctuation: true, diarization: true",
            )
        } else {
            spec.to_owned()
        };
        std::fs::write(resources.join(format!("providers/{name}.yaml")), format!("apiVersion: waltr.dev/v1alpha1\nkind: Provider\nmetadata: {{name: {name}}}\nspec:\n  {spec}\n")).unwrap();
    }
    let state =
        crate::resource_catalog::AgentDefinitionState::load_unified(root.clone(), materializer)
            .unwrap();
    let snapshot = state.snapshot();
    let routine = snapshot.routine(&definition.metadata.name).unwrap().clone();
    let configuration = Arc::new(snapshot.configuration().clone());
    let trigger = routine
        .definition()
        .spec
        .triggers
        .iter()
        .find(|t| t.interaction.is_some())
        .unwrap();
    let opened_inputs = trigger
        .interaction
        .as_ref()
        .unwrap()
        .input_ports
        .iter()
        .map(|p| (p.name.clone(), p.format.clone()))
        .collect();
    let trigger = trigger.name.clone();
    drop(snapshot);
    drop(state);
    std::fs::remove_dir_all(root).unwrap();
    Request {
        id: "interaction-test".into(),
        routine,
        configuration,
        trigger,
        params: BTreeMap::new(),
        opened_inputs,
        limits: InteractionLimits::default(),
        cancellation: CancellationToken::new(),
        recorder: None,
    }
}

pub async fn next(events: &mut Events) -> Event {
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("event timeout")
        .expect("event stream ended")
}

pub struct ScriptedInference {
    pub release: Arc<tokio::sync::Notify>,
}
impl LlmStream for ScriptedInference {
    fn name(&self) -> &'static str {
        "scripted_interaction"
    }
    fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        let release = self.release.clone();
        Box::pin(async move {
            Ok(Box::pin(futures_util::stream::unfold(
                (0, release),
                |(step, release)| async move {
                    let event = match step {
                        0 => CompletionEvent::Delta("hello".into()),
                        1 => {
                            release.notified().await;
                            CompletionEvent::Delta(" world".into())
                        }
                        2 => CompletionEvent::Finished,
                        _ => return None,
                    };
                    Some((Ok(event), (step + 1, release)))
                },
            )) as BoxCompletionStream)
        })
    }
}

pub struct Host {
    pub sessions: SessionCoordinator<MemorySessionStore>,
    pub created: AtomicUsize,
    pub subscribed: AtomicBool,
    pub interrupts: AtomicUsize,
}
impl Host {
    pub async fn new(
        release: Arc<tokio::sync::Notify>,
    ) -> (Arc<Self>, crate::sandbox::LocalSandbox) {
        let mut registry = AgentDefinitionRegistry::new();
        registry
            .load_yaml(
                "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {name: test}\nspec: {}\n",
            )
            .unwrap();
        let sandbox = crate::sandbox::LocalSandbox::start(std::env::temp_dir())
            .await
            .unwrap();
        let factory = AgentSessionRuntimeFactory::new(
            sandbox.handle(),
            std::env::temp_dir(),
            FileCredentialStore::new(std::env::temp_dir().join("interaction-test-auth.json")),
            registry.resolve_agent("test").unwrap(),
        )
        .with_runtime_provider(move |_| {
            Ok(AgentRuntimeProvider::new(
                ModelRef::new(
                    ProviderId::OpenAi,
                    "scripted",
                    ModelApi::OpenAiChatCompletions,
                ),
                Arc::new(ScriptedInference {
                    release: release.clone(),
                }),
            ))
        });
        (
            Arc::new(Self {
                sessions: SessionCoordinator::new(MemorySessionStore::new())
                    .with_runtime_factory(factory),
                created: AtomicUsize::new(0),
                subscribed: AtomicBool::new(false),
                interrupts: AtomicUsize::new(0),
            }),
            sandbox,
        )
    }
}
#[tonic::async_trait]
impl SessionHost for Host {
    fn set_completion_admission_handler(
        &self,
        _: Option<Arc<dyn SessionCompletionCoordinationHandler>>,
    ) {
        panic!("unused test host API")
    }
    async fn create(&self, _: Option<String>) -> Result<AgentSession, tonic::Status> {
        panic!("unused test host API")
    }
    async fn create_exact(&self, request: SessionCreateRequest) -> Result<AgentSession, LlmError> {
        self.created.fetch_add(1, Ordering::SeqCst);
        assert!(
            request
                .inference_settings
                .as_ref()
                .unwrap()
                .provider
                .reasoning_effort
                .is_none()
        );
        self.sessions.create(request).await
    }
    async fn capture_latest_completed(
        &self,
        _: &SessionId,
    ) -> Result<CompletedSessionCapture, SessionCaptureError> {
        panic!("unused test host API")
    }
    async fn acquire_pause_reject_busy(
        &self,
        _: &SessionId,
        _: SessionCoordinationReason,
    ) -> Result<SessionPauseLease, SessionPauseError> {
        panic!("unused test host API")
    }
    async fn capture_paused_effective(
        &self,
        _: &SessionPauseLease,
    ) -> Result<CompletedSessionCapture, SessionPauseError> {
        panic!("unused test host API")
    }
    async fn apply_paused_mask_mutation(
        &self,
        _: &SessionPauseLease,
        _: SessionMaskMutation,
    ) -> Result<SessionMaskMutationOutcome, SessionPauseError> {
        panic!("unused test host API")
    }
    async fn arm_paused_continuation(
        &self,
        _: &SessionPauseLease,
        _: TimelineCursor,
        _: AgentRunLimits,
    ) -> Result<AgentSessionRunSubmission, SessionPauseError> {
        panic!("unused test host API")
    }
    async fn release_pause(&self, _: &SessionPauseLease) -> Result<(), SessionPauseError> {
        panic!("unused test host API")
    }
    async fn start_durable_prompt(
        &self,
        id: SessionId,
        limits: AgentRunLimits,
    ) -> Result<AgentSessionRunSubmission, AgentSessionRunError> {
        assert!(
            self.subscribed.load(Ordering::SeqCst),
            "must subscribe before starting"
        );
        self.sessions.start_durable_prompt(id, limits).await
    }
    async fn list(&self) -> Result<Vec<SessionSummary>, LlmError> {
        self.sessions.list().await
    }
    async fn load(&self, id: &SessionId) -> Result<AgentSession, LlmError> {
        self.sessions.load(id).await
    }
    async fn wipe(&self) -> Result<(), LlmError> {
        panic!("unused test host API")
    }
    async fn interrupt(&self, id: &SessionId) -> Result<AgentSession, LlmError> {
        self.interrupts.fetch_add(1, Ordering::SeqCst);
        self.sessions.interrupt(id).await
    }
    async fn active_state(&self, id: &SessionId) -> Option<AgentSessionActiveRunState> {
        self.sessions.active_state(id).await
    }
    async fn submit_prompt(
        &self,
        _: SessionId,
        _: String,
    ) -> Result<AgentSessionRunSubmission, AgentSessionRunError> {
        panic!("unused test host API")
    }
    async fn set_metadata_value_conditionally(
        &self,
        _: &SessionId,
        _: String,
        _: Value,
        _: MetadataSetCondition,
    ) -> Result<MetadataSetOutcome, LlmError> {
        panic!("unused test host API")
    }
    fn watch_sessions(&self) -> Pin<Box<dyn Stream<Item = SessionListUpdate> + Send>> {
        Box::pin(futures_util::stream::empty())
    }
    fn observe_timeline(&self, id: SessionId) -> Option<SessionTimelineProjectionSubscription> {
        self.subscribed.store(true, Ordering::SeqCst);
        Some(self.sessions.watch_timeline_projection(id))
    }
    async fn shutdown(&self) {
        self.sessions.shutdown().await;
    }
}

pub struct SpeechServer {
    pub endpoint: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for SpeechServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
#[derive(Clone)]
struct Speech;
type Output<T> = Pin<Box<dyn Stream<Item = Result<T, tonic::Status>> + Send>>;
impl SpeechServer {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(
                    waltr_audio::tts::text_to_speech_server::TextToSpeechServer::new(Speech),
                )
                .add_service(waltr_audio::stt::speech_server::SpeechServer::new(Speech))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        Self { endpoint, task }
    }
}
#[tonic::async_trait]
impl waltr_audio::tts::text_to_speech_server::TextToSpeech for Speech {
    async fn list_voices(
        &self,
        _: tonic::Request<waltr_audio::tts::ListVoicesRequest>,
    ) -> Result<tonic::Response<waltr_audio::tts::ListVoicesResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused test RPC"))
    }
    async fn synthesize_speech(
        &self,
        _: tonic::Request<waltr_audio::tts::SynthesizeSpeechRequest>,
    ) -> Result<tonic::Response<waltr_audio::tts::SynthesizeSpeechResponse>, tonic::Status> {
        panic!("streaming required")
    }
    type StreamingSynthesizeStream = Output<waltr_audio::tts::StreamingSynthesizeResponse>;
    async fn streaming_synthesize(
        &self,
        request: tonic::Request<tonic::Streaming<waltr_audio::tts::StreamingSynthesizeRequest>>,
    ) -> Result<tonic::Response<Self::StreamingSynthesizeStream>, tonic::Status> {
        use waltr_audio::tts::*;
        let mut input = request.into_inner();
        let first = input.message().await?.unwrap();
        let Some(streaming_synthesize_request::StreamingRequest::StreamingConfig(config)) =
            first.streaming_request
        else {
            panic!("missing config")
        };
        assert_eq!(config.voice.unwrap().name, "test-voice");
        assert_eq!(
            config.streaming_audio_config.unwrap().audio_encoding,
            AudioEncoding::Pcm as i32
        );
        let output = futures_util::stream::try_unfold(input, |mut input| async move {
            let Some(message) = input.message().await? else {
                return Ok(None);
            };
            assert!(matches!(
                message.streaming_request,
                Some(streaming_synthesize_request::StreamingRequest::Input(_))
            ));
            Ok(Some((
                StreamingSynthesizeResponse {
                    audio_content: vec![1, 0],
                    ..Default::default()
                },
                input,
            )))
        });
        Ok(tonic::Response::new(Box::pin(output)))
    }
}
#[tonic::async_trait]
impl waltr_audio::stt::speech_server::Speech for Speech {
    async fn create_recognizer(
        &self,
        _: tonic::Request<waltr_audio::stt::CreateRecognizerRequest>,
    ) -> Result<tonic::Response<waltr::grpc::google::longrunning::Operation>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused test RPC"))
    }
    async fn list_recognizers(
        &self,
        _: tonic::Request<waltr_audio::stt::ListRecognizersRequest>,
    ) -> Result<tonic::Response<waltr_audio::stt::ListRecognizersResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused test RPC"))
    }
    async fn get_recognizer(
        &self,
        _: tonic::Request<waltr_audio::stt::GetRecognizerRequest>,
    ) -> Result<tonic::Response<waltr_audio::stt::Recognizer>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused test RPC"))
    }
    async fn update_recognizer(
        &self,
        _: tonic::Request<waltr_audio::stt::UpdateRecognizerRequest>,
    ) -> Result<tonic::Response<waltr::grpc::google::longrunning::Operation>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused test RPC"))
    }
    async fn delete_recognizer(
        &self,
        _: tonic::Request<waltr_audio::stt::DeleteRecognizerRequest>,
    ) -> Result<tonic::Response<waltr::grpc::google::longrunning::Operation>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused test RPC"))
    }
    async fn undelete_recognizer(
        &self,
        _: tonic::Request<waltr_audio::stt::UndeleteRecognizerRequest>,
    ) -> Result<tonic::Response<waltr::grpc::google::longrunning::Operation>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused test RPC"))
    }
    async fn batch_recognize(
        &self,
        _: tonic::Request<waltr_audio::stt::BatchRecognizeRequest>,
    ) -> Result<tonic::Response<waltr::grpc::google::longrunning::Operation>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused test RPC"))
    }
    async fn get_config(
        &self,
        _: tonic::Request<waltr_audio::stt::GetConfigRequest>,
    ) -> Result<tonic::Response<waltr_audio::stt::Config>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused test RPC"))
    }
    async fn update_config(
        &self,
        _: tonic::Request<waltr_audio::stt::UpdateConfigRequest>,
    ) -> Result<tonic::Response<waltr_audio::stt::Config>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused test RPC"))
    }
    async fn create_custom_class(
        &self,
        _: tonic::Request<waltr_audio::stt::CreateCustomClassRequest>,
    ) -> Result<tonic::Response<waltr::grpc::google::longrunning::Operation>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused test RPC"))
    }
    async fn list_custom_classes(
        &self,
        _: tonic::Request<waltr_audio::stt::ListCustomClassesRequest>,
    ) -> Result<tonic::Response<waltr_audio::stt::ListCustomClassesResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused test RPC"))
    }
    async fn get_custom_class(
        &self,
        _: tonic::Request<waltr_audio::stt::GetCustomClassRequest>,
    ) -> Result<tonic::Response<waltr_audio::stt::CustomClass>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused test RPC"))
    }
    async fn update_custom_class(
        &self,
        _: tonic::Request<waltr_audio::stt::UpdateCustomClassRequest>,
    ) -> Result<tonic::Response<waltr::grpc::google::longrunning::Operation>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused test RPC"))
    }
    async fn delete_custom_class(
        &self,
        _: tonic::Request<waltr_audio::stt::DeleteCustomClassRequest>,
    ) -> Result<tonic::Response<waltr::grpc::google::longrunning::Operation>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused test RPC"))
    }
    async fn undelete_custom_class(
        &self,
        _: tonic::Request<waltr_audio::stt::UndeleteCustomClassRequest>,
    ) -> Result<tonic::Response<waltr::grpc::google::longrunning::Operation>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused test RPC"))
    }
    async fn create_phrase_set(
        &self,
        _: tonic::Request<waltr_audio::stt::CreatePhraseSetRequest>,
    ) -> Result<tonic::Response<waltr::grpc::google::longrunning::Operation>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused test RPC"))
    }
    async fn list_phrase_sets(
        &self,
        _: tonic::Request<waltr_audio::stt::ListPhraseSetsRequest>,
    ) -> Result<tonic::Response<waltr_audio::stt::ListPhraseSetsResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused test RPC"))
    }
    async fn get_phrase_set(
        &self,
        _: tonic::Request<waltr_audio::stt::GetPhraseSetRequest>,
    ) -> Result<tonic::Response<waltr_audio::stt::PhraseSet>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused test RPC"))
    }
    async fn update_phrase_set(
        &self,
        _: tonic::Request<waltr_audio::stt::UpdatePhraseSetRequest>,
    ) -> Result<tonic::Response<waltr::grpc::google::longrunning::Operation>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused test RPC"))
    }
    async fn delete_phrase_set(
        &self,
        _: tonic::Request<waltr_audio::stt::DeletePhraseSetRequest>,
    ) -> Result<tonic::Response<waltr::grpc::google::longrunning::Operation>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused test RPC"))
    }
    async fn undelete_phrase_set(
        &self,
        _: tonic::Request<waltr_audio::stt::UndeletePhraseSetRequest>,
    ) -> Result<tonic::Response<waltr::grpc::google::longrunning::Operation>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused test RPC"))
    }
    async fn recognize(
        &self,
        _: tonic::Request<waltr_audio::stt::RecognizeRequest>,
    ) -> Result<tonic::Response<waltr_audio::stt::RecognizeResponse>, tonic::Status> {
        panic!("streaming required")
    }
    type StreamingRecognizeStream = Output<waltr_audio::stt::StreamingRecognizeResponse>;
    async fn streaming_recognize(
        &self,
        request: tonic::Request<tonic::Streaming<waltr_audio::stt::StreamingRecognizeRequest>>,
    ) -> Result<tonic::Response<Self::StreamingRecognizeStream>, tonic::Status> {
        use waltr_audio::stt::*;
        let mut input = request.into_inner();
        let first = input.message().await?.unwrap();
        assert_eq!(first.recognizer, "recognizer");
        if let Some(streaming_recognize_request::StreamingRequest::StreamingConfig(config)) =
            first.streaming_request
        {
            if let Some(diarization) = config
                .config
                .and_then(|config| config.features)
                .and_then(|features| features.diarization_config)
            {
                assert_eq!(diarization, SpeakerDiarizationConfig::default());
                let output = futures_util::stream::try_unfold(
                    (input, false),
                    |(mut input, finalized)| async move {
                        if finalized {
                            return Ok(None);
                        }
                        let is_final = input.message().await?.is_none();
                        let response = StreamingRecognizeResponse {
                            results: vec![StreamingRecognitionResult {
                                alternatives: vec![SpeechRecognitionAlternative {
                                    transcript: "Hello, there!".into(),
                                    words: if is_final {
                                        vec![
                                            WordInfo {
                                                word: "Hello,".into(),
                                                speaker_label: "S1".into(),
                                                ..Default::default()
                                            },
                                            WordInfo {
                                                word: "there!".into(),
                                                speaker_label: "S2".into(),
                                                ..Default::default()
                                            },
                                        ]
                                    } else {
                                        Vec::new()
                                    },
                                    ..Default::default()
                                }],
                                is_final,
                                ..Default::default()
                            }],
                            ..Default::default()
                        };
                        Ok(Some((response, (input, is_final))))
                    },
                );
                return Ok(tonic::Response::new(Box::pin(output)));
            }
        }
        let output =
            futures_util::stream::try_unfold((input, 0), |(mut input, ordinal)| async move {
                let Some(_) = input.message().await? else {
                    return Ok(None);
                };
                let response = StreamingRecognizeResponse {
                    results: vec![StreamingRecognitionResult {
                        alternatives: vec![SpeechRecognitionAlternative {
                            transcript: format!("word{ordinal}"),
                            ..Default::default()
                        }],
                        is_final: true,
                        ..Default::default()
                    }],
                    ..Default::default()
                };
                Ok(Some((response, (input, ordinal + 1))))
            });
        Ok(tonic::Response::new(Box::pin(output)))
    }
}
