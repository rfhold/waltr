use super::*;
use crate::{
    access_auth::AuthenticatedPrincipal,
    routine::{
        adapters::{
            authorized_tools::{
                AdapterBindings, InvocationAdapterFactory, RoutineAgentHostFactory,
            },
            context::{CoordinatorContext, CoordinatorSessionContext},
            helper::CoordinatorHelperRunner,
            media::{MediaBindingFactory, MediaBindings, MediaDriver, MediaHostFactory},
        },
        code_mode::CodeModeHost,
        context::ContextPort,
        ports::HelperRunner,
        rpc::{AuthorizedStart, StartPolicy, StartRequest, principal_id},
        text_results::TextResultStore,
    },
};
use crate::{
    agent_host::authorized_tools::{
        AuthorizedToolError, AuthorizedToolExecutor, AuthorizedToolFuture, AuthorizedToolRequest,
        ConcreteToolTarget, SessionAuthorizedToolExecutor,
    },
    permission::{PermissionLevel, PermissionPolicy},
};
use futures_util::FutureExt as _;
use serde_json::Value;
use waltr_agent::{InferenceSettings, SessionCreateRequest, SessionId};
mod context;
mod lifecycle;
mod semantic;
mod tools;

pub(crate) trait CodeComposition: Send + Sync {
    fn public_enabled(&self, principal: &AuthenticatedPrincipal) -> bool;
    fn public_port(
        &self,
        principal: AuthenticatedPrincipal,
    ) -> PortFuture<Arc<dyn crate::routine::code_mode::PublicCodePort>>;
    fn factory(
        &self,
        principal: AuthenticatedPrincipal,
        record: &waltr_agent::SessionRecord,
    ) -> PortFuture<AgentSessionRuntimeFactory>;
    fn stop(&self) -> Result<(), PortError>;
    fn join(&self) -> PortFuture<()>;
}

