use super::*;
use crate::routine::adapters::tool_streams::ToolStreams;
use crate::routine::ports::{HelperRequest, HelperSelection};

struct Helpers {
    streams: Mutex<Option<Arc<ToolStreams>>>,
    first_spoken: Arc<tokio::sync::Notify>,
    done: Arc<AtomicBool>,
}
impl HelperRunner for Helpers {
    fn pin_named(&self, names: Vec<String>) -> PortFuture<()> {
        assert_eq!(names, ["routine-speaker"]);
        Box::pin(async { Ok(()) })
    }
    fn bind_tool_streams(&self, streams: Arc<ToolStreams>) -> PortFuture<()> {
        *self.streams.lock().unwrap() = Some(streams);
        Box::pin(async { Ok(()) })
    }
    fn run_with_inputs(
        &self,
        request: HelperRequest,
        _: Option<SessionCopy>,
        _: Option<HelperPayload>,
        cancel: CancellationToken,
    ) -> PortFuture<HelperResult> {
        self.run(request, cancel)
    }
    fn run(&self, request: HelperRequest, cancel: CancellationToken) -> PortFuture<HelperResult> {
        let streams = self.streams.lock().unwrap().clone().unwrap();
        let first_spoken = self.first_spoken.clone();
        let done = self.done.clone();
        Box::pin(async move {
            assert_eq!(
                request.selection,
                HelperSelection::Named {
                    name: "routine-speaker".into()
                }
            );
            let mut producer = streams.claim(&request.selection)?;
            let tool = producer.tools(cancel.clone()).remove(0).tool;
            for (id, text) in [("one", "First sentence."), ("two", "Second sentence.")] {
                let result = tool
                    .call(ToolExecutionContext::new(ToolCall::new(
                        id,
                        "speak",
                        json!({"text":text}),
                    )))
                    .await
                    .map_err(|_| PortError::Unavailable)?;
                assert!(!result.is_error);
                if id == "one" {
                    tokio::select! { () = cancel.cancelled() => return Err(PortError::Unavailable), () = first_spoken.notified() => {} }
                }
            }
            done.store(true, Ordering::SeqCst);
            producer.complete();
            Ok(HelperResult {
                session_id: "helper".into(),
                definition_identity: "test".into(),
                response: "é".repeat(4096),
                captures: vec![],
                evidence: vec![],
            })
        })
    }
    fn cleanup(&self) -> PortFuture<()> {
        Box::pin(async { Ok(()) })
    }
}
struct Speech {
    seen: Arc<Mutex<Vec<String>>>,
    first_spoken: Arc<tokio::sync::Notify>,
    helper_done: Arc<AtomicBool>,
}
impl SpeechBackend for Speech {
    fn synthesize_utterances(&self, input: Reader, output: Writer) -> Result<media::Producer, media::Error> {
        self.synthesize(input, output)
    }
    fn preflight(&self, format: Format, _: bool) -> Result<(), media::Error> {
        if format == Format::Text {
            Ok(())
        } else {
            Err(media::Error::Invalid)
        }
    }
    fn transcribe(&self, _: Reader, _: Writer) -> Result<media::Producer, media::Error> {
        Err(media::Error::Invalid)
    }
    fn transcribe_snapshot(
        &self,
        _: Reader,
        _: Workspace,
    ) -> Result<(media::Producer, media::snapshot::SnapshotReader), media::Error> {
        Err(media::Error::Invalid)
    }
    fn synthesize(
        &self,
        mut input: Reader,
        mut output: Writer,
    ) -> Result<media::Producer, media::Error> {
        let seen = self.seen.clone();
        let first = self.first_spoken.clone();
        let done = self.helper_done.clone();
        Ok(media::Producer::spawn(input.cancellation(), async move {
            while let Some(packet) = input.read().await? {
                let Chunk::Text(text) = packet.chunk() else {
                    return Err(media::Error::Invalid);
                };
                assert!(text.committed);
                let initial = seen.lock().unwrap().is_empty();
                seen.lock().unwrap().push(text.text.clone());
                output.write(output.key(), Chunk::Audio(vec![0, 0])).await?;
                if initial {
                    assert!(
                        !done.load(Ordering::SeqCst),
                        "speech must consume the first segment before helper completion"
                    );
                    first.notify_one();
                }
            }
            output.close(output.key()).await
        }))
    }
}

