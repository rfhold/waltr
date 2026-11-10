use super::*;
use crate::{CompletionEvent, PromptTurn, SessionStore as _, provider::*};
use futures_util::{SinkExt as _, StreamExt as _};

fn budget() -> TransientInputBudget {
    TransientInputBudget::isolated_for_test()
}

fn capabilities() -> TransientModelCapabilities {
    TransientModelCapabilities {
        png: true,
        jpeg: true,
        webp: true,
        pdf: true,
        text: true,
        max_body_bytes: MAX_TRANSIENT_BODY_BYTES,
    }
}

fn target() -> TransientPromptTarget {
    TransientPromptTarget {
        cursor: TimelineCursor::new(1),
        turn: 0,
        prompt_index: 0,
    }
}

fn input(
    budget: &TransientInputBudget,
    kind: TransientInputKind,
    bytes: Vec<u8>,
) -> TransientInput {
    TransientInput::new(
        budget,
        target(),
        capabilities(),
        vec![budget.attachment(kind, bytes).unwrap()],
    )
    .unwrap()
}

fn request() -> CompletionRequest<'static> {
    CompletionRequest {
        prompt_turns: vec![PromptTurn {
            turn: 0,
            prompt: "initial task".into(),
        }],
        ..CompletionRequest::new("initial task")
    }
}

fn provider(api: usize, url: &str) -> Arc<dyn LlmStream> {
    match api {
        0 => Arc::new(AnthropicMessages::new("fake").with_url(url)),
        1 => Arc::new(OpenAiChatCompletions::new("fake").with_url(url)),
        2 => Arc::new(OpenAiResponses::new("fake").with_url(url)),
        _ => Arc::new(OpenAiResponsesWebSocket::new("fake").with_url(url)),
    }
}

async fn capture(api: usize, input: TransientInput, request: CompletionRequest<'_>) -> Value {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (send, receive) = tokio::sync::oneshot::channel();
    let server = if api == 3 {
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let message = socket.next().await.unwrap().unwrap().into_text().unwrap();
            send.send(serde_json::from_str(&message).unwrap()).unwrap();
            drop(
                socket
                    .send(tokio_tungstenite::tungstenite::Message::Close(None))
                    .await,
            );
        })
    } else {
        let send = Arc::new(std::sync::Mutex::new(Some(send)));
        let router = axum::Router::new().route(
            "/",
            axum::routing::post(move |body: axum::body::Bytes| {
                let send = send.clone();
                async move {
                    send.lock()
                        .unwrap()
                        .take()
                        .unwrap()
                        .send(serde_json::from_slice(&body).unwrap())
                        .unwrap();
                    ([("content-type", "text/event-stream")], "data: [DONE]\n\n")
                }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        })
    };
    let scheme = if api == 3 { "ws" } else { "http" };
    let bound = bind_provider(provider(api, &format!("{scheme}://{address}/")), input).unwrap();
    let stream = bound.stream_request(request).await.unwrap();
    let body = receive.await.unwrap();
    drop(stream);
    if api == 3 {
        server.await.unwrap();
    } else {
        server.abort();
        drop(server.await);
    }
    body
}

#[tokio::test]
async fn actual_http_and_websocket_encode_each_supported_kind() {
    for api in 0..4 {
        for kind in [
            TransientInputKind::Png,
            TransientInputKind::Jpeg,
            TransientInputKind::WebP,
            TransientInputKind::Pdf,
            TransientInputKind::Utf8,
        ] {
            let budget = budget();
            let bytes = b"canary\n\" exact UTF-8 \xc3\xa9".to_vec();
            let body = capture(api, input(&budget, kind, bytes.clone()), request()).await;
            let content = if api < 2 {
                &body["messages"][0]["content"]
            } else {
                &body["input"][0]["content"]
            };
            assert_eq!(content.as_array().unwrap().len(), 2);
            assert_eq!(content[1]["text"], "initial task");
            if kind == TransientInputKind::Utf8 {
                assert_eq!(content[0]["text"], std::str::from_utf8(&bytes).unwrap());
                assert_eq!(
                    content[0]["type"],
                    if api >= 2 { "input_text" } else { "text" }
                );
            } else {
                let encoded = STANDARD.encode(&bytes);
                match api {
                    0 => {
                        assert_eq!(
                            content[0]["type"],
                            if kind == TransientInputKind::Pdf {
                                "document"
                            } else {
                                "image"
                            }
                        );
                        assert_eq!(content[0]["source"]["media_type"], kind.media_type());
                        assert_eq!(content[0]["source"]["data"], encoded);
                    }
                    1 if kind == TransientInputKind::Pdf => assert_eq!(
                        content[0]["file"]["file_data"],
                        format!("data:{};base64,{encoded}", kind.media_type())
                    ),
                    1 => assert_eq!(
                        content[0]["image_url"]["url"],
                        format!("data:{};base64,{encoded}", kind.media_type())
                    ),
                    _ if kind == TransientInputKind::Pdf => assert_eq!(
                        content[0]["file_data"],
                        format!("data:{};base64,{encoded}", kind.media_type())
                    ),
                    _ => assert_eq!(
                        content[0]["image_url"],
                        format!("data:{};base64,{encoded}", kind.media_type())
                    ),
                }
            }
            assert_eq!(budget.source.used.load(Ordering::SeqCst), 0);
            assert_eq!(budget.request.used.load(Ordering::SeqCst), 0);
        }
    }
}