pub(super) struct Composition<S: SessionStore + Clone + Send + Sync + 'static> {
    state: AgentDefinitionState,
    options: RoutineAuthority,
    sessions: SessionCoordinator<S>,
    factory: Arc<AgentSessionRuntimeFactory>,
    blobs: Arc<FilesystemBlobStore>,
    text: Arc<dyn TextResultStore>,
    code: Arc<CodeModeHost<S>>,
    stopped: std::sync::atomic::AtomicBool,
    pools: Arc<Mutex<BTreeMap<String, Pools>>>,
    process_pools: Pools,
    service: InvocationService,
    me: std::sync::OnceLock<std::sync::Weak<Self>>,
    scopes: Mutex<Vec<lifecycle::Scope<S>>>,
    completion_predecessors: Mutex<BTreeMap<(String, String), tokio::sync::oneshot::Receiver<crate::routine::service::InvocationHandle>>>,
    scope_maintenance: Arc<tokio::sync::Mutex<()>>,
    invocation_coalescer: crate::routine::triggers::InvocationCoalescer,
}
impl<S: SessionStore + Clone + Send + Sync + 'static> Composition<S> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        state: AgentDefinitionState,
        options: RoutineAuthority,
        sessions: SessionCoordinator<S>,
        factory: Arc<AgentSessionRuntimeFactory>,
        service: InvocationService,
        persistence: Persistence,
        blobs: Arc<FilesystemBlobStore>,
        text: Arc<dyn TextResultStore>,
    ) -> Result<Arc<Self>, PortError> {
        let code = Arc::new(CodeModeHost::new(
            service.clone(),
            persistence.script_code_dispatch_store(),
            sessions.clone(),
            factory.clone(),
        ).with_catalog(state.clone()));
        let owner = Arc::new(Self {
            state,
            options,
            sessions,
            factory,
            blobs,
            text,
            code,
            stopped: std::sync::atomic::AtomicBool::new(false),
            pools: Arc::new(Mutex::new(BTreeMap::new())),
            process_pools: Pools::process(),
            service,
            me: std::sync::OnceLock::new(),
            scopes: Mutex::new(Vec::new()),
            completion_predecessors: Mutex::new(BTreeMap::new()),
            scope_maintenance: Arc::new(tokio::sync::Mutex::new(())),
            invocation_coalescer: crate::routine::triggers::InvocationCoalescer::default(),
        });
        owner
            .me
            .set(Arc::downgrade(&owner))
            .map_err(|_| PortError::Unavailable)?;
        Ok(owner)
    }
}
impl<S: SessionStore + Clone + Send + Sync + 'static> CodeComposition for Composition<S> {
    fn public_enabled(&self, principal: &AuthenticatedPrincipal) -> bool {
        !self.stopped.load(std::sync::atomic::Ordering::Acquire)
            && principal.access_ended().now_or_never().is_none()
            && self.options.code.iter().any(|grant| {
                grant.issuer == principal.issuer()
                    && grant.subject == principal.subject()
                    && grant.public_mcp_session.is_some()
            })
    }
    fn public_port(
        &self,
        principal: AuthenticatedPrincipal,
    ) -> PortFuture<Arc<dyn crate::routine::code_mode::PublicCodePort>> {
        let owner = self.me.get().and_then(std::sync::Weak::upgrade);
        Box::pin(async move {
            let owner = owner.ok_or(PortError::Unavailable)?;
            if !owner.public_enabled(&principal) {
                return Err(PortError::Denied);
            }
            let id = owner
                .options
                .code
                .iter()
                .find(|grant| {
                    grant.issuer == principal.issuer() && grant.subject == principal.subject()
                })
                .and_then(|grant| grant.public_mcp_session.clone())
                .ok_or(PortError::Denied)?;
            let record = owner
                .sessions
                .load(&id)
                .await
                .map_err(|_| PortError::Denied)?
                .record;
            if !record
                .agent_definition
                .as_ref()
                .and_then(|bundle| {
                    serde_json::from_value::<crate::agent_host::AgentDefinitionBundle>(
                        bundle.clone(),
                    )
                    .ok()
                })
                .and_then(|bundle| bundle.resolve().ok())
                .is_some_and(|agent| agent.features.code_mode == Some(true))
            {
                return Err(PortError::Denied);
            }
            let factory = owner.factory(principal.clone(), &record).await?;
            let (runtime, dispatcher) = factory
                .for_concrete_calls()
                .runtime_with_dispatcher(&record, owner.sessions.clone(), None)
                .await
                .map_err(|_| PortError::Denied)?;
            let adapter = owner.code.bind(&principal, &record, runtime, dispatcher)?;
            Ok(Arc::new(adapter) as Arc<dyn crate::routine::code_mode::PublicCodePort>)
        })
    }
    fn factory(
        &self,
        principal: AuthenticatedPrincipal,
        record: &waltr_agent::SessionRecord,
    ) -> PortFuture<AgentSessionRuntimeFactory> {
        let owner = self.me.get().and_then(std::sync::Weak::upgrade);
        let record = record.clone();
        Box::pin(async move {
            let owner = owner.ok_or(PortError::Unavailable)?;
            let this = owner.as_ref();
            if this.stopped.load(std::sync::atomic::Ordering::Acquire)
                || principal.access_ended().now_or_never().is_some()
            {
                return Err(PortError::Denied);
            }
            let selected = this.options.code.iter().find(|grant| {
                grant.issuer == principal.issuer()
                    && grant.subject == principal.subject()
                    && grant.sessions.contains(&record.id)
            });
            if let Some(grant) = selected {
                this.code
                    .grant_current(principal.clone(), &record, grant.profile.clone())?;
            }
            let factory = this
                .code
                .install((*this.factory).clone(), principal.clone());
            let tools_owner = this.me.get().cloned().ok_or(PortError::Unavailable)?;
            let tools_principal = principal.clone();
            let factory = factory.with_host_tool_provider(move |record, runtime, dispatcher| {
                tools::provide(
                    tools_owner.clone(),
                    tools_principal.clone(),
                    record,
                    &runtime,
                    dispatcher.permission_policy(),
                )
            });
            let (events, completion) = this.run_scope(principal, &record).await?;
            Ok(factory.with_run_coordination(events, completion))
        })
    }
    fn stop(&self) -> Result<(), PortError> {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Release);
        for scope in self.scopes.lock().map_err(|_| PortError::Uncertain)?.iter() {
            scope.semantic.stop();
        }
        self.code.stop()
    }
    fn join(&self) -> PortFuture<()> {
        let owner = self.me.get().and_then(std::sync::Weak::upgrade);
        Box::pin(async move {
            let owner = owner.ok_or(PortError::Uncertain)?;
            let _maintenance = owner.scope_maintenance.lock().await;
            let handlers = match owner.scopes.lock() {
                Ok(scopes) => scopes
                    .iter()
                    .map(|scope| (scope.completion.clone(), scope.semantic.clone()))
                    .collect::<Vec<_>>(),
                Err(_) => return Err(PortError::Uncertain),
            };
            let mut result = Ok(());
            for (handler, semantic) in handlers {
                result = result.and(semantic.shutdown().await);
                result = result.and(handler.shutdown().await);
            }
            result
        })
    }
}
impl<S: SessionStore + Clone + Send + Sync + 'static> StartPolicy for Composition<S> {
    fn source_access(&self, _: String, source: Option<String>) -> PortFuture<()> {
        let sessions = self.sessions.clone();
        Box::pin(async move {
            if let Some(source) = source {
                sessions
                    .load(&SessionId::new(source))
                    .await
                    .map_err(|_| PortError::Denied)?;
            }
            Ok(())
        })
    }
    fn authorize(&self, request: StartRequest) -> PortFuture<AuthorizedStart> {
        self.authorize_context(request, None)
    }
}
impl<S: SessionStore + Clone + Send + Sync + 'static> Composition<S> {
    fn authorize_context(
        &self,
        request: StartRequest,
        pinned_context: Option<Arc<CoordinatorContext<S>>>,
    ) -> PortFuture<AuthorizedStart> {
        self.authorize_context_policy(request, pinned_context, None, None)
    }

    fn authorize_context_policy(
        &self,
        request: StartRequest,
        pinned_context: Option<Arc<CoordinatorContext<S>>>,
        caller_policy: Option<PermissionPolicy>,
        approved_context: Option<tools::ApprovedContextAdmission>,
    ) -> PortFuture<AuthorizedStart> {
        let mut sessions = self.sessions.clone();
        let factory = self.factory.clone();
        let composition = self.me.get().cloned();
        let state = self.state.clone();
        let blobs = self.blobs.clone();
        let text = self.text.clone();
        let stopped = self.stopped.load(std::sync::atomic::Ordering::Acquire);
        let options = self.options.clone();
        let pools = self.pools.clone();
        let process_pools = self.process_pools.clone();
        let service = self.service.clone();
        Box::pin(async move {
            if stopped
                || request.principal != principal_id(&request.authenticated)
                || request
                    .authenticated
                    .access_ended()
                    .now_or_never()
                    .is_some()
            {
                return Err(PortError::Denied);
            }
            let snapshot = state.snapshot();
            let name = &request.definition.envelope().name;
            if !snapshot
                .scripts()
                .get(name)
                .is_some_and(|current| current.revision() == request.definition.revision())
            {
                return Err(PortError::Conflict);
            }
            let mut inference = None;
            let tool_admitted = caller_policy.is_some();
            let mut restriction =
                caller_policy.unwrap_or_else(|| PermissionPolicy::new(PermissionLevel::Deny));
            let mut bundle = None;
            let mut context: Option<Arc<dyn ContextPort>> = None;
            let mut context_owner = None;
            let copy = None;
            if let Some(source) = &request.source_session {
                let id = SessionId::new(source.clone());
                let record = sessions
                    .load(&id)
                    .await
                    .map_err(|_| PortError::Denied)?
                    .record;
                let approved_original = match approved_context {
                    Some(evidence) => {
                        restriction = evidence.consume(&request, &record)?;
                        true
                    }
                    None => false,
                };
                if request.definition.envelope().spec.capabilities.iter().any(|cap| matches!(cap.as_str(), "context.create" | "context.start" | "context.continue")) {
                    let owner = composition
                        .and_then(|owner| owner.upgrade())
                        .ok_or(PortError::Unavailable)?;
                    sessions = sessions.clone().with_runtime_factory(
                        owner
                            .factory(request.authenticated.clone(), &record)
                            .await?
                            .session_factory(sessions.clone()),
                    );
                }
                if factory.session_roles.source_excluded(&record) {
                    return Err(PortError::Denied);
                }
                if !tool_admitted {
                    restriction = factory
                        .runtime_for_record(&record, sessions.clone())
                        .await
                        .map_err(|_| PortError::Denied)?
                        .1
                        .permission_policy();
                }
                inference = record.inference_settings.clone();
                bundle = record.agent_definition.clone();
                if request
                    .definition
                    .envelope()
                    .spec
                    .capabilities
                    .iter()
                    .any(|cap| cap.starts_with("context."))
                {
                    let caps = &request.definition.envelope().spec.capabilities;
                    let original_allowed = tools::original_source_allowed(caps, &restriction, approved_original);
                    if caps.contains("context.original") && !original_allowed {
                        return Err(PortError::Denied);
                    }
                    let pinned = if let Some(pinned) = pinned_context {
                        Ok(Arc::new(pinned.with_original_ceiling(original_allowed)))
                    } else if caps.contains("context.continue") {
                        CoordinatorContext::pin_paused(sessions.clone(), &id, &request.principal, original_allowed).await.map(Arc::new)
                    } else {
                        CoordinatorContext::pin(sessions.clone(), &id, &request.principal, original_allowed).await.map(Arc::new)
                    };
                    match pinned {
                        Ok(pinned) => {
                            if !pinned.snapshot_identity().is_some_and(|identity| identity.matches(&record)) {
                                return Err(PortError::Conflict);
                            }
                            if caps.contains("context.original") { pinned.resolve_source("original")?; }
                            context_owner = Some(pinned.clone());
                            context = Some(pinned);
                        }
                        Err(PortError::Conflict) if caps.iter().filter(|cap| cap.starts_with("context.")).all(|cap|
                            matches!(cap.as_str(), "context.snapshot" | "context.metadata" | "context.transact")) => {
                            let incarnation = record.metadata.incarnation.as_deref().ok_or(PortError::Conflict)?;
                            context = Some(Arc::new(CoordinatorSessionContext::pin_for_incarnation(sessions.clone(), &id, incarnation).await?));
                        }
                        Err(error) => return Err(error),
                    }
                }
            } else {
                if name == "configuration" {
                    return Err(PortError::Denied);
                }
            }
            if name == "configuration" {
                let level = restriction.evaluate_tool("configure-waltr");
                if level == PermissionLevel::Deny
                    || (!tool_admitted && level != PermissionLevel::Allow)
                {
                    return Err(PortError::Denied);
                }
            }
            if request.definition.envelope().spec.capabilities.contains("children.call") {
                let context = context_owner.ok_or(PortError::Denied)?;
                let helper_sessions = sessions.clone();
                let helper_factory = factory.clone();
                let helper_inference = inference.clone();
                let helper_principal = request.principal.clone();
                let helper_restriction = restriction.clone();
                let helper_catalog = snapshot.clone();
                let helpers = Arc::new(move || {
                    Ok(Arc::new(
                        CoordinatorHelperRunner::new(
                            helper_sessions.clone(),
                            helper_factory.clone(),
                            helper_inference.clone(),
                            BTreeMap::new(),
                            BTreeMap::new(),
                            None,
                            helper_principal.clone(),
                        )?
                        .with_catalog(helper_catalog.clone(), helper_inference.clone())
                        .with_permission_restriction(helper_restriction.clone()),
                    ) as Arc<dyn HelperRunner>)
                });
                let definitions = snapshot
                    .scripts()
                    .iter()
                    .filter(|(_, definition)| definition.envelope().spec.capabilities.is_subset(&request.definition.envelope().spec.capabilities))
                    .map(|(name, definition)| (name.clone(), definition.clone()))
                    .collect();
                let bindings = context::ContextBindings {
                    context,
                    helpers,
                    service,
                    definitions,
                    principal: request.authenticated.clone(),
                    capabilities: request.definition.envelope().spec.capabilities.clone(),
                    scope: None,
                };
                return Ok(AuthorizedStart {
                    source_session: request.source_session,
                    factory: super::lease::bind(
                        Arc::new(RoutineAgentHostFactory {
                            bindings: Arc::new(bindings),
                        }),
                        request.authenticated,
                        None,
                    ),
                });
            }
            let media = request.transport.is_some();
            let helpers =
                CoordinatorHelperRunner::new(
                    sessions.clone(),
                    factory.clone(),
                    inference.clone(),
                    BTreeMap::new(),
                    BTreeMap::new(),
                    copy,
                    request.principal.clone(),
                )?
                .with_catalog(snapshot.clone(), inference.clone());
            let helpers = Arc::new(if !media || request.source_session.is_some() || tool_admitted {
                helpers.with_permission_restriction(restriction.clone())
            } else { helpers });
            let result: Arc<dyn HostFactory> = if let Some(transport) = request.transport {
                let principal_pools = {
                    let mut pools = pools.lock().map_err(|_| PortError::Unavailable)?;
                    if pools.len() >= 128 && !pools.contains_key(&request.principal) {
                        return Err(PortError::Busy);
                    }
                    pools
                        .entry(request.principal.clone())
                        .or_insert_with(Pools::principal)
                        .clone()
                };
                let speech = Arc::new(crate::routine::media::speech::CatalogSpeech(snapshot.clone()));
                Arc::new(MediaHostFactory {
                    bindings: transport.bind_factory(Arc::new(MediaComposition {
                        principal: request.principal.clone(),
                        helpers,
                        blobs,
                        text,
                        speech,
                        context,
                        principal_pools,
                        process_pools,
                        snapshot: true,
                    })),
                })
            } else {
                let tools = Arc::new(LazyTools {
                    factory: Arc::new(
                        factory
                            .as_ref()
                            .clone()
                            .for_concrete_calls()
                            .with_permission_restriction(restriction),
                    ),
                    sessions,
                    inference,
                    catalog: snapshot,
                    bundle,
                    executor: Arc::new(tokio::sync::Mutex::new(None)),
                    stopped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                });
                let mut capabilities =
                    BTreeSet::from(["helper.run".into(), "helper.validate".into()]);
                if !options.targets.is_empty() {
                    capabilities.extend(["tools.execute".into(), "tools.schemas".into()]);
                    capabilities.extend(options.targets.keys().cloned());
                }
                for cap in &request.definition.envelope().spec.capabilities {
                    let context_supported = context.is_some() && crate::routine::context::CAPABILITIES.contains(&cap.as_str());
                    if cap.starts_with("helper.named.") || cap.starts_with("helper.capture.") && request.definition.envelope().spec.captures.contains_key(cap.trim_start_matches("helper.capture.")) || context_supported {
                        capabilities.insert(cap.clone());
                    }
                }
                Arc::new(RoutineAgentHostFactory {
                    bindings: Arc::new(Bound {
                        principal: request.principal,
                        helpers,
                        tools,
                        context,
                        capabilities,
                        targets: options.targets,
                    }),
                })
            };
            Ok(AuthorizedStart {
                source_session: request.source_session,
                factory: super::lease::bind(result, request.authenticated, None),
            })
        })
    }
}

