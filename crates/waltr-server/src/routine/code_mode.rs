//! Opt-in agent and ordinary MCP code calls over the existing invocation owner.
#[cfg(all(test, feature = "test"))]
pub(crate) mod tests;
use super::{
    adapters::{
        authorized_tools::{AdapterBindings, InvocationAdapterFactory, RoutineAgentHostFactory},
        helper::CoordinatorHelperRunner,
    },
    definition::Definition,
    history::{Caller, Origin, Ownership},
    ports::{PortError, PortFuture, SelectedEntrypoint},
    service::{ChildInvocation, InvocationService},
};
use crate::access_auth::AuthenticatedPrincipal;
use crate::{
    agent_host::{
        Capability,
        authorized_tools::{
            AuthorizedToolExecutor, ConcreteToolTarget, SessionAuthorizedToolExecutor,
        },
        dispatcher::CapabilityDispatcher,
        runtime_factory::ServerAgentRuntimeFactory as AgentSessionRuntimeFactory,
    },
    permission::PermissionPolicy,
};
use futures_util::FutureExt as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;
use waltr_agent::{
    AgentRuntime, AgentSessionRuntimeError, AgentTool, HostDispatchBinding, SessionCoordinator,
    SessionCreateRequest, SessionId, SessionRecord, SessionStore,
};

/// Immutable association committed before invocation admission. No field is guest authority.
#[derive(Clone, Debug)]
pub struct CodeDispatch {
    /// Trusted authenticated owner.
    pub principal: String,
    /// Actual outer session identity.
    pub session_id: String,
    /// Durable session generation.
    pub incarnation: String,
    /// Original provider call identity (fresh host identity for public MCP calls).
    pub tool_call_id: String,
    /// Exact compiled inline definition revision.
    pub source_revision: String,
    /// Host-reserved invocation identity.
    pub invocation_id: String,
}
impl CodeDispatch {
    pub(crate) fn validate(&self) -> Result<(), PortError> {
        if [
            &self.principal,
            &self.session_id,
            &self.incarnation,
            &self.tool_call_id,
            &self.source_revision,
            &self.invocation_id,
        ]
        .iter()
        .any(|v| v.is_empty() || v.len() > 512)
        {
            return Err(PortError::Invalid);
        }
        Ok(())
    }
}
/// Durable non-replay port. Duplicate identities always refuse admission, even after a lost ACK.
pub trait CodeDispatchStore: Send + Sync {
    /// Insert exactly once, before any host effect or script admission.
    fn associate(&self, dispatch: CodeDispatch) -> PortFuture<()>;
    /// Record a bounded terminal outcome only for the exact immutable association.
    fn complete(&self, dispatch: CodeDispatch, outcome: Value) -> PortFuture<()>;
}

/// Optional ordinary MCP execution/discovery port, already scoped to actual caller authority.
/// A shared handler cannot substitute request metadata for this authenticated binding.
/// Implementations must not discover, publish schemas for, or execute upstream MCP targets.
/// Named agent helpers retain their own internal authority; they are not direct MCP proxies.
pub trait PublicCodePort: Send + Sync {
    /// A fresh explicit call. Request correlation and MRTR state are never retry authority.
    fn call(&self, input: CodeInput, cancellation: CancellationToken) -> PortFuture<Value>;
}

/// Trusted profile, never decoded from code-tool or MCP arguments.
#[derive(Clone)]
pub struct CodeProfile {
    /// Independent caller restriction intersected with the named agent's complete policy.
    pub restriction: PermissionPolicy,
    /// Explicit exact concrete targets; keys are stable `tool.*` capabilities.
    pub targets: BTreeMap<String, ConcreteToolTarget>,
    /// Explicit named helpers, retaining normal named-definition permissions and consent.
    pub helpers: BTreeSet<String>,
}
impl std::fmt::Debug for CodeProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeProfile")
            .field("targets", &self.targets)
            .field("helpers", &self.helpers)
            .finish_non_exhaustive()
    }
}
#[derive(Clone)]
struct Grant {
    principal: AuthenticatedPrincipal,
    session: SessionId,
    incarnation: String,
    profile: CodeProfile,
    live: CancellationToken,
    origin_scoped: bool,
}
impl Grant {
    fn check(&self) -> Result<(), PortError> {
        if self.live.is_cancelled() || self.principal.access_ended().now_or_never().is_some() {
            Err(PortError::Denied)
        } else {
            Ok(())
        }
    }
    fn capabilities(&self) -> BTreeSet<String> {
        let mut caps = BTreeSet::from(["tools.execute".into(), "tools.schemas".into()]);
        caps.extend(self.profile.targets.keys().cloned());
        if !self.profile.helpers.is_empty() {
            caps.insert("helper.run".into());
            caps.extend(
                self.profile
                    .helpers
                    .iter()
                    .map(|name| format!("helper.named.{name}")),
            );
        }
        caps
    }
}