#[tokio::test]
async fn tool_streams_speech_example_loads_and_feeds_existing_synthesis_incrementally() {
    if !crate::test_support::isolated_process_test(
        "routine::adapters::media::tests::tool_streams_speech_tests::tool_streams_speech_example_loads_and_feeds_existing_synthesis_incrementally",
    ) {
        return;
    }
    run_speech_example(false).await;
}
#[tokio::test]
async fn tool_streams_media_undrained_root_fails_without_cleanup_quarantine() {
    if !crate::test_support::isolated_process_test(
        "routine::adapters::media::tests::tool_streams_speech_tests::tool_streams_media_undrained_root_fails_without_cleanup_quarantine",
    ) {
        return;
    }
    run_speech_example(true).await;
}
async fn run_speech_example(undrained: bool) {
    let root = crate::test_support::temp_root().canonicalize().unwrap();
    let private = root.join("private");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&private).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let blobs = Arc::new(
        FilesystemBlobStore::new(FilesystemConfig {
            private_root: private,
            workspace_root: blob::filesystem::WorkspaceRoot::Local(workspace),
            clock: Arc::new(blob::MonotonicClock::default()),
        })
        .unwrap(),
    );
    let engine = Arc::new(Engine::new(1).unwrap());
    let definition = Definition::parse(
        include_str!("../../../../../examples/routines/streamed-speech.yaml"),
        engine.as_ref(),
    )
    .unwrap();
    let definition = if undrained {
        let mut envelope: Value = serde_json::from_str(definition.canonical()).unwrap();
        envelope["spec"]["source"] = json!(
            r#"export default async function(input) {
            const feed = await host.call('media.text.open', {});
            const speech = host.call('media.synthesize', {output:'audio'}, null, null, feed);
            const helper = host.call('helper.run', {selection:{kind:'named',name:'routine-speaker'},task:input.task,maxTurns:16,deadlineMs:25000,responseBytes:65536,completionBytes:65536});
            const first = await host.call('helper.next', {stream:'speech'});
            await host.call('media.text.write', {text:first.value.arguments.text}, null, null, feed);
            await helper;
            await host.call('media.text.close', {}, null, null, feed);
            await speech;
            return true;
        }"#
        );
        Definition::parse(&envelope.to_string(), engine.as_ref()).unwrap()
    } else {
        let mut envelope: Value = serde_json::from_str(definition.canonical()).unwrap();
        let source = envelope["spec"]["source"].as_str().unwrap().replace(
            "await Promise.all([helper, consume, speech]);",
            "const completed = await Promise.all([helper, consume, speech]);\nconst answer = await host.call('media.readText', {field:'response',max:8192}, null, null, completed[0]);\nif (answer.length !== 4096 || answer[0] !== 'é' || answer[4095] !== 'é') throw new Error('UTF-8 text changed');",
        );
        assert!(source.contains("media.readText"));
        envelope["spec"]["source"] = json!(source);
        Definition::parse(&envelope.to_string(), engine.as_ref()).unwrap()
    };
    AgentDefinitionCatalog::from_sources(
        vec![include_str!("../../../../../examples/agents/routine-speaker.yaml").into()],
        "routine-speaker",
    )
    .unwrap();
    let cancel = CancellationToken::new();
    let streams = Streams::new(
        stream::Limits::default(),
        Pool::principal(),
        Pool::process(),
        cancel.clone(),
    )
    .unwrap();
    let (writer, mut reader) = streams
        .open(
            stream::Direction::Output,
            Format::Pcm16Le {
                rate: 24000,
                channels: 1,
            },
            false,
        )
        .unwrap();
    let received = Arc::new(AtomicUsize::new(0));
    let count = received.clone();
    let work: Work = Box::pin(async move {
        while let Some(_packet) = reader.read().await.map_err(|_| PortError::Unavailable)? {
            count.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    });
    let driver = Arc::new(Driver {
        cold: Mutex::new(Some(vec![work])),
        tasks: tokio::sync::Mutex::new(vec![]),
        cancellation: cancel,
        joined: AtomicBool::new(false),
        completed: AtomicBool::new(true),
        uncertain: AtomicBool::new(false),
    });
    let persistence = Persistence::initialize(
        DatabaseConfig::from_urls(
            &format!("sqlite:{}?mode=rwc", root.join("speech.sqlite").display()),
            "sqlite::memory:",
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let first_spoken = Arc::new(tokio::sync::Notify::new());
    let done = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(Mutex::new(vec![]));
    let bindings = MediaBindings {
        driver: Arc::new(OwnedDriver(driver.clone())),
        snapshot: false,
        principal: "owner".into(),
        streams: streams.clone(),
        inputs: BTreeMap::new(),
        outputs: BTreeMap::from([("audio".into(), writer)]),
        uploads: BTreeMap::new(),
        speech: Arc::new(Speech {
            seen: seen.clone(),
            first_spoken: first_spoken.clone(),
            helper_done: done.clone(),
        }),
        helpers: Arc::new(Helpers {
            streams: Mutex::new(None),
            first_spoken,
            done,
        }),
        context: None,
        blobs: blobs.clone(),
        text: persistence.script_text_result_store(),
        attachments: TransientInputBudget::process(),
        workspace_principal: WorkspacePool::principal(),
        workspace_process: WorkspacePool::process(),
        result_principal: ResultPool::principal(),
        result_process: ResultPool::process(),
        leases: Arc::new(Mutex::new(vec![])),
    };
    let service = InvocationService::new(
        engine.clone(),
        persistence.script_invocation_store(),
        Arc::new(MediaHostFactory {
            bindings: Arc::new(Bind(Mutex::new(Some(bindings)))),
        }),
        Arc::new(SystemClock),
        1,
    )
    .unwrap();
    let handle = service
        .invoke(
            definition,
            "run".into(),
            Caller {
                principal: "owner".into(),
                origin: Origin::Interactive,
                ownership: Ownership::Leased,
                correlation: "example".into(),
            },
            json!({"task":"Say two sentences."}),
        )
        .unwrap()
        .await
        .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), handle.join())
        .await
        .unwrap();
    assert_eq!(
        outcome.status,
        Some(if undrained {
            Status::Failed
        } else {
            Status::Succeeded
        }),
        "{outcome:?}"
    );
    assert!(outcome.cleanup_complete);
    assert_eq!(
        *seen.lock().unwrap(),
        if undrained {
            vec!["First sentence."]
        } else {
            vec!["First sentence.", "Second sentence."]
        }
    );
    if !undrained {
        assert_eq!(received.load(Ordering::SeqCst), 2);
    }
    assert!(driver.joined.load(Ordering::SeqCst));
    assert_eq!(streams.retained_bytes(), 0);
    service.shutdown().await.unwrap();
    engine.shutdown().await.unwrap();
    blobs.shutdown().await.unwrap();
}