#[test]
fn trusted_admission_bounds_and_arc_credit_ownership() {
    let budget = budget();
    assert!(
        budget
            .attachment(TransientInputKind::Utf8, vec![255])
            .is_err()
    );
    assert!(
        budget
            .attachment(
                TransientInputKind::Pdf,
                vec![0; MAX_TRANSIENT_ATTACHMENT_BYTES + 1]
            )
            .is_err()
    );
    let attachment = budget
        .attachment(
            TransientInputKind::Pdf,
            vec![0; MAX_TRANSIENT_ATTACHMENT_BYTES],
        )
        .unwrap();
    let clone = attachment.clone();
    drop(attachment);
    assert_eq!(
        budget.source.used.load(Ordering::SeqCst),
        MAX_TRANSIENT_ATTACHMENT_BYTES
    );
    let exact = TransientInput::new(
        &budget,
        target(),
        capabilities(),
        vec![clone.clone(), clone.clone()],
    )
    .unwrap();
    assert!(
        TransientInput::new(&budget, target(), capabilities(), vec![clone.clone(); 3]).is_err()
    );
    assert!(
        TransientInput::new(&budget, target(), capabilities(), vec![clone.clone(); 5]).is_err()
    );
    let mut no_pdf = capabilities();
    no_pdf.pdf = false;
    assert!(TransientInput::new(&budget, target(), no_pdf, vec![clone.clone()]).is_err());
    drop(exact);
    drop(clone);
    assert_eq!(budget.source.used.load(Ordering::SeqCst), 0);
    let four = (0..4)
        .map(|_| budget.attachment(TransientInputKind::Utf8, vec![b'x']))
        .collect::<Result<Vec<_>>>()
        .unwrap();
    assert!(TransientInput::new(&budget, target(), capabilities(), four).is_ok());
    let all = (0..8)
        .map(|_| {
            budget.attachment(
                TransientInputKind::Pdf,
                vec![0; MAX_TRANSIENT_ATTACHMENT_BYTES],
            )
        })
        .collect::<Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        budget.source.used.load(Ordering::SeqCst),
        SOURCE_PROCESS_BYTES
    );
    assert!(budget.attachment(TransientInputKind::Pdf, vec![0]).is_err());
    drop(all);
    assert_eq!(budget.source.used.load(Ordering::SeqCst), 0);
    let first = TransientInputBudget::process();
    let second = TransientInputBudget::process();
    assert!(Arc::ptr_eq(&first.source, &second.source));
    assert!(Arc::ptr_eq(&first.request, &second.request));
}

#[test]
fn audit_and_preflight_never_encode_binary_and_debug_is_redacted() {
    let budget = budget();
    let input = input(&budget, TransientInputKind::Pdf, b"secret payload".to_vec());
    let native = provider(0, "http://unused.invalid");
    let bound = bind_provider(native.clone(), input.clone()).unwrap();
    let audit = bound.project_request(&request()).unwrap().to_string();
    for output in [audit, format!("{input:?}"), input.audit().to_string()] {
        assert!(!output.contains("secret payload"));
        assert!(!output.contains(&STANDARD.encode(b"secret payload")));
        assert!(output.contains("sha256"));
    }
    let mut measure = input.clone();
    measure.measure_binary = true;
    let permit = AdmittedTransientInput {
        input: measure,
        _credit: budget.request.reserve(1).unwrap().into(),
    };
    native
        .preflight_transient_request(&request(), &permit)
        .unwrap();
    assert_eq!(input.attachments[0].0.encodings.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn exact_body_boundary_counts_escaped_text_before_network() {
    for api in 0..4 {
        let budget = budget();
        let input = input(&budget, TransientInputKind::Utf8, b"\"\n\t\\\x01".to_vec());
        let body = capture(api, input.clone(), request()).await;
        let length = serde_json::to_vec(&body).unwrap().len();
        let native = provider(api, "http://unused.invalid");
        for (limit, expected) in [(length, true), (length - 1, false)] {
            let mut measured = input.clone();
            measured.measure_binary = true;
            measured.capabilities.max_body_bytes = limit;
            let permit = AdmittedTransientInput {
                input: measured,
                _credit: budget.request.reserve(1).unwrap().into(),
            };
            assert_eq!(
                native
                    .preflight_transient_request(&request(), &permit)
                    .is_ok(),
                expected
            );
        }
    }
}

struct PendingProvider(Arc<AtomicUsize>);
impl LlmStream for PendingProvider {
    fn name(&self) -> &'static str {
        "pending"
    }
    fn transient_api(&self) -> Option<TransientInputApi> {
        Some(TransientInputApi::OpenAiResponses)
    }
    fn preflight_transient_request(
        &self,
        _: &CompletionRequest<'_>,
        _: &AdmittedTransientInput,
    ) -> Result<()> {
        Ok(())
    }
    fn stream_transient_request<'a>(
        &'a self,
        _: CompletionRequest<'a>,
        _: &'a AdmittedTransientInput,
    ) -> BoxStreamFuture<'a> {
        Box::pin(async {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Box::pin(futures_util::stream::pending()) as crate::BoxCompletionStream)
        })
    }
    fn stream_request<'a>(&'a self, _: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async {
            Ok(
                Box::pin(futures_util::stream::iter([Ok(CompletionEvent::Delta(
                    "done".into(),
                ))])) as crate::BoxCompletionStream,
            )
        })
    }
}

