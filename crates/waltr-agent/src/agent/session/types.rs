//! Durable session model types.

#![allow(missing_docs)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    CompletionTurn, ModelRef, ProviderTurnMetadata, ThinkingBlock, ToolCall, ToolResult, TurnItem,
};

use super::super::{AgentState, AgentTranscriptEvent};

/// Durable session identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(String);

impl SessionId {
    /// Generate a new durable session identifier.
    #[must_use]
    pub fn generate() -> Self {
        Self(format!("session_{}", uuid::Uuid::now_v7()))
    }

    /// Create a session identifier.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the session identifier string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for SessionId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for SessionId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// Optimistic storage revision.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Revision(u64);

impl Revision {
    /// Initial revision for a newly created session.
    pub const ZERO: Self = Self(0);

    /// Create a revision from its raw value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the raw revision value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Return the next revision.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// Provider request identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestId(String);

impl RequestId {
    /// Create a provider request identifier.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the provider request identifier string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for RequestId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for RequestId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// External request identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExternalRequestId(String);

impl ExternalRequestId {
    /// Create an external request identifier.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the external request identifier string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ExternalRequestId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ExternalRequestId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// In-memory view of a durable agent session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSession {
    /// Complete persisted session record.
    pub record: SessionRecord,
}

impl AgentSession {
    /// Create an agent session from a stored record.
    #[must_use]
    pub const fn new(record: SessionRecord) -> Self {
        Self { record }
    }

    /// Return the session id.
    #[must_use]
    pub const fn id(&self) -> &SessionId {
        &self.record.id
    }

    /// Return the current revision.
    #[must_use]
    pub const fn revision(&self) -> Revision {
        self.record.revision
    }
}

/// Complete stored session record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Session identity.
    pub id: SessionId,
    /// Current optimistic revision.
    pub revision: Revision,
    /// Latest compact session snapshot.
    pub snapshot: SessionSnapshot,
    /// Ordered event journal entries not necessarily compacted into the snapshot.
    pub events: Vec<SessionEvent>,
    /// Provider requests recorded before dispatch.
    pub requests: Vec<RequestRecord>,
}

impl SessionRecord {
    /// Create a session record with an empty journal.
    pub fn new(id: impl Into<SessionId>) -> Self {
        let id = id.into();
        Self {
            snapshot: SessionSnapshot::new(id.clone()),
            id,
            revision: Revision::ZERO,
            events: Vec::new(),
            requests: Vec::new(),
        }
    }

    /// Apply and append one durable event to this record.
    pub fn apply_event(&mut self, event: SessionEvent) {
        if let SessionEvent::ProviderRequestRecorded { request } = &event {
            self.requests.push(request.clone());
        }
        self.snapshot.apply_event(&event);
        self.events.push(event);
    }

    /// Apply and append durable events to this record in order.
    pub fn apply_events(&mut self, events: impl IntoIterator<Item = SessionEvent>) {
        for event in events {
            self.apply_event(event);
        }
    }
}

/// Compact session state used to resume execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// Session identity.
    pub id: SessionId,
    /// Revision this snapshot represents.
    pub revision: Revision,
    /// User prompts or conversation inputs recorded for replay.
    pub prompts: Vec<String>,
    /// Most recent user prompt or conversation input.
    pub current_prompt: Option<String>,
    /// Next provider turn number.
    pub turn: u64,
    /// Provider-neutral transcript state.
    pub state: AgentState,
    /// Completed turns accumulated for provider request replay, in turn order.
    pub turns: Vec<CompletionTurn>,
    /// Runtime status captured at the checkpoint.
    pub status: SessionStatus,
    /// Pending, canceled, finished, and failed work tracked for deterministic resume.
    pub pending_work: Vec<PendingWork>,
    /// External requests created by suspensions.
    pub external_requests: Vec<ExternalRequest>,
    /// Namespaced durable middleware state blobs.
    #[serde(default)]
    pub middleware_state: BTreeMap<String, Value>,
}

impl SessionSnapshot {
    /// Create an empty session snapshot.
    #[must_use]
    pub fn new(id: SessionId) -> Self {
        Self {
            id,
            revision: Revision::ZERO,
            prompts: Vec::new(),
            current_prompt: None,
            turn: 0,
            state: AgentState::default(),
            turns: Vec::new(),
            status: SessionStatus::Idle,
            pending_work: Vec::new(),
            external_requests: Vec::new(),
            middleware_state: BTreeMap::new(),
        }
    }

