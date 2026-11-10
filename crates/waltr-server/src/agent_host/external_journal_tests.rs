use super::*;
use crate::{
    agent_host::{
        Capability,
        dispatcher::{CapabilityDispatcher, CapabilityPolicy, PolicyFuture},
        external_journal::{JournalError, JournalFuture},
    },
    permission::{PermissionClaim, PermissionPolicy},
    persistence::{DatabaseConfig, Persistence},
};
use std::sync::atomic::{AtomicUsize, Ordering};
use waltr_agent::{ToolDefinition, ToolExecutionContext};

struct Allow;
impl CapabilityPolicy for Allow {
    fn authorize<'a>(
        &'a self,
        _: &'a ToolExecutionContext,
        _: &'a str,
        _: &'a PermissionPolicy,
        _: &'a [PermissionClaim],
    ) -> PolicyFuture<'a> {
        Box::pin(async { Ok(None) })
    }
}

struct FaultStore {
    store: Arc<dyn ExternalDispatchStore>,
    association_failure: bool,
    settlement_failure: bool,
}
impl ExternalDispatchStore for FaultStore {
    fn associate(&self, dispatch: ExternalDispatch) -> JournalFuture<()> {
        if self.association_failure {
            Box::pin(async { Err(JournalError::Unavailable) })
        } else {
            self.store.associate(dispatch)
        }
    }
    fn complete(
        &self,
        dispatch: ExternalDispatch,
        outcome: serde_json::Value,
    ) -> JournalFuture<()> {
        if self.settlement_failure {
            Box::pin(async { Err(JournalError::Unavailable) })
        } else {
            self.store.complete(dispatch, outcome)
        }
    }
}

async fn database(root: &std::path::Path) -> Persistence {
    Persistence::initialize(
        DatabaseConfig::from_sqlite_paths(root.join("waltr.sqlite"), root.join("mcp.sqlite"))
            .unwrap(),
    )
    .await
    .unwrap()
}

fn association(
    dispatcher: &CapabilityDispatcher,
    id: &str,
    name: &str,
    arguments: serde_json::Value,
) -> ExternalDispatch {
    let capability = dispatcher
        .capabilities()
        .find(|cap| cap.definition().name == name)
        .unwrap();
    ExternalDispatch {
        operation_id: id.into(),
        principal: serde_json::json!({"issuer":"test","subject":"actual-owner","scopes":["full"]}),
        identity: capability.identity(),
        source_revision: dispatcher.revision().into(),
        declaration: serde_json::to_value(capability.definition()).unwrap(),
        call: waltr_agent::ToolCall::new(id, name, arguments),
    }
}

async fn row(persistence: &Persistence, id: &str) -> (String, String, Option<String>) {
    sqlx::query_as(
        "SELECT association,state,outcome FROM external_capability_dispatch WHERE operation_id=$1",
    )
    .bind(id)
    .fetch_one(persistence.sqlite_pool().unwrap())
    .await
    .unwrap()
}

async fn close(persistence: Persistence) {
    persistence.sqlite_pool().unwrap().close().await;
    persistence.mcp_sqlite_pool().unwrap().close().await;
}

#[tokio::test]
async fn external_journal_fence_precedes_upstream_handler_and_known_error_receipt() {
    let root = tempfile::tempdir().unwrap();
    let persistence = database(root.path()).await;
    let effects = Arc::new(AtomicUsize::new(0));
    let pool = persistence.sqlite_pool().unwrap().clone();
    let counter = effects.clone();
    let capability = Capability::new_async(
        ToolDefinition::new("upstream_mutation", "test", serde_json::json!({})),
        move |context| {
            let pool = pool.clone();
            let counter = counter.clone();
            Box::pin(async move {
                let state: (String, Option<String>) = sqlx::query_as(
                    "SELECT state,outcome FROM external_capability_dispatch WHERE operation_id=$1",
                )
                .bind(&context.call().id)
                .fetch_one(&pool)
                .await
                .unwrap();
                assert_eq!(state, ("uncertain".into(), None));
                assert!(context.session_id().is_none());
                counter.fetch_add(1, Ordering::SeqCst);
                let error = context.call().arguments["known_error"] == true;
                let result = context.result(if error {
                    "known upstream error"
                } else {
                    "known success"
                });
                Ok(if error { result.with_error() } else { result })
            })
        },
    );
    let dispatcher = CapabilityDispatcher::new(vec![capability], None, Arc::new(Allow)).unwrap();
    let dispatch = association(
        &dispatcher,
        "known",
        "upstream_mutation",
        serde_json::json!({"exact":"input","known_error":true}),
    );
    let failing = FaultStore {
        store: persistence.external_dispatch_store(),
        association_failure: true,
        settlement_failure: false,
    };
    assert!(
        dispatch_durable(
            &failing,
            &dispatcher,
            dispatch.clone(),
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    let store = persistence.external_dispatch_store();
    assert!(
        dispatch_durable(
            store.as_ref(),
            &dispatcher,
            dispatch.clone(),
            CancellationToken::new()
        )
        .await
        .is_ok()
    );
    let (stored, state, outcome) = row(&persistence, "known").await;
    assert_eq!(
        serde_json::from_str::<ExternalDispatch>(&stored).unwrap(),
        dispatch
    );
    assert_eq!(state, "complete");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&outcome.unwrap()).unwrap()["isError"],
        true
    );
    assert!(
        dispatch_durable(
            store.as_ref(),
            &dispatcher,
            dispatch,
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    let success = association(
        &dispatcher,
        "known-success",
        "upstream_mutation",
        serde_json::json!({"known_error":false}),
    );
    assert!(
        dispatch_durable(
            store.as_ref(),
            &dispatcher,
            success,
            CancellationToken::new()
        )
        .await
        .is_ok()
    );
    let (_, state, outcome) = row(&persistence, "known-success").await;
    assert_eq!(state, "complete");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&outcome.unwrap()).unwrap()["isError"],
        false
    );
    assert_eq!(effects.load(Ordering::SeqCst), 2);
    close(persistence).await;
}

