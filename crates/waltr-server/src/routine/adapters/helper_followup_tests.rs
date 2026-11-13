use super::*;
use crate::permission::{PermissionEffect, PermissionPolicy, PermissionToolSelector};
use tokio_util::sync::CancellationToken;

#[test]
fn repeated_audio_resource_compiles_with_bounded_declarations() {
    let engine = Engine::new(1).unwrap();
    let definition = Definition::parse(
        include_str!("../../../../../.waltr/resources/routines/audio-ask-and-speak.yaml"),
        &engine,
    )
    .unwrap();
    assert!(
        definition
            .envelope()
            .spec
            .capabilities
            .contains("media.inputs")
    );
    assert!(definition.envelope().spec.source.contains("helper.open"));
}

struct Provider {
    inner: ScriptedProvider,
    entered: Arc<tokio::sync::Notify>,
    gate: Arc<tokio::sync::Semaphore>,
    calls: Arc<AtomicUsize>,
    active: Arc<AtomicUsize>,
}
struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl LlmStream for Provider {
    fn name(&self) -> &'static str {
        "followup-test"
    }
    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            self.active.fetch_add(1, Ordering::SeqCst);
            let _active = Active(self.active.clone());
            if self.calls.load(Ordering::SeqCst) == 0 {
                self.entered.notify_one();
                self.gate.acquire().await.unwrap().forget();
            }
            self.inner.stream_request(request).await
        })
    }
}

