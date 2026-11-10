use super::*;
use waltr_agent::{MemorySessionStore, PendingTimelineEvent, PendingTimelineItemId, SessionJournalEvent};
mod gated_store;

async fn completed() -> (SessionCoordinator<MemorySessionStore>, SessionId) {
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let id = SessionId::generate();
    let mut request = SessionCreateRequest::new(id.clone());
    request.agent_definition = Some(json!({"opaque":"definition"}));
    request.inference_settings = Some(waltr_agent::InferenceSettings {
        provider: waltr_agent::InferenceProviderSettings {
            key: "test".into(), api: waltr_agent::ModelApi::OpenAiResponses,
            url: "https://private.invalid".into(), model_id: "test".into(),
            model_cost: None, headers: Default::default(), oauth: None, reasoning_effort: None,
        },
    });
    sessions.create(request).await.unwrap();
    let created = sessions.store().load(&id).await.unwrap();
    let events = sessions.store().append_timeline(&id, created.execution_token().unwrap(), vec![
        PendingTimelineEvent::assistant_text_snapshot(id.clone(), 0, PendingTimelineItemId::New, "source evidence"),
    ]).await.unwrap();
    sessions.store().append_journal(&id, events.commit.execution_token, vec![SessionJournalEvent::TurnFinished { turn: 0 }]).await.unwrap();
    assert_eq!(events.events.len(), 1);
    (sessions, id)
}

async fn call(context: &impl ContextPort, name: &str, args: Value) -> Result<Value, PortError> {
    context.execute(ContextOperation::parse(name, args)?, CancellationToken::new()).await
}

fn transaction(revision: &str, predicates: Value, updates: Value, masks: Value) -> Value {
    json!({"source":"effective","expectedRevision":revision,"predicates":predicates,"updates":updates,"masks":masks})
}

#[tokio::test]
async fn production_context_constructor_preserves_null_absence_and_item_scope() {
    let (sessions, id) = completed().await;
    let context = CoordinatorContext::pin(sessions.clone(), &id, "owner", false).await.unwrap();
    assert_eq!(call(&context, "context.metadata", json!({"source":"effective","key":"free"})).await.unwrap(), json!({"state":"absent"}));
    let snapshot = call(&context, "context.snapshot", json!({})).await.unwrap();
    let item = snapshot["items"][0]["item_id"].clone();
    assert!(snapshot["items"][0]["cursor"].is_string());
    assert!(!snapshot.to_string().contains("private.invalid"));
    call(&context, "context.transact", transaction(snapshot["revision"].as_str().unwrap(),
        json!([{ "key":"free","value":{"state":"absent"}}]),
        json!([{ "key":"free","value":{"state":"present","value":null}},
            {"itemId":item,"key":"arbitrary.schema","value":{"state":"present","value":{"opaque":[1,null]}}}]), Value::Null)).await.unwrap();
    assert_eq!(call(&context, "context.metadata", json!({"source":"effective","key":"free"})).await.unwrap(), json!({"state":"present","value":null}));
    assert_eq!(call(&context, "context.metadata", json!({"source":"effective","itemId":item,"key":"arbitrary.schema"})).await.unwrap()["value"], json!({"opaque":[1,null]}));
    let current = sessions.store().load(&id).await.unwrap();
    let revision = current.revision.get().to_string();
    let bad = transaction(&revision, json!([{ "key":"free","value":{"state":"absent"}}]), json!([{ "key":"partial","value":{"state":"present","value":true}}]), Value::Null);
    assert_eq!(call(&context, "context.transact", bad).await, Err(PortError::Conflict));
    assert_eq!(sessions.store().load(&id).await.unwrap(), current);
    call(&context, "context.transact", transaction(&revision,
        json!([{ "key":"free","value":{"state":"present","value":null}}]),
        json!([{ "key":"free","value":{"state":"absent"}}]), Value::Null)).await.unwrap();
    assert_eq!(call(&context, "context.metadata", json!({"source":"effective","key":"free"})).await.unwrap()["state"], "absent");
    context.cleanup().await.unwrap();
}

