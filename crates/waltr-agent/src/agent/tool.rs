//! Agent tool abstractions.

use std::{fmt, future::Future, pin::Pin, sync::Arc};

use crate::{
    PendingTimelineEvent, PendingTimelineItemId, Result, TimelineChannel, TimelineContent,
    TimelineEventKind, TimelineItemId, ToolCall, ToolDefinition, ToolResult,
};
use tokio_util::sync::CancellationToken;

use super::session::SessionId;

/// Host tool available to an agent loop.
#[derive(Clone)]
pub struct AgentTool {
    pub(super) execution_guard: Option<Arc<HostExecutionGuard>>,
    pub(crate) host_dispatch: Option<Arc<HostDispatchPreparation>>,
    definition: ToolDefinition,
    identity: Option<String>,
    handler: Arc<ToolHandler>,
    pub(crate) model_exposure: bool,
}

/// Future returned by an asynchronous tool handler.
/// For fenced calls, `Err` means the outcome is uncertain and forbids replay.
/// Return an error-valued `ToolResult` for known failures, including pre-effect denial.
pub type ToolFuture = Pin<Box<dyn Future<Output = Result<ToolResult>> + Send>>;
type ToolHandler = dyn Fn(ToolExecutionContext) -> ToolFuture + Send + Sync;
/// Host execution authority check without host-specific error categories.
pub type ToolGuardFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
pub(super) type HostExecutionGuard = dyn Fn(CancellationToken) -> ToolGuardFuture + Send + Sync;

/// Host-selected immutable association, prepared without executing effects.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostDispatchBinding {
    /// Reserved host execution identity.
    pub invocation_id: String,
    /// Exact immutable source/configuration revision.
    pub source_revision: String,
}

type HostDispatchPreparation = dyn Fn(&ToolCall) -> Result<HostDispatchBinding> + Send + Sync;

pub(super) fn host_dispatch_unsettled(record: &crate::SessionRecord) -> bool {
    let mut dispatches = std::collections::BTreeMap::new();
    for event in &record.journal {
        if let crate::SessionJournalEvent::MiddlewareStateUpdated { namespace, state } = event
            && namespace == "host-dispatch"
        {
            let (Some(id), Some(active), Some(call)) = (
                state
                    .get("invocationId")
                    .and_then(serde_json::Value::as_str),
                state.get("active").and_then(serde_json::Value::as_bool),
                state.get("callId").and_then(serde_json::Value::as_str),
            ) else {
                return true;
            };
            dispatches.insert(id, (active, call));
        }
    }
    let snapshot = record.snapshot();
    dispatches.values().any(|(active, call)| {
        *active
            || !(snapshot.middleware_state.get(super::host_effect::RECEIPTS)
                .and_then(|receipts| receipts.get(*call)).is_some()
                || snapshot.turns.iter().flat_map(|turn| &turn.items).any(
                |item| matches!(item,crate::TurnItem::ToolResult(result)if &result.call_id==call),
            ))
    })
}

/// Context supplied to an agent tool invocation.
#[derive(Debug, Clone)]
pub struct ToolExecutionContext {
    execution_guard: Option<ExecutionGuard>,
    pub(crate) host_dispatch: Option<HostDispatchBinding>,
    pub(crate) session_incarnation: Option<String>,
    call: ToolCall,
    progress: ToolProgressSink,
    cancellation_token: CancellationToken,
    session_id: Option<SessionId>,
    turn: Option<u64>,
    pub(crate) result_metadata: ToolResultMetadata,
}

#[derive(Clone)]
struct ExecutionGuard(Arc<HostExecutionGuard>);

impl fmt::Debug for ExecutionGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ExecutionGuard")
    }
}

/// Invocation-local host metadata, published only with an unmodified tool result.
#[derive(Clone, Default)]
pub struct ToolResultMetadata {
    metadata: Arc<std::sync::Mutex<serde_json::Map<String, serde_json::Value>>>,
    state: Arc<std::sync::Mutex<std::collections::BTreeMap<String, serde_json::Value>>>,
    writes: Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
    effective_call: Arc<std::sync::Mutex<Option<ToolCall>>>,
}

impl ToolResultMetadata {
    pub(crate) fn take_effective_call(&self) -> Option<ToolCall> {
        self.effective_call
            .lock()
            .expect("tool call lock poisoned")
            .take()
    }
    /// Attach host presentation data; never put credentials or dispatch authority here.
    pub fn insert(&self, key: impl Into<String>, value: serde_json::Value) {
        self.metadata
            .lock()
            .expect("tool metadata lock poisoned")
            .insert(key.into(), value);
    }