async fn fixture(
    named: bool,
) -> (
    CoordinatorHelperRunner<MemorySessionStore>,
    SessionCoordinator<MemorySessionStore>,
    Arc<Provider>,
    Arc<super::super::tool_streams::ToolStreams>,
    crate::sandbox::LocalSandbox,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let provider = Arc::new(Provider {
        entered: Arc::new(tokio::sync::Notify::new()),
        gate: Arc::new(tokio::sync::Semaphore::new(0)),
        calls,
        active: Arc::new(AtomicUsize::new(0)),
        inner: ScriptedProvider::new("followup-test", move |request| {
            let call = seen.fetch_add(1, Ordering::SeqCst);
            let debug = format!("{request:?}");
            assert!(debug.contains("initial-question"));
            if call >= 1 {
                assert!(
                    debug.contains("followup-one") && debug.contains("followup-two"),
                    "{debug}"
                );
            }
            let event = if call % 2 == 0 {
                CompletionEvent::ToolCall(ToolCall::new(
                    format!("speak-{call}"),
                    "speak",
                    json!({"text":format!("finding-{call}")}),
                ))
            } else {
                CompletionEvent::Delta(format!("answer-{call}"))
            };
            vec![Ok(event), Ok(CompletionEvent::Finished)]
        }),
    });
    let root = crate::test_support::temp_root();
    let sandbox = crate::sandbox::LocalSandbox::start(root.clone())
        .await
        .unwrap();
    let catalog = AgentDefinitionCatalog::from_sources(vec!["apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {name: writer}\nspec:\n  systemPrompt: ORIGINAL\n  permissions: [{name: speak, permission: allow}]\n".into()], "writer").unwrap();
    let native = provider.clone();
    let factory = Arc::new(
        AgentSessionRuntimeFactory::new(
            sandbox.handle(),
            root.clone(),
            FileCredentialStore::new(root.join("unused.json")),
            catalog.resolved("writer").unwrap().clone(),
        )
        .with_agent_catalog(catalog)
        .with_runtime_provider(move |_| Ok(AgentRuntimeProvider::new(model(), native.clone())))
        .with_capability_policy(ServerPermissionPolicy::sessions(InteractionBroker::new())),
    );
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let sessions = sessions
        .clone()
        .with_runtime_factory(factory.session_factory(sessions));
    let runtime = HostRuntimeBuilder::new(model(), provider.clone()).with_permission_policy(
        PermissionPolicy::default().with_tool_rule(
            PermissionToolSelector::exact("speak"),
            PermissionEffect::allow(),
        ),
    );
    let runner = CoordinatorHelperRunner::new(
        sessions.clone(),
        factory,
        inference(),
        BTreeMap::from([(
            "writer".into(),
            RestrictedHelperProfile {
                runtime,
                inference: inference(),
                prompt: "prefix".into(),
                max_turns: 4,
            },
        )]),
        BTreeMap::new(),
        None,
        "owner".into(),
    )
    .unwrap();
    runner.pin_named(vec!["writer".into()]).await.unwrap();
    runner.bind_captures(BTreeMap::new()).await.unwrap();
    let streams = super::super::tool_streams::ToolStreams::new(serde_json::from_value(json!({"speech":{
        "selection": selection(named), "tool":"speak", "description":"Queue a finding", "maxCalls":4,"maxBytes":4096,
        "contract":{"schemaVersion":"waltr.value/v1","schema":{"type":"object","properties":{"text":{"type":"string","maxLength":64}},"required":["text"]}}
    }})).unwrap());
    runner.bind_tool_streams(streams.clone()).await.unwrap();
    (runner, sessions, provider, streams, sandbox)
}
fn selection(named: bool) -> HelperSelection {
    if named {
        HelperSelection::Named {
            name: "writer".into(),
        }
    } else {
        HelperSelection::Restricted {
            profile: "writer".into(),
        }
    }
}
async fn open(
    runner: &CoordinatorHelperRunner<MemorySessionStore>,
    named: bool,
    cancel: CancellationToken,
) -> Arc<dyn HelperTarget> {
    runner
        .open(
            HelperRequest {
                selection: selection(named),
                task: "initial-question".into(),
                capture: None,
                max_turns: 4,
                deadline: Duration::from_secs(5),
                observer: None,
            },
            HelperPayload {
                budget: TransientInputBudget::process(),
                attachments: Vec::new(),
                capabilities: TransientModelCapabilities {
                    text: true,
                    png: false,
                    jpeg: false,
                    webp: false,
                    pdf: false,
                    max_body_bytes: 65536,
                },
                response_bytes: 4096,
                completion_bytes: 65536,
            },
            cancel,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn helper_target_uses_next_turn_history_and_keeps_stream_open_until_seal() {
    for named in [false, true] {
        let (runner, sessions, provider, streams, sandbox) = fixture(named).await;
        let cancel = CancellationToken::new();
        let target = open(&runner, named, cancel.clone()).await;
        provider.entered.notified().await;
        target.submit("followup-one".into()).await.unwrap();
        target.submit("followup-two".into()).await.unwrap();
        let summaries = sessions.list().await.unwrap();
        assert_eq!(summaries.len(), 1);
        let id = summaries[0].id.clone();
        assert_eq!(sessions.active_state(&id).await.unwrap().queued_count, 0);
        provider.gate.add_permits(1);
        tokio::time::timeout(Duration::from_secs(2), target.idle())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            2,
            "followups must reach the second model request without another run"
        );
        for text in ["finding-0"] {
            assert_eq!(
                streams
                    .next("speech", cancel.clone())
                    .await
                    .unwrap()
                    .unwrap()["arguments"]["text"],
                text
            );
        }
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                streams.next("speech", cancel.clone())
            )
            .await
            .is_err(),
            "idle must not close speech"
        );
        let result = target.seal().await.unwrap();
        assert_eq!(result.session_id, id);
        assert_eq!(result.response, "answer-1");
        assert!(
            streams
                .next("speech", cancel.clone())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            target.submit("too-late".into()).await,
            Err(PortError::Denied)
        );
        let record = sessions.store().load(&id).await.unwrap();
        let history = format!(
            "{:?}",
            record.materialize_context(SessionContextMode::Effective)
        );
        for text in [
            "initial-question",
            "followup-one",
            "followup-two",
            "answer-1",
        ] {
            assert!(history.contains(text), "{history}");
        }
        runner.cleanup().await.unwrap();
        sessions.shutdown().await;
        sandbox.shutdown().await;
    }
}

