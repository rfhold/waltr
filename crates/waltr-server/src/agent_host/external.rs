//! Authenticated sessionless calls and retained, non-replayed MCP continuations.

use super::external_journal::{ExternalDispatch, ExternalDispatchStore};
use super::runtime_factory::ServerAgentRuntimeFactory;
use crate::{
    access_auth::AuthenticatedPrincipal,
    interaction::{
        ExternalInteractionRoute, Interaction, InteractionBroker, InteractionDestination,
        InteractionError, InteractionFuture, InteractionResponse,
    },
};
use mcp::server::{ServerContext, ServerError, ServerResult};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

pub struct ExternalToolHost {
    factory: ServerAgentRuntimeFactory,
    broker: InteractionBroker,
    operations: Mutex<BTreeMap<String, Arc<Operation>>>,
    tasks: Mutex<tokio::task::JoinSet<()>>,
    shutdown: CancellationToken,
    lifecycle: tokio::sync::RwLock<()>,
    journal: std::sync::OnceLock<Arc<dyn ExternalDispatchStore>>,
}

impl std::fmt::Debug for ExternalToolHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalToolHost").finish_non_exhaustive()
    }
}

struct Operation {
    principal: AuthenticatedPrincipal,
    call: mcp::McpToolCall,
    cancellation: CancellationToken,
    state: Mutex<OperationState>,
}
struct OperationState {
    events: mpsc::Receiver<Event>,
    pending: Option<Pending>,
}
struct Pending {
    key: String,
    request: mcp::McpElicitationRequest,
    answer: oneshot::Sender<mcp::McpElicitationResponse>,
}
enum Event {
    Input(Pending),
    Complete(ServerResult<mcp::McpToolResult>),
}
struct Route {
    modes: mcp::McpElicitationCapabilities,
    events: mpsc::Sender<Event>,
}

impl ExternalInteractionRoute for Route {
    fn elicitation_capabilities(&self) -> mcp::McpElicitationCapabilities {
        self.modes
    }
    fn interact<'a>(
        &'a self,
        _: &'a str,
        interaction: Interaction,
        cancellation: CancellationToken,
    ) -> InteractionFuture<'a> {
        Box::pin(async move {
            let (request, url) = match interaction {
                Interaction::Form { request } => (request, false),
                Interaction::UrlConsent { request } => (request, true),
                _ => return Err(InteractionError::Unavailable),
            };
            if !self.modes.supports(&request) {
                return Err(InteractionError::Unavailable);
            }
            let (answer, receive) = oneshot::channel();
            self.events
                .send(Event::Input(Pending {
                    key: uuid::Uuid::new_v4().to_string(),
                    request,
                    answer,
                }))
                .await
                .map_err(|_| InteractionError::Cancelled)?;
            let response = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(InteractionError::Cancelled),
                response = receive => response.map_err(|_| InteractionError::Cancelled)?,
            };
            Ok(if url {
                InteractionResponse::UrlConsent(response)
            } else {
                InteractionResponse::Form(response)
            })
        })
    }
}

impl ExternalToolHost {
    pub(crate) fn new(factory: ServerAgentRuntimeFactory, broker: InteractionBroker) -> Arc<Self> {
        Arc::new(Self {
            factory,
            broker,
            operations: Mutex::new(BTreeMap::new()),
            tasks: Mutex::new(tokio::task::JoinSet::new()),
            shutdown: CancellationToken::new(),
            lifecycle: tokio::sync::RwLock::new(()),
            journal: std::sync::OnceLock::new(),
        })
    }

    /// Install the host's durable store before admission. Absence always fails closed.
    pub(crate) fn install_journal(
        &self,
        journal: Arc<dyn ExternalDispatchStore>,
    ) -> ServerResult<()> {
        self.journal
            .set(journal)
            .map_err(|_| ServerError::internal("external capability journal is already installed"))
    }