#[tokio::test]
async fn shared_request_credit_is_fail_fast_and_retained_until_stream_drop() {
    let budget = budget();
    let input = input(&budget, TransientInputKind::Pdf, vec![1]);
    let calls = Arc::new(AtomicUsize::new(0));
    let bound = bind_provider(Arc::new(PendingProvider(calls.clone())), input).unwrap();
    assert_eq!(budget.used_for_test(), (1, 0));
    let first = bound.stream_request(request()).await.unwrap();
    let second = bound.stream_request(request()).await.unwrap();
    assert!(bound.stream_request(request()).await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(budget.request.used.load(Ordering::SeqCst), ENCODER_SLOTS);
    drop(first);
    let third = bound.stream_request(request()).await.unwrap();
    drop(second);
    drop(third);
    drop(bound);
    assert_eq!(budget.request.used.load(Ordering::SeqCst), 0);
    assert_eq!(budget.source.used.load(Ordering::SeqCst), 0);
}

struct RejectOnce(AtomicUsize);
impl crate::CompletionObserver for RejectOnce {
    fn observe(
        &self,
        proposal: crate::CompletionProposal,
        _: tokio_util::sync::CancellationToken,
    ) -> crate::CompletionObserverFuture {
        let reject = self.0.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            Ok(if reject {
                crate::CompletionDecision::Reject {
                    feedback: crate::CompletionFeedback {
                        proposal_id: proposal.proposal_id,
                        call_id: None,
                        message: "correct the answer".into(),
                    },
                }
            } else {
                crate::CompletionDecision::Accept
            })
        })
    }
}

struct ObserveDraft(Arc<std::sync::Mutex<Vec<String>>>);
impl crate::middleware::AgentMiddleware for ObserveDraft {
    async fn before_provider_request(
        &self,
        _: &mut crate::middleware::context::RequestContext<'_>,
        draft: &mut crate::middleware::context::ModelRequestDraft,
    ) -> Result<crate::middleware::HookOutcome> {
        self.0.lock().unwrap().push(format!("{draft:?}"));
        Ok(crate::middleware::HookOutcome::Continue)
    }
}

async fn create_task<S: crate::SessionStore + Clone + Send + Sync + 'static>(
    sessions: &crate::SessionCoordinator<S>,
) -> (crate::SessionId, TransientPromptTarget) {
    let id = crate::SessionId::generate();
    let mut create = crate::SessionCreateRequest::new(id.clone());
    create.inference_settings = Some(crate::InferenceSettings {
        provider: crate::InferenceProviderSettings {
            key: "test".into(),
            api: crate::ModelApi::OpenAiChatCompletions,
            url: "https://unused.invalid".into(),
            model_id: "fake".into(),
            model_cost: None,
            headers: Default::default(),
            oauth: None,
            reasoning_effort: None,
        },
    });
    create.start = crate::SessionStart::Fresh {
        messages: vec![
            crate::SessionStartMessage::user("historical prompt").unwrap(),
            crate::SessionStartMessage::user("initial task").unwrap(),
        ],
    };
    sessions.create(create).await.unwrap();
    let record = sessions.store().load(&id).await.unwrap();
    let event = record
        .timeline_events()
        .filter(|event| {
            event
                .channel
                .as_ref()
                .is_some_and(|channel| channel.as_str() == crate::timeline::CHANNEL_USER_TEXT)
        })
        .last()
        .unwrap();
    let context = record.materialize_context(crate::SessionContextMode::Effective);
    (
        id,
        TransientPromptTarget {
            cursor: event.cursor,
            turn: context.next_turn,
            prompt_index: context.prompt_turns.len() - 1,
        },
    )
}

fn model() -> crate::ModelRef {
    crate::ModelRef::new(
        crate::ProviderId::OpenAi,
        "fake",
        crate::ModelApi::OpenAiChatCompletions,
    )
}