#[derive(Default)]
struct FailingHelpers {
    streams: Mutex<Option<Arc<ToolStreams>>>,
    joined: AtomicBool,
    failed: Arc<AtomicBool>,
}
impl HelperRunner for FailingHelpers {
    fn pin_named(&self, _: Vec<String>) -> PortFuture<()> {
        Box::pin(async { Ok(()) })
    }
    fn bind_tool_streams(&self, streams: Arc<ToolStreams>) -> PortFuture<()> {
        *self.streams.lock().unwrap() = Some(streams);
        Box::pin(async { Ok(()) })
    }
    fn run_with_inputs(
        &self,
        request: HelperRequest,
        _: Option<SessionCopy>,
        _: Option<HelperPayload>,
        cancel: CancellationToken,
    ) -> PortFuture<HelperResult> {
        self.run(request, cancel)
    }
    fn run(&self, request: HelperRequest, cancel: CancellationToken) -> PortFuture<HelperResult> {
        let streams = self.streams.lock().unwrap().clone().unwrap();
        let failed = self.failed.clone();
        Box::pin(async move {
            let producer = streams.claim(&request.selection)?;
            while !streams.consumer_pending("speech") {
                if cancel.is_cancelled() {
                    return Err(PortError::Unavailable);
                }
                tokio::task::yield_now().await;
            }
            drop(producer);
            failed.store(true, Ordering::SeqCst);
            Err(PortError::Unavailable)
        })
    }
    fn cleanup(&self) -> PortFuture<()> {
        self.joined.store(true, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

struct Rebind(Mutex<std::collections::VecDeque<MediaBindings>>);
impl MediaBindingFactory for Rebind {
    fn bind(
        &self,
        _: InvocationBinding,
        _: Arc<Definition>,
        _: SelectedEntrypoint,
        _: Caller,
    ) -> PortFuture<MediaBindings> {
        let result = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(PortError::Conflict);
        Box::pin(async move { result })
    }
}

#[tokio::test]
async fn tool_streams_media_queue_termination_joins_and_preserves_reusable_capacity() {
    if !crate::test_support::isolated_process_test(
        "routine::adapters::media::tests::tool_streams_speech_tests::tool_streams_media_queue_termination_joins_and_preserves_reusable_capacity",
    ) {
        return;
    }
    let root = crate::test_support::temp_root().canonicalize().unwrap();
    let private = root.join("private");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&private).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let blobs = Arc::new(
        FilesystemBlobStore::new(FilesystemConfig {
            private_root: private,
            workspace_root: blob::filesystem::WorkspaceRoot::Local(workspace),
            clock: Arc::new(blob::MonotonicClock::default()),
        })
        .unwrap(),
    );
    let persistence = Persistence::initialize(
        DatabaseConfig::from_urls(
            &format!(
                "sqlite:{}?mode=rwc",
                root.join("termination.sqlite").display()
            ),
            "sqlite::memory:",
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let engine = Arc::new(Engine::new(1).unwrap());
    let example = Definition::parse(
        include_str!("../../../../../examples/routines/streamed-speech.yaml"),
        engine.as_ref(),
    )
    .unwrap();
    let mut envelope: Value = serde_json::from_str(example.canonical()).unwrap();
    envelope["spec"]["ports"] = json!({});
    let mut bindings = std::collections::VecDeque::new();
    let mut owners = vec![];
    for _ in 0..4 {
        let cancel = CancellationToken::new();
        let streams = Streams::new(
            stream::Limits::default(),
            Pool::principal(),
            Pool::process(),
            cancel.clone(),
        )
        .unwrap();
        let driver = Arc::new(Driver {
            cold: Mutex::new(Some(vec![])),
            tasks: tokio::sync::Mutex::new(vec![]),
            cancellation: cancel,
            joined: AtomicBool::new(false),
            completed: AtomicBool::new(true),
            uncertain: AtomicBool::new(false),
        });
        let helpers = Arc::new(FailingHelpers::default());
        bindings.push_back(MediaBindings {
            driver: Arc::new(OwnedDriver(driver.clone())),
            snapshot: false,
            principal: "owner".into(),
            streams: streams.clone(),
            inputs: BTreeMap::new(),
            outputs: BTreeMap::new(),
            uploads: BTreeMap::new(),
            speech: Arc::new(Speech {
                seen: Arc::new(Mutex::new(vec![])),
                first_spoken: Arc::new(tokio::sync::Notify::new()),
                helper_done: Arc::new(AtomicBool::new(false)),
            }),
            helpers: helpers.clone(),
            context: None,
            blobs: blobs.clone(),
            text: persistence.script_text_result_store(),
            attachments: TransientInputBudget::process(),
            workspace_principal: WorkspacePool::principal(),
            workspace_process: WorkspacePool::process(),
            result_principal: ResultPool::principal(),
            result_process: ResultPool::process(),
            leases: Arc::new(Mutex::new(vec![])),
        });
        owners.push((helpers, driver, streams));
    }
    let service = InvocationService::new(
        engine.clone(),
        persistence.script_invocation_store(),
        Arc::new(MediaHostFactory {
            bindings: Arc::new(Rebind(Mutex::new(bindings))),
        }),
        Arc::new(SystemClock),
        1,
    )
    .unwrap();
    for (index, source) in [
        "export default async function() { await host.call('helper.next', {stream:'speech'}); return true; }",
        "export default async function() { await host.call('media.finish', {}); return true; }",
        "export default async function() { const next = host.call('helper.next', {stream:'speech'}); const helper = host.call('helper.run', {selection:{kind:'named',name:'routine-speaker'},task:'fail',maxTurns:1,deadlineMs:5000,responseBytes:1024,completionBytes:1024}); await Promise.all([next,helper]); return true; }",
        "export default async function() { await host.call('media.finish', {}); return true; }",
    ]
    .into_iter()
    .enumerate()
    {
        envelope["spec"]["source"] = json!(source);
        let definition = Definition::parse(&envelope.to_string(), engine.as_ref()).unwrap();
        let handle = service
            .invoke(
                definition,
                "run".into(),
                Caller {
                    principal: "owner".into(),
                    origin: Origin::Interactive,
                    ownership: Ownership::Leased,
                    correlation: format!("termination-{index}"),
                },
                json!({"task":"unused"}),
            )
            .unwrap()
            .await
            .unwrap();
        let (helpers, driver, streams) = &owners[index];
        if index == 0 {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let registry = helpers.streams.lock().unwrap().clone().unwrap();
                    if registry.consumer_pending("speech") {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            handle.cancel();
        }
        let outcome = tokio::time::timeout(Duration::from_secs(5), handle.join())
            .await
            .unwrap();
        assert_eq!(
            outcome.status,
            Some(match index {
                0 | 2 => Status::Uncertain,
                _ => Status::Succeeded,
            }),
            "{outcome:?}"
        );
        assert!(outcome.cleanup_complete, "{outcome:?}");
        assert!(helpers.joined.load(Ordering::SeqCst));
        assert_eq!(helpers.failed.load(Ordering::SeqCst), index == 2);
        assert!(driver.joined.load(Ordering::SeqCst));
        assert_eq!(streams.retained_bytes(), 0);
        assert!(
            service.is_ready(),
            "queue termination must release healthy capacity"
        );
    }
    service.shutdown().await.unwrap();
    engine.shutdown().await.unwrap();
    blobs.shutdown().await.unwrap();
}