    /// Tool calls accumulated across all turns, in emission order.
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.turns.iter().flat_map(CompletionTurn::tool_calls)
    }

    /// Tool results accumulated across all turns, in emission order.
    pub fn tool_results(&self) -> impl Iterator<Item = &ToolResult> {
        self.turns.iter().flat_map(CompletionTurn::tool_results)
    }

    /// Thinking blocks accumulated across all turns, in emission order.
    pub fn thinking_blocks(&self) -> impl Iterator<Item = &ThinkingBlock> {
        self.turns.iter().flat_map(CompletionTurn::thinking_blocks)
    }

    /// Apply one durable event to this snapshot.
    pub fn apply_event(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::PromptAppended { prompt } => {
                self.prompts.push(prompt.clone());
                self.current_prompt = Some(prompt.clone());
                self.status = SessionStatus::BuildingRequest;
            }
            SessionEvent::ProviderRequestRecorded { .. } => {
                self.status = SessionStatus::StreamingProvider;
            }
            SessionEvent::ProviderTextDelta { turn, delta }
            | SessionEvent::TextDelta { turn, delta } => {
                self.state.events.push(AgentTranscriptEvent::Text {
                    turn: turn_index(*turn),
                    text: delta.clone(),
                });
                self.status = SessionStatus::StreamingProvider;
            }
            SessionEvent::ProviderThinkingDelta { turn, delta }
            | SessionEvent::ThinkingDelta { turn, delta } => {
                self.state.events.push(AgentTranscriptEvent::Thinking {
                    turn: turn_index(*turn),
                    text: delta.clone(),
                });
                self.status = SessionStatus::StreamingProvider;
            }
            SessionEvent::ThinkingBlockRecorded { turn, block } => {
                CompletionTurn::entry(&mut self.turns, *turn)
                    .items
                    .push(TurnItem::Thinking(block.clone()));
                self.status = SessionStatus::StreamingProvider;
            }
            SessionEvent::ProviderTurnMetadataRecorded { turn, metadata } => {
                CompletionTurn::entry(&mut self.turns, *turn).metadata = Some(metadata.clone());
                self.status = SessionStatus::StreamingProvider;
            }
            SessionEvent::ToolCallRecorded { turn, call }
            | SessionEvent::ToolCall { turn, call } => self.record_tool_call(*turn, call),
            SessionEvent::ToolResultRecorded { turn, result }
            | SessionEvent::ToolResult { turn, result } => self.record_tool_result(*turn, result),
            SessionEvent::MiddlewareStateUpdated { namespace, state } => {
                self.middleware_state
                    .insert(namespace.clone(), state.clone());
            }
            SessionEvent::ExternalRequestCreated { request } => {
                upsert_external_request(&mut self.external_requests, request.clone());
                self.status = SessionStatus::Suspended(Suspension::AwaitingExternalInput {
                    request: Box::new(request.clone()),
                });
            }
            SessionEvent::ExternalRequestResponded {
                request_id,
                response,
            } => self.respond_external_request(request_id, response),
            SessionEvent::ExternalRequestCanceled { request_id, reason } => {
                self.cancel_external_request(request_id, reason);
            }
            SessionEvent::Suspended { suspension } => {
                self.status = SessionStatus::Suspended(suspension.clone());
            }
            SessionEvent::WorkCanceled { work_id, reason } => self.cancel_work(work_id, reason),
            SessionEvent::Interrupted => {
                self.status = SessionStatus::Suspended(Suspension::Interrupted {
                    work: self.pending_work.clone(),
                });
            }
            SessionEvent::TurnFinished { turn } => {
                self.turn = self.turn.max(turn + 1);
                self.status = SessionStatus::Idle;
            }
            SessionEvent::Finished => {
                self.status = SessionStatus::Finished;
            }
            SessionEvent::Failed { message } => {
                self.status = SessionStatus::Failed {
                    message: message.clone(),
                };
            }
        }
    }

    fn record_tool_call(&mut self, turn: u64, call: &ToolCall) {
        self.state.events.push(AgentTranscriptEvent::ToolCall {
            turn: turn_index(turn),
            call: call.clone(),
        });
        CompletionTurn::entry(&mut self.turns, turn)
            .items
            .push(TurnItem::ToolCall(call.clone()));
        self.pending_work.push(PendingWork {
            id: call.id.clone(),
            turn,
            tool_call: Some(call.clone()),
            status: PendingWorkStatus::Pending,
        });
        self.status = SessionStatus::EvaluatingToolCall;
    }

    fn record_tool_result(&mut self, turn: u64, result: &ToolResult) {
        self.state.events.push(AgentTranscriptEvent::ToolResult {
            turn: turn_index(turn),
            result: result.clone(),
        });
        CompletionTurn::entry(&mut self.turns, turn)
            .items
            .push(TurnItem::ToolResult(result.clone()));
        if let Some(work) = self
            .pending_work
            .iter_mut()
            .rev()
            .find(|work| work.id == result.call_id)
        {
            work.status = PendingWorkStatus::Finished(result.clone());
        }
        self.status = SessionStatus::RunningTool;
    }

    fn respond_external_request(
        &mut self,
        request_id: &ExternalRequestId,
        response: &ExternalResponse,
    ) {
        if let Some(request) = self
            .external_requests
            .iter_mut()
            .rev()
            .find(|request| request.id == *request_id)
        {
            request.status = ExternalRequestStatus::Responded(response.clone());
        }
        self.status = SessionStatus::RunningTool;
    }

    fn cancel_external_request(&mut self, request_id: &ExternalRequestId, reason: &CancelReason) {
        if let Some(request) = self
            .external_requests
            .iter_mut()
            .rev()
            .find(|request| request.id == *request_id)
        {
            request.status = ExternalRequestStatus::Canceled(reason.clone());
        }
    }

    fn cancel_work(&mut self, work_id: &str, reason: &CancelReason) {
        if let Some(work) = self
            .pending_work
            .iter_mut()
            .find(|work| work.id == *work_id)
        {
            work.status = PendingWorkStatus::Canceled(reason.clone());
        }
    }

    /// Apply durable events to this snapshot in order.
    pub fn apply_events<'a>(&mut self, events: impl IntoIterator<Item = &'a SessionEvent>) {
        for event in events {
            self.apply_event(event);
        }
    }
}