/// Host-configured authority and composition. Empty by default; authentication implies no grant.
pub struct CodeModeHost<S: SessionStore + Clone + Send + Sync + 'static> {
    service: InvocationService,
    dispatches: Arc<dyn CodeDispatchStore>,
    sessions: SessionCoordinator<S>,
    helpers: Arc<AgentSessionRuntimeFactory>,
    catalog: Option<crate::resource_catalog::AgentDefinitionState>,
    grants: Mutex<BTreeMap<(String, String), Grant>>,
    stopped: std::sync::atomic::AtomicBool,
}
impl<S: SessionStore + Clone + Send + Sync + 'static> std::fmt::Debug for CodeModeHost<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeModeHost").finish_non_exhaustive()
    }
}
impl<S: SessionStore + Clone + Send + Sync + 'static> Drop for CodeModeHost<S> {
    fn drop(&mut self) {
        if let Ok(grants) = self.grants.get_mut() {
            for grant in grants.values() {
                grant.live.cancel();
            }
        }
    }
}
impl<S: SessionStore + Clone + Send + Sync + 'static> CodeModeHost<S> {
    pub(crate) fn stop(&self) -> Result<(), PortError> {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Release);
        for grant in self
            .grants
            .lock()
            .map_err(|_| PortError::Unavailable)?
            .values()
        {
            grant.live.cancel();
        }
        Ok(())
    }
    /// Reuse the actual service, ordinary store/coordinator and normal helper runtime factory.
    pub fn new(
        service: InvocationService,
        dispatches: Arc<dyn CodeDispatchStore>,
        sessions: SessionCoordinator<S>,
        helpers: Arc<AgentSessionRuntimeFactory>,
    ) -> Self {
        Self {
            service,
            dispatches,
            sessions,
            helpers,
            catalog: None,
            grants: Mutex::new(BTreeMap::new()),
            stopped: std::sync::atomic::AtomicBool::new(false),
        }
    }
    pub(crate) fn with_catalog(mut self, catalog: crate::resource_catalog::AgentDefinitionState) -> Self {
        self.catalog = Some(catalog);
        self
    }
    /// Bind one authenticated principal and actual session incarnation to an operator profile.
    /// Replacing or revoking a grant cancels its active executions; request metadata grants nothing.
    pub fn grant(
        &self,
        principal: AuthenticatedPrincipal,
        record: &SessionRecord,
        profile: CodeProfile,
    ) -> Result<(), PortError> {
        self.insert_grant(principal, record, profile, false)
    }

    fn insert_grant(
        &self,
        principal: AuthenticatedPrincipal,
        record: &SessionRecord,
        profile: CodeProfile,
        origin_scoped: bool,
    ) -> Result<(), PortError> {
        if principal.access_ended().now_or_never().is_some()
            || profile.targets.len() > 128
            || profile.helpers.len() > 32
            || profile.targets.keys().any(|key| {
                !key.strip_prefix("tool.")
                    .is_some_and(super::definition::identifier)
            })
            || profile
                .helpers
                .iter()
                .any(|name| !super::definition::identifier(name))
        {
            return Err(PortError::Denied);
        }
        let incarnation = record
            .metadata
            .incarnation
            .clone()
            .ok_or(PortError::Denied)?;
        let grant = Grant {
            principal,
            session: record.id.clone(),
            incarnation,
            profile,
            live: CancellationToken::new(),
            origin_scoped,
        };
        if grant.capabilities().len() > 64 {
            return Err(PortError::Invalid);
        }
        let mut grants = self.grants.lock().map_err(|_| PortError::Unavailable)?;
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return Err(PortError::Denied);
        }
        if origin_scoped
            && grants
                .get(&(
                    super::rpc::principal_id(&grant.principal),
                    record.id.as_str().into(),
                ))
                .is_some_and(|current| {
                    current.origin_scoped
                        && !current.live.is_cancelled()
                        && current.incarnation == grant.incarnation
                })
        {
            return Ok(());
        }
        if grants.len() >= 128
            && !grants.contains_key(&(
                super::rpc::principal_id(&grant.principal),
                record.id.as_str().into(),
            ))
        {
            return Err(PortError::Busy);
        }
        if let Some(old) = grants.insert(
            (
                super::rpc::principal_id(&grant.principal),
                record.id.as_str().into(),
            ),
            grant,
        ) {
            old.live.cancel();
        }
        Ok(())
    }
    pub(crate) fn grant_current(
        &self,
        principal: AuthenticatedPrincipal,
        record: &SessionRecord,
        profile: CodeProfile,
    ) -> Result<(), PortError> {
        if self
            .grants
            .lock()
            .map_err(|_| PortError::Unavailable)?
            .get(&(
                super::rpc::principal_id(&principal),
                record.id.as_str().into(),
            ))
            .is_some_and(|grant| {
                grant.origin_scoped
                    && !grant.live.is_cancelled()
                    && record.metadata.incarnation.as_deref() == Some(grant.incarnation.as_str())
            })
        {
            return Ok(());
        }
        self.insert_grant(principal, record, profile, true)
    }
    /// Revoke only the exact host-selected owner/session binding.
    pub fn revoke(
        &self,
        principal: &AuthenticatedPrincipal,
        session: &SessionId,
    ) -> Result<(), PortError> {
        if let Some(grant) = self
            .grants
            .lock()
            .map_err(|_| PortError::Unavailable)?
            .remove(&(super::rpc::principal_id(principal), session.as_str().into()))
        {
            grant.live.cancel();
        }
        Ok(())
    }
    /// Create a principal-scoped adapter. Hosts must not share this adapter across callers.
    /// Production per-request transport registration remains a separate composition step.
    pub fn bind(
        self: &Arc<Self>,
        principal: &AuthenticatedPrincipal,
        record: &SessionRecord,
        runtime: AgentRuntime,
        dispatcher: CapabilityDispatcher,
    ) -> Result<Arc<CodeModeAdapter<S>>, PortError> {
        let mut grant = self
            .grants
            .lock()
            .map_err(|_| PortError::Unavailable)?
            .get(&(
                super::rpc::principal_id(principal),
                record.id.as_str().into(),
            ))
            .cloned()
            .ok_or(PortError::Denied)?;
        if grant.origin_scoped {
            // The operator policy is shared; its execution lease is this exact request.
            grant.principal = principal.clone();
            grant.origin_scoped = false;
        }
        grant.check()?;
        if principal.access_ended().now_or_never().is_some()
            || record.metadata.incarnation.as_deref() != Some(grant.incarnation.as_str())
        {
            return Err(PortError::Denied);
        }
        // Both the grant lease and the current authentication lease constrain this adapter.
        Ok(Arc::new(CodeModeAdapter {
            host: self.clone(),
            runtime,
            dispatcher: dispatcher.narrowed_permissions(grant.profile.restriction.clone()),
            record: record.clone(),
            grant,
            request_principal: principal.clone(),
            public_only: false,
        }))
    }
    /// Install actual normal-session runtime composition for a host-bound principal.
    /// Only definitions with `codeMode: true` call this provider; each session still needs a grant.
    pub fn install(
        self: &Arc<Self>,
        factory: AgentSessionRuntimeFactory,
        principal: AuthenticatedPrincipal,
    ) -> AgentSessionRuntimeFactory {
        let host = self.clone();
        factory.with_code_tool_provider(move |record, runtime, dispatcher| {
            host.bind(&principal, record, runtime, dispatcher)
                .map(|adapter| adapter.agent_tool())
                .map_err(|_| {
                    AgentSessionRuntimeError::FailedPrecondition(
                        "code mode has no current host authority for this session".into(),
                    )
                })
        })
    }
}

