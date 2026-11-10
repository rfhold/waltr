//! Typed routine host operations over neutral helper and authorized-tool ports.

use super::helper::ScriptCompletionObserver;
use crate::agent_host::authorized_tools::{
    AuthorizedToolError, AuthorizedToolExecutor, AuthorizedToolRequest, ConcreteToolTarget,
};
use crate::routine::{
    children::{ChildCall, ChildPort},
    context::{ContextOperation, ContextPort},
    definition::Definition,
    history::Caller,
    ports::{
        HelperRequest, HelperRunner, HelperSelection, HostFactory, OperationPolicy, PortError,
        PortFuture, PreparedHost, Reservation, SelectedEntrypoint,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use waltr_agent::ToolCall;
use waltr_script::{
    CleanupFuture, HostContext, HostDispatcher, HostFuture, HostReply, HostRequest, InvocationId,
    Limits, StructuredValue,
};

/// Fresh per-invocation authenticated bindings. A target capability is a constructor-selected
/// exact identity, never a sanitized model alias. Discovery is not an execution grant.
pub struct AdapterBindings {
    /// Trusted principal this private set of consumer sessions belongs to.
    pub principal: String,
    /// Independently replaceable helper lifecycle.
    pub helpers: Arc<dyn HelperRunner>,
    /// Independently replaceable complete agent execution lifecycle.
    pub tools: Arc<dyn AuthorizedToolExecutor>,
    /// Exact supported capability declarations, including helper/capture identities.
    pub capabilities: BTreeSet<String>,
    /// `tool.<id>` capability to exact configured server/tool mapping.
    pub targets: BTreeMap<String, ConcreteToolTarget>,
}
impl std::fmt::Debug for AdapterBindings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdapterBindings").finish_non_exhaustive()
    }
}

/// Authenticated host composition. Implementations return new private invocation owners,
/// not a shared mutable current-session runtime. No production registration is implicit.
pub trait InvocationAdapterFactory: Send + Sync {
    /// Optional same-service bounded child owner. No default child authority.
    fn bind_children(
        &self,
        _definition: Arc<Definition>,
        _selected: SelectedEntrypoint,
        _caller: Caller,
    ) -> PortFuture<Option<Arc<dyn ChildPort>>> {
        Box::pin(async { Ok(None) })
    }
    /// Return constructor-scoped child bindings; default cannot manufacture child authority.
    fn child_bindings(
        &self,
        _scope: crate::routine::children::ChildScope,
    ) -> PortFuture<Arc<dyn InvocationAdapterFactory>> {
        Box::pin(async { Err(PortError::UnsupportedCapability) })
    }
    /// Supply an invocation-pinned typed context owner. Default grants no context operations.
    fn bind_context(
        &self,
        _definition: Arc<Definition>,
        _selected: SelectedEntrypoint,
        _caller: Caller,
    ) -> PortFuture<Option<Arc<dyn ContextPort>>> {
        Box::pin(async { Ok(None) })
    }
    /// Resolve all required adapters and source/owner bindings before service admission.
    fn bind(
        &self,
        definition: Arc<Definition>,
        selected: SelectedEntrypoint,
        caller: Caller,
    ) -> PortFuture<AdapterBindings>;
}

/// Shared host factory with typed helpers and complete execution ports.
pub struct RoutineAgentHostFactory {
    /// Constructor-injected authenticated composition.
    pub bindings: Arc<dyn InvocationAdapterFactory>,
}
impl std::fmt::Debug for RoutineAgentHostFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoutineAgentHostFactory")
            .finish_non_exhaustive()
    }
}