    pub(super) fn take(&self) -> serde_json::Map<String, serde_json::Value> {
        std::mem::take(&mut *self.metadata.lock().expect("tool metadata lock poisoned"))
    }

    pub(crate) fn hydrate_state(
        &self,
        state: std::collections::BTreeMap<String, serde_json::Value>,
    ) {
        *self.state.lock().expect("host tool state lock poisoned") = state;
    }

    pub(crate) fn take_state(&self) -> Vec<(String, serde_json::Value)> {
        std::mem::take(&mut *self.writes.lock().expect("host tool state lock poisoned"))
    }

    /// Host context snapshots never include the generic engine's reserved execution namespaces.
    pub fn context_snapshot(&self) -> std::collections::BTreeMap<String, serde_json::Value> {
        self.state
            .lock()
            .expect("host tool state lock poisoned")
            .clone()
    }

    /// Context writes commit with the completed invocation, separately from presentation metadata.
    pub fn set_context(&self, namespace: String, value: serde_json::Value) -> Result<()> {
        if super::host::reserved_context_namespace(&namespace) {
            return Err(crate::LlmError::Protocol {
                provider: "host-context",
                message: "reserved execution namespace".into(),
            });
        }
        self.state
            .lock()
            .expect("host tool state lock poisoned")
            .insert(namespace.clone(), value.clone());
        self.writes
            .lock()
            .expect("host tool state lock poisoned")
            .push((namespace, value));
        Ok(())
    }
}

impl fmt::Debug for ToolResultMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolResultMetadata")
            .finish_non_exhaustive()
    }
}

impl ToolExecutionContext {
    /// Create a context with no progress receiver.
    #[must_use]
    pub fn new(call: ToolCall) -> Self {
        Self::with_progress(call, ToolProgressSink::noop())
    }

    /// Create a context with a progress receiver.
    #[must_use]
    pub fn with_progress(call: ToolCall, progress: ToolProgressSink) -> Self {
        Self::with_progress_and_cancellation(call, progress, CancellationToken::new())
    }

    /// Create a context with progress and cancellation receivers.
    #[must_use]
    pub fn with_progress_and_cancellation(
        call: ToolCall,
        progress: ToolProgressSink,
        cancellation_token: CancellationToken,
    ) -> Self {
        Self {
            execution_guard: None,
            host_dispatch: None,
            session_incarnation: None,
            call,
            progress,
            cancellation_token,
            session_id: None,
            turn: None,
            result_metadata: ToolResultMetadata::default(),
        }
    }

    /// Attach parent session metadata to this invocation.
    #[must_use]
    pub fn with_session_context(mut self, session_id: SessionId, turn: u64) -> Self {
        self.session_id = Some(session_id);
        self.turn = Some(turn);
        self
    }

    /// Bind an admitted host-owned operation. Hosts must fence durable work before dispatch.
    #[must_use]
    pub fn with_host_operation(mut self, binding: HostDispatchBinding) -> Self {
        self.host_dispatch = Some(binding);
        self
    }

    /// Metadata sink associated with this invocation's final result.
    pub fn result_metadata(&self) -> &ToolResultMetadata {
        &self.result_metadata
    }

    /// Recheck the original host authority after an adapter has awaited consent or preparation.
    pub async fn check_execution(&self) -> Result<()> {
        if let Some(guard) = &self.execution_guard {
            (guard.0)(self.cancellation_token.clone()).await?;
        }
        Ok(())
    }

    /// Return the provider tool call.
    #[must_use]
    pub const fn call(&self) -> &ToolCall {
        &self.call
    }

    /// A trusted host adapter may normalize input without changing the fenced call identity.
    pub fn replace_arguments(&mut self, arguments: serde_json::Value) {
        self.call.arguments = arguments;
        *self
            .result_metadata
            .effective_call
            .lock()
            .expect("tool call lock poisoned") = Some(self.call.clone());
    }

    /// Durable association committed by the agent before polling this tool.
    #[must_use]
    pub fn host_dispatch(&self) -> Option<&HostDispatchBinding> {
        self.host_dispatch.as_ref()
    }

    /// Durable record incarnation, not an in-memory runtime generation.
    #[must_use]
    pub fn session_incarnation(&self) -> Option<&str> {
        self.session_incarnation.as_deref()
    }