/// Runtime status persisted with a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    /// Session is ready for new input.
    Idle,
    /// A provider request is being assembled.
    BuildingRequest,
    /// Provider streaming is in progress.
    StreamingProvider,
    /// Tool call middleware is being evaluated.
    EvaluatingToolCall,
    /// A local tool executor is running.
    RunningTool,
    /// Runtime is waiting for external input or pending work.
    Suspended(Suspension),
    /// Session completed normally.
    Finished,
    /// Session failed with a durable error message.
    Failed { message: String },
}

/// Ordered durable session journal event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionEvent {
    /// User prompt was appended to the conversation.
    PromptAppended { prompt: String },
    /// Provider request was recorded before dispatch.
    ProviderRequestRecorded { request: RequestRecord },
    /// Provider emitted assistant text.
    ProviderTextDelta { turn: u64, delta: String },
    /// Provider emitted thinking text.
    ProviderThinkingDelta { turn: u64, delta: String },
    /// Provider emitted a replayable thinking block.
    ThinkingBlockRecorded { turn: u64, block: ThinkingBlock },
    /// Provider surfaced response metadata for the turn.
    ProviderTurnMetadataRecorded {
        turn: u64,
        metadata: ProviderTurnMetadata,
    },
    /// Provider requested a tool call.
    ToolCallRecorded { turn: u64, call: ToolCall },
    /// Tool result was appended.
    ToolResultRecorded { turn: u64, result: ToolResult },
    /// Provider turn finished.
    TurnFinished { turn: u64 },
    /// Provider emitted assistant text.
    TextDelta { turn: u64, delta: String },
    /// Provider emitted thinking text.
    ThinkingDelta { turn: u64, delta: String },
    /// Provider requested a tool call.
    ToolCall { turn: u64, call: ToolCall },
    /// Tool result was appended.
    ToolResult { turn: u64, result: ToolResult },
    /// Middleware namespace state was replaced wholesale.
    MiddlewareStateUpdated { namespace: String, state: Value },
    /// External request was recorded before suspension was emitted.
    ExternalRequestCreated { request: ExternalRequest },
    /// External request received a durable response.
    ExternalRequestResponded {
        request_id: ExternalRequestId,
        response: ExternalResponse,
    },
    /// External request was canceled before a response was applied.
    ExternalRequestCanceled {
        request_id: ExternalRequestId,
        reason: CancelReason,
    },
    /// Runtime suspended.
    Suspended { suspension: Suspension },
    /// Pending work was canceled.
    WorkCanceled {
        work_id: String,
        reason: CancelReason,
    },
    /// Runtime was interrupted.
    Interrupted,
    /// Session completed normally.
    Finished,
    /// Session failed.
    Failed { message: String },
}

