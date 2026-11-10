use super::*;
use crate::{
    agent_host::runtime_factory::ServerAgentRuntimeFactory,
    auth::FileCredentialStore,
    interaction::{InteractionBroker, InteractionResponse},
    permission::{PermissionResponse, ServerPermissionPolicy},
};
use waltr_agent::AgentRuntimeProvider;
use waltr::grpc::waltr::routine::v2::routine_service_server::RoutineService as _;

struct AskProvider;
impl LlmStream for AskProvider {
    fn name(&self) -> &'static str { "original-history-live-ask" }
    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            let called = request.tool_calls().any(|call| call.name == "manage-context");
            let event = if request.prompt == "RUN_ASK" && !called {
                CompletionEvent::ToolCall(ToolCall::new("exact-management-call", "manage-context", json!({"instruction":"Read only the authorized original history"})))
            } else { CompletionEvent::Delta("done".into()) };
            Ok(Box::pin(futures_util::stream::iter([Ok(event), Ok(CompletionEvent::Finished)])) as BoxCompletionStream)
        })
    }
}

#[tokio::test]
async fn normal_dispatcher_live_ask_grants_only_the_exact_original_source_and_cancel_retires_it() {
    if !crate::test_support::isolated_process_test("routine::production::runtime::tests::context_workflows::context_authority::normal_dispatcher_live_ask_grants_only_the_exact_original_source_and_cancel_retires_it") { return; }
    let root = crate::test_support::temp_root();
    let resources = root.join("resources");
    seed(&resources);
    let agents = resources.join(".waltr/resources/agents");
    std::fs::create_dir_all(&agents).unwrap();
    for (name, permission) in [("ask-source", "ask"), ("denied-source", "deny")] {
        std::fs::write(agents.join(format!("{name}.yaml")), format!("apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {{name: {name}}}\nspec:\n  systemPrompt: SOURCE\n  permissions:\n    - {{name: manage-context, permission: {permission}}}\n")).unwrap();
    }
    let routines = resources.join(".waltr/resources/routines");
    for name in ["context-management", "context-categorization"] {
        let source = if name == "context-management" {
            "export default async function(){const original=(await host.call('context.original',{})).result;let denied=false;try{await host.call('context.history',{source:'foreign-source'});}catch(_){denied=true;}if(!denied)throw new Error('foreign source accepted');return JSON.stringify(original.items);}"
        } else { "export default async function(){return 'unused';}" };
        std::fs::write(routines.join(format!("{name}.yaml")), json!({"apiVersion":"waltr.routine/v1","kind":"Routine","name":name,"description":"Focused original history authority fixture","spec":{
            "source":source,"entrypoints":if name == "context-management" { json!({"session-tool":{"trigger":"tool","selector":"manage-context"},"manual":{"trigger":"manual"}}) } else { json!({"manual":{"trigger":"manual"}}) },
            "parameters":{"schemaVersion":"waltr.value/v1","schema":{"type":"object","properties":{"instruction":{"type":"string","maxLength":8192}},"required":["instruction"]}},
            "result":{"schemaVersion":"waltr.value/v1","schema":{"type":"string","maxLength":32768}},
            "capabilities":["context.original","context.history"]
        }}).to_string()).unwrap();
    }
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let config = ConfigService::new(resources, crate::test_support::default_catalog_loader()).await.unwrap();
    let state = config.state().clone();
    let inference = bindings::default_inference(&state.snapshot()).unwrap();
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let plain = AgentRuntime::builder(bindings::model(&inference), Arc::new(AskProvider)).build();
    for name in ["approved", "denied", "cancelled", "foreign"] {
        let mut create = SessionCreateRequest::new(SessionId::new(name));
        create.inference_settings = Some(inference.clone());
        create.agent_definition = Some(serde_json::to_value(state.snapshot().catalog().bundle(if name == "denied" { "denied-source" } else { "ask-source" }).unwrap()).unwrap());
        sessions.create(create).await.unwrap();
        let events = sessions.prompt(&plain, SessionId::new(name), format!("PRIVATE_ORIGINAL_{name}")).stream().collect::<Vec<_>>().await;
        assert!(events.iter().all(Result::is_ok));
        sessions.set_metadata_value(&SessionId::new(name), "title", json!("already titled")).await.unwrap();
    }
    let sandbox = crate::sandbox::LocalSandbox::start(workspace.clone()).await.unwrap();
    let broker = InteractionBroker::new();
    let native_model = bindings::model(&inference);
    let factory = Arc::new(ServerAgentRuntimeFactory::new(
        sandbox.handle(), workspace.clone(), FileCredentialStore::new(root.join("unused-auth.json")),
        state.snapshot().catalog().resolved("ask-source").unwrap().clone(),
    ).with_agent_catalog(state.snapshot().catalog().clone())
        .with_runtime_provider(move |_| Ok(AgentRuntimeProvider::new(native_model.clone(), Arc::new(AskProvider))))
        .with_capability_policy(ServerPermissionPolicy::sessions(broker.clone())));
    let persistence = Persistence::initialize(crate::persistence::DatabaseConfig::from_urls(
        &format!("sqlite:{}?mode=rwc", root.join("waltr.sqlite").display()), "sqlite::memory:",
    ).unwrap()).await.unwrap();
    let history = persistence.script_invocation_store();
    let management_revision = state.snapshot().scripts()["context-management"].revision().to_owned();
    let production = ProductionRuntime::new(ProductionInput { state, persistence, workspace, private_parent:root.clone(), authority:RoutineAuthority::default() }, sessions.clone(), factory).await.unwrap();
    let origin = crate::local_admission::ValidatedLocalOrigin(Arc::new(CancellationToken::new()));
    let principal = crate::access_auth::AuthenticatedPrincipal::local_process(&origin);
    for name in ["approved", "denied", "cancelled"] {
        let id = SessionId::new(name);
        let record = sessions.load(&id).await.unwrap().record;
        let factory = production.code.factory(principal.clone(), &record).await.unwrap();
        let (runtime, dispatcher) = factory.runtime_for_record(&record, sessions.clone()).await.unwrap();
        assert!(!dispatcher.revision().is_empty());
        let before = history.list(0, 100).await.unwrap().into_iter().map(|row| row.cursor).max().unwrap_or(0);
        let mut direct = tonic::Request::new(p::StartInvocationRequest { selection: Some(p::InvocationSelection {
            routine:"context-management".into(), revision:management_revision.clone(), entrypoint:"manual".into(),
            source_session:Some(name.into()), correlation:"not-a-live-approval".into(),
            parameters:Some(p::StructuredValue {json:serde_json::to_vec(&json!({"instruction":"Read original"})).unwrap()}),
            ..Default::default()
        }) });
        direct.extensions_mut().insert(principal.clone());
        assert_eq!(production.rpc.start_invocation(direct).await.unwrap_err().code(), tonic::Code::PermissionDenied, "direct Ask has no live dispatcher approval");
        let token = CancellationToken::new();
        let builder = sessions.prompt(&runtime, id.clone(), "RUN_ASK").with_cancellation_token(token.clone());
        let mut subscription = broker.subscribe(id.clone());
        let run = tokio::spawn(async move { builder.stream().collect::<Vec<_>>().await });
        if name != "denied" {
            let pending = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop { let snapshot = subscription.next().await.unwrap(); if !snapshot.pending.is_empty() { break snapshot; } }
            }).await.unwrap();
            let request = &pending.pending[0];
            assert!(history.list(0, 100).await.unwrap().iter().all(|row| row.revision != management_revision || row.cursor <= before));
            let answer = || InteractionResponse::Permission(PermissionResponse::AllowOnce);
            assert!(broker.respond(&SessionId::new("foreign"), &pending.generation, &request.id, &request.operation_id, answer()).is_err());
            assert!(broker.respond(&id, &pending.generation, &request.id, "foreign-operation", answer()).is_err());
            if name == "cancelled" { token.cancel(); } else {
                broker.respond(&id, &pending.generation, &request.id, &request.operation_id, answer()).unwrap();
            }
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), run).await.unwrap().unwrap();
        assert!(broker.snapshot(&id).pending.is_empty());
        let rows = history.list(0, 100).await.unwrap();
        let rows = rows.into_iter().filter(|row| row.revision == management_revision && row.cursor > before).collect::<Vec<_>>();
        if name == "approved" {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].status, crate::routine::history::Status::Succeeded);
            let record = sessions.load(&id).await.unwrap().record;
            let results = record.timeline_events().filter_map(|event| match &event.content {
                waltr_agent::TimelineContent::ToolResult { result } if result.call_id == "exact-management-call" => Some(result.text_projection().to_owned()), _ => None,
            }).collect::<Vec<_>>().join("\n");
            assert!(results.contains("PRIVATE_ORIGINAL_approved"));
            assert!(!results.contains("PRIVATE_ORIGINAL_foreign"));
            assert!(broker.respond(&id, "retired-generation", "retired-request", "foreign-operation", InteractionResponse::Permission(PermissionResponse::AllowOnce)).is_err());
        } else { assert!(rows.is_empty(), "denial/cancellation must not enter the handler"); }
    }
    production.shutdown().await.unwrap();
    sessions.shutdown().await;
}