    pub(crate) async fn list(&self) -> ServerResult<Vec<mcp::McpToolDefinition>> {
        let _admission = self.lifecycle.read().await;
        if self.shutdown.is_cancelled() {
            return Err(ServerError::internal("capability host is shutting down"));
        }
        let dispatcher = self
            .factory
            .external_dispatcher(self.broker.clone(), InteractionDestination::Unavailable)
            .await
            .map_err(host_error)?;
        let tools = dispatcher
            .capabilities()
            .map(|capability| {
                let definition = capability.definition();
                mcp::progressive::tool_definition(
                    &format!("capability__{}", definition.name),
                    &definition.description,
                    definition.input_schema.clone(),
                )
            })
            .collect();
        Ok(tools)
    }

    pub(crate) async fn call(
        &self,
        call: mcp::McpToolCall,
        context: ServerContext,
        principal: AuthenticatedPrincipal,
    ) -> ServerResult<mcp::McpToolResult> {
        let admission = self.lifecycle.read().await;
        if self.shutdown.is_cancelled() {
            return Err(ServerError::internal("capability host is shutting down"));
        }
        let (id, operation) = if let Some(id) = context.request_state() {
            let operation = self.continuation(id, &call, &principal).await?;
            (id.to_owned(), operation)
        } else {
            if context.input_responses().is_some() {
                return Err(ServerError::invalid_params(
                    "input responses require a live continuation",
                ));
            }
            let modes = context.elicitation_capabilities()?;
            let journal = self.journal.get().cloned().ok_or_else(|| {
                ServerError::internal("durable external capability journal is unavailable")
            })?;
            let mut operations = self.operations.lock().await;
            operations.retain(|_, operation| !operation.cancellation.is_cancelled());
            if operations.len() >= 128 {
                return Err(ServerError::internal("too many live capability operations"));
            }
            let id = uuid::Uuid::new_v4().to_string();
            let cancellation = self.shutdown.child_token();
            let (events, receive) = mpsc::channel(8);
            let operation = Arc::new(Operation {
                principal: principal.clone(),
                call: call.clone(),
                cancellation: cancellation.clone(),
                state: Mutex::new(OperationState {
                    events: receive,
                    pending: None,
                }),
            });
            operations.insert(id.clone(), operation.clone());
            drop(operations);
            let factory = self.factory.clone();
            let broker = self.broker.clone();
            let operation_id = id.clone();
            let owned_call = call.clone();
            let mut tasks = self.tasks.lock().await;
            while tasks.try_join_next().is_some() {}
            tasks.spawn(async move {
                let result = async {
                    let route = Arc::new(Route { modes, events: events.clone() });
                    let dispatcher = factory.external_dispatcher(broker,
                        InteractionDestination::External(route)).await.map_err(host_error)?;
                    let result = async {
                        let name = owned_call.name.strip_prefix("capability__")
                            .ok_or_else(|| ServerError::invalid_params("invalid capability name"))?;
                        let capability = dispatcher.capabilities().find(|capability| capability.definition().name == name)
                            .ok_or_else(|| ServerError::invalid_params("capability is not server-enabled"))?;
                        let dispatch = ExternalDispatch {
                            operation_id: operation_id.clone(),
                            principal: serde_json::json!({"issuer":principal.issuer(),"subject":principal.subject(),"scopes":principal.scopes().collect::<Vec<_>>(),"method":format!("{:?}",principal.method())}),
                            identity: capability.identity(),
                            source_revision: dispatcher.revision().into(),
                            declaration: serde_json::to_value(capability.definition()).map_err(host_error)?,
                            call: waltr_agent::ToolCall::new(operation_id, name, owned_call.arguments),
                        };
                        dispatch_durable(journal.as_ref(), &dispatcher, dispatch, cancellation.clone()).await
                    }.await;
                    result
                }.await;
                drop(events.send(Event::Complete(result)).await);
            });
            (id, operation)
        };
        drop(admission);
        let mut state = operation.state.try_lock().map_err(|_| {
            ServerError::invalid_params("capability continuation already has a waiter")
        })?;
        if let Some(pending) = &state.pending {
            let response = context.elicitation_response(&pending.key, &pending.request)?;
            if let Some(response) = response {
                let pending = state.pending.take().expect("checked pending");
                pending
                    .answer
                    .send(response)
                    .map_err(|_| ServerError::invalid_params("capability continuation ended"))?;
            } else {
                return Ok(mcp::McpToolResult::new(context.elicitation_required(
                    &pending.key,
                    &pending.request,
                    Some(&id),
                )?));
            }
        } else if context.input_responses().is_some() {
            return Err(ServerError::invalid_params("stale capability response"));
        }
        let event = tokio::select! {
            biased;
            _ = context.cancelled() => { operation.cancellation.cancel(); None },
            event = state.events.recv() => event,
        };
        match event {
            Some(Event::Input(pending)) => {
                let output =
                    context.elicitation_required(&pending.key, &pending.request, Some(&id))?;
                state.pending = Some(pending);
                Ok(mcp::McpToolResult::new(output))
            }
            event => {
                self.operations.lock().await.remove(&id);
                match event {
                    Some(Event::Complete(result)) => result,
                    _ => Err(ServerError::internal(
                        "capability operation cancelled; do not replay uncertain effects",
                    )),
                }
            }
        }
    }