/// Input used to resume or interrupt a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResumeInput {
    /// Resume with a new user prompt.
    Prompt(String),
    /// Resume with externally completed tool results.
    ToolResults(Vec<ToolResult>),
    /// Resume with an answer to a pending external request.
    ExternalResponse {
        request_id: ExternalRequestId,
        response: ExternalResponse,
    },
    /// Request interruption of active work.
    Interrupt,
}

/// Durable runtime suspension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Suspension {
    /// Runtime is waiting for tool results.
    AwaitingToolResults { work: Vec<PendingWork> },
    /// Runtime is waiting for an external response.
    AwaitingExternalInput { request: Box<ExternalRequest> },
    /// Runtime was interrupted while work was active.
    Interrupted { work: Vec<PendingWork> },
}

/// External request emitted by middleware or a suspending tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalRequest {
    /// External request identity.
    pub id: ExternalRequestId,
    /// Request kind used by hosts to route UI/approval behavior.
    pub kind: ExternalRequestKind,
    /// Tool call this request belongs to, when applicable.
    pub tool_call_id: Option<String>,
    /// Human-readable prompt or description.
    pub prompt: String,
    /// Provider-neutral request payload.
    pub payload: Value,
    /// Current request status.
    pub status: ExternalRequestStatus,
}

/// Kind of external request represented by a suspension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExternalRequestKind {
    /// Permission decision request.
    Permission,
    /// User question request.
    Question,
    /// Tool input request.
    ToolInput,
    /// Confirmation request.
    Confirmation,
    /// Host-defined request kind.
    Custom(String),
}

/// External request status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExternalRequestStatus {
    /// Request is awaiting a response.
    Pending,
    /// Request received a response.
    Responded(ExternalResponse),
    /// Request was canceled before response.
    Canceled(CancelReason),
}

/// Host response to an external request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalResponse {
    /// Provider-neutral response payload.
    pub payload: Value,
}

impl ExternalResponse {
    /// Create an external response from provider-neutral JSON payload.
    #[must_use]
    pub const fn new(payload: Value) -> Self {
        Self { payload }
    }
}

/// Work item tracked across suspensions and cancellation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingWork {
    /// Stable work identity, usually the provider tool call id.
    pub id: String,
    /// Provider turn that created the work.
    pub turn: u64,
    /// Tool call being executed or awaited, when applicable.
    pub tool_call: Option<ToolCall>,
    /// Current work status.
    pub status: PendingWorkStatus,
}

