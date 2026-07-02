//! Agent tool abstractions.

use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{
    PendingTimelineEvent, Result, TimelineChannel, TimelineContent, TimelineEventKind,
    TimelineItemId, ToolCall, ToolDefinition, ToolResult,
};
use tokio_util::sync::CancellationToken;

use super::session::SessionId;

/// Host tool available to an agent loop.
#[derive(Clone)]
pub struct AgentTool {
    definition: ToolDefinition,
    handler: Arc<ToolHandler>,
}

/// Future returned by an asynchronous tool handler.
pub type ToolFuture = Pin<Box<dyn Future<Output = Result<ToolResult>> + Send>>;
type ToolHandler = dyn Fn(ToolExecutionContext) -> ToolFuture + Send + Sync;

/// Context supplied to an agent tool invocation.
#[derive(Clone, Debug)]
pub struct ToolExecutionContext {
    call: ToolCall,
    progress: ToolProgressSink,
    cancellation_token: CancellationToken,
    session_id: Option<SessionId>,
    turn: Option<u64>,
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
    pub const fn with_progress_and_cancellation(
        call: ToolCall,
        progress: ToolProgressSink,
        cancellation_token: CancellationToken,
    ) -> Self {
        Self {
            call,
            progress,
            cancellation_token,
            session_id: None,
            turn: None,
        }
    }

    /// Attach parent session metadata to this invocation.
    #[must_use]
    pub fn with_session_context(mut self, session_id: SessionId, turn: u64) -> Self {
        self.session_id = Some(session_id);
        self.turn = Some(turn);
        self
    }

    /// Return the provider tool call.
    #[must_use]
    pub const fn call(&self) -> &ToolCall {
        &self.call
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
        next_sequence: AtomicU64,
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
                next_sequence: AtomicU64::new(0),
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
                next_sequence,
                emit,
            } => emit(PendingTimelineEvent::tool_progress_info(
                session_id.clone(),
                *turn,
                progress_item_id(parent_id, next_sequence.fetch_add(1, Ordering::Relaxed)),
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
                next_sequence,
                emit,
            } => emit(PendingTimelineEvent::tool_progress_error(
                session_id.clone(),
                *turn,
                progress_item_id(parent_id, next_sequence.fetch_add(1, Ordering::Relaxed)),
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
                next_sequence,
                emit,
            } => {
                let mut event = PendingTimelineEvent::new(
                    session_id.clone(),
                    *turn,
                    progress_item_id(parent_id, next_sequence.fetch_add(1, Ordering::Relaxed)),
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
    /// Create a host tool from a definition and synchronous handler.
    pub fn new(
        definition: ToolDefinition,
        handler: impl Fn(ToolExecutionContext) -> Result<ToolResult> + Send + Sync + 'static,
    ) -> Self {
        Self {
            definition,
            handler: Arc::new(move |context| Box::pin(std::future::ready(handler(context)))),
        }
    }

    /// Create a host tool from a definition and asynchronous handler.
    pub fn new_async(
        definition: ToolDefinition,
        handler: impl Fn(ToolExecutionContext) -> ToolFuture + Send + Sync + 'static,
    ) -> Self {
        Self {
            definition,
            handler: Arc::new(handler),
        }
    }

    /// Return the provider-facing tool definition.
    #[must_use]
    pub const fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    pub(super) fn call(&self, context: ToolExecutionContext) -> ToolFuture {
        (self.handler)(context)
    }
}

fn progress_item_id(parent_id: &TimelineItemId, sequence: u64) -> TimelineItemId {
    TimelineItemId::new(format!("{}/progress/{sequence}", parent_id.as_str()))
}

impl fmt::Debug for AgentTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentTool")
            .field("definition", &self.definition)
            .finish_non_exhaustive()
    }
}