#[tokio::test]
async fn production_context_rejects_reserved_bounds_foreign_sources_and_unknown_operations() {
    let (sessions, id) = completed().await;
    let context = CoordinatorContext::pin(sessions.clone(), &id, "owner", false).await.unwrap();
    let snapshot = call(&context, "context.snapshot", json!({})).await.unwrap();
    let revision = snapshot["revision"].as_str().unwrap();
    let before = sessions.store().load(&id).await.unwrap();
    for key in ["_private", "waltr.permission", "engine.state", ""] {
        assert_eq!(call(&context, "context.transact", transaction(revision, json!([]),
            json!([{ "key":key,"value":{"state":"present","value":true}}]), Value::Null)).await, Err(PortError::Invalid));
    }
    assert!(call(&context, "context.transact", transaction(revision, json!([]),
        json!([{ "itemId":"i999999","key":"opaque","value":{"state":"present","value":true}}]), Value::Null)).await.is_err());
    assert!(call(&context, "context.history", json!({"source":id.as_str()})).await.is_err());
    assert_eq!(call(&context, "context.original", json!({})).await, Err(PortError::Denied));
    assert!(ContextOperation::parse("context.title", json!({"title":"forbidden alias"})).is_err());
    assert!(ContextOperation::parse("context.transact", transaction(revision, json!([]),
        json!([{ "key":"oversized","value":{"state":"present","value":"x".repeat(65_536)}}]), Value::Null)).is_err());
    assert!(ContextOperation::parse("context.metadata", json!({"source":"effective","key":"x","sessionId":"forged"})).is_err());
    let too_many_keys = (0..129).map(|index| json!({"key":format!("free{index}"),"value":{"state":"present","value":null}})).collect::<Vec<_>>();
    assert_eq!(call(&context, "context.transact", transaction(revision, json!([]), json!(too_many_keys), Value::Null)).await, Err(PortError::Invalid));
    assert_eq!(sessions.store().load(&id).await.unwrap(), before);
    context.cleanup().await.unwrap();
}

#[tokio::test]
async fn metadata_only_race_keeps_execution_token_but_rejects_atomic_metadata_mask_transaction() {
    let (sessions, id) = completed().await;
    let context = CoordinatorContext::pin(sessions.clone(), &id, "owner", true).await.unwrap();
    let snapshot = call(&context, "context.snapshot", json!({})).await.unwrap();
    let before = sessions.store().load(&id).await.unwrap();
    sessions.set_metadata_value(&id, "title", json!("concurrent title")).await.unwrap();
    let concurrent = sessions.store().load(&id).await.unwrap();
    assert_eq!(before.execution_token().unwrap(), concurrent.execution_token().unwrap());
    assert!(concurrent.revision > before.revision);
    let args = transaction(snapshot["revision"].as_str().unwrap(), json!([]),
        json!([{ "key":"partial","value":{"state":"present","value":true}}]),
        json!({"expected":[],"removeKeys":[],"removeOverlaps":[],"add":[{
            "key":"partial-mask","through":snapshot["boundary"],"replacement":"must not commit"}]}));
    assert_eq!(call(&context, "context.transact", args).await, Err(PortError::Conflict));
    assert_eq!(sessions.store().load(&id).await.unwrap(), concurrent);
    context.cleanup().await.unwrap();
}