/// Pending work status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PendingWorkStatus {
    /// Work is waiting to start or complete.
    Pending,
    /// Work was canceled before completion.
    Canceled(CancelReason),
    /// Work completed with a tool result.
    Finished(ToolResult),
    /// Work failed with a durable error message.
    Failed { message: String },
}

/// Reason pending work was canceled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CancelReason {
    /// New user prompt superseded pending work.
    SupersededByUserPrompt,
    /// Session was interrupted.
    SessionInterrupted,
    /// Tool could not be executed.
    ToolUnavailable,
}

/// Provider request record persisted before dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestRecord {
    /// Request identity.
    pub id: RequestId,
    /// Provider turn this request belongs to.
    pub turn: u64,
    /// Model identity used for the request.
    pub model: ModelRef,
    /// Hash of the provider-neutral request inputs.
    pub canonical_request_hash: String,
    /// Provider-neutral request payload used by tests until request drafts exist.
    pub canonical_request: Value,
    /// Provider-projected request payload.
    pub projected_request: Value,
}

fn turn_index(turn: u64) -> usize {
    usize::try_from(turn).unwrap_or(usize::MAX)
}

fn upsert_external_request(requests: &mut Vec<ExternalRequest>, request: ExternalRequest) {
    if let Some(existing) = requests
        .iter_mut()
        .rev()
        .find(|existing| existing.id == request.id)
    {
        *existing = request;
    } else {
        requests.push(request);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{AgentTranscriptEvent, ThinkingBlockKind};

    fn sample_thinking_block() -> ThinkingBlock {
        ThinkingBlock {
            kind: ThinkingBlockKind::OpenAiResponses,
            text: "reasoning".to_owned(),
            payload: json!({ "id": "thinking-1" }),
        }
    }

    fn sample_turn_metadata() -> crate::ProviderTurnMetadata {
        crate::ProviderTurnMetadata {
            provider: "openai_responses".to_owned(),
            url: "https://api.openai.com/v1/responses".to_owned(),
            model: "gpt-test".to_owned(),
            usage: Some(crate::TokenUsage {
                input_tokens: Some(5),
                output_tokens: Some(2),
            }),
            resume: json!({ "response_id": "resp-1" }),
        }
    }

    fn provider_fact_events(call: &ToolCall, result: &ToolResult) -> Vec<SessionEvent> {
        vec![
            SessionEvent::PromptAppended {
                prompt: "summarize".to_owned(),
            },
            SessionEvent::ProviderTextDelta {
                turn: 0,
                delta: "hello".to_owned(),
            },
            SessionEvent::ProviderThinkingDelta {
                turn: 0,
                delta: "think".to_owned(),
            },
            SessionEvent::ThinkingBlockRecorded {
                turn: 0,
                block: sample_thinking_block(),
            },
            SessionEvent::ToolCallRecorded {
                turn: 0,
                call: call.clone(),
            },
            SessionEvent::ToolResultRecorded {
                turn: 0,
                result: result.clone(),
            },
            SessionEvent::ProviderTurnMetadataRecorded {
                turn: 0,
                metadata: sample_turn_metadata(),
            },
            SessionEvent::TurnFinished { turn: 0 },
            SessionEvent::Finished,
        ]
    }

    #[test]
    fn snapshot_rebuilds_provider_facts_from_events() {
        let id = SessionId::new("session-1");
        let call = ToolCall::new("call-1", "read", json!({ "path": "README.md" }));
        let result = ToolResult::new("call-1", "read", "contents");

        let mut snapshot = SessionSnapshot::new(id);
        snapshot.apply_events(&provider_fact_events(&call, &result));

        assert_eq!(snapshot.current_prompt.as_deref(), Some("summarize"));
        assert_eq!(snapshot.prompts, ["summarize"]);
        assert_eq!(
            snapshot.turns,
            [CompletionTurn {
                turn: 0,
                metadata: Some(sample_turn_metadata()),
                items: vec![
                    TurnItem::Thinking(sample_thinking_block()),
                    TurnItem::ToolCall(call.clone()),
                    TurnItem::ToolResult(result.clone()),
                ],
            }]
        );
        assert_eq!(snapshot.tool_calls().collect::<Vec<_>>(), [&call]);
        assert_eq!(snapshot.tool_results().collect::<Vec<_>>(), [&result]);
        assert_eq!(snapshot.turn, 1);
        assert_eq!(snapshot.status, SessionStatus::Finished);
        assert_eq!(
            snapshot.pending_work,
            [PendingWork {
                id: "call-1".to_owned(),
                turn: 0,
                tool_call: Some(call),
                status: PendingWorkStatus::Finished(result),
            }]
        );
        assert!(matches!(
            &snapshot.state.events[..],
            [
                AgentTranscriptEvent::Text { .. },
                AgentTranscriptEvent::Thinking { .. },
                AgentTranscriptEvent::ToolCall { .. },
                AgentTranscriptEvent::ToolResult { .. },
            ]
        ));
    }

    #[test]
    fn middleware_state_fold_replaces_namespace_wholesale() {
        let events = vec![
            SessionEvent::MiddlewareStateUpdated {
                namespace: "agents".to_owned(),
                state: json!({ "loaded": ["a"], "stale": true }),
            },
            SessionEvent::MiddlewareStateUpdated {
                namespace: "cost".to_owned(),
                state: json!({ "tokens": 10 }),
            },
            SessionEvent::MiddlewareStateUpdated {
                namespace: "agents".to_owned(),
                state: json!({ "loaded": ["a", "b"] }),
            },
        ];

        let mut snapshot = SessionSnapshot::new(SessionId::new("session-1"));
        snapshot.apply_events(&events);

        assert_eq!(
            snapshot.middleware_state.get("agents"),
            Some(&json!({ "loaded": ["a", "b"] })),
            "later writes must replace the namespace blob, not merge it"
        );
        assert_eq!(
            snapshot.middleware_state.get("cost"),
            Some(&json!({ "tokens": 10 }))
        );

        let mut incremental = std::collections::BTreeMap::new();
        for event in &events {
            if let SessionEvent::MiddlewareStateUpdated { namespace, state } = event {
                incremental.insert(namespace.clone(), state.clone());
            }
        }
        assert_eq!(snapshot.middleware_state, incremental);
    }

    #[test]
    fn middleware_state_fold_does_not_change_status() {
        let mut snapshot = SessionSnapshot::new(SessionId::new("session-1"));
        snapshot.apply_event(&SessionEvent::MiddlewareStateUpdated {
            namespace: "agents".to_owned(),
            state: json!({}),
        });

        assert_eq!(snapshot.status, SessionStatus::Idle);
    }

    #[test]
    fn snapshot_without_middleware_state_field_deserializes_to_empty_map() {
        let mut value = serde_json::to_value(SessionSnapshot::new(SessionId::new("session-1")))
            .expect("serialize snapshot");
        value
            .as_object_mut()
            .expect("snapshot object")
            .remove("middleware_state")
            .expect("field present in current schema");

        let snapshot: SessionSnapshot = serde_json::from_value(value).expect("legacy snapshot");

        assert!(snapshot.middleware_state.is_empty());
    }

    #[test]
    fn middleware_state_round_trips_through_serde() {
        let mut snapshot = SessionSnapshot::new(SessionId::new("session-1"));
        snapshot.apply_event(&SessionEvent::MiddlewareStateUpdated {
            namespace: "permissions".to_owned(),
            state: json!({ "grants": ["shell"] }),
        });

        let value = serde_json::to_value(&snapshot).expect("serialize snapshot");
        let restored: SessionSnapshot = serde_json::from_value(value).expect("restore snapshot");

        assert_eq!(restored.middleware_state, snapshot.middleware_state);
    }
}