impl HostFactory for RoutineAgentHostFactory {
    fn preflight_child(
        &self,
        binding: crate::routine::history::InvocationBinding,
        definition: Arc<Definition>,
        selected: SelectedEntrypoint,
        caller: Caller,
        scope: crate::routine::children::ChildScope,
    ) -> PortFuture<PreparedHost> {
        let bindings = self.bindings.clone();
        Box::pin(async move {
            if caller.principal != scope.principal
                || !definition
                    .envelope()
                    .spec
                    .capabilities
                    .is_subset(&scope.capabilities)
            {
                return Err(PortError::Denied);
            }
            let snapshot = scope.snapshot.provenance();
            let factory = RoutineAgentHostFactory {
                bindings: bindings.child_bindings(scope).await?,
            };
            let host = factory
                .preflight(binding, definition, selected, caller)
                .await?;
            if host.snapshot.as_ref() != Some(&snapshot) {
                return Err(PortError::Conflict);
            }
            Ok(host)
        })
    }
    fn preflight(
        &self,
        binding: crate::routine::history::InvocationBinding,
        definition: Arc<Definition>,
        selected: SelectedEntrypoint,
        caller: Caller,
    ) -> PortFuture<PreparedHost> {
        let bindings = self.bindings.clone();
        Box::pin(async move {
            if binding.principal() != caller.principal {
                return Err(PortError::Denied);
            }
            let required = definition.envelope().spec.capabilities.clone();
            if definition
                .envelope()
                .spec
                .ports
                .values()
                .any(|port| port.required)
            {
                return Err(PortError::UnsupportedCapability);
            }
            let context = bindings
                .bind_context(definition.clone(), selected.clone(), caller.clone())
                .await?;
            let children = bindings
                .bind_children(definition.clone(), selected.clone(), caller.clone())
                .await?;
            let bound = bindings
                .bind(definition.clone(), selected, caller.clone())
                .await?;
            if bound.principal != caller.principal {
                return Err(PortError::Denied);
            }
            if !required.is_subset(&bound.capabilities)
                || required.iter().any(|cap| !known_capability(cap))
            {
                return Err(PortError::UnsupportedCapability);
            }
            if required.iter().any(|cap| cap.starts_with("context.")) && context.is_none() {
                return Err(PortError::UnsupportedCapability);
            }
            if required.contains("children.call") && children.is_none() {
                return Err(PortError::UnsupportedCapability);
            }
            if required
                .iter()
                .filter(|cap| cap.starts_with("tool."))
                .any(|cap| !bound.targets.contains_key(cap))
            {
                return Err(PortError::UnsupportedCapability);
            }
            bound
                .helpers
                .bind_captures(definition.envelope().spec.captures.clone())
                .await?;
            bound
                .helpers
                .pin_named(
                    required
                        .iter()
                        .filter_map(|cap| cap.strip_prefix("helper.named.").map(str::to_owned))
                        .collect(),
                )
                .await?;
            if !definition.envelope().spec.captures.is_empty() {
                bound.helpers.preflight_captures().await?;
            }
            let snapshot = context
                .as_ref()
                .and_then(|context| context.snapshot_identity())
                .map(|identity| identity.provenance());
            let adapter = Arc::new(AgentHost {
                bindings: bound,
                capabilities: required,
                limits: definition.limits().clone(),
                cancellation: CancellationToken::new(),
                cleaned: Arc::new(AtomicBool::new(false)),
                invocation: std::sync::Mutex::new(None),
                context,
                children,
            });
            Ok(PreparedHost {
                dispatcher: adapter.clone(),
                policy: adapter.clone(),
                reservation: Box::new(JoinedReservation(adapter)),
                snapshot,
            })
        })
    }
}

fn known_capability(cap: &str) -> bool {
    matches!(
        cap,
        "helper.run"
            | "helper.validate"
            | "tools.execute"
            | "tools.schemas"
            | "context.snapshot"
            | "context.original"
            | "context.history"
            | "context.metadata"
            | "context.transact"
            | "context.create"
            | "context.start"
            | "context.continue"
            | "children.call"
    ) || [
        "helper.named.",
        "helper.restricted.",
        "helper.capture.",
        "tool.",
    ]
    .into_iter()
    .any(|prefix| cap.strip_prefix(prefix).is_some_and(|id| !id.is_empty()))
}