    /// Return the progress sink for this invocation.
    #[must_use]
    pub const fn progress(&self) -> &ToolProgressSink {
        &self.progress
    }

    /// Return the cancellation token for this invocation.
    #[must_use]
    pub const fn cancellation_token(&self) -> &CancellationToken {
        &self.cancellation_token
    }

    /// Return the durable parent session id, when this tool runs inside one.
    #[must_use]
    pub const fn session_id(&self) -> Option<&SessionId> {
        self.session_id.as_ref()
    }

    /// Return the parent provider turn, when this tool runs inside a session.
    #[must_use]
    pub const fn turn(&self) -> Option<u64> {
        self.turn
    }

    /// Return whether this invocation has been canceled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancellation_token.is_cancelled()
    }

    /// Create the provider-facing final result for this call.
    #[must_use]
    pub fn result(&self, content: impl Into<String>) -> ToolResult {
        ToolResult::new(self.call.id.clone(), self.call.name.clone(), content)
    }
}

type ToolProgressEmitter = dyn Fn(PendingTimelineEvent) -> Result<()> + Send + Sync;

/// Sink for generic timeline progress emitted while a tool runs.
#[derive(Clone)]
pub struct ToolProgressSink {
    inner: Arc<ToolProgressSinkInner>,
}

enum ToolProgressSinkInner {
    Noop,
    Timeline {
        session_id: SessionId,
        turn: u64,
        parent_id: TimelineItemId,
        emit: Arc<ToolProgressEmitter>,
    },
}

impl ToolProgressSink {
    /// Create a sink that discards progress.
    #[must_use]
    pub fn noop() -> Self {
        Self {
            inner: Arc::new(ToolProgressSinkInner::Noop),
        }
    }

    /// Create a sink that converts progress into pending timeline events.
    #[must_use]
    pub fn timeline(
        session_id: impl Into<SessionId>,
        turn: u64,
        parent_id: impl Into<TimelineItemId>,
        emit: impl Fn(PendingTimelineEvent) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self {
            inner: Arc::new(ToolProgressSinkInner::Timeline {
                session_id: session_id.into(),
                turn,
                parent_id: parent_id.into(),
                emit: Arc::new(emit),
            }),
        }
    }

    /// Emit informational tool progress.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying timeline sink rejects the progress event.
    pub fn info(&self, delta: impl Into<String>) -> Result<()> {
        match &*self.inner {
            ToolProgressSinkInner::Noop => Ok(()),
            ToolProgressSinkInner::Timeline {
                session_id,
                turn,
                parent_id,
                emit,
            } => emit(PendingTimelineEvent::tool_progress_info(
                session_id.clone(),
                *turn,
                PendingTimelineItemId::New,
                parent_id.clone(),
                delta,
            )),
        }
    }

    /// Emit error tool progress.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying timeline sink rejects the progress event.
    pub fn error(&self, delta: impl Into<String>) -> Result<()> {
        match &*self.inner {
            ToolProgressSinkInner::Noop => Ok(()),
            ToolProgressSinkInner::Timeline {
                session_id,
                turn,
                parent_id,
                emit,
            } => emit(PendingTimelineEvent::tool_progress_error(
                session_id.clone(),
                *turn,
                PendingTimelineItemId::New,
                parent_id.clone(),
                delta,
            )),
        }
    }

    /// Emit a generic tool progress event.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying timeline sink rejects the progress event.
    pub fn event(
        &self,
        channel: impl Into<TimelineChannel>,
        content: TimelineContent,
    ) -> Result<()> {
        match &*self.inner {
            ToolProgressSinkInner::Noop => Ok(()),
            ToolProgressSinkInner::Timeline {
                session_id,
                turn,
                parent_id,
                emit,
            } => {
                let mut event = PendingTimelineEvent::new(
                    session_id.clone(),
                    *turn,
                    PendingTimelineItemId::New,
                    TimelineEventKind::ContentDelta,
                    Some(channel.into()),
                    content,
                );
                event.parent_id = Some(parent_id.clone());
                emit(event)
            }
        }
    }
}

impl Default for ToolProgressSink {
    fn default() -> Self {
        Self::noop()
    }
}