    async fn continuation(
        &self,
        id: &str,
        call: &mcp::McpToolCall,
        principal: &AuthenticatedPrincipal,
    ) -> ServerResult<Arc<Operation>> {
        self.operations
            .lock()
            .await
            .get(id)
            .filter(|operation| {
                operation.principal == *principal
                    && operation.call.name == call.name
                    && operation.call.arguments == call.arguments
            })
            .cloned()
            .ok_or_else(|| {
                ServerError::invalid_params(
                    "unknown, stale or unauthorized capability continuation",
                )
            })
    }

    pub(crate) async fn shutdown(&self) {
        self.shutdown.cancel();
        let _admission = self.lifecycle.write().await;
        let mut tasks = self.tasks.lock().await;
        while tasks.join_next().await.is_some() {}
        self.operations.lock().await.clear();
    }
}

async fn dispatch_durable(
    journal: &dyn ExternalDispatchStore,
    dispatcher: &super::dispatcher::CapabilityDispatcher,
    association: ExternalDispatch,
    cancellation: CancellationToken,
) -> ServerResult<mcp::McpToolResult> {
    let exact = dispatcher.capabilities().any(|capability| {
        capability.identity() == association.identity
            && capability.definition().name == association.call.name
            && serde_json::to_value(capability.definition()).ok().as_ref()
                == Some(&association.declaration)
    });
    if !exact
        || association.source_revision != dispatcher.revision()
        || association.operation_id != association.call.id
    {
        return Err(ServerError::invalid_params(
            "external dispatch declaration mismatch",
        ));
    }
    journal.associate(association.clone()).await.map_err(|_| {
        ServerError::internal("external dispatch association failed; no capability was dispatched")
    })?;
    let context = waltr_agent::ToolExecutionContext::with_progress_and_cancellation(
        association.call.clone(),
        waltr_agent::ToolProgressSink::noop(),
        cancellation.clone(),
    )
    .with_host_operation(waltr_agent::HostDispatchBinding {
        invocation_id: association.operation_id.clone(),
        source_revision: association.source_revision.clone(),
    });
    let result = {
        let dispatch = dispatcher.dispatch(&association.identity, &association.source_revision, context);
        tokio::pin!(dispatch);
        tokio::select! {
            result = &mut dispatch => result,
            _ = tokio::time::sleep(Duration::from_secs(300)) => { cancellation.cancel(); dispatch.await }
        }
    }.map_err(|_| ServerError::internal("external capability effect is uncertain; do not replay"))?;
    let content = result.content.blocks().iter().map(|block| match block {
        waltr_agent::ToolResultContentBlock::Text { text } => serde_json::json!({"type":"text","text":text}),
        waltr_agent::ToolResultContentBlock::Image(image) => serde_json::json!({"type":"image","data":image.data(),"mimeType":image.media_type()}),
    }).collect::<Vec<_>>();
    let outcome = serde_json::json!({"content":content,"isError":result.is_error});
    journal
        .complete(association, outcome.clone())
        .await
        .map_err(|_| {
            ServerError::internal("external capability settlement is uncertain; do not replay")
        })?;
    Ok(mcp::McpToolResult::new(outcome))
}

