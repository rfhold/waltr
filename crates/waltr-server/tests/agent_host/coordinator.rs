use super::{sandbox::TestSandbox, scripted_provider::ScriptedProvider};
use crate::{
    agent_host::{
        AgentDefinitionBundle, AgentDefinitionCatalog, runtime_factory::ServerAgentRuntimeFactory,
    },
    auth::FileCredentialStore,
    interaction::InteractionBroker,
    permission::ServerPermissionPolicy,
};
use futures_util::StreamExt;
use serde_json::json;
use std::sync::Arc;
use waltr_agent::*;

struct Fixture {
    sessions: SessionCoordinator<MemorySessionStore>,
    factory: ServerAgentRuntimeFactory,
    _sandbox: TestSandbox,
    _root: tempfile::TempDir,
}

fn settings() -> InferenceSettings {
    InferenceSettings {
        provider: InferenceProviderSettings {
            key: "test".into(),
            api: ModelApi::OpenAiResponses,
            url: "https://unused.invalid".into(),
            model_id: "test".into(),
            model_cost: None,
            headers: Default::default(),
            oauth: None,
            reasoning_effort: None,
        },
    }
}

impl Fixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let sandbox = TestSandbox::new(root.path()).await;
        let catalog = AgentDefinitionCatalog::from_sources(vec![r#"
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: {name: alpha}
spec: {systemPrompt: alpha prompt, features: [{task: {maxSubagentDepth: 1, defaultPermissions: true}}]}
---
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: {name: beta}
spec: {systemPrompt: beta prompt, features: [{task: {maxSubagentDepth: 1, defaultPermissions: true}}]}
---
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: {name: gamma}
spec: {systemPrompt: gamma prompt}
"#.into()], "alpha").unwrap();
        let provider = Arc::new(ScriptedProvider::new("switch", |request| {
            let name = if request.prompt.contains("spawn beta") {
                "beta"
            } else {
                "alpha"
            };
            if request.prompt.contains("spawn ")
                && !request
                    .tool_results()
                    .any(|result| result.call_id == format!("{name}-task"))
            {
                vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                    format!("{name}-task"),
                    "task",
                    json!({"mode":"sync", "prompt":format!("{name} child")}),
                )))]
            } else {
                vec![
                    Ok(CompletionEvent::Delta("done".into())),
                    Ok(CompletionEvent::Finished),
                ]
            }
        }));
        let factory = ServerAgentRuntimeFactory::new(
            sandbox.handle(),
            root.path().to_path_buf(),
            FileCredentialStore::new(root.path().join("credentials.json")),
            catalog.resolved("alpha").unwrap().clone(),
        )
        .with_agent_catalog(catalog)
        .with_capability_policy(ServerPermissionPolicy::sessions(InteractionBroker::new()))
        .with_runtime_provider(move |_| {
            Ok(AgentRuntimeProvider::new(
                ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiResponses),
                provider.clone(),
            ))
        });
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let sessions = sessions
            .clone()
            .with_runtime_factory(factory.session_factory(sessions));
        Self {
            sessions,
            factory,
            _sandbox: sandbox,
            _root: root,
        }
    }
    async fn create(&self) -> SessionId {
        let mut request = SessionCreateRequest::new(SessionId::generate());
        request.inference_settings = Some(settings());
        self.sessions.create(request).await.unwrap().id().clone()
    }
    async fn bundle(&self, id: &SessionId) -> Option<AgentDefinitionBundle> {
        self.sessions
            .load(id)
            .await
            .unwrap()
            .record
            .agent_definition
            .map(|value| serde_json::from_value(value).unwrap())
    }
}

#[tokio::test]
async fn switch_is_immediate_and_next_cycle_coalesces_one_audit() {
    let fixture = Fixture::new().await;
    let id = fixture.create().await;
    fixture
        .factory
        .switch_agent(&fixture.sessions, &id, "beta")
        .await
        .unwrap();
    assert_eq!(fixture.bundle(&id).await.unwrap().agent_name, "beta");
    fixture
        .factory
        .switch_agent(&fixture.sessions, &id, "gamma")
        .await
        .unwrap();
    assert!(
        fixture
            .sessions
            .timeline(&id, TimelineQuery::forward(None, 20))
            .await
            .unwrap()
            .is_empty()
    );
    drop(fixture.sessions.resolve_runtime(&id).await.unwrap());
    let bundle = fixture.bundle(&id).await.unwrap();
    assert_eq!(bundle.agent_name, "gamma");
    assert!(bundle.pending_switch_audit.is_none());
    let events = fixture
        .sessions
        .timeline(&id, TimelineQuery::forward(None, 20))
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    let TimelineContent::Json { value } = &events[0].content else {
        panic!("audit JSON")
    };
    assert_eq!(value["kind"], "agent_switch");
    assert_eq!(value["oldAgentName"], "alpha");
    assert_eq!(value["newAgentName"], "gamma");
    assert!(value.get("source").is_none());
    assert!(value.get("prompt").is_none());
    drop(fixture.sessions.resolve_runtime(&id).await.unwrap());
    assert_eq!(
        fixture
            .sessions
            .timeline(&id, TimelineQuery::forward(None, 20))
            .await
            .unwrap()
            .len(),
        1
    );
    fixture.sessions.shutdown().await;
}

#[tokio::test]
async fn capture_is_immutable_and_missing_selection_does_not_mutate() {
    let fixture = Fixture::new().await;
    let id = fixture.create().await;
    let before = fixture.sessions.load(&id).await.unwrap().record;
    assert!(
        fixture
            .factory
            .switch_agent(&fixture.sessions, &id, "missing")
            .await
            .is_err()
    );
    assert_eq!(
        fixture
            .sessions
            .load(&id)
            .await
            .unwrap()
            .record
            .agent_definition,
        before.agent_definition
    );
    fixture
        .factory
        .switch_agent(&fixture.sessions, &id, "beta")
        .await
        .unwrap();
    assert!(before.agent_definition.is_none());
    assert_eq!(fixture.bundle(&id).await.unwrap().agent_name, "beta");
    fixture.sessions.shutdown().await;
}

#[tokio::test]
async fn task_child_inherits_the_captured_runtime_bundle_not_later_selection() {
    let fixture = Fixture::new().await;
    let id = fixture.create().await;
    for name in ["alpha", "beta"] {
        if name == "beta" {
            fixture
                .factory
                .switch_agent(&fixture.sessions, &id, "beta")
                .await
                .unwrap();
        }
        let runtime = fixture.sessions.resolve_runtime(&id).await.unwrap();
        fixture
            .sessions
            .prompt(&runtime, id.clone(), format!("spawn {name}"))
            .stream()
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<waltr_agent::Result<Vec<_>>>()
            .unwrap();
        let child = fixture
            .sessions
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|summary| {
                summary.metadata_values.get("task.tool_call_id")
                    == Some(&json!(format!("{name}-task")))
            })
            .expect("actual child")
            .id;
        assert_eq!(fixture.bundle(&child).await.unwrap().agent_name, name);
    }
    fixture.sessions.shutdown().await;
}