/// Code arguments contain no profile, capabilities, owner or retry grant.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "camelCase", deny_unknown_fields)]
pub enum CodeInput {
    /// Bounded JavaScript async function body. Return only chosen business data, not raw tool logs.
    Execute {
        /// JavaScript body; no imports or ambient filesystem/environment/network.
        source: String,
    },
    /// Schema-free bounded exact-target discovery.
    Discover {
        /// Exclusive exact-target cursor.
        #[serde(default)]
        after: Option<ConcreteToolTarget>,
    },
    /// Selected schemas only; never a bulk inventory dump.
    Schemas {
        /// At most eight exact targets.
        targets: Vec<ConcreteToolTarget>,
    },
}
impl CodeInput {
    /// Bounded model/public schema, without eagerly publishing MCP tool schemas.
    pub fn schema() -> Value {
        json!({"oneOf":[
            {"type":"object","properties":{"action":{"const":"execute"},"source":{"type":"string","maxLength":32768}},"required":["action","source"],"additionalProperties":false},
            {"type":"object","properties":{"action":{"const":"discover"},"after":{"type":"object","properties":{"server":{"type":"string","maxLength":256},"tool":{"type":"string","maxLength":256}},"required":["server","tool"],"additionalProperties":false}},"required":["action"],"additionalProperties":false},
            {"type":"object","properties":{"action":{"const":"schemas"},"targets":{"type":"array","maxItems":8,"items":{"type":"object","properties":{"server":{"type":"string","maxLength":256},"tool":{"type":"string","maxLength":256}},"required":["server","tool"],"additionalProperties":false}}},"required":["action","targets"],"additionalProperties":false}
        ]})
    }
}