fn host_error(error: impl std::fmt::Display) -> ServerError {
    ServerError::internal(error.to_string())
}

#[cfg(test)]
#[path = "external_journal_tests.rs"]
mod journal_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn external_default_is_sessionless_full_access_over_enabled_worker_capabilities() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("source.txt"), "external-data").unwrap();
        let sandbox = crate::sandbox::LocalSandbox::start(root.path().to_path_buf())
            .await
            .unwrap();
        let catalog = crate::agent_host::AgentDefinitionCatalog::from_sources(vec![
            "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {name: default}\nspec:\n  features:\n    - filesystem: {root: '$(session.workspace.root)', profile: auto, defaultPermissions: false}\n  permissions:\n    - {name: read, permission: deny}\n".into()], "default").unwrap();
        let factory = ServerAgentRuntimeFactory::new(
            sandbox.handle(),
            root.path().to_path_buf(),
            crate::auth::FileCredentialStore::new(root.path().join("credentials.json")),
            catalog.resolved("default").unwrap().clone(),
        )
        .with_runtime_provider(|_| {
            panic!("external capability calls must never construct an inference provider")
        });
        let dispatcher = factory
            .external_dispatcher(
                InteractionBroker::new(),
                InteractionDestination::Unavailable,
            )
            .await
            .unwrap();
        let identity = dispatcher
            .capabilities()
            .find(|capability| capability.definition().name == "read")
            .unwrap()
            .identity();
        let context = waltr_agent::ToolExecutionContext::new(waltr_agent::ToolCall::new(
            "external",
            "read",
            serde_json::json!({"filePath":"source.txt"}),
        ))
        .with_host_operation(waltr_agent::HostDispatchBinding {
            invocation_id: "host-operation".into(),
            source_revision: dispatcher.revision().into(),
        });
        assert!(context.session_id().is_none());
        let result = dispatcher
            .dispatch(&identity, dispatcher.revision(), context)
            .await
            .unwrap();
        assert!(!result.is_error, "{}", result.text_projection());
        assert!(result.text_projection().contains("external-data"));
        sandbox.shutdown().await;
    }

    #[tokio::test]
    async fn external_route_keeps_the_original_future_and_rejects_unadvertised_modes() {
        let (events, mut receiver) = mpsc::channel(2);
        let request = mcp::McpElicitationRequest::Url {
            message: "Consent only".into(),
            url: "https://example.com/consent".into(),
        };
        let route = Arc::new(Route {
            modes: mcp::McpElicitationCapabilities::default(),
            events: events.clone(),
        });
        assert!(matches!(
            route
                .interact(
                    "op",
                    Interaction::UrlConsent {
                        request: request.clone()
                    },
                    CancellationToken::new()
                )
                .await,
            Err(InteractionError::Unavailable)
        ));
        let modes = mcp::McpElicitationCapabilities {
            form: false,
            url: true,
        };
        let route = Arc::new(Route { modes, events });
        let entered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let owner = tokio::spawn({
            let entered = entered.clone();
            async move {
                entered.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                route
                    .interact(
                        "op",
                        Interaction::UrlConsent { request },
                        CancellationToken::new(),
                    )
                    .await
            }
        });
        let Some(Event::Input(pending)) = receiver.recv().await else {
            panic!("expected live input");
        };
        assert!(!owner.is_finished());
        pending
            .answer
            .send(mcp::McpElicitationResponse {
                action: mcp::McpElicitationAction::Accept,
                content: None,
            })
            .unwrap();
        assert!(matches!(
            owner.await.unwrap().unwrap(),
            InteractionResponse::UrlConsent(_)
        ));
        assert_eq!(entered.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