async fn durable_canary_test<S: crate::SessionStore + Clone + Send + Sync + 'static>(store: S) {
    let budget = budget();
    let bodies = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let capture = bodies.clone();
    let router = axum::Router::new().route("/", axum::routing::post(move |body: axum::body::Bytes| {
        let capture = capture.clone();
        async move {
            let mut bodies = capture.lock().unwrap();
            bodies.push(serde_json::from_slice(&body).unwrap());
            let delta = if bodies.len() == 1 {
                json!({"tool_calls":[{"index":0,"id":"effect-1","type":"function","function":{"name":"effect","arguments":"{}"}}]})
            } else { json!({"content":"safe answer"}) };
            ([("content-type", "text/event-stream")], format!("data: {}\n\ndata: [DONE]\n\n", json!({"choices":[{"delta":delta,"finish_reason":if bodies.len() == 1 {"tool_calls"} else {"stop"}}]})))
        }
    }));
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let sessions = crate::SessionCoordinator::new(store);
    let (id, target) = create_task(&sessions).await;
    let canary = "PRIVATE_UTF8_attachment_4134\n\"";
    let bytes = b"PRIVATE_PDF_attachment_6012";
    let input = TransientInput::new(
        &budget,
        target,
        capabilities(),
        vec![
            budget
                .attachment(TransientInputKind::Utf8, canary.as_bytes().to_vec())
                .unwrap(),
            budget
                .attachment(TransientInputKind::Pdf, bytes.to_vec())
                .unwrap(),
        ],
    )
    .unwrap();
    let effects = Arc::new(AtomicUsize::new(0));
    let count = effects.clone();
    let mut runtime = crate::AgentRuntime::builder(
        model(),
        Arc::new(OpenAiChatCompletions::new("fake").with_url(format!("http://{address}/"))),
    )
    .with_tool(crate::AgentTool::new(
        crate::ToolDefinition::new("effect", "effect", json!({"type":"object"})),
        move |ctx| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(ctx.result("safe result"))
        },
    ))
    .build();
    let middleware = Arc::new(std::sync::Mutex::new(Vec::new()));
    runtime
        .middleware_chain
        .push(ObserveDraft(middleware.clone()));
    let validation = crate::CompletionValidation::new(
        Arc::new(RejectOnce(AtomicUsize::new(0))),
        5,
        std::time::Duration::from_secs(10),
    )
    .unwrap();
    let events = sessions
        .resume(&runtime, id.clone(), crate::ResumeInput::Continue)
        .with_transient_input(input)
        .unwrap()
        .with_completion_validation(validation)
        .stream()
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().all(Result::is_ok), "{events:?}");
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 3);
    for body in bodies.iter() {
        let messages = body["messages"].as_array().unwrap();
        let historical = messages
            .iter()
            .position(|message| message["content"] == "historical prompt")
            .unwrap();
        let task = messages
            .iter()
            .position(|message| {
                message["content"].as_array().is_some_and(|content| {
                    content.iter().any(|part| part["text"] == "initial task")
                })
            })
            .unwrap();
        assert!(historical < task);
        assert_eq!(
            body.to_string()
                .matches("PRIVATE_UTF8_attachment_4134")
                .count(),
            1
        );
        assert_eq!(body.to_string().matches(&STANDARD.encode(bytes)).count(), 1);
        if let Some(feedback) = messages.iter().position(|message| {
            message["content"]
                .as_str()
                .is_some_and(|text| text.contains("completionValidation"))
        }) {
            assert!(task < feedback);
        }
    }
    drop(bodies);
    let record = sessions.store().load(&id).await.unwrap();
    let persisted = serde_json::to_string(&record).unwrap();
    let middleware = middleware.lock().unwrap().join("\n");
    for output in [&persisted, &middleware] {
        assert!(!output.contains("PRIVATE_UTF8_attachment_4134"));
        assert!(!output.contains("PRIVATE_PDF_attachment_6012"));
        assert!(!output.contains(&STANDARD.encode(bytes)));
        assert!(!output.contains(&STANDARD.encode(canary)));
    }
    assert!(persisted.contains("metadataOnly"));
    assert!(persisted.contains("sha256"));
    assert!(persisted.contains("invocationLocal"));
    assert_eq!(
        record
            .snapshot()
            .middleware_state
            .get("transient-input")
            .unwrap()["active"],
        false
    );
    assert_eq!(budget.source.used.load(Ordering::SeqCst), 0);
    assert_eq!(budget.request.used.load(Ordering::SeqCst), 0);
    sessions.shutdown().await;
    server.abort();
    drop(server.await);
}

#[tokio::test]
async fn memory_history_audit_and_middleware_are_metadata_only_across_tools_and_rejection() {
    durable_canary_test(crate::MemorySessionStore::new()).await;
}

#[derive(Clone)]
struct ForwardingStore(crate::MemorySessionStore);