struct AgentHost {
    children: Option<Arc<dyn ChildPort>>,
    context: Option<Arc<dyn ContextPort>>,
    bindings: AdapterBindings,
    capabilities: BTreeSet<String>,
    limits: Limits,
    cancellation: CancellationToken,
    cleaned: Arc<AtomicBool>,
    invocation: std::sync::Mutex<Option<InvocationId>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct HelperArguments {
    pub selection: HelperSelection,
    pub source: Option<String>,
    pub task: String,
    pub capture: Option<String>,
    pub max_turns: u64,
    pub deadline_ms: u64,
    pub response_bytes: usize,
    pub completion_bytes: usize,
    pub field: Option<String>,
}
impl HelperArguments {
    pub(crate) fn validate(
        &self,
        capabilities: &BTreeSet<String>,
        callback: bool,
    ) -> Result<(), PortError> {
        let selection = match &self.selection {
            HelperSelection::Named { name } => format!("helper.named.{name}"),
            HelperSelection::Restricted { profile } => format!("helper.restricted.{profile}"),
        };
        if !capabilities.contains("helper.run")
            || !capabilities.contains(&selection)
            || self
                .capture
                .as_ref()
                .is_some_and(|id| !capabilities.contains(&format!("helper.capture.{id}")))
            || callback && !capabilities.contains("helper.validate")
        {
            return Err(PortError::Denied);
        }
        if self.task.is_empty()
            || self.task.len() > 16384
            || !(1..=64).contains(&self.max_turns)
            || !(1..=300000).contains(&self.deadline_ms)
            || !(1..=1048576).contains(&self.response_bytes)
            || !(1..=32 * 1024 * 1024).contains(&self.completion_bytes)
        {
            return Err(PortError::Invalid);
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ToolArguments {
    target: ConcreteToolTarget,
    call_id: String,
    arguments: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemaArguments {
    targets: Vec<ConcreteToolTarget>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildArguments {
    calls: Vec<ChildCall>,
}

enum Operation {
    Children(ChildArguments),
    Context(ContextOperation),
    Helper(HelperArguments),
    Tool(ToolArguments),
    Schemas(SchemaArguments),
}

impl AgentHost {
    fn parse(&self, request: &HostRequest) -> Result<Operation, PortError> {
        if self.cancellation.is_cancelled()
            || request.bytes.is_some()
            || request.handle.is_some()
            || !self.capabilities.contains(&request.operation)
        {
            return Err(PortError::Denied);
        }
        let value = request.arguments.clone().into_json();
        if serde_json::to_vec(&value)
            .map_err(|_| PortError::Invalid)?
            .len()
            > 65536
        {
            return Err(PortError::Invalid);
        }
        match request.operation.as_str() {
            "children.call" => {
                if request.callback.is_some() {
                    return Err(PortError::Invalid);
                }
                let args: ChildArguments =
                    serde_json::from_value(value).map_err(|_| PortError::Invalid)?;
                self.children
                    .as_ref()
                    .ok_or(PortError::UnsupportedCapability)?
                    .check(&args.calls)?;
                Ok(Operation::Children(args))
            }
            name if name.starts_with("context.") => {
                if request.callback.is_some() {
                    return Err(PortError::Invalid);
                }
                let operation = ContextOperation::parse(name, value)?;
                self.context
                    .as_ref()
                    .ok_or(PortError::UnsupportedCapability)?
                    .check(&operation)?;
                Ok(Operation::Context(operation))
            }
            "helper.run" => {
                let args: HelperArguments =
                    serde_json::from_value(value).map_err(|_| PortError::Invalid)?;
                args.validate(&self.capabilities, request.callback.is_some())?;
                if args.field.is_some()
                    || args.response_bytes > 65536
                    || args.completion_bytes > 65536
                {
                    return Err(PortError::Invalid);
                }
                if let Some(source) = &args.source {
                    self.context
                        .as_ref()
                        .ok_or(PortError::Denied)?
                        .resolve_source(source)?;
                }
                Ok(Operation::Helper(args))
            }
            "tools.execute" => {
                if request.callback.is_some() {
                    return Err(PortError::Invalid);
                }
                let args: ToolArguments =
                    serde_json::from_value(value).map_err(|_| PortError::Invalid)?;
                self.target_allowed(&args.target)?;
                if args.call_id.is_empty() || args.call_id.len() > 256 {
                    return Err(PortError::Invalid);
                }
                Ok(Operation::Tool(args))
            }
            "tools.schemas" => {
                if request.callback.is_some() {
                    return Err(PortError::Invalid);
                }
                let args: SchemaArguments =
                    serde_json::from_value(value).map_err(|_| PortError::Invalid)?;
                if args.targets.len() > 32 {
                    return Err(PortError::Invalid);
                }
                for target in &args.targets {
                    self.target_allowed(target)?;
                }
                Ok(Operation::Schemas(args))
            }
            _ => Err(PortError::Denied),
        }
    }

    fn target_allowed(&self, target: &ConcreteToolTarget) -> Result<(), PortError> {
        if self
            .bindings
            .targets
            .iter()
            .any(|(capability, configured)| {
                configured == target && self.capabilities.contains(capability)
            })
        {
            Ok(())
        } else {
            Err(PortError::Denied)
        }
    }
}

impl OperationPolicy for AgentHost {
    fn before(&self, request: &HostRequest, context: &HostContext) -> PortFuture<Value> {
        let result = (|| {
            let mut invocation = self.invocation.lock().map_err(|_| PortError::Unavailable)?;
            if invocation.is_some_and(|id| id != context.invocation()) {
                return Err(PortError::Denied);
            }
            *invocation = Some(context.invocation());
            // These are entry/capability checks. Complete contextual agent policy is applied
            // exactly once by the executor, after durable intent and before concrete effects.
            Ok(match self.parse(request)? {
                Operation::Context(operation) => json!({"operation":request.operation,
                    "context":self.context.as_ref().ok_or(PortError::Denied)?.intent(&operation)?}),
                Operation::Children(args) => {
                    json!({"operation":"children.call","count":args.calls.len()})
                }
                Operation::Helper(_) => json!({"operation":"helper.run"}),
                Operation::Tool(args) => json!({"target":args.target,"callId":args.call_id}),
                Operation::Schemas(args) => json!({"targets":args.targets}),
            })
        })();
        Box::pin(async move { result })
    }

    fn project_result(&self, name: &str, reply: &HostReply) -> Result<Option<Value>, PortError> {
        if !(matches!(
            name,
            "helper.run" | "tools.execute" | "tools.schemas" | "children.call"
        ) || name.starts_with("context.") && known_capability(name))
        {
            return Err(PortError::Denied);
        }
        let HostReply::Value(value) = reply else {
            return Err(PortError::Invalid);
        };
        let value = value.clone().into_json();
        let status = value
            .get("status")
            .and_then(Value::as_str)
            .ok_or(PortError::Invalid)?;
        if !matches!(status, "accepted" | "completed" | "denied") {
            return Err(PortError::Invalid);
        }
        let projection = match name {
            "context.create" => {
                json!({"status":status,"sessionId":value.pointer("/result/sessionId"),"destination":value.pointer("/result/destination")})
            }
            "context.transact" => {
                json!({"status":status,"revision":value.pointer("/result/revision"),"cursor":value.pointer("/result/cursor")})
            }
            _ => json!({"status":status}),
        };
        Ok(Some(projection))
    }
}

impl HostDispatcher for AgentHost {
    fn dispatch(&self, request: HostRequest, _: HostContext) -> HostFuture {
        let operation = self.parse(&request);
        let helpers = self.bindings.helpers.clone();
        let tools = self.bindings.tools.clone();
        let cancellation = self.cancellation.clone();
        let limits = self.limits.clone();
        let context = self.context.clone();
        let children = self.children.clone();
        Box::pin(async move {
            let value = match operation.map_err(|_| waltr_script::Error::Denied)? {
                Operation::Children(args) => {
                    let results = children
                        .ok_or(waltr_script::Error::Denied)?
                        .call(args.calls, cancellation)
                        .await
                        .map_err(|_| waltr_script::Error::Cleanup)?;
                    json!({"status":"completed","result":results})
                }
                Operation::Context(operation) => {
                    let result = context
                        .ok_or(waltr_script::Error::Denied)?
                        .execute(operation, cancellation)
                        .await
                        .map_err(|error| match error {
                            PortError::Denied | PortError::UnsupportedCapability => {
                                waltr_script::Error::Denied
                            }
                            PortError::Invalid => waltr_script::Error::Invalid,
                            _ => waltr_script::Error::Cleanup,
                        })?;
                    json!({"status":"completed","result":result})
                }
                Operation::Helper(args) => {
                    let source = args
                        .source
                        .as_ref()
                        .map(|source| {
                            context
                                .as_ref()
                                .ok_or(PortError::Denied)?
                                .resolve_source(source)
                        })
                        .transpose()
                        .map_err(|_| waltr_script::Error::Denied)?;
                    let observer = request.callback.map(|callback| {
                        Arc::new(ScriptCompletionObserver {
                            callback,
                            limits: limits.clone(),
                        }) as Arc<dyn waltr_agent::CompletionObserver>
                    });
                    let result = helpers
                        .run_with_inputs(
                            HelperRequest {
                                selection: args.selection,
                                task: args.task,
                                capture: args.capture,
                                max_turns: args.max_turns,
                                deadline: Duration::from_millis(args.deadline_ms),
                                observer,
                            },
                            source,
                            Some(crate::routine::ports::HelperPayload {
                                budget: waltr_agent::TransientInputBudget::process(),
                                attachments: Vec::new(),
                                capabilities: waltr_agent::TransientModelCapabilities {
                                    png: false,
                                    jpeg: false,
                                    webp: false,
                                    pdf: false,
                                    text: true,
                                    max_body_bytes: 65536,
                                },
                                response_bytes: args.response_bytes,
                                completion_bytes: args.completion_bytes,
                            }),
                            cancellation,
                        )
                        .await
                        .map_err(|_| waltr_script::Error::Cleanup)?;
                    json!({"status":"accepted","result":result})
                }
                Operation::Tool(args) => {
                    let call =
                        ToolCall::new(args.call_id, args.target.permission_name(), args.arguments);
                    match tools
                        .execute(
                            AuthorizedToolRequest {
                                target: args.target,
                                call,
                            },
                            cancellation,
                        )
                        .await
                    {
                        Ok(result) => json!({"status":"completed","result":result}),
                        Err(AuthorizedToolError::Denied) => json!({"status":"denied"}),
                        Err(AuthorizedToolError::Invalid) => {
                            return Err(waltr_script::Error::Invalid);
                        }
                        Err(AuthorizedToolError::Unavailable | AuthorizedToolError::Uncertain) => {
                            return Err(waltr_script::Error::Cleanup);
                        }
                    }
                }
                Operation::Schemas(args) => {
                    let schemas = tools
                        .schemas(args.targets, cancellation)
                        .await
                        .map_err(|_| waltr_script::Error::Denied)?;
                    json!({"status":"completed","schemas":schemas})
                }
            };
            Ok(HostReply::Value(StructuredValue::new(value, &limits)?))
        })
    }

    fn cleanup(&self, _: InvocationId) -> CleanupFuture {
        self.cancellation.cancel();
        let helpers = self.bindings.helpers.clone();
        let tools = self.bindings.tools.clone();
        let cleaned = self.cleaned.clone();
        let context = self.context.clone();
        let children = self.children.clone();
        Box::pin(async move {
            let (helper, tool, context, children) = tokio::join!(
                helpers.cleanup(),
                tools.cleanup(),
                async move {
                    match context {
                        Some(context) => context.cleanup().await,
                        None => Ok(()),
                    }
                },
                async move {
                    match children {
                        Some(children) => children.cleanup().await,
                        None => Ok(()),
                    }
                }
            );
            helper.map_err(|_| waltr_script::Error::Cleanup)?;
            tool.map_err(|_| waltr_script::Error::Cleanup)?;
            context.map_err(|_| waltr_script::Error::Cleanup)?;
            children.map_err(|_| waltr_script::Error::Cleanup)?;
            cleaned.store(true, Ordering::Release);
            Ok(())
        })
    }
}

struct JoinedReservation(Arc<AgentHost>);
impl Reservation for JoinedReservation {
    fn prepare(&self) -> PortFuture<()> {
        match &self.0.context {
            Some(context) => context.prepare(),
            None => Box::pin(async { Ok(()) }),
        }
    }
    fn abort(&self) -> PortFuture<()> {
        let host = self.0.clone();
        Box::pin(async move {
            host.cancellation.cancel();
            let (helper, tool, context, children) = tokio::join!(
                host.bindings.helpers.cleanup(),
                host.bindings.tools.cleanup(),
                async {
                    match &host.context {
                        Some(context) => context.cleanup().await,
                        None => Ok(()),
                    }
                },
                async {
                    match &host.children {
                        Some(children) => children.cleanup().await,
                        None => Ok(()),
                    }
                },
            );
            helper?;
            tool.map_err(|_| PortError::Uncertain)?;
            context?;
            children?;
            host.cleaned.store(true, Ordering::Release);
            Ok(())
        })
    }
    fn finalize(&self, _: bool) -> PortFuture<()> {
        // Finalization is reached only after the engine has joined HostDispatcher::cleanup.
        let host = self.0.clone();
        Box::pin(async move {
            if host.cleaned.load(Ordering::Acquire) {
                Ok(())
            } else {
                Err(PortError::Uncertain)
            }
        })
    }
}