#[tokio::test]
async fn execution_changes_invalidate_pinned_context_and_append_admission() {
    for change in ["settings", "definition", "mask", "journal"] {
        let (sessions, id) = completed().await;
        let context = CoordinatorContext::pin(sessions.clone(), &id, "owner", true).await.unwrap();
        let snapshot = call(&context, "context.snapshot", json!({})).await.unwrap();
        let before = sessions.store().load(&id).await.unwrap();
        let token = before.execution_token().unwrap();
        match change {
            "settings" => { sessions.store().set_inference_settings(&id, None).await.unwrap(); }
            "definition" => { sessions.store().set_agent_definition(&id, Some(json!({"changed":true}))).await.unwrap(); }
            "mask" => {
                let args = transaction(snapshot["revision"].as_str().unwrap(), json!([]), json!([]),
                    json!({"expected":[],"removeKeys":[],"removeOverlaps":[],"add":[{
                        "key":"external-mask","through":snapshot["boundary"],"replacement":"external"}]}));
                let other = CoordinatorContext::pin(sessions.clone(), &id, "other", true).await.unwrap();
                call(&other, "context.transact", args).await.unwrap();
                other.cleanup().await.unwrap();
            }
            "journal" => { sessions.store().append_journal(&id, token.clone(), vec![
                SessionJournalEvent::PromptAppended { prompt:"external work".into() }
            ]).await.unwrap(); }
            _ => unreachable!(),
        }
        let changed = sessions.store().load(&id).await.unwrap();
        assert!(changed.revision > before.revision, "{change}");
        assert_ne!(changed.execution_token().unwrap(), token, "{change}");
        assert!(matches!(sessions.store().append_journal(&id, token, vec![
            SessionJournalEvent::TurnFinished { turn: 1 }
        ]).await, Err(waltr_agent::SessionStoreError::StaleExecution { .. })), "{change}");
        assert_eq!(call(&context, "context.transact", transaction(snapshot["revision"].as_str().unwrap(),
            json!([]), json!([{ "key":"partial","value":{"state":"present","value":true}}]), Value::Null)).await,
            Err(PortError::Conflict), "{change}");
        assert_eq!(sessions.store().load(&id).await.unwrap(), changed, "{change}");
        context.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn production_context_stale_predicates_and_masks_never_partially_commit() {
    let (sessions, id) = completed().await;
    let first = CoordinatorContext::pin(sessions.clone(), &id, "owner", true).await.unwrap();
    let second = CoordinatorContext::pin(sessions.clone(), &id, "owner", true).await.unwrap();
    let snapshot = call(&first, "context.snapshot", json!({})).await.unwrap();
    let masks = json!({"expected":[],"removeKeys":[],"removeOverlaps":[],"add":[{
        "key":"program-chosen-key","through":snapshot["boundary"],"replacement":"program-chosen replacement"}]});
    let args = transaction(snapshot["revision"].as_str().unwrap(),
        json!([{ "key":"checkpoint","value":{"state":"absent"}}]),
        json!([{ "key":"checkpoint","value":{"state":"present","value":{"routineOwned":true}}}]), masks);
    call(&first, "context.transact", args.clone()).await.unwrap();
    let committed = sessions.store().load(&id).await.unwrap();
    assert_eq!(committed.active_masks()[0].key, "program-chosen-key");
    assert_eq!(call(&second, "context.transact", args).await, Err(PortError::Conflict));
    assert_eq!(sessions.store().load(&id).await.unwrap(), committed);
    let next = call(&first, "context.snapshot", json!({})).await.unwrap();
    sessions.store().append_journal(&id, committed.execution_token().unwrap(), vec![SessionJournalEvent::PromptAppended { prompt:"concurrent user work".into() }]).await.unwrap();
    let journal = sessions.store().load(&id).await.unwrap();
    assert_eq!(call(&first, "context.transact", transaction(next["revision"].as_str().unwrap(), json!([]),
        json!([{ "key":"partial","value":{"state":"present","value":true}}]), Value::Null)).await, Err(PortError::Conflict));
    assert_eq!(sessions.store().load(&id).await.unwrap(), journal);
    first.cleanup().await.unwrap();
    second.cleanup().await.unwrap();
}

#[tokio::test]
async fn production_context_creates_exact_message_and_configuration_without_automatic_start() {
    let (sessions, id) = completed().await;
    let context = CoordinatorContext::pin(sessions.clone(), &id, "owner", false).await.unwrap();
    let message = "  exact initial message\n\nnot a Rust prompt template\n";
    let result = call(&context, "context.create", json!({"source":"effective","copyHistory":false,
        "inheritDefinition":true,"inheritInference":true,"message":message})).await.unwrap();
    let destination = SessionId::new(result["sessionId"].as_str().unwrap());
    let created = sessions.store().load(&destination).await.unwrap();
    let source = sessions.store().load(&id).await.unwrap();
    assert_eq!(created.agent_definition, source.agent_definition);
    assert_eq!(created.inference_settings, source.inference_settings);
    assert_eq!(created.materialize_context(waltr_agent::SessionContextMode::Raw).current_prompt.as_deref(), Some(message));
    assert!(sessions.active_state(&destination).await.is_none());
    assert_eq!(call(&context, "context.start", json!({"destination":destination.as_str()})).await, Err(PortError::Denied));
    // No runtime factory is installed. The first dispatch is uncertain, not a retry grant.
    assert_eq!(call(&context, "context.start", json!({"destination":result["destination"]})).await, Err(PortError::Uncertain));
    assert_eq!(call(&context, "context.start", json!({"destination":result["destination"]})).await, Err(PortError::Denied));
    assert_eq!(sessions.store().load(&destination).await.unwrap(), created);
    context.cleanup().await.unwrap();
}

#[tokio::test]
async fn production_context_paused_commit_cancel_and_cleanup_retain_history_without_continuation() {
    let (sessions, id) = completed().await;
    let context = CoordinatorContext::compaction(sessions.clone(), &id, "owner").await.unwrap();
    context.prepare().await.unwrap();
    let snapshot = call(&context, "context.snapshot", json!({})).await.unwrap();
    let result = call(&context, "context.transact", transaction(snapshot["revision"].as_str().unwrap(), json!([]), json!([]),
        json!({"expected":[],"removeKeys":[],"removeOverlaps":[],"add":[{
            "key":"chosen-by-js","through":snapshot["boundary"],"replacement":"exact replacement"}]}))).await.unwrap();
    assert_eq!(call(&context, "context.continue", json!({"cursor":"999999"})).await, Err(PortError::Conflict));
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert_eq!(context.execute(ContextOperation::parse("context.continue", json!({"cursor":result["cursor"]})).unwrap(), cancellation).await, Err(PortError::Denied));
    context.cleanup().await.unwrap();
    let reason = waltr_agent::SessionCoordinationReason::new("test.pause", None, None).unwrap();
    let lease = sessions.acquire_pause_reject_busy(&id, reason).await.unwrap();
    sessions.release_pause(&lease).await.unwrap();
    assert_eq!(sessions.store().load(&id).await.unwrap().active_masks()[0].key, "chosen-by-js");
}

#[tokio::test]
async fn child_context_cannot_upgrade_effective_source_to_original_even_with_known_handle() {
    let (sessions, id) = completed().await;
    let context = CoordinatorContext::pin(sessions, &id, "owner", true).await.unwrap();
    let selected = call(&context, "context.history", json!({"source":"original"})).await.unwrap();
    let child = context.with_original_ceiling(false);
    assert!(child.resolve_source("effective").is_ok());
    assert_eq!(child.resolve_source("original").unwrap_err(), PortError::Denied);
    assert_eq!(child.resolve_source(selected["source"].as_str().unwrap()).unwrap_err(), PortError::Denied);
    child.cleanup().await.unwrap();
    context.cleanup().await.unwrap();
}

#[tokio::test]
async fn create_uses_validated_immutable_source_even_when_source_changes_during_destination_commit() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let (original, id) = completed().await;
        let before = original.store().load(&id).await.unwrap();
        let store = gated_store::GatedStore { memory: original.store().clone(), source: id.clone(),
            entered: Arc::new(tokio::sync::Notify::new()), resume: Arc::new(tokio::sync::Notify::new()) };
        let sessions = SessionCoordinator::new(store.clone());
        let context = Arc::new(CoordinatorContext::pin(sessions.clone(), &id, "owner", false).await.unwrap());
        let snapshot = call(&*context, "context.snapshot", json!({})).await.unwrap();
        let message = "  exact JS message\n\nno host suffix  ";
        let owned = context.clone();
        let create = tokio::spawn(async move {
            call(&*owned, "context.create", json!({"source":"effective","copyHistory":true,
                "inheritDefinition":true,"inheritInference":true,"message":message})).await
        });
        store.entered.notified().await;
        store.memory.append_timeline(&id, before.execution_token().unwrap(), vec![PendingTimelineEvent::assistant_text_snapshot(
            id.clone(), 1, PendingTimelineItemId::New, "later source bytes must not leak")]).await.unwrap();
        store.memory.set_agent_definition(&id, Some(json!({"changed":"after validation"}))).await.unwrap();
        store.memory.set_inference_settings(&id, None).await.unwrap();
        store.resume.notify_one();
        let result = create.await.unwrap().unwrap();
        let destination = store.memory.load(&SessionId::new(result["sessionId"].as_str().unwrap())).await.unwrap();
        assert_eq!(destination.agent_definition, before.agent_definition);
        assert_eq!(destination.inference_settings, before.inference_settings);
        assert!(serde_json::to_string(&destination).unwrap().contains("source evidence"));
        assert!(!serde_json::to_string(&destination).unwrap().contains("later source bytes"));
        assert_eq!(destination.journal.iter().filter_map(|event| match event {
            SessionJournalEvent::PromptAppended { prompt } => Some(prompt.as_str()), _ => None,
        }).collect::<Vec<_>>(), vec![message]);
        assert_ne!(store.memory.load(&id).await.unwrap().revision.get().to_string(), snapshot["revision"].as_str().unwrap());
        assert_eq!(call(&*context, "context.create", json!({"source":"effective","copyHistory":true,
            "inheritDefinition":true,"inheritInference":true,"message":"before validation must conflict"})).await, Err(PortError::Conflict));
        assert_eq!(store.memory.list().await.unwrap().len(), 2);
        context.cleanup().await.unwrap();
        sessions.shutdown().await;
        original.shutdown().await;
    }).await.expect("bounded source/create interleaving");
}