/// Principal-scoped ordinary agent/MCP adapter. It owns no engine or second policy loop.
pub struct CodeModeAdapter<S: SessionStore + Clone + Send + Sync + 'static> {
    host: Arc<CodeModeHost<S>>,
    runtime: AgentRuntime,
    dispatcher: CapabilityDispatcher,
    record: SessionRecord,
    grant: Grant,
    request_principal: AuthenticatedPrincipal,
    public_only: bool,
}
impl<S: SessionStore + Clone + Send + Sync + 'static> std::fmt::Debug for CodeModeAdapter<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeModeAdapter").finish_non_exhaustive()
    }
}
impl<S: SessionStore + Clone + Send + Sync + 'static> CodeModeAdapter<S> {
    fn live(&self) -> CodeLive<S> {
        CodeLive {
            sessions: self.host.sessions.clone(),
            grant: self.grant.clone(),
            principal: self.request_principal.clone(),
        }
    }
    fn check(&self) -> Result<(), PortError> {
        self.grant.check()?;
        if self
            .request_principal
            .access_ended()
            .now_or_never()
            .is_some()
        {
            Err(PortError::Denied)
        } else {
            Ok(())
        }
    }
    fn definition(&self, source: &str, origin: Origin) -> Result<Arc<Definition>, PortError> {
        self.check()?;
        if source.is_empty() || source.len() > 32768 {
            return Err(PortError::Invalid);
        }
        // The wrapper exports a closed bounded string contract, decoded only after engine validation.
        let source = format!(
            "export default async function() {{ const result = await (async () => {{\n{source}\n}})(); return JSON.stringify(result === undefined ? null : result); }}"
        );
        self.host.service.code_definition(&json!({"apiVersion":"waltr.routine/v1","kind":"Routine","name":"code-mode","description":"Host-bound inline code call","spec":{"source":source,"entrypoints":{"run":{"trigger":if origin==Origin::Mcp {"mcp"}else{"inline"}}},"parameters":{"schemaVersion":"waltr.value/v1","schema":{"type":"null"}},"result":{"schemaVersion":"waltr.value/v1","schema":{"type":"string","maxLength":60000}},"capabilities":self.grant.capabilities()}}).to_string())
    }
    fn prepare(&self, input: &CodeInput, origin: Origin) -> Result<HostDispatchBinding, PortError> {
        self.check()?;
        let revision = match input {
            CodeInput::Execute { source } => self.definition(source, origin)?.revision().to_owned(),
            _ => "code-discovery-v1".into(),
        };
        Ok(HostDispatchBinding {
            invocation_id: uuid::Uuid::new_v4().to_string(),
            source_revision: revision,
        })
    }
    /// One normally authorized model tool, guarded by the neutral durable agent fence.
    pub fn agent_tool(self: &Arc<Self>) -> Capability {
        let adapter = self.clone();
        let prepare = self.clone();
        AgentTool::new_async(waltr_agent::ToolDefinition { name:"code".into(),description:"Optional bounded JavaScript over the same Routine host API. action=discover returns exact allowed names/descriptions (no schemas); action=schemas retrieves only selected exact target schemas. action=execute accepts an async function body using global host.call('tools.execute',{target:{server,tool},callId,arguments}), host.call('tools.schemas',{targets}), and explicitly configured helper.run. Return selected business data; do not return raw logs. No imports/ambient filesystem/environment/network. Code capability is not concrete consent: normal exact permissions, middleware, Skills and Apps authority still apply. Interrupted code is never automatically replayed.".into(),input_schema:CodeInput::schema() }, move |context| {
            let adapter=adapter.clone(); Box::pin(async move {
                let input:CodeInput=serde_json::from_value(context.call().arguments.clone()).map_err(|_|agent_error(PortError::Invalid))?;
                let binding=context.host_dispatch().cloned().ok_or_else(||agent_error(PortError::Denied))?;
                if context.session_id()!=Some(&adapter.record.id) || context.session_incarnation()!=Some(adapter.grant.incarnation.as_str()) { return Err(agent_error(PortError::Denied)); }
                let output=adapter.run(input,binding,(Origin::Inline,context.call().id.clone()),context.cancellation_token().clone()).await.map_err(agent_error)?;
                Ok(waltr_agent::ToolResult::new(context.call().id.clone(),context.call().name.clone(),output.to_string()))
            })
        }).with_host_dispatch(move |call| {
            let input:CodeInput=serde_json::from_value(call.arguments.clone()).map_err(|_|agent_error(PortError::Invalid))?;
            prepare.prepare(&input,Origin::Inline).map_err(agent_error)
        }).into()
    }
    async fn run(
        &self,
        input: CodeInput,
        binding: HostDispatchBinding,
        call: (Origin, String),
        cancel: CancellationToken,
    ) -> Result<Value, PortError> {
        let (origin, call_id) = call;
        self.check()?;
        let current = self.live().check().await?;
        if cancel.is_cancelled() {
            return Err(PortError::Denied);
        }
        match input {
            CodeInput::Discover { after } => {
                let allowed: Vec<_> = self.grant.profile.targets.values().cloned().collect();
                let items = self
                    .dispatcher
                    .concrete_tool_page(&allowed, after.as_ref(), 16)
                    .map_err(|_| PortError::Denied)?;
                let next = items.last().map(|item| item.target.clone());
                bounded(json!({"tools":items,"after":next}))
            }
            CodeInput::Schemas { targets } => {
                if targets.len() > 8
                    || targets.iter().any(|target| {
                        !self
                            .grant
                            .profile
                            .targets
                            .values()
                            .any(|configured| configured == target)
                    })
                {
                    return Err(PortError::Denied);
                }
                if self.public_only {
                    return bounded(json!({"schemas":[]}));
                }
                let executor = self.executor().await?;
                let result = tokio::select! { biased;
                    ()=cancel.cancelled()=>Err(PortError::Denied),
                    ()=self.grant.live.cancelled()=>Err(PortError::Denied),
                    ()=self.grant.principal.access_ended()=>Err(PortError::Denied),
                    ()=self.request_principal.access_ended()=>Err(PortError::Denied),
                    result=executor.schemas(targets,cancel.clone())=>result.map_err(|_|PortError::Denied)
                };
                executor.cleanup().await.map_err(|_| PortError::Uncertain)?;
                self.live().check().await?;
                bounded(json!({"schemas":result?}))
            }
            CodeInput::Execute { source } => {
                let definition = self.definition(&source, origin)?;
                if binding.source_revision != definition.revision() {
                    return Err(PortError::Conflict);
                }
                let dispatch = CodeDispatch {
                    principal: super::rpc::principal_id(&self.grant.principal),
                    session_id: self.grant.session.as_str().into(),
                    incarnation: self.grant.incarnation.clone(),
                    tool_call_id: call_id,
                    source_revision: binding.source_revision,
                    invocation_id: binding.invocation_id,
                };
                self.host.dispatches.associate(dispatch.clone()).await?;
                self.live().check().await?;
                if cancel.is_cancelled() {
                    return Err(PortError::Denied);
                }
                let factory = Arc::new(CodeHostFactory {
                    live: self.live(),
                    inner: RoutineAgentHostFactory {
                        bindings: Arc::new(CodeBindings {
                            live: self.live(),
                            sessions: self.host.sessions.clone(),
                            helpers: self.host.helpers.clone(),
                            runtime: self.runtime.clone(),
                            record: current,
                            grant: self.grant.clone(),
                            public_only: self.public_only,
                            catalog: self.host.catalog.as_ref().map(|state| state.snapshot()),
                        }),
                    },
                });
                let child = self.host.service.invoke_code(
                    ChildInvocation {
                        definition,
                        entrypoint: "run".into(),
                        caller: Caller {
                            principal: dispatch.principal.clone(),
                            origin,
                            ownership: if origin == Origin::Mcp {
                                Ownership::Leased
                            } else {
                                Ownership::Tool
                            },
                            correlation: dispatch.tool_call_id.clone(),
                        },
                        input: Value::Null,
                    },
                    factory,
                    dispatch.invocation_id.clone(),
                )?;
                let owner = child.owner;
                let admitted = tokio::select! {biased; ()=cancel.cancelled()=>{owner.cancel();Err(PortError::Uncertain)},()=self.grant.live.cancelled()=>{owner.cancel();Err(PortError::Uncertain)},()=self.grant.principal.access_ended()=>{owner.cancel();Err(PortError::Uncertain)},()=self.request_principal.access_ended()=>{owner.cancel();Err(PortError::Uncertain)},result=child.admitted=>result};
                if admitted.is_err() {
                    owner.cancel();
                }
                let outcome = tokio::select! {biased; ()=cancel.cancelled()=>{owner.cancel();owner.join().await},()=self.grant.live.cancelled()=>{owner.cancel();owner.join().await},()=self.grant.principal.access_ended()=>{owner.cancel();owner.join().await},()=self.request_principal.access_ended()=>{owner.cancel();owner.join().await},outcome=owner.join()=>outcome};
                let result = outcome
                    .result
                    .as_ref()
                    .and_then(Value::as_str)
                    .and_then(|result| serde_json::from_str::<Value>(result).ok());
                let output = bounded(
                    json!({"invocationId":dispatch.invocation_id,"status":outcome.status,"cleanupComplete":outcome.cleanup_complete,"result":result,"admissionError":admitted.err().map(|error|format!("{error:?}"))}),
                ).map_err(|_| PortError::Uncertain)?;
                self.host
                    .dispatches
                    .complete(dispatch, output.clone())
                    .await
                    .map_err(|_| PortError::Uncertain)?;
                Ok(output)
            }
        }
    }
    async fn executor(&self) -> Result<Arc<dyn AuthorizedToolExecutor>, PortError> {
        let mut request = SessionCreateRequest::new(SessionId::generate());
        request.inference_settings = self.record.inference_settings.clone();
        request.agent_definition = self.record.agent_definition.clone();
        let id = self
            .host
            .sessions
            .create(request)
            .await
            .map_err(|_| PortError::Unavailable)?
            .id()
            .clone();
        let record = self
            .host
            .sessions
            .load(&id)
            .await
            .map_err(|_| PortError::Unavailable)?
            .record;
        let (runtime, dispatcher) = self
            .host
            .helpers
            .as_ref()
            .clone()
            .for_concrete_calls()
            .with_permission_restriction(self.grant.profile.restriction.clone())
            .runtime_for_record(&record, self.host.sessions.clone())
            .await
            .map_err(|_| PortError::Unavailable)?;
        Ok(Arc::new(SessionAuthorizedToolExecutor::new(
            self.host.sessions.clone(),
            id,
            runtime,
            dispatcher,
        )))
    }
}
fn bounded(value: Value) -> Result<Value, PortError> {
    if serde_json::to_vec(&value)
        .map_err(|_| PortError::Invalid)?
        .len()
        > 65536
    {
        Err(PortError::Invalid)
    } else {
        Ok(value)
    }
}
fn agent_error(error: PortError) -> waltr_agent::LlmError {
    waltr_agent::LlmError::Protocol {
        provider: "code",
        message: format!("code host category: {error:?}; interrupted calls must not be replayed"),
    }
}