#[tokio::test]
async fn helper_target_cleanup_joins_active_provider_and_next_turn_prompts() {
    let (runner, sessions, provider, _, sandbox) = fixture(true).await;
    let target = open(&runner, true, CancellationToken::new()).await;
    provider.entered.notified().await;
    target.submit("followup-one".into()).await.unwrap();
    target.submit("followup-two".into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), runner.cleanup())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(provider.active.load(Ordering::SeqCst), 0);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert!(target.seal().await.is_err());
    let summaries = sessions.list().await.unwrap();
    assert!(sessions.active_state(&summaries[0].id).await.is_none());
    sessions.shutdown().await;
    sandbox.shutdown().await;
}

#[tokio::test]
async fn helper_target_seal_race_rejects_or_joins_admitted_followup() {
    let (runner, sessions, provider, streams, sandbox) = fixture(true).await;
    let target = open(&runner, true, CancellationToken::new()).await;
    provider.entered.notified().await;
    target.submit("followup-one".into()).await.unwrap();
    target.submit("followup-two".into()).await.unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let seal = {
        let target = target.clone();
        let barrier = barrier.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            target.seal().await
        })
    };
    let submit = {
        let target = target.clone();
        let barrier = barrier.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            target.submit("racing-followup".into()).await
        })
    };
    barrier.wait().await;
    let admitted = submit.await.unwrap();
    assert!(admitted.is_ok() || admitted == Err(PortError::Denied));
    assert!(!seal.is_finished(), "seal must join every accepted run");
    provider.gate.add_permits(1);
    let result = seal.await.unwrap().unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    let record = sessions.store().load(&result.session_id).await.unwrap();
    let history = format!(
        "{:?}",
        record.materialize_context(SessionContextMode::Effective)
    );
    assert_eq!(history.contains("racing-followup"), admitted.is_ok());
    while streams
        .next("speech", CancellationToken::new())
        .await
        .unwrap()
        .is_some()
    {}
    assert_eq!(
        target.submit("after-seal".into()).await,
        Err(PortError::Denied)
    );
    runner.cleanup().await.unwrap();
    sessions.shutdown().await;
    sandbox.shutdown().await;
}

#[tokio::test]
async fn helper_target_deadline_joins_next_turn_waiters_without_reset() {
    let (runner, sessions, provider, _, sandbox) = fixture(true).await;
    let target = open(&runner, true, CancellationToken::new()).await;
    provider.entered.notified().await;
    target.submit("followup-one".into()).await.unwrap();
    target.submit("followup-two".into()).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(7), target.seal())
            .await
            .unwrap()
            .is_err()
    );
    runner.cleanup().await.unwrap();
    assert_eq!(provider.active.load(Ordering::SeqCst), 0);
    let summaries = sessions.list().await.unwrap();
    assert!(sessions.active_state(&summaries[0].id).await.is_none());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    sessions.shutdown().await;
    sandbox.shutdown().await;
}

#[tokio::test]
async fn helper_target_submission_ceiling_is_lifetime_bounded() {
    let (runner, sessions, provider, streams, sandbox) = fixture(true).await;
    let target = open(&runner, true, CancellationToken::new()).await;
    provider.entered.notified().await;
    for index in 0..16 {
        let prompt = match index {
            0 => "followup-one".into(),
            1 => "followup-two".into(),
            _ => format!("followup-{index}"),
        };
        target.submit(prompt).await.unwrap();
    }
    assert_eq!(
        target.submit("over-limit".into()).await,
        Err(PortError::Denied)
    );
    provider.gate.add_permits(1);
    target.idle().await.unwrap();
    assert_eq!(
        target.submit("still-over-limit".into()).await,
        Err(PortError::Denied)
    );
    target.seal().await.unwrap();
    while streams
        .next("speech", CancellationToken::new())
        .await
        .unwrap()
        .is_some()
    {}
    runner.cleanup().await.unwrap();
    sessions.shutdown().await;
    sandbox.shutdown().await;
}