#[tokio::test]
async fn early_and_completed_transactions_require_pinned_durable_incarnation() {
    let (sessions, id) = completed().await;
    let record = sessions.store().load(&id).await.unwrap();
    assert!(matches!(CoordinatorSessionContext::pin_for_incarnation(sessions.clone(), &id, "previous-incarnation").await,
        Err(PortError::Conflict)));
    let early_context = CoordinatorSessionContext::pin_for_incarnation(sessions.clone(), &id, record.metadata.incarnation.as_deref().unwrap()).await.unwrap();
    early_context.cleanup().await.unwrap();
    let args = || {
        let operation = ContextOperation::parse("context.transact", transaction(&record.revision.get().to_string(),
            json!([]), json!([{ "key":"value","value":{"state":"present","value":null}}]), Value::Null)).unwrap();
        let ContextOperation::Transact(args) = operation else { unreachable!() };
        args
    };
    let early = mutation(&record, None, args()).unwrap();
    let identity = ContextIdentity::capture(&record, record.last_timeline_cursor().unwrap(), "owner").unwrap();
    let copy = SessionCopy::from_record(&record, identity.boundary()).unwrap();
    let completed = mutation(&record, Some((&copy, identity)), args()).unwrap();
    assert_eq!(early.expected_incarnation, record.metadata.incarnation);
    assert_eq!(completed.expected_incarnation, record.metadata.incarnation);
    let mut legacy = record.clone();
    legacy.metadata.incarnation = None;
    assert!(matches!(mutation(&legacy, None, args()), Err(PortError::Conflict)));
}