pub(super) fn default_inference(
    snapshot: &crate::resource_catalog::AgentDefinitionSnapshot,
) -> Result<InferenceSettings, PortError> {
    let configuration = snapshot.configuration();
    let defaults = configuration
        .inference_defaults
        .as_ref()
        .ok_or(PortError::Invalid)?;
    crate::session::pinned_inference_settings(
        defaults.provider_key.clone(),
        defaults.model_key.clone(),
        configuration,
    )
    .map_err(|_| PortError::Invalid)
}
#[cfg(test)]
pub(super) fn model(inference: &InferenceSettings) -> waltr_agent::ModelRef {
    use waltr_agent::{ModelApi, ProviderId};
    let api = inference.provider.api;
    let provider = match api {
        ModelApi::AnthropicMessages => ProviderId::Anthropic,
        _ => ProviderId::OpenAi,
    };
    waltr_agent::ModelRef::new(provider, inference.provider.model_id.clone(), api)
}
struct Bound {
    principal: String,
    helpers: Arc<dyn HelperRunner>,
    tools: Arc<dyn AuthorizedToolExecutor>,
    context: Option<Arc<dyn ContextPort>>,
    capabilities: BTreeSet<String>,
    targets: BTreeMap<String, ConcreteToolTarget>,
}
impl InvocationAdapterFactory for Bound {
    fn bind(
        &self,
        _: Arc<Definition>,
        _: SelectedEntrypoint,
        _: Caller,
    ) -> PortFuture<AdapterBindings> {
        let bound = AdapterBindings {
            principal: self.principal.clone(),
            helpers: self.helpers.clone(),
            tools: self.tools.clone(),
            capabilities: self.capabilities.clone(),
            targets: self.targets.clone(),
        };
        Box::pin(async move { Ok(bound) })
    }
    fn bind_context(
        &self,
        _: Arc<Definition>,
        _: SelectedEntrypoint,
        _: Caller,
    ) -> PortFuture<Option<Arc<dyn ContextPort>>> {
        let context = self.context.clone();
        Box::pin(async move { Ok(context) })
    }
}
#[derive(Clone)]
struct LazyTools<S: SessionStore + Clone + Send + Sync + 'static> {
    factory: Arc<AgentSessionRuntimeFactory>,
    sessions: SessionCoordinator<S>,
    inference: Option<InferenceSettings>,
    catalog: Arc<crate::resource_catalog::AgentDefinitionSnapshot>,
    bundle: Option<Value>,
    executor: Arc<tokio::sync::Mutex<Option<Arc<SessionAuthorizedToolExecutor<S>>>>>,
    stopped: Arc<std::sync::atomic::AtomicBool>,
}
impl<S: SessionStore + Clone + Send + Sync + 'static> LazyTools<S> {
    async fn executor(&self) -> Result<Arc<SessionAuthorizedToolExecutor<S>>, PortError> {
        let mut guard = self.executor.lock().await;
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return Err(PortError::Denied);
        }
        if let Some(executor) = &*guard {
            return Ok(executor.clone());
        }
        let mut request = SessionCreateRequest::new(SessionId::generate());
        request.inference_settings = Some(match &self.inference { Some(inference) => inference.clone(), None => default_inference(&self.catalog)? });
        request.agent_definition = self.bundle.clone();
        let created = self
            .sessions
            .create(request)
            .await
            .map_err(|_| PortError::Unavailable)?;
        let record = created.record;
        let id = record.id.clone();
        self.factory.session_roles.register(&record, crate::agent_host::session_roles::SessionRole::Ephemeral)
            .map_err(|_| PortError::Unavailable)?;
        let (runtime, dispatcher) = match self
            .factory
            .runtime_for_record(&record, self.sessions.clone())
            .await
        {
            Ok(pair) => pair,
            Err(_) => {
                self.sessions
                    .delete(&id)
                    .await
                    .map_err(|_| PortError::Uncertain)?;
                return Err(PortError::Unavailable);
            }
        };
        let executor = Arc::new(SessionAuthorizedToolExecutor::new(
            self.sessions.clone(),
            id,
            runtime,
            dispatcher,
        ));
        *guard = Some(executor.clone());
        Ok(executor)
    }
}