impl<S: SessionStore + Clone + Send + Sync + 'static> PublicCodePort for Arc<CodeModeAdapter<S>> {
    fn call(&self, input: CodeInput, cancellation: CancellationToken) -> PortFuture<Value> {
        let mut grant = self.grant.clone();
        // Concrete targets identify upstream MCP tools, never Waltr's own capabilities.
        grant.profile.targets.clear();
        let adapter = CodeModeAdapter {
            host: self.host.clone(),
            runtime: self.runtime.clone(),
            dispatcher: self.dispatcher.clone(),
            record: self.record.clone(),
            grant,
            request_principal: self.request_principal.clone(),
            public_only: true,
        };
        Box::pin(async move {
            let binding = adapter.prepare(&input, Origin::Mcp)?;
            adapter
                .run(
                    input,
                    binding,
                    (Origin::Mcp, uuid::Uuid::new_v4().to_string()),
                    cancellation,
                )
                .await
        })
    }
}

#[derive(Clone)]
struct CodeLive<S: SessionStore + Clone + Send + Sync + 'static> {
    sessions: SessionCoordinator<S>,
    grant: Grant,
    principal: AuthenticatedPrincipal,
}
impl<S: SessionStore + Clone + Send + Sync + 'static> CodeLive<S> {
    async fn check(&self) -> Result<SessionRecord, PortError> {
        self.grant.check()?;
        if self.principal.access_ended().now_or_never().is_some() {
            return Err(PortError::Denied);
        }
        let current = self
            .sessions
            .load(&self.grant.session)
            .await
            .map_err(|_| PortError::Denied)?
            .record;
        if current.metadata.incarnation.as_deref() != Some(self.grant.incarnation.as_str()) {
            return Err(PortError::Denied);
        }
        self.grant.check()?;
        Ok(current)
    }
}
struct CodeHostFactory<S: SessionStore + Clone + Send + Sync + 'static> {
    inner: RoutineAgentHostFactory,
    live: CodeLive<S>,
}
impl<S: SessionStore + Clone + Send + Sync + 'static> super::ports::HostFactory
    for CodeHostFactory<S>
{
    fn preflight(
        &self,
        binding: super::history::InvocationBinding,
        definition: Arc<Definition>,
        selected: SelectedEntrypoint,
        caller: Caller,
    ) -> PortFuture<super::ports::PreparedHost> {
        let pending = super::ports::HostFactory::preflight(
            &self.inner,
            binding,
            definition,
            selected,
            caller,
        );
        let live = self.live.clone();
        Box::pin(async move {
            live.check().await?;
            let mut prepared = pending.await?;
            prepared.policy = Arc::new(CodePolicy {
                inner: prepared.policy,
                live: live.clone(),
            });
            prepared.dispatcher = Arc::new(CodeDispatcher {
                inner: prepared.dispatcher,
                live: live.clone(),
            });
            prepared.reservation = Box::new(CodeReservation {
                inner: prepared.reservation,
                live,
            });
            Ok(prepared)
        })
    }
}
struct CodeReservation<S: SessionStore + Clone + Send + Sync + 'static> {
    inner: Box<dyn super::ports::Reservation>,
    live: CodeLive<S>,
}
impl<S: SessionStore + Clone + Send + Sync + 'static> super::ports::Reservation
    for CodeReservation<S>
{
    fn source_session(&self) -> Option<String> {
        self.inner.source_session()
    }
    fn public_definition(&self) -> Option<Value> {
        self.inner.public_definition()
    }
    fn prepare(&self) -> PortFuture<()> {
        let live = self.live.clone();
        let pending = self.inner.prepare();
        Box::pin(async move {
            live.check().await?;
            pending.await
        })
    }
    fn abort(&self) -> PortFuture<()> {
        self.inner.abort()
    }
    fn finalize(&self, interrupted: bool) -> PortFuture<()> {
        self.inner.finalize(interrupted)
    }
}
struct CodePolicy<S: SessionStore + Clone + Send + Sync + 'static> {
    inner: Arc<dyn super::ports::OperationPolicy>,
    live: CodeLive<S>,
}
impl<S: SessionStore + Clone + Send + Sync + 'static> super::ports::OperationPolicy
    for CodePolicy<S>
{
    fn before(
        &self,
        request: &waltr_script::HostRequest,
        context: &waltr_script::HostContext,
    ) -> PortFuture<Value> {
        let live = self.live.clone();
        let arguments = request.arguments.clone().into_json();
        if request.operation == "tools.schemas"
            && arguments
                .get("targets")
                .and_then(Value::as_array)
                .is_some_and(|targets| targets.len() > 8)
        {
            return Box::pin(async { Err(PortError::Invalid) });
        }
        let pending = self.inner.before(request, context);
        Box::pin(async move {
            live.check().await?;
            pending.await
        })
    }
    fn project_result(
        &self,
        name: &str,
        reply: &waltr_script::HostReply,
    ) -> Result<Option<Value>, PortError> {
        self.inner.project_result(name, reply)
    }
}
struct CodeDispatcher<S: SessionStore + Clone + Send + Sync + 'static> {
    inner: Arc<dyn waltr_script::HostDispatcher>,
    live: CodeLive<S>,
}
impl<S: SessionStore + Clone + Send + Sync + 'static> waltr_script::HostDispatcher
    for CodeDispatcher<S>
{
    fn dispatch(
        &self,
        request: waltr_script::HostRequest,
        context: waltr_script::HostContext,
    ) -> waltr_script::HostFuture {
        let inner = self.inner.clone();
        let live = self.live.clone();
        Box::pin(async move {
            live.check()
                .await
                .map_err(|_| waltr_script::Error::Denied)?;
            inner.dispatch(request, context).await
        })
    }
    fn cleanup(&self, invocation: waltr_script::InvocationId) -> waltr_script::CleanupFuture {
        self.inner.cleanup(invocation)
    }
}