#[derive(Default)]
struct OrderedProvider(Mutex<Vec<String>>);
impl waltr_agent::LlmStream for OrderedProvider {
    fn name(&self) -> &'static str { "ordered-context-fixture" }
    fn stream_request<'a>(&'a self, request: waltr_agent::CompletionRequest<'a>) -> waltr_agent::BoxStreamFuture<'a> {
        self.0.lock().unwrap().push(request.prompt.to_string());
        Box::pin(async { Ok(Box::pin(futures_util::stream::iter([
            Ok(waltr_agent::CompletionEvent::Delta("done".into())),
            Ok(waltr_agent::CompletionEvent::Finished),
        ])) as waltr_agent::BoxCompletionStream) })
    }
}

struct OrderedHost(Arc<OrderedProvider>);
impl waltr_agent::AgentHostRuntimeFactory for OrderedHost {
    fn build<'a>(&'a self, _: &'a SessionRecord) -> waltr_agent::HostRuntimeFuture<'a> {
        Box::pin(async move { Ok(waltr_agent::AgentRuntime::builder(waltr_agent::ModelRef::new(
            waltr_agent::ProviderId::OpenAi, "fixture", waltr_agent::ModelApi::OpenAiResponses), self.0.clone()).build()) })
    }
}

#[tokio::test]
async fn successful_primitive_continuation_runs_before_queued_user_work_after_joined_cleanup() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let (sessions, id) = completed().await;
        let provider = Arc::new(OrderedProvider::default());
        let credentials = waltr_agent::FileCredentialStore::new(crate::test_support::temp_root().join("unused-auth.json"));
        let factory = waltr_agent::AgentSessionRuntimeFactory::new(credentials)
            .with_host_runtime_factory(Arc::new(OrderedHost(provider.clone())));
        let sessions = sessions.with_runtime_factory(factory);
        let context = CoordinatorContext::compaction(sessions.clone(), &id, "owner").await.unwrap();
        context.prepare().await.unwrap();
        let queued = sessions.submit_prompt(id.clone(), "queued after continuation", waltr_agent::RunConflictPolicy::QueueIfActive).await.unwrap();
        assert!(provider.0.lock().unwrap().is_empty());
        let before = call(&context, "context.snapshot", json!({})).await.unwrap();
        let mutation = call(&context, "context.transact", transaction(before["revision"].as_str().unwrap(), json!([]), json!([]),
            json!({"expected":[],"removeKeys":[],"removeOverlaps":[],"add":[{
                "key":"arbitrary-js-key","through":before["boundary"],"replacement":"JS continuation prompt"}]}))).await.unwrap();
        let armed = call(&context, "context.continue", json!({"cursor":mutation["cursor"]})).await.unwrap();
        assert_eq!(armed["armed"], true);
        assert_eq!(call(&context, "context.continue", json!({"cursor":mutation["cursor"]})).await, Err(PortError::Conflict));
        assert!(provider.0.lock().unwrap().is_empty(), "pause retains both runs until cleanup");
        context.cleanup().await.unwrap();
        queued.handle.wait().await.unwrap();
        let requests = provider.0.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains("JS continuation prompt"));
        assert!(!requests[0].contains("queued after continuation"));
        assert!(requests[1].contains("queued after continuation"));
        let record = sessions.store().load(&id).await.unwrap();
        assert!(!record.journal.iter().any(|event| matches!(event, SessionJournalEvent::Rollback { .. })));
        assert_eq!(record.active_masks()[0].key, "arbitrary-js-key");
        sessions.shutdown().await;
    }).await.expect("bounded primitive continuation ordering");
}