#[test]
fn precharged_buffer_reserves_before_fill_and_transfers_credit_without_copy() {
    let budget = budget();
    let mut held = (0..8)
        .map(|_| {
            budget
                .attachment_buffer(TransientInputKind::Pdf, 8 * MIB)
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(budget.used_for_test().0, 64 * MIB);
    assert!(
        budget
            .attachment_buffer(TransientInputKind::Pdf, 1)
            .is_err()
    );
    let mut buffer = held.pop().unwrap();
    let address = buffer.bytes.as_ptr();
    buffer.append(&vec![1; 8 * MIB]).unwrap();
    assert!(buffer.append(&[2]).is_err());
    let attachment = buffer.finish().unwrap();
    assert_eq!(attachment.0.bytes.as_ptr(), address);
    let clone = attachment.clone();
    drop(attachment);
    assert_eq!(budget.used_for_test().0, 64 * MIB);
    drop(clone);
    drop(held);
    assert_eq!(budget.used_for_test().0, 0);
    let mut invalid = budget
        .attachment_buffer(TransientInputKind::Utf8, 1)
        .unwrap();
    invalid.append(&[255]).unwrap();
    assert!(invalid.finish().is_err());
    assert!(
        budget
            .attachment_buffer(TransientInputKind::Pdf, 2)
            .unwrap()
            .finish()
            .is_err()
    );
    let unwind = std::panic::catch_unwind(|| {
        let _buffer = budget
            .attachment_buffer(TransientInputKind::Pdf, 8 * MIB)
            .unwrap();
        panic!("cancel owned construction");
    });
    assert!(unwind.is_err());
    assert_eq!(budget.used_for_test().0, 0);
    let attachments = (0..3)
        .map(|_| {
            let mut buffer = budget
                .attachment_buffer(TransientInputKind::Pdf, 8 * MIB)
                .unwrap();
            buffer.append(&vec![0; 8 * MIB]).unwrap();
            buffer.finish().unwrap()
        })
        .collect::<Vec<_>>();
    let input =
        TransientInput::new(&budget, target(), capabilities(), attachments[..2].to_vec()).unwrap();
    assert!(TransientInput::new(&budget, target(), capabilities(), attachments.clone()).is_err());
    drop(attachments);
    assert_eq!(budget.used_for_test().0, 16 * MIB);
    drop(input);
    assert_eq!(budget.used_for_test().0, 0);
}

macro_rules! forwarding_methods {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $out:ty;)*) => { $(
        fn $name(&self, $($arg: $ty),*) -> impl std::future::Future<Output = crate::SessionStoreResult<$out>> + Send {
            self.0.$name($($arg),*)
        }
    )* };
}

impl crate::SessionStore for ForwardingStore {
    forwarding_methods! {
        create(request: crate::SessionCreateRequest) -> crate::SessionRecord;
        load(id: &crate::SessionId) -> crate::SessionRecord;
        delete(id: &crate::SessionId) -> ();
        wipe() -> ();
        list() -> Vec<crate::SessionSummary>;
        append_timeline(id: &crate::SessionId, revision: crate::ExecutionToken, events: Vec<crate::PendingTimelineEvent>) -> crate::TimelineAppendReceipt;
        append_journal(id: &crate::SessionId, revision: crate::ExecutionToken, events: Vec<crate::SessionJournalEvent>) -> crate::SessionCommitReceipt;
        mutate(id: &crate::SessionId, mutation: crate::SessionMutation) -> crate::SessionMutationOutcome;
        set_metadata_value_conditionally(id: &crate::SessionId, key: String, value: Value, condition: crate::MetadataSetCondition) -> crate::MetadataSetOutcome;
        remove_metadata_value(id: &crate::SessionId, key: String) -> crate::SessionRecord;
        set_inference_settings(id: &crate::SessionId, settings: Option<crate::InferenceSettings>) -> crate::SessionRecord;
        set_agent_definition(id: &crate::SessionId, definition: Option<serde_json::Value>) -> crate::SessionRecord;
        timeline(id: &crate::SessionId, query: crate::TimelineQuery) -> Vec<crate::AgentTimelineEvent>;
        timeline_item_text(id: &crate::SessionId, item: &crate::TimelineItemId) -> crate::TimelineItemText;
    }
}

#[tokio::test]
async fn custom_store_uses_ordinary_contract_with_attachments_and_corrections() {
    durable_canary_test(ForwardingStore(crate::MemorySessionStore::new())).await;
}

#[tokio::test]
async fn filesystem_history_audit_and_middleware_are_metadata_only_across_tools_and_rejection() {
    let temp = tempfile::tempdir().unwrap();
    durable_canary_test(crate::FilesystemSessionStore::new(temp.path())).await;
    fn check(path: &std::path::Path) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                check(&path);
            } else {
                let content = std::fs::read(path).unwrap();
                assert!(
                    !String::from_utf8_lossy(&content).contains("PRIVATE_UTF8_attachment_4134")
                );
                assert!(
                    !String::from_utf8_lossy(&content)
                        .contains(&STANDARD.encode(b"PRIVATE_PDF_attachment_6012"))
                );
            }
        }
    }
    check(temp.path());
}