struct MediaComposition {
    principal: String,
    helpers: Arc<dyn HelperRunner>,
    blobs: Arc<FilesystemBlobStore>,
    text: Arc<dyn TextResultStore>,
    speech: Arc<dyn crate::routine::media::speech::SpeechBackend>,
    context: Option<Arc<dyn ContextPort>>,
    principal_pools: Pools,
    process_pools: Pools,
    snapshot: bool,
}
#[derive(Clone)]
struct Pools {
    workspace: crate::routine::media::snapshot::WorkspacePool,
    result: crate::routine::adapters::media::ResultPool,
}
impl Pools {
    fn principal() -> Self {
        Self {
            workspace: crate::routine::media::snapshot::WorkspacePool::principal(),
            result: crate::routine::adapters::media::ResultPool::principal(),
        }
    }
    fn process() -> Self {
        Self {
            workspace: crate::routine::media::snapshot::WorkspacePool::process(),
            result: crate::routine::adapters::media::ResultPool::process(),
        }
    }
}
impl MediaBindingFactory for MediaComposition {
    fn bind(
        &self,
        binding: InvocationBinding,
        _: Arc<Definition>,
        _: SelectedEntrypoint,
        _: Caller,
    ) -> PortFuture<MediaBindings> {
        let result = (|| {
            if binding.principal() != self.principal {
                Err(PortError::Denied)
            } else {
                Ok(MediaBindings {
                    driver: Arc::new(Cold),
                    snapshot: self.snapshot,
                    principal: self.principal.clone(),
                    streams: crate::routine::media::stream::Streams::new(
                        Default::default(),
                        crate::routine::media::stream::Pool::principal(),
                        crate::routine::media::stream::Pool::process(),
                        CancellationToken::new(),
                    )
                    .map_err(|_| PortError::Invalid)?,
                    inputs: BTreeMap::new(),
                    outputs: BTreeMap::new(),
                    uploads: BTreeMap::new(),
                    speech: self.speech.clone(),
                    helpers: self.helpers.clone(),
                    context: self.context.clone(),
                    blobs: self.blobs.clone(),
                    text: self.text.clone(),
                    attachments: waltr_agent::TransientInputBudget::process(),
                    workspace_principal: self.principal_pools.workspace.clone(),
                    workspace_process: self.process_pools.workspace.clone(),
                    result_principal: self.principal_pools.result.clone(),
                    result_process: self.process_pools.result.clone(),
                    leases: Arc::new(Mutex::new(Vec::new())),
                })
            }
        })();
        Box::pin(async move { result })
    }
}
impl<S: SessionStore + Clone + Send + Sync + 'static> AuthorizedToolExecutor for LazyTools<S> {
    fn execute(
        &self,
        request: AuthorizedToolRequest,
        cancellation: CancellationToken,
    ) -> AuthorizedToolFuture<waltr_agent::ToolResult> {
        let owner = self.clone();
        Box::pin(async move {
            owner
                .executor()
                .await
                .map_err(|_| AuthorizedToolError::Unavailable)?
                .execute(request, cancellation)
                .await
        })
    }
    fn schemas(
        &self,
        targets: Vec<ConcreteToolTarget>,
        cancellation: CancellationToken,
    ) -> AuthorizedToolFuture<Vec<waltr_agent::ToolDefinition>> {
        let owner = self.clone();
        Box::pin(async move {
            owner
                .executor()
                .await
                .map_err(|_| AuthorizedToolError::Unavailable)?
                .schemas(targets, cancellation)
                .await
        })
    }
    fn cleanup(&self) -> AuthorizedToolFuture<()> {
        let owner = self.clone();
        Box::pin(async move {
            owner
                .stopped
                .store(true, std::sync::atomic::Ordering::Release);
            let executor = owner.executor.lock().await.clone();
            if let Some(executor) = executor {
                executor.cleanup().await?;
            }
            Ok(())
        })
    }
}
struct Cold;
impl MediaDriver for Cold {
    fn start(&self) -> PortFuture<()> {
        Box::pin(async { Err(PortError::Denied) })
    }
    fn cancel(&self) {}
    fn finish(&self) -> PortFuture<()> {
        Box::pin(async { Err(PortError::Denied) })
    }
    fn join(&self) -> PortFuture<()> {
        Box::pin(async { Ok(()) })
    }
}