struct CodeBindings<S: SessionStore + Clone + Send + Sync + 'static> {
    live: CodeLive<S>,
    sessions: SessionCoordinator<S>,
    helpers: Arc<AgentSessionRuntimeFactory>,
    runtime: AgentRuntime,
    record: SessionRecord,
    grant: Grant,
    public_only: bool,
    catalog: Option<Arc<crate::resource_catalog::AgentDefinitionSnapshot>>,
}
impl<S: SessionStore + Clone + Send + Sync + 'static> InvocationAdapterFactory for CodeBindings<S> {
    fn bind(
        &self,
        _: Arc<Definition>,
        _: SelectedEntrypoint,
        caller: Caller,
    ) -> PortFuture<AdapterBindings> {
        let sessions = self.sessions.clone();
        let _runtime = self.runtime.clone();
        let record = self.record.clone();
        let grant = self.grant.clone();
        let helpers = self.helpers.clone();
        let live = self.live.clone();
        let public_only = self.public_only;
        let catalog = self.catalog.clone();
        Box::pin(async move {
            grant.check()?;
            if caller.principal != super::rpc::principal_id(&grant.principal) {
                return Err(PortError::Denied);
            }
            let inference = record
                .inference_settings
                .clone()
                .ok_or(PortError::Invalid)?;
            let tools: Arc<dyn AuthorizedToolExecutor> = if public_only {
                Arc::new(PublicToolsClosed)
            } else {
                let mut request = SessionCreateRequest::new(SessionId::generate());
                request.inference_settings = Some(inference.clone());
                request.agent_definition = record.agent_definition.clone();
                let id = sessions
                    .create(request)
                    .await
                    .map_err(|_| PortError::Unavailable)?
                    .id()
                    .clone();
                let tool_record = sessions
                    .load(&id)
                    .await
                    .map_err(|_| PortError::Unavailable)?
                    .record;
                let (runtime, dispatcher) = helpers
                    .as_ref()
                    .clone()
                    .for_concrete_calls()
                    .with_permission_restriction(grant.profile.restriction.clone())
                    .runtime_for_record(&tool_record, sessions.clone())
                    .await
                    .map_err(|_| PortError::Unavailable)?;
                Arc::new(SessionAuthorizedToolExecutor::new(
                    sessions.clone(),
                    id,
                    runtime,
                    dispatcher,
                ))
            };
            let mut helper = CoordinatorHelperRunner::new(
                sessions,
                helpers,
                inference.clone(),
                BTreeMap::new(),
                BTreeMap::new(),
                None,
                caller.principal.clone(),
            )?
            .with_permission_restriction(grant.profile.restriction.clone())
            .with_origin_guard(move || {
                let live = live.clone();
                Box::pin(async move { live.check().await.map(|_| ()) })
            });
            if let Some(catalog) = catalog {
                helper = helper.with_catalog(catalog, (!public_only).then_some(inference));
            }
            Ok(AdapterBindings {
                principal: caller.principal,
                tools,
                helpers: Arc::new(helper),
                capabilities: grant.capabilities(),
                targets: grant.profile.targets,
            })
        })
    }
}

struct PublicToolsClosed;
impl AuthorizedToolExecutor for PublicToolsClosed {
    fn execute(
        &self,
        _: crate::agent_host::authorized_tools::AuthorizedToolRequest,
        _: CancellationToken,
    ) -> crate::agent_host::authorized_tools::AuthorizedToolFuture<waltr_agent::ToolResult> {
        Box::pin(async { Err(crate::agent_host::authorized_tools::AuthorizedToolError::Denied) })
    }
    fn schemas(
        &self,
        targets: Vec<ConcreteToolTarget>,
        _: CancellationToken,
    ) -> crate::agent_host::authorized_tools::AuthorizedToolFuture<Vec<waltr_agent::ToolDefinition>>
    {
        Box::pin(async move {
            if targets.is_empty() {
                Ok(Vec::new())
            } else {
                Err(crate::agent_host::authorized_tools::AuthorizedToolError::Denied)
            }
        })
    }
    fn cleanup(&self) -> crate::agent_host::authorized_tools::AuthorizedToolFuture<()> {
        Box::pin(async { Ok(()) })
    }
}