#[tokio::test]
async fn interrupted_input_without_observer_is_fenced_before_resume_effects() {
    let budget = budget();
    let sessions = crate::SessionCoordinator::new(crate::MemorySessionStore::new());
    let (id, target) = create_task(&sessions).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let effects = Arc::new(AtomicUsize::new(0));
    let count = effects.clone();
    let runtime = crate::AgentRuntime::builder(model(), Arc::new(PendingProvider(calls.clone())))
        .with_tool(crate::AgentTool::new(
            crate::ToolDefinition::new("effect", "effect", json!({"type":"object"})),
            move |ctx| {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(ctx.result("effect"))
            },
        ))
        .build();
    let input = TransientInput::new(
        &budget,
        target,
        capabilities(),
        vec![budget.attachment(TransientInputKind::Pdf, vec![1]).unwrap()],
    )
    .unwrap();
    let run = sessions
        .resume_with_transient_input(&runtime, id.clone(), input)
        .unwrap();
    assert_eq!(budget.request.used.load(Ordering::SeqCst), 0);
    let mut stream = run.stream();
    while calls.load(Ordering::SeqCst) == 0 {
        tokio::select! { _ = stream.next() => {}, () = tokio::time::sleep(std::time::Duration::from_millis(10)) => {} }
    }
    drop(stream);
    assert_eq!(budget.request.used.load(Ordering::SeqCst), 0);
    assert_eq!(budget.source.used.load(Ordering::SeqCst), 0);
    let before = sessions.store().load(&id).await.unwrap().revision;
    for resume in [
        crate::ResumeInput::Continue,
        crate::ResumeInput::Prompt("@tool(effect, {})".into()),
    ] {
        let events = sessions
            .resume(&runtime, id.clone(), resume)
            .stream()
            .collect::<Vec<_>>()
            .await;
        assert!(
            events
                .iter()
                .any(|event| event.as_ref().is_err_and(|error| error
                    .to_string()
                    .contains("interrupted consumer execution cannot resume")))
        );
        assert_eq!(sessions.store().load(&id).await.unwrap().revision, before);
    }
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn owned_helper_cancellation_joins_driver_before_releasing_payload() {
    let budget = budget();
    let sessions = crate::SessionCoordinator::new(crate::MemorySessionStore::new());
    let (id, target) = create_task(&sessions).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime =
        crate::AgentRuntime::builder(model(), Arc::new(PendingProvider(calls.clone()))).build();
    let owner = crate::SessionExecutionOwner::new(sessions.clone(), id.clone());
    let input = TransientInput::new(
        &budget,
        target,
        capabilities(),
        vec![budget.attachment(TransientInputKind::Pdf, vec![1]).unwrap()],
    )
    .unwrap();
    let validation = crate::CompletionValidation::new(
        Arc::new(RejectOnce(AtomicUsize::new(0))),
        3,
        std::time::Duration::from_secs(10),
    )
    .unwrap();
    let waiter = tokio::spawn({
        let owner = owner.clone();
        async move {
            owner
                .helper_with_transient_input(
                    runtime,
                    input,
                    validation,
                    crate::AgentRunLimits::default(),
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(budget.source.used.load(Ordering::SeqCst), 1);
    assert_eq!(budget.request.used.load(Ordering::SeqCst), 1);
    waiter.abort();
    drop(waiter.await);
    owner.close().await.unwrap();
    assert!(sessions.active_state(&id).await.is_none());
    assert_eq!(budget.source.used.load(Ordering::SeqCst), 0);
    assert_eq!(budget.request.used.load(Ordering::SeqCst), 0);
    sessions.shutdown().await;
}

#[test]
fn request_and_source_credit_cleanup_on_panic() {
    let budget = budget();
    let input = input(&budget, TransientInputKind::Pdf, vec![1]);
    let clone = input.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _permit = AdmittedTransientInput {
            input: clone,
            _credit: budget.request.reserve(1).unwrap().into(),
        };
        panic!("test panic after admission")
    }));
    assert!(result.is_err());
    assert_eq!(budget.request.used.load(Ordering::SeqCst), 0);
    assert_eq!(budget.source.used.load(Ordering::SeqCst), 1);
    drop(input);
    assert_eq!(budget.source.used.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn actual_provider_replay_keeps_initial_target_before_tools_and_feedback() {
    for api in 0..4 {
        let budget = budget();
        let mut input = input(
            &budget,
            TransientInputKind::Pdf,
            b"attachment-bound-initial-only".to_vec(),
        );
        input.target.prompt_index = 1;
        input.target.turn = 1;
        let request = CompletionRequest {
            prompt: "corrective feedback",
            prompt_turns: vec![
                PromptTurn {
                    turn: 0,
                    prompt: "historical prompt".into(),
                },
                PromptTurn {
                    turn: 1,
                    prompt: "initial task".into(),
                },
                PromptTurn {
                    turn: 3,
                    prompt: "corrective feedback".into(),
                },
            ],
            turns: vec![
                crate::CompletionTurn {
                    turn: 0,
                    items: vec![crate::TurnItem::AssistantText("older answer".into())],
                    metadata: None,
                },
                crate::CompletionTurn {
                    turn: 1,
                    items: vec![
                        crate::TurnItem::ToolCall(crate::ToolCall::new(
                            "call-1",
                            "effect",
                            json!({}),
                        )),
                        crate::TurnItem::ToolResult(crate::ToolResult::new(
                            "call-1",
                            "effect",
                            "effect-result",
                        )),
                    ],
                    metadata: None,
                },
                crate::CompletionTurn {
                    turn: 2,
                    items: vec![crate::TurnItem::AssistantText("rejected candidate".into())],
                    metadata: None,
                },
            ],
            ..CompletionRequest::new("corrective feedback")
        };
        let body = capture(api, input, request).await;
        let replay = if api < 2 {
            body["messages"].as_array().unwrap()
        } else {
            body["input"].as_array().unwrap()
        };
        let target = replay
            .iter()
            .position(|item| item["content"].to_string().contains("initial task"))
            .unwrap();
        let encoded = STANDARD.encode(b"attachment-bound-initial-only");
        assert!(replay[target].to_string().contains(&encoded));
        assert_eq!(body.to_string().matches(&encoded).count(), 1);
        let history = replay
            .iter()
            .position(|item| item.to_string().contains("historical prompt"))
            .unwrap();
        let tools = replay
            .iter()
            .position(|item| item.to_string().contains("effect-result"))
            .unwrap();
        let feedback = replay
            .iter()
            .position(|item| item.to_string().contains("corrective feedback"))
            .unwrap();
        assert!(history < target && target < tools && tools < feedback);
        if api == 0 {
            assert!(
                replay[target]["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|part| part["text"] == "initial task"
                        && part.get("cache_control").is_some())
            );
        }
    }
}

#[tokio::test]
async fn exact_local_32_mib_preflight_boundary_with_16_mib_escaped_source() {
    let budget = budget();
    let small = TransientInput::new(
        &budget,
        target(),
        capabilities(),
        vec![
            budget
                .attachment(TransientInputKind::Utf8, vec![b'x'])
                .unwrap(),
            budget
                .attachment(TransientInputKind::Utf8, vec![b'x'])
                .unwrap(),
        ],
    )
    .unwrap();
    let body = capture(1, small, request()).await;
    let overhead = serde_json::to_vec(&body).unwrap().len() - 2;
    let native = provider(1, "http://unused.invalid");
    for extra in [0usize, 1] {
        let mut bytes = vec![b'"'; MAX_TRANSIENT_ATTACHMENT_BYTES];
        bytes[..overhead - extra].fill(b'x');
        let mut input = TransientInput::new(
            &budget,
            target(),
            capabilities(),
            vec![
                budget
                    .attachment(
                        TransientInputKind::Utf8,
                        vec![b'"'; MAX_TRANSIENT_ATTACHMENT_BYTES],
                    )
                    .unwrap(),
                budget.attachment(TransientInputKind::Utf8, bytes).unwrap(),
            ],
        )
        .unwrap();
        input.measure_binary = true;
        let permit = AdmittedTransientInput {
            input,
            _credit: budget.request.reserve(1).unwrap().into(),
        };
        assert_eq!(
            native
                .preflight_transient_request(&request(), &permit)
                .is_ok(),
            extra == 0
        );
    }
}

#[test]
fn source_capacity_and_each_mime_capability_fail_closed() {
    let budget = budget();
    let mut spare = Vec::with_capacity(MAX_TRANSIENT_ATTACHMENT_BYTES + 1);
    spare.push(1);
    assert!(budget.attachment(TransientInputKind::Pdf, spare).is_err());
    for kind in [
        TransientInputKind::Png,
        TransientInputKind::Jpeg,
        TransientInputKind::WebP,
        TransientInputKind::Pdf,
        TransientInputKind::Utf8,
    ] {
        let mut caps = capabilities();
        match kind {
            TransientInputKind::Png => caps.png = false,
            TransientInputKind::Jpeg => caps.jpeg = false,
            TransientInputKind::WebP => caps.webp = false,
            TransientInputKind::Pdf => caps.pdf = false,
            TransientInputKind::Utf8 => caps.text = false,
        }
        assert!(
            TransientInput::new(
                &budget,
                target(),
                caps,
                vec![budget.attachment(kind, vec![b'x']).unwrap()]
            )
            .is_err()
        );
    }
    assert_eq!(budget.source.used.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn body_and_target_failure_precede_binary_encoding_and_transport() {
    let budget = budget();
    let mut input = TransientInput::new(
        &budget,
        target(),
        capabilities(),
        vec![
            budget
                .attachment(
                    TransientInputKind::Utf8,
                    vec![0; MAX_TRANSIENT_ATTACHMENT_BYTES],
                )
                .unwrap(),
            budget.attachment(TransientInputKind::Pdf, vec![1]).unwrap(),
        ],
    )
    .unwrap();
    let attachment = input.attachments[1].clone();
    let bound = bind_provider(provider(1, "http://unused.invalid"), input.clone()).unwrap();
    assert!(bound.stream_request(request()).await.is_err());
    assert_eq!(attachment.0.encodings.load(Ordering::SeqCst), 0);
    assert_eq!(budget.request.used.load(Ordering::SeqCst), 0);
    drop(bound);
    assert_eq!(budget.request.used.load(Ordering::SeqCst), 0);
    input.target.turn = 99;
    let calls = Arc::new(AtomicUsize::new(0));
    let bound = bind_provider(Arc::new(PendingProvider(calls.clone())), input).unwrap();
    assert!(bound.stream_request(request()).await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

struct TextOnlyProvider;
impl LlmStream for TextOnlyProvider {
    fn name(&self) -> &'static str {
        "text-only"
    }
    fn stream_request<'a>(&'a self, _: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        panic!("unsupported input must not dispatch")
    }
}

#[tokio::test]
async fn unsupported_api_and_wrong_task_cursor_have_no_effects() {
    let budget = budget();
    assert!(
        bind_provider(
            Arc::new(TextOnlyProvider),
            input(&budget, TransientInputKind::Pdf, vec![1])
        )
        .is_err()
    );
    let sessions = crate::SessionCoordinator::new(crate::MemorySessionStore::new());
    let (id, mut target) = create_task(&sessions).await;
    let record = sessions.store().load(&id).await.unwrap();
    target.cursor = record
        .timeline_events()
        .find(|event| {
            event
                .channel
                .as_ref()
                .is_some_and(|channel| channel.as_str() == crate::timeline::CHANNEL_USER_TEXT)
        })
        .unwrap()
        .cursor;
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime =
        crate::AgentRuntime::builder(model(), Arc::new(PendingProvider(calls.clone()))).build();
    let input = TransientInput::new(
        &budget,
        target,
        capabilities(),
        vec![budget.attachment(TransientInputKind::Pdf, vec![1]).unwrap()],
    )
    .unwrap();
    let events = sessions
        .resume(&runtime, id.clone(), crate::ResumeInput::Continue)
        .with_transient_input(input)
        .unwrap()
        .stream()
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().any(Result::is_err));
    assert_eq!(
        sessions.store().load(&id).await.unwrap().revision,
        record.revision
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(budget.source.used.load(Ordering::SeqCst), 0);
    assert_eq!(budget.request.used.load(Ordering::SeqCst), 0);
}

#[derive(Clone)]
struct TraceWriter(Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for TraceWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn reflected_provider_errors_never_escape_into_errors_or_tracing() {
    use tracing::instrument::WithSubscriber as _;
    for api in 0..4 {
        for status_error in [false, true] {
            if api == 3 && status_error {
                continue;
            }
            let canary = "REFLECTED_PRIVATE_ATTACHMENT_3195";
            let encoded = STANDARD.encode(canary);
            let reflected = format!("{canary} {encoded}");
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let error = json!({"type":"error", "error":{"type":"invalid_request_error","message":reflected}});
            let server = if api == 3 {
                tokio::spawn(async move {
                    let (tcp, _) = listener.accept().await.unwrap();
                    let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
                    socket.next().await.unwrap().unwrap();
                    socket
                        .send(tokio_tungstenite::tungstenite::Message::Text(
                            error.to_string().into(),
                        ))
                        .await
                        .unwrap();
                })
            } else {
                let response = if status_error {
                    error.to_string()
                } else {
                    format!("event: error\ndata: {error}\n\n")
                };
                let router = axum::Router::new().route(
                    "/",
                    axum::routing::post(move || {
                        let response = response.clone();
                        async move {
                            (
                                if status_error {
                                    axum::http::StatusCode::BAD_REQUEST
                                } else {
                                    axum::http::StatusCode::OK
                                },
                                [("content-type", "text/event-stream")],
                                response,
                            )
                        }
                    }),
                );
                tokio::spawn(async move {
                    axum::serve(listener, router).await.unwrap();
                })
            };
            let trace = Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = trace.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(move || TraceWriter(sink.clone()))
                .finish();
            let budget = budget();
            let scheme = if api == 3 { "ws" } else { "http" };
            let bound = bind_provider(
                provider(api, &format!("{scheme}://{address}/")),
                input(&budget, TransientInputKind::Pdf, canary.as_bytes().to_vec()),
            )
            .unwrap();
            let errors = async {
                match bound.stream_request(request()).await {
                    Ok(stream) => stream
                        .collect::<Vec<_>>()
                        .await
                        .into_iter()
                        .filter_map(std::result::Result::err)
                        .collect::<Vec<_>>(),
                    Err(error) => vec![error],
                }
            }
            .with_subscriber(subscriber)
            .await;
            assert!(!errors.is_empty());
            let printed = format!("{errors:?}");
            let trace = String::from_utf8(trace.lock().unwrap().clone()).unwrap();
            for output in [printed, trace] {
                assert!(!output.contains(canary));
                assert!(!output.contains(&encoded));
            }
            if api == 3 {
                server.await.unwrap();
            } else {
                server.abort();
                drop(server.await);
            }
        }
    }
}