impl fmt::Debug for ToolProgressSink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &*self.inner {
            ToolProgressSinkInner::Noop => formatter.write_str("ToolProgressSink::Noop"),
            ToolProgressSinkInner::Timeline {
                session_id,
                turn,
                parent_id,
                ..
            } => formatter
                .debug_struct("ToolProgressSink::Timeline")
                .field("session_id", session_id)
                .field("turn", turn)
                .field("parent_id", parent_id)
                .finish_non_exhaustive(),
        }
    }
}

impl AgentTool {
    /// Recheck host authority without exposing its implementation or policy types.
    pub fn check_execution(&self, cancellation: CancellationToken) -> ToolGuardFuture {
        self.execution_guard.as_ref().map_or_else(
            || Box::pin(async { Ok(()) }) as ToolGuardFuture,
            |guard| guard(cancellation),
        )
    }
    /// Create a host tool from a definition and synchronous handler.
    pub fn new(
        definition: ToolDefinition,
        handler: impl Fn(ToolExecutionContext) -> Result<ToolResult> + Send + Sync + 'static,
    ) -> Self {
        Self {
            host_dispatch: None,
            definition,
            identity: None,
            execution_guard: None,
            handler: Arc::new(move |context| Box::pin(std::future::ready(handler(context)))),
            model_exposure: true,
        }
    }

    /// Create a host tool from a definition and asynchronous handler.
    pub fn new_async(
        definition: ToolDefinition,
        handler: impl Fn(ToolExecutionContext) -> ToolFuture + Send + Sync + 'static,
    ) -> Self {
        Self {
            host_dispatch: None,
            definition,
            identity: None,
            execution_guard: None,
            handler: Arc::new(handler),
            model_exposure: true,
        }
    }

    /// Return the provider-facing tool definition.
    #[must_use]
    pub const fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    /// Opaque host identity for evidence, never an execution grant or model alias.
    #[must_use]
    pub fn with_identity(mut self, identity: impl Into<String>) -> Self {
        self.identity = Some(identity.into());
        self
    }

    /// Return the host-selected evidence identity.
    pub fn identity(&self) -> Option<&str> {
        self.identity.as_deref()
    }

    /// Require a durable non-replay association before invoking the handler.
    /// Preparation must be effect-free. Interrupted dispatches refuse automatic resume.
    #[must_use]
    pub fn with_host_dispatch(
        mut self,
        prepare: impl Fn(&ToolCall) -> Result<HostDispatchBinding> + Send + Sync + 'static,
    ) -> Self {
        self.host_dispatch = Some(Arc::new(prepare));
        self
    }

    /// Revalidate host-owned execution authority without exposing host policy types.
    #[must_use]
    pub fn with_execution_guard(
        mut self,
        guard: impl Fn(CancellationToken) -> ToolGuardFuture + Send + Sync + 'static,
    ) -> Self {
        self.execution_guard = Some(Arc::new(guard));
        self
    }

    /// Select whether this host tool is advertised to the model.
    #[must_use]
    pub fn with_model_exposure(mut self, exposed: bool) -> Self {
        self.model_exposure = exposed;
        self
    }

    pub fn call(&self, context: ToolExecutionContext) -> ToolFuture {
        if self.host_dispatch.is_some() && context.host_dispatch.is_none() {
            return Box::pin(async {
                Err(crate::LlmError::Protocol {
                    provider: "host-dispatch",
                    message: "durable provider tool dispatch required".into(),
                })
            });
        }
        let Some(guard) = self.execution_guard.clone() else {
            return (self.handler)(context);
        };
        let handler = self.handler.clone();
        Box::pin(async move {
            let mut context = context;
            context.execution_guard = Some(ExecutionGuard(guard.clone()));
            guard(context.cancellation_token().clone())
                .await
                .map_err(|_| crate::LlmError::Protocol {
                    provider: "host-guard",
                    message: "host execution authority ended".into(),
                })?;
            handler(context).await
        })
    }

    /// Whether the declaration is intended for model-visible discovery.
    pub const fn is_model_exposed(&self) -> bool {
        self.model_exposure
    }

    /// Whether calls require a durable host association.
    pub fn requires_host_dispatch(&self) -> bool {
        self.host_dispatch.is_some()
    }

    /// Effect-free association preparation for host-owned tool adapters.
    pub fn prepare_host_dispatch(&self, call: &ToolCall) -> Result<Option<HostDispatchBinding>> {
        self.host_dispatch
            .as_ref()
            .map(|prepare| prepare(call))
            .transpose()
    }
}

impl fmt::Debug for AgentTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentTool")
            .field("definition", &self.definition)
            .finish_non_exhaustive()
    }
}
