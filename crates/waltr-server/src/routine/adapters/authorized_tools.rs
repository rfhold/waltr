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
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use waltr_agent::ToolCall;
use waltr_script::{
    CleanupFuture, HostContext, HostDispatcher, HostFuture, HostHandle, HostReply, HostRequest,
    InvocationId, Limits, StructuredValue,
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

pub(crate) trait RoutineMcpPort: Send + Sync {
    fn call(
        self: Arc<Self>,
        operation: String,
        request: McpArguments,
        cancellation: CancellationToken,
    ) -> PortFuture<waltr_agent::ToolResult>;
    fn results(&self) -> &ToolResults;
    fn cleanup(self: Arc<Self>) -> PortFuture<()>;
}

const TOOL_RESULT_HANDLE: &str = "tools.result";

pub(crate) struct ToolResults {
    pools: [super::media::ResultPool; 3],
    values: Mutex<Vec<(HostHandle, Arc<RetainedToolResult>)>>,
}

struct RetainedToolResult {
    value: waltr_agent::ToolResult,
    _permits: Vec<tokio::sync::OwnedSemaphorePermit>,
}

pub(crate) struct ToolResultImageRef {
    result: Arc<RetainedToolResult>,
    index: usize,
}

impl ToolResultImageRef {
    pub(crate) fn image(&self) -> Result<&waltr_agent::ToolResultImage, waltr_script::Error> {
        match self.result.value.content.blocks().get(self.index) {
            Some(waltr_agent::ToolResultContentBlock::Image(image)) => Ok(image),
            _ => Err(waltr_script::Error::Invalid),
        }
    }
}

impl ToolResults {
    pub(crate) fn new(
        principal: super::media::ResultPool,
        process: super::media::ResultPool,
    ) -> Self {
        Self {
            pools: [super::media::ResultPool::principal(), principal, process],
            values: Mutex::default(),
        }
    }

    pub(crate) fn clear(&self) -> Result<(), PortError> {
        self.values
            .lock()
            .map_err(|_| PortError::Uncertain)?
            .clear();
        Ok(())
    }

    fn store(
        &self,
        context: &HostContext,
        result: waltr_agent::ToolResult,
    ) -> Result<HostReply, waltr_script::Error> {
        use waltr_agent::ToolResultContentBlock::{Image, Text};
        let bytes = result.content.blocks().iter().fold(
            result
                .text_projection()
                .len()
                .saturating_add(std::mem::size_of_val(&result)),
            |bytes, block| {
                bytes
                    .saturating_add(std::mem::size_of_val(block))
                    .saturating_add(match block {
                        Image(image) => image
                            .data()
                            .len()
                            .saturating_add(image.media_type().len())
                            .saturating_add(image.sha256().len()),
                        Text { text } => text.len(),
                    })
            },
        );
        let bytes = u32::try_from(bytes).map_err(|_| waltr_script::Error::Limit)?;
        let mut values = self
            .values
            .lock()
            .map_err(|_| waltr_script::Error::Cleanup)?;
        if values.len() >= 32 {
            return Err(waltr_script::Error::Limit);
        }
        let permits = self
            .pools
            .iter()
            .map(|pool| pool.reserve(bytes))
            .collect::<Result<Vec<_>, _>>()?;
        let handle = context.issue_handle(TOOL_RESULT_HANDLE)?;
        values.push((
            handle.clone(),
            Arc::new(RetainedToolResult {
                value: result,
                _permits: permits,
            }),
        ));
        Ok(HostReply::Handle(handle))
    }

    fn get(
        &self,
        context: &HostContext,
        handle: &HostHandle,
    ) -> Result<Arc<RetainedToolResult>, waltr_script::Error> {
        context.validate_handle(handle, TOOL_RESULT_HANDLE)?;
        self.values
            .lock()
            .map_err(|_| waltr_script::Error::Cleanup)?
            .iter()
            .find(|(key, _)| key == handle)
            .map(|(_, value)| value.clone())
            .ok_or(waltr_script::Error::Identity)
    }

    pub(crate) fn read(
        &self,
        context: &HostContext,
        handle: &HostHandle,
        limits: &Limits,
    ) -> Result<HostReply, waltr_script::Error> {
        use waltr_agent::ToolResultContentBlock::{Image, Text};
        let result = self.get(context, handle)?;
        let result = &result.value;
        let mut remaining = limits.value_bytes;
        for block in result.content.blocks() {
            if let Text { text } = block {
                if text.len() > remaining || text.len() > limits.string_bytes {
                    return result_limit(limits);
                }
                remaining -= text.len();
            }
        }
        let blocks = result.content.blocks().iter().map(|block| match block {
            Text { text } => json!({"type":"text","text":text}),
            Image(image) => json!({"type":"image","mediaType":image.media_type(),"bytes":image.decoded_bytes()}),
        }).collect::<Vec<_>>();
        completed_mcp_value(
            json!({"status":"completed","isError":result.is_error,"content":blocks}),
            limits,
        )
    }

    pub(crate) fn image(
        &self,
        context: &HostContext,
        handle: &HostHandle,
        index: usize,
    ) -> Result<ToolResultImageRef, waltr_script::Error> {
        let result = self.get(context, handle)?;
        if result.value.is_error {
            return Err(waltr_script::Error::Invalid);
        }
        let index = result
            .value
            .content
            .blocks()
            .iter()
            .enumerate()
            .filter(|(_, block)| matches!(block, waltr_agent::ToolResultContentBlock::Image(_)))
            .nth(index)
            .map(|(index, _)| index)
            .ok_or(waltr_script::Error::Invalid)?;
        Ok(ToolResultImageRef { result, index })
    }
}

fn completed_mcp_value(value: Value, limits: &Limits) -> Result<HostReply, waltr_script::Error> {
    match StructuredValue::new(value, limits) {
        Err(waltr_script::Error::Value) => result_limit(limits),
        result => result.map(HostReply::Value),
    }
}

fn result_limit(limits: &Limits) -> Result<HostReply, waltr_script::Error> {
    StructuredValue::new(
        json!({"status":"completed","resultUnavailable":"limit"}),
        limits,
    )
    .map(HostReply::Value)
}

pub(crate) async fn execute_mcp(
    mcp: Arc<dyn RoutineMcpPort>,
    operation: String,
    args: McpArguments,
    context: &HostContext,
    limits: &Limits,
    cancellation: CancellationToken,
) -> Result<HostReply, waltr_script::Error> {
    let handle = args.result.is_some();
    let value = match mcp.clone().call(operation, args, cancellation).await {
        Ok(result) if handle => {
            return match mcp.results().store(context, result) {
                Err(waltr_script::Error::Limit) => result_limit(limits),
                reply => reply,
            };
        }
        Ok(result) => json!({"status":"completed","result":result}),
        Err(PortError::Denied) => json!({"status":"denied"}),
        Err(PortError::Invalid) => return Err(waltr_script::Error::Invalid),
        Err(_) => return Err(waltr_script::Error::Cleanup),
    };
    completed_mcp_value(value, limits)
}

pub(crate) fn tool_result_handle(
    request: &HostRequest,
) -> Result<&HostHandle, waltr_script::Error> {
    if request.callback.is_some()
        || request.bytes.is_some()
        || !request
            .arguments
            .clone()
            .into_json()
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
    {
        return Err(waltr_script::Error::Invalid);
    }
    request
        .handle
        .as_ref()
        .filter(|handle| handle.kind() == TOOL_RESULT_HANDLE)
        .ok_or(waltr_script::Error::Identity)
}

pub(crate) fn is_tool_result(request: &HostRequest) -> bool {
    request
        .handle
        .as_ref()
        .is_some_and(|handle| handle.kind() == TOOL_RESULT_HANDLE)
}

pub(crate) fn tool_image_attachment(
    image: &waltr_agent::ToolResultImage,
    capabilities: waltr_agent::TransientModelCapabilities,
    max_bytes: usize,
    budget: &waltr_agent::TransientInputBudget,
) -> Result<waltr_agent::TransientAttachment, waltr_script::Error> {
    use base64::Engine as _;
    use waltr_agent::TransientInputKind;
    let kind = match image.media_type() {
        "image/png" if capabilities.png => TransientInputKind::Png,
        "image/jpeg" if capabilities.jpeg => TransientInputKind::Jpeg,
        "image/webp" if capabilities.webp => TransientInputKind::WebP,
        _ => return Err(waltr_script::Error::Denied),
    };
    if image.decoded_bytes() > max_bytes as u64 {
        return Err(waltr_script::Error::Limit);
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(image.data())
        .map_err(|_| waltr_script::Error::Invalid)?;
    budget
        .attachment(kind, bytes)
        .map_err(|_| waltr_script::Error::Limit)
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
        self.preflight_mcp(binding, definition, selected, caller, None)
    }
}

impl RoutineAgentHostFactory {
    pub(crate) fn preflight_mcp(
        &self,
        binding: crate::routine::history::InvocationBinding,
        definition: Arc<Definition>,
        selected: SelectedEntrypoint,
        caller: Caller,
        mcp: Option<Arc<dyn RoutineMcpPort>>,
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
            if !definition.envelope().spec.mcp_tools.is_empty() && mcp.is_none() {
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
            let streams = super::tool_streams::ToolStreams::new(
                definition.envelope().spec.tool_streams.clone(),
            );
            if !definition.envelope().spec.tool_streams.is_empty() {
                bound.helpers.bind_tool_streams(streams.clone()).await?;
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
                mcp,
                mcp_targets: definition
                    .envelope()
                    .spec
                    .mcp_tools
                    .iter()
                    .cloned()
                    .collect(),
                streams,
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
            | "helper.streams"
            | "helper.validate"
            | "tools.execute"
            | "tools.schemas"
            | "tools.read"
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
    streams: Arc<super::tool_streams::ToolStreams>,
    children: Option<Arc<dyn ChildPort>>,
    mcp: Option<Arc<dyn RoutineMcpPort>>,
    mcp_targets: BTreeSet<ConcreteToolTarget>,
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
    pub workspace: Option<crate::routine::context::WorkspaceSelection>,
    #[serde(default)]
    pub inference: super::helper::InferenceSelector,
    #[serde(default)]
    pub input: super::helper::HelperInput,
    pub selection: HelperSelection,
    pub source: Option<String>,
    pub task: String,
    pub capture: Option<String>,
    pub max_turns: u64,
    pub deadline_ms: u64,
    pub response_bytes: usize,
    pub completion_bytes: usize,
    pub field: Option<String>,
    pub image_index: Option<usize>,
}
impl HelperArguments {
    pub(crate) fn validate(
        &self,
        capabilities: &BTreeSet<String>,
        callback: bool,
    ) -> Result<(), PortError> {
        self.input.validate()?;
        if self.workspace.is_some() && matches!(self.selection, HelperSelection::Restricted { .. })
        {
            return Err(PortError::Invalid);
        }
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
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct McpArguments {
    pub target: ConcreteToolTarget,
    pub arguments: serde_json::Map<String, Value>,
    pub workspace: Option<crate::routine::context::WorkspaceSelection>,
    pub result: Option<McpResultMode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum McpResultMode {
    Handle,
}

pub(crate) fn parse_mcp(
    request: &HostRequest,
    allowed: impl FnOnce(&ConcreteToolTarget) -> bool,
) -> Result<McpArguments, PortError> {
    if request.callback.is_some() || request.handle.is_some() || request.bytes.is_some() {
        return Err(PortError::Invalid);
    }
    let args: McpArguments = serde_json::from_value(request.arguments.clone().into_json())
        .map_err(|_| PortError::Invalid)?;
    if !allowed(&args.target) {
        return Err(PortError::Denied);
    }
    if args.workspace.as_ref().is_some_and(|workspace| {
        workspace.machine.is_empty()
            || workspace.machine.len() > 256
            || workspace.machine.chars().any(char::is_control)
            || workspace
                .working_directory
                .as_ref()
                .is_some_and(|directory| {
                    directory.len() > 4096 || directory.chars().any(char::is_control)
                })
    }) {
        return Err(PortError::Invalid);
    }
    Ok(args)
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
    Next(super::tool_streams::Next),
    Children(ChildArguments),
    Context(ContextOperation),
    Helper(HelperArguments),
    Tool(ToolArguments),
    Mcp(McpArguments),
    ReadTool,
    Schemas(SchemaArguments),
}

impl AgentHost {
    fn parse(&self, request: &HostRequest) -> Result<Operation, PortError> {
        if self.cancellation.is_cancelled()
            || request.bytes.is_some()
            || request.handle.is_some()
                && !(self.mcp.is_some()
                    && is_tool_result(request)
                    && matches!(request.operation.as_str(), "tools.read" | "helper.run"))
            || !self
                .capabilities
                .contains(if request.operation == "helper.next" {
                    "helper.streams"
                } else if request.operation == "tools.read" && self.mcp.is_some() {
                    "tools.execute"
                } else {
                    &request.operation
                })
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
            "helper.next" => {
                if request.callback.is_some() {
                    return Err(PortError::Invalid);
                }
                let args: super::tool_streams::Next =
                    serde_json::from_value(value).map_err(|_| PortError::Invalid)?;
                self.streams.check(&args.stream)?;
                Ok(Operation::Next(args))
            }
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
                    || args.image_index.is_some() && !is_tool_result(request)
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
            "tools.read" if self.mcp.is_some() => {
                tool_result_handle(request).map_err(|_| PortError::Invalid)?;
                Ok(Operation::ReadTool)
            }
            "tools.execute" => {
                if self.mcp.is_some() {
                    return parse_mcp(request, |target| self.mcp_targets.contains(target))
                        .map(Operation::Mcp);
                }
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
                Operation::Next(_) => json!({"operation":"helper.next"}),
                Operation::Context(operation) => json!({"operation":request.operation,
                    "context":self.context.as_ref().ok_or(PortError::Denied)?.intent(&operation)?}),
                Operation::Children(args) => {
                    json!({"operation":"children.call","count":args.calls.len()})
                }
                Operation::Helper(_) => json!({"operation":"helper.run"}),
                Operation::Tool(args) => json!({"target":args.target,"callId":args.call_id}),
                Operation::Mcp(args) => json!({"target":args.target}),
                Operation::ReadTool => json!({"operation":"tools.read"}),
                Operation::Schemas(args) => json!({"targets":args.targets}),
            })
        })();
        Box::pin(async move { result })
    }

    fn project_result(&self, name: &str, reply: &HostReply) -> Result<Option<Value>, PortError> {
        if !(matches!(
            name,
            "helper.run"
                | "helper.next"
                | "tools.execute"
                | "tools.schemas"
                | "children.call"
                | "tools.read"
        ) || name.starts_with("context.") && known_capability(name))
        {
            return Err(PortError::Denied);
        }
        if name == "tools.execute"
            && matches!(reply, HostReply::Handle(h) if h.kind() == TOOL_RESULT_HANDLE)
        {
            return Ok(Some(json!({"status":"completed"})));
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
    fn dispatch(&self, request: HostRequest, host_context: HostContext) -> HostFuture {
        let operation = self.parse(&request);
        let helpers = self.bindings.helpers.clone();
        let tools = self.bindings.tools.clone();
        let cancellation = self.cancellation.clone();
        let limits = self.limits.clone();
        let context = self.context.clone();
        let children = self.children.clone();
        let mcp = self.mcp.clone();
        let streams = self.streams.clone();
        Box::pin(async move {
            let value = match operation.map_err(|_| waltr_script::Error::Denied)? {
                Operation::Next(args) => {
                    let value = streams
                        .next(&args.stream, cancellation)
                        .await
                        .map_err(|_| waltr_script::Error::Cleanup)?;
                    json!({"status":"completed", "value":value})
                }
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
                    let image = request
                        .handle
                        .as_ref()
                        .map(|handle| {
                            mcp.as_ref()
                                .ok_or(waltr_script::Error::Denied)?
                                .results()
                                .image(&host_context, handle, args.image_index.unwrap_or(0))
                        })
                        .transpose()?;
                    let helpers = helpers
                        .select_inference(&args.inference)
                        .map_err(|_| waltr_script::Error::Invalid)?
                        .unwrap_or(helpers);
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
                    let budget = waltr_agent::TransientInputBudget::process();
                    let mut capabilities = waltr_agent::TransientModelCapabilities {
                        png: false,
                        jpeg: false,
                        webp: false,
                        pdf: false,
                        text: args.input.text,
                        max_body_bytes: args.input.max_bytes.min(65536),
                    };
                    let attachment = if let Some(image) = image {
                        capabilities = helpers
                            .input_capabilities(args.selection.clone())
                            .await
                            .map_err(|_| waltr_script::Error::Denied)?;
                        capabilities.text &= args.input.text;
                        helpers
                            .preflight_payload(
                                args.selection.clone(),
                                capabilities,
                                args.capture.clone(),
                            )
                            .await
                            .map_err(|_| waltr_script::Error::Denied)?;
                        Some(tool_image_attachment(
                            image.image()?,
                            capabilities,
                            args.input.max_bytes,
                            &budget,
                        )?)
                    } else {
                        None
                    };
                    let observer = request.callback.map(|callback| {
                        Arc::new(ScriptCompletionObserver {
                            callback,
                            limits: limits.clone(),
                        }) as Arc<dyn waltr_agent::CompletionObserver>
                    });
                    let result = helpers
                        .run_with_inputs(
                            HelperRequest {
                                workspace: args.workspace.map(Into::into),
                                selection: args.selection,
                                task: args.task,
                                capture: args.capture,
                                max_turns: args.max_turns,
                                deadline: Duration::from_millis(args.deadline_ms),
                                observer,
                            },
                            source,
                            Some(crate::routine::ports::HelperPayload {
                                budget,
                                attachments: attachment.into_iter().collect(),
                                capabilities,
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
                Operation::Mcp(args) => {
                    let operation = crate::routine::service::committed_operation(&host_context)
                        .map_err(|_| waltr_script::Error::Denied)?;
                    return execute_mcp(
                        mcp.ok_or(waltr_script::Error::Denied)?,
                        operation,
                        args,
                        &host_context,
                        &limits,
                        cancellation,
                    )
                    .await;
                }
                Operation::ReadTool => {
                    return mcp.ok_or(waltr_script::Error::Denied)?.results().read(
                        &host_context,
                        tool_result_handle(&request)?,
                        &limits,
                    );
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
        self.streams.cancel();
        self.cancellation.cancel();
        let helpers = self.bindings.helpers.clone();
        let tools = self.bindings.tools.clone();
        let cleaned = self.cleaned.clone();
        let context = self.context.clone();
        let children = self.children.clone();
        let mcp = self.mcp.clone();
        Box::pin(async move {
            let (helper, tool, context, children, mcp) = tokio::join!(
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
                },
                cleanup_mcp(mcp)
            );
            helper.map_err(|_| waltr_script::Error::Cleanup)?;
            tool.map_err(|_| waltr_script::Error::Cleanup)?;
            context.map_err(|_| waltr_script::Error::Cleanup)?;
            children.map_err(|_| waltr_script::Error::Cleanup)?;
            mcp.map_err(|_| waltr_script::Error::Cleanup)?;
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
            host.streams.cancel();
            host.cancellation.cancel();
            let (helper, tool, context, children, mcp) = tokio::join!(
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
                cleanup_mcp(host.mcp.clone()),
            );
            helper?;
            tool.map_err(|_| PortError::Uncertain)?;
            context?;
            children?;
            mcp?;
            host.cleaned.store(true, Ordering::Release);
            Ok(())
        })
    }
    fn validate_success(&self) -> Result<(), PortError> {
        if self.0.streams.was_drained() {
            Ok(())
        } else {
            Err(PortError::Invalid)
        }
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

async fn cleanup_mcp(mcp: Option<Arc<dyn RoutineMcpPort>>) -> Result<(), PortError> {
    match mcp {
        Some(mcp) => mcp.cleanup().await,
        None => Ok(()),
    }
}