#[tokio::test]
async fn external_journal_failure_prevents_worker_prepare_and_write() {
    let root = tempfile::tempdir().unwrap();
    let persistence = database(root.path()).await;
    let sandbox = crate::sandbox::LocalSandbox::start(root.path().into())
        .await
        .unwrap();
    let catalog = crate::agent_host::AgentDefinitionCatalog::from_sources(vec!["apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {name: default}\nspec:\n  features:\n    - filesystem: {root: '$(session.workspace.root)', profile: auto}\n".into()], "default").unwrap();
    let factory = ServerAgentRuntimeFactory::new(
        sandbox.handle(),
        root.path().into(),
        crate::auth::FileCredentialStore::new(root.path().join("auth.json")),
        catalog.resolved("default").unwrap().clone(),
    );
    let (dispatcher, runtime) = factory
        .external_dispatcher(
            InteractionBroker::new(),
            InteractionDestination::Unavailable,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(runtime.is_none());
    let dispatch = association(
        &dispatcher,
        "worker",
        "write",
        serde_json::json!({"filePath":"must-not-exist","content":"effect"}),
    );
    let failing = FaultStore {
        store: persistence.external_dispatch_store(),
        association_failure: true,
        settlement_failure: false,
    };
    assert!(
        dispatch_durable(&failing, &dispatcher, dispatch, CancellationToken::new())
            .await
            .is_err()
    );
    assert!(!root.path().join("must-not-exist").exists());
    sandbox.shutdown().await;
    close(persistence).await;
}

#[tokio::test]
async fn external_journal_restart_preserves_interrupted_effect_and_rejects_repeat_and_stale_settlement()
 {
    let root = tempfile::tempdir().unwrap();
    let persistence = database(root.path()).await;
    let effects = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let (counter, signal) = (effects.clone(), entered.clone());
    let capability = Capability::new_async(
        ToolDefinition::new("effect", "test", serde_json::json!({})),
        move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            signal.notify_one();
            Box::pin(std::future::pending())
        },
    );
    let dispatcher =
        Arc::new(CapabilityDispatcher::new(vec![capability], None, Arc::new(Allow)).unwrap());
    let dispatch = association(
        &dispatcher,
        "interrupted",
        "effect",
        serde_json::json!({"exact":"original"}),
    );
    let store = persistence.external_dispatch_store();
    let owner = tokio::spawn({
        let store = store.clone();
        let dispatcher = dispatcher.clone();
        let dispatch = dispatch.clone();
        async move {
            dispatch_durable(
                store.as_ref(),
                &dispatcher,
                dispatch,
                CancellationToken::new(),
            )
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    owner.abort();
    drop(owner.await);
    close(persistence).await;
    let reopened = database(root.path()).await;
    let (stored, state, outcome) = row(&reopened, "interrupted").await;
    assert_eq!(
        serde_json::from_str::<ExternalDispatch>(&stored).unwrap(),
        dispatch
    );
    assert_eq!(state, "uncertain");
    assert!(outcome.is_none());
    let store = reopened.external_dispatch_store();
    assert!(
        dispatch_durable(
            store.as_ref(),
            &dispatcher,
            dispatch.clone(),
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    let mut stale = dispatch;
    stale.source_revision.push_str("stale");
    assert_eq!(
        store
            .complete(stale, serde_json::json!({"fake":"receipt"}))
            .await,
        Err(JournalError::Conflict)
    );
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    close(reopened).await;
}

#[tokio::test]
async fn external_journal_settlement_failure_stays_uncertain_and_never_reexecutes() {
    let root = tempfile::tempdir().unwrap();
    let persistence = database(root.path()).await;
    let effects = Arc::new(AtomicUsize::new(0));
    let counter = effects.clone();
    let capability = Capability::new(
        ToolDefinition::new("effect", "test", serde_json::json!({})),
        move |context| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(context.result("done"))
        },
    );
    let dispatcher = CapabilityDispatcher::new(vec![capability], None, Arc::new(Allow)).unwrap();
    let dispatch = association(
        &dispatcher,
        "settlement-failed",
        "effect",
        serde_json::json!({}),
    );
    let failing = FaultStore {
        store: persistence.external_dispatch_store(),
        association_failure: false,
        settlement_failure: true,
    };
    assert!(
        dispatch_durable(
            &failing,
            &dispatcher,
            dispatch.clone(),
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    close(persistence).await;
    let reopened = database(root.path()).await;
    let (_, state, outcome) = row(&reopened, "settlement-failed").await;
    assert_eq!(state, "uncertain");
    assert!(outcome.is_none());
    assert!(
        dispatch_durable(
            reopened.external_dispatch_store().as_ref(),
            &dispatcher,
            dispatch,
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    close(reopened).await;
}

#[tokio::test]
async fn external_journal_unknown_handler_result_has_no_completion_receipt() {
    let root = tempfile::tempdir().unwrap();
    let persistence = database(root.path()).await;
    let effects = Arc::new(AtomicUsize::new(0));
    let counter = effects.clone();
    let capability = Capability::new(
        ToolDefinition::new("effect", "test", serde_json::json!({})),
        move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Err(waltr_agent::LlmError::Protocol {
                provider: "upstream",
                message: "effect outcome unknown".into(),
            })
        },
    );
    let dispatcher = CapabilityDispatcher::new(vec![capability], None, Arc::new(Allow)).unwrap();
    let dispatch = association(&dispatcher, "unknown", "effect", serde_json::json!({}));
    assert!(
        dispatch_durable(
            persistence.external_dispatch_store().as_ref(),
            &dispatcher,
            dispatch.clone(),
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    close(persistence).await;
    let reopened = database(root.path()).await;
    let (_, state, outcome) = row(&reopened, "unknown").await;
    assert_eq!(state, "uncertain");
    assert!(outcome.is_none());
    assert!(
        dispatch_durable(
            reopened.external_dispatch_store().as_ref(),
            &dispatcher,
            dispatch,
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    close(reopened).await;
}

#[tokio::test]
async fn external_journal_reopened_host_never_reconstructs_or_rebinds_continuations() {
    let root = tempfile::tempdir().unwrap();
    let persistence = database(root.path()).await;
    let catalog = crate::agent_host::AgentDefinitionCatalog::from_sources(
        vec![
            "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {name: default}\nspec: {}\n"
                .into(),
        ],
        "default",
    )
    .unwrap();
    let factory = ServerAgentRuntimeFactory::new(
        crate::agent_host::sandbox_gateway::SandboxGateway::new("external-test".into()).handle(),
        root.path().into(),
        crate::auth::FileCredentialStore::new(root.path().join("auth.json")),
        catalog.resolved("default").unwrap().clone(),
    );
    let host = ExternalToolHost::new(factory.clone(), InteractionBroker::new());
    host.install_journal(persistence.external_dispatch_store())
        .unwrap();
    let principal = AuthenticatedPrincipal::apps_test_principal("owner");
    let call = mcp::McpToolCall {
        name: "capability__effect".into(),
        arguments: serde_json::json!({"input":"exact"}),
        progress_token: None,
    };
    let durable = ExternalDispatch {
        operation_id: "operation".into(),
        principal: serde_json::json!({"issuer":principal.issuer(),"subject":principal.subject()}),
        identity: "builtin:effect".into(),
        source_revision: "exact-source".into(),
        declaration: serde_json::to_value(ToolDefinition::new(
            "effect",
            "test",
            serde_json::json!({}),
        ))
        .unwrap(),
        call: waltr_agent::ToolCall::new("operation", "effect", call.arguments.clone()),
    };
    persistence
        .external_dispatch_store()
        .associate(durable.clone())
        .await
        .unwrap();
    let (_, receive) = mpsc::channel(1);
    host.operations.lock().await.insert(
        "operation".into(),
        Arc::new(Operation {
            principal: principal.clone(),
            call: call.clone(),
            cancellation: CancellationToken::new(),
            state: Mutex::new(OperationState {
                events: receive,
                pending: None,
            }),
        }),
    );
    assert!(
        host.continuation("operation", &call, &principal)
            .await
            .is_ok()
    );
    assert!(
        host.continuation(
            "operation",
            &call,
            &AuthenticatedPrincipal::apps_test_principal("different")
        )
        .await
        .is_err()
    );
    let mut changed = call.clone();
    changed.arguments = serde_json::json!({"input":"changed"});
    assert!(
        host.continuation("operation", &changed, &principal)
            .await
            .is_err()
    );
    host.shutdown().await;
    close(persistence).await;
    let persistence = database(root.path()).await;
    let (stored, state, outcome) = row(&persistence, "operation").await;
    assert_eq!(
        serde_json::from_str::<ExternalDispatch>(&stored).unwrap(),
        durable
    );
    assert_eq!(state, "uncertain");
    assert!(outcome.is_none());
    let reopened = ExternalToolHost::new(factory, InteractionBroker::new());
    reopened
        .install_journal(persistence.external_dispatch_store())
        .unwrap();
    assert!(
        reopened
            .continuation("operation", &call, &principal)
            .await
            .is_err()
    );
    assert!(reopened.tasks.lock().await.is_empty());
    reopened.shutdown().await;
    close(persistence).await;
}
