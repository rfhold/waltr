//! Durable session model types.

#![allow(missing_docs)]

use std::{collections::BTreeMap, time::SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    AuthenticationFailure, CompletionTurn, ModelRef, PromptTurn, ProviderTurnMetadata,
    ThinkingBlock, ToolCall, ToolResult, TurnItem,
    definition::AgentDefinitionBundle,
    timeline::{
        AgentTimelineEvent, CHANNEL_ASSISTANT_TEXT, CHANNEL_THINKING, TimelineContent,
        TimelineCursor, TimelineEventKind, TimelineStatus,
    },
};

use super::{
    ActiveSessionMask, InferenceSettings, SessionCapabilities, SessionCoordinationState,
    SessionMask, SessionMaskId, SessionProgress,
};

/// Condition applied by an atomic metadata set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataSetCondition {
    /// Always replace or create the value.
    Unconditional,
    /// Set the value only when the key is absent, including when no JSON null is present.
    Absent,
}

/// Result of an atomic conditional metadata set.
#[derive(Debug, Clone, PartialEq)]
pub struct MetadataSetOutcome {
    /// Complete record observed after evaluating the condition.
    pub record: SessionRecord,
    /// Whether the value was written.
    pub changed: bool,
}

/// Durable session identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Session identity.
    pub id: SessionId,
    /// Current optimistic revision.
    pub revision: Revision,
    /// Durable metadata used by hosts to list and order sessions.
    #[serde(default = "SessionMetadata::now")]
    pub metadata: SessionMetadata,
    /// Durable settings used for session inference.
    #[serde(default)]
    pub inference_settings: Option<InferenceSettings>,
    /// Self-contained agent definitions selected for this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_definition: Option<AgentDefinitionBundle>,
    /// Greatest timeline cursor ever allocated for this session.
    #[serde(default)]
    pub timeline_cursor_high_water: TimelineCursor,
    /// Ordered durable journal, including cursor-assigned timeline events.
    pub journal: Vec<SessionJournalEvent>,
}

/// Durable session metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetadata {
    /// Time the session record was created.
    pub created_at: SystemTime,
    /// Time the session record was last written.
    pub updated_at: SystemTime,
    /// Time the session last had user-visible activity.
    pub last_activity_at: SystemTime,
    /// Host-defined durable metadata values.
    #[serde(default)]
    pub values: BTreeMap<String, Value>,
}

impl SessionMetadata {
    /// Create metadata with all timestamps set to now.
    #[must_use]
    pub fn now() -> Self {
        let now = SystemTime::now();
        Self {
            created_at: now,
            updated_at: now,
            last_activity_at: now,
            values: BTreeMap::new(),
        }
    }
}

/// Compact session row for listing durable sessions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// Session identity.
    pub id: SessionId,
    /// Current optimistic revision.
    pub revision: Revision,
    /// Current status derived from the journal.
    pub status: SessionStatus,
    /// Most recent user prompt or conversation input.
    pub current_prompt: Option<String>,
    /// Time the session record was created.
    pub created_at: SystemTime,
    /// Time the session record was last written.
    pub updated_at: SystemTime,
    /// Time the session last had user-visible activity.
    pub last_activity_at: SystemTime,
    /// Durable usage and cost totals derived from journaled provider turns.
    #[serde(default)]
    pub metrics: SessionMetrics,
    /// Durable settings used for session inference.
    #[serde(default)]
    pub inference_settings: Option<InferenceSettings>,
    /// Currently selected durable agent name, when the session has an explicit bundle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// Host-defined durable metadata values.
    #[serde(default)]
    pub metadata_values: BTreeMap<String, Value>,
    /// Process-local run, barrier, pause, queue, and continuation facts.
    #[serde(default)]
    pub coordination: SessionCoordinationState,
    /// Unified durable and process-local progress projection.
    #[serde(default)]
    pub progress: SessionProgress,
    /// Actions authorized by the current durable and process-local state.
    #[serde(default)]
    pub capabilities: SessionCapabilities,
}

/// Durable usage and cost totals for a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetrics {
    /// Non-cache/base-priced input tokens consumed by provider requests.
    pub input_tokens: u64,
    /// Output/completion tokens produced by provider responses.
    pub output_tokens: u64,
    /// Input tokens served from provider cache.
    pub cache_read_tokens: u64,
    /// Input tokens written to provider cache.
    pub cache_write_tokens: u64,
    /// Image input tokens consumed by provider requests.
    pub image_input_tokens: u64,
    /// Image count consumed by provider requests.
    pub image_input_count: u64,
    /// Total billable/token-counted input and output tokens.
    pub total_tokens: u64,
    /// Full input tokens represented by provider usage reports, including cache reads and writes.
    #[serde(default)]
    pub total_input_tokens: u64,
    /// Full context tokens represented by provider usage reports, including cache reads, writes, and output.
    #[serde(default)]
    pub context_tokens: u64,
    /// Currency for total cost, or `mixed` when journaled turns differ.
    pub cost_currency: Option<String>,
    /// Total cost in currency micro-units.
    pub total_cost_micros: u64,
}

impl SessionMetrics {
    fn add_turn_metadata(&mut self, metadata: &ProviderTurnMetadata) {
        if let Some(usage) = metadata.usage {
            self.input_tokens += usage.input_tokens.unwrap_or_default();
            self.output_tokens += usage.output_tokens.unwrap_or_default();
            self.cache_read_tokens += usage.cache_read_tokens.unwrap_or_default();
            self.cache_write_tokens += usage.cache_write_tokens.unwrap_or_default();
            self.image_input_tokens += usage.image_input_tokens.unwrap_or_default();
            self.image_input_count += usage.image_input_count.unwrap_or_default();
            self.total_tokens += usage.total_tokens();
            self.total_input_tokens += usage.total_input_tokens();
            self.context_tokens += usage.context_tokens();
        }
        if let Some(cost) = &metadata.cost {
            self.total_cost_micros += cost.total_micros;
            match &self.cost_currency {
                None => self.cost_currency = Some(cost.currency.clone()),
                Some(currency) if currency == &cost.currency => {}
                Some(_) => self.cost_currency = Some("mixed".to_owned()),
            }
        }
    }
}

impl SessionRecord {
    /// Create a session record with an empty journal.
    pub fn new(id: impl Into<SessionId>) -> Self {
        Self {
            id: id.into(),
            revision: Revision::ZERO,
            metadata: SessionMetadata::now(),
            inference_settings: None,
            agent_definition: None,
            timeline_cursor_high_water: TimelineCursor::ZERO,
            journal: Vec::new(),
        }
    }

    /// Set session inference settings and mark metadata as updated.
    pub fn set_inference_settings(&mut self, inference_settings: Option<InferenceSettings>) {
        self.inference_settings = inference_settings;
        self.touch_metadata();
    }

    /// Set the self-contained agent-definition bundle and mark metadata as updated.
    pub fn set_agent_definition(&mut self, agent_definition: Option<AgentDefinitionBundle>) {
        self.agent_definition = agent_definition;
        self.touch_metadata();
    }

    /// Borrow a host-defined metadata value.
    #[must_use]
    pub fn metadata_value(&self, key: &str) -> Option<&Value> {
        self.metadata.values.get(key)
    }

    /// Set a host-defined metadata value and mark the record metadata as updated.
    pub fn set_metadata_value(&mut self, key: impl Into<String>, value: Value) {
        self.metadata.values.insert(key.into(), value);
        self.touch_metadata();
    }

    /// Remove a host-defined metadata value and mark the record metadata as updated.
    pub fn remove_metadata_value(&mut self, key: &str) -> Option<Value> {
        let value = self.metadata.values.remove(key);
        if value.is_some() {
            self.touch_metadata();
        }
        value
    }

    /// Mark this record metadata as updated without changing activity time.
    pub fn touch_metadata(&mut self) {
        self.metadata.updated_at = SystemTime::now();
    }

    /// Derive the current compact session projection from the journal.
    #[must_use]
    pub fn snapshot(&self) -> SessionSnapshot {
        let mut snapshot = SessionSnapshot::from_journal(self.id.clone(), &self.journal);
        snapshot.revision = self.revision;
        snapshot
    }

    /// Derive the compact listing row for this record.
    #[must_use]
    pub fn summary(&self) -> SessionSummary {
        let snapshot = self.snapshot();
        let metrics = snapshot.metrics();
        let mut summary = SessionSummary {
            id: self.id.clone(),
            revision: self.revision,
            status: snapshot.status,
            current_prompt: snapshot.current_prompt,
            created_at: self.metadata.created_at,
            updated_at: self.metadata.updated_at,
            last_activity_at: self.metadata.last_activity_at,
            metrics,
            inference_settings: self.inference_settings.clone(),
            agent_name: self
                .agent_definition
                .as_ref()
                .map(|bundle| bundle.agent_name.clone()),
            metadata_values: self.metadata.values.clone(),
            coordination: SessionCoordinationState::default(),
            progress: SessionProgress::default(),
            capabilities: SessionCapabilities::default(),
        };
        summary.set_coordination(SessionCoordinationState::default());
        summary
    }

    /// Mark this record as having user-visible activity now.
    pub fn touch_activity(&mut self) {
        self.touch_activity_at(SystemTime::now());
    }

    /// Mark this record as having user-visible activity at a specific time.
    pub const fn touch_activity_at(&mut self, time: SystemTime) {
        self.metadata.updated_at = time;
        self.metadata.last_activity_at = time;
    }

    /// Append one durable journal event to this record.
    pub fn append_journal_event(&mut self, event: SessionJournalEvent) {
        match &event {
            SessionJournalEvent::Timeline(timeline) => {
                self.timeline_cursor_high_water =
                    self.timeline_cursor_high_water.max(timeline.cursor);
            }
            SessionJournalEvent::MasksMutated { cursor, .. } => {
                self.timeline_cursor_high_water = self.timeline_cursor_high_water.max(*cursor);
            }
            _ => {}
        }
        self.journal.push(event);
    }

    /// Append durable journal events to this record in order.
    pub fn append_journal_events(&mut self, events: impl IntoIterator<Item = SessionJournalEvent>) {
        for event in events {
            self.append_journal_event(event);
        }
    }

    /// Timeline events stored in journal order.
    pub fn timeline_events(&self) -> impl Iterator<Item = &AgentTimelineEvent> {
        self.effective_journal_events()
            .into_iter()
            .filter_map(|event| match event {
                SessionJournalEvent::Timeline(event) => Some(event),
                SessionJournalEvent::PromptAppended { .. }
                | SessionJournalEvent::ProviderRequestRecorded { .. }
                | SessionJournalEvent::ProviderThinkingBlockRecorded { .. }
                | SessionJournalEvent::ProviderTurnMetadataRecorded { .. }
                | SessionJournalEvent::MiddlewareStateUpdated { .. }
                | SessionJournalEvent::ExternalRequestResponded { .. }
                | SessionJournalEvent::ExternalRequestCanceled { .. }
                | SessionJournalEvent::Suspended { .. }
                | SessionJournalEvent::WorkCanceled { .. }
                | SessionJournalEvent::Interrupted
                | SessionJournalEvent::TurnInterrupted { .. }
                | SessionJournalEvent::TurnFinished { .. }
                | SessionJournalEvent::MasksMutated { .. }
                | SessionJournalEvent::Rollback { .. }
                | SessionJournalEvent::Finished
                | SessionJournalEvent::Failed { .. } => None,
            })
    }

    /// Physical timeline events stored in journal order, including rolled-back history.
    pub fn all_timeline_events(&self) -> impl Iterator<Item = &AgentTimelineEvent> {
        self.journal.iter().filter_map(|event| match event {
            SessionJournalEvent::Timeline(event) => Some(event),
            SessionJournalEvent::PromptAppended { .. }
            | SessionJournalEvent::ProviderRequestRecorded { .. }
            | SessionJournalEvent::ProviderThinkingBlockRecorded { .. }
            | SessionJournalEvent::ProviderTurnMetadataRecorded { .. }
            | SessionJournalEvent::MiddlewareStateUpdated { .. }
            | SessionJournalEvent::ExternalRequestResponded { .. }
            | SessionJournalEvent::ExternalRequestCanceled { .. }
            | SessionJournalEvent::Suspended { .. }
            | SessionJournalEvent::WorkCanceled { .. }
            | SessionJournalEvent::Interrupted
            | SessionJournalEvent::TurnInterrupted { .. }
            | SessionJournalEvent::TurnFinished { .. }
            | SessionJournalEvent::MasksMutated { .. }
            | SessionJournalEvent::Rollback { .. }
            | SessionJournalEvent::Finished
            | SessionJournalEvent::Failed { .. } => None,
        })
    }

    /// Journal events visible after applying append-only rollback barriers.
    #[must_use]
    pub fn effective_journal_events(&self) -> Vec<&SessionJournalEvent> {
        let mut events: Vec<(usize, &SessionJournalEvent)> = Vec::new();
        for (index, event) in self.journal.iter().enumerate() {
            match event {
                SessionJournalEvent::Rollback { target } => {
                    let keep = usize::try_from(target.journal_len).unwrap_or(usize::MAX);
                    events.retain(|(event_index, _)| *event_index < keep);
                    if let Some(prompt_index) = target.remove_prompt_index {
                        remove_prompt_appended(&mut events, prompt_index);
                    }
                }
                _ => events.push((index, event)),
            }
        }
        events.into_iter().map(|(_, event)| event).collect()
    }

    /// Last assigned timeline cursor, if any.
    #[must_use]
    pub fn last_timeline_cursor(&self) -> Option<crate::timeline::TimelineCursor> {
        self.all_timeline_events().last().map(|event| event.cursor)
    }

    /// Greatest allocated cursor, including rolled-back or filtered timeline events.
    #[must_use]
    pub fn timeline_cursor_high_water(&self) -> TimelineCursor {
        self.journal
            .iter()
            .filter_map(|event| match event {
                SessionJournalEvent::Timeline(event) => Some(event.cursor),
                SessionJournalEvent::MasksMutated { cursor, .. } => Some(*cursor),
                _ => None,
            })
            .max()
            .unwrap_or(TimelineCursor::ZERO)
            .max(self.timeline_cursor_high_water)
    }

    /// Provider request records stored in journal order.
    pub fn provider_requests(&self) -> impl Iterator<Item = &RequestRecord> {
        self.effective_journal_events()
            .into_iter()
            .filter_map(|event| match event {
                SessionJournalEvent::ProviderRequestRecorded { request } => Some(request),
                SessionJournalEvent::Timeline(_)
                | SessionJournalEvent::PromptAppended { .. }
                | SessionJournalEvent::ProviderThinkingBlockRecorded { .. }
                | SessionJournalEvent::ProviderTurnMetadataRecorded { .. }
                | SessionJournalEvent::MiddlewareStateUpdated { .. }
                | SessionJournalEvent::ExternalRequestResponded { .. }
                | SessionJournalEvent::ExternalRequestCanceled { .. }
                | SessionJournalEvent::Suspended { .. }
                | SessionJournalEvent::WorkCanceled { .. }
                | SessionJournalEvent::Interrupted
                | SessionJournalEvent::TurnInterrupted { .. }
                | SessionJournalEvent::TurnFinished { .. }
                | SessionJournalEvent::MasksMutated { .. }
                | SessionJournalEvent::Rollback { .. }
                | SessionJournalEvent::Finished
                | SessionJournalEvent::Failed { .. } => None,
            })
    }

    /// Derive active masks from rollback-effective journal history.
    #[must_use]
    pub fn active_masks(&self) -> Vec<SessionMask> {
        self.active_session_masks()
            .into_iter()
            .map(|active| active.mask)
            .collect()
    }

    /// Derive active masks and their administrative activation cursors.
    #[must_use]
    pub fn active_session_masks(&self) -> Vec<ActiveSessionMask> {
        let mut active = Vec::<ActiveSessionMask>::new();
        for event in self.effective_journal_events() {
            if let SessionJournalEvent::MasksMutated {
                cursor,
                removed_mask_ids,
                added_masks,
            } = event
            {
                active.retain(|active| !removed_mask_ids.contains(&active.mask.id));
                active.extend(added_masks.iter().cloned().map(|mask| ActiveSessionMask {
                    mask,
                    mutation_cursor: *cursor,
                }));
            }
        }
        active
    }
}

impl SessionSummary {
    /// Replace process-local coordination and recompute its derived projections together.
    pub(crate) fn set_coordination(&mut self, coordination: SessionCoordinationState) {
        self.progress = SessionProgress::from_state(&self.status, &coordination);
        self.capabilities = SessionCapabilities::from_state(&self.status, &coordination);
        self.coordination = coordination;
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
    /// User prompts with the provider turn they started.
    #[serde(default)]
    pub prompt_turns: Vec<PromptTurn>,
    /// Most recent user prompt or conversation input.
    pub current_prompt: Option<String>,
    /// Next provider turn number.
    pub turn: u64,
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
    pub const fn new(id: SessionId) -> Self {
        Self {
            id,
            revision: Revision::ZERO,
            prompts: Vec::new(),
            prompt_turns: Vec::new(),
            current_prompt: None,
            turn: 0,
            turns: Vec::new(),
            status: SessionStatus::Idle,
            pending_work: Vec::new(),
            external_requests: Vec::new(),
            middleware_state: BTreeMap::new(),
        }
    }

    /// Rebuild a snapshot projection from durable journal records.
    #[must_use]
    pub fn from_journal(id: SessionId, journal: &[SessionJournalEvent]) -> Self {
        let mut snapshot = Self::new(id);
        let mut events: Vec<(usize, &SessionJournalEvent)> = Vec::new();
        for (index, event) in journal.iter().enumerate() {
            match event {
                SessionJournalEvent::Rollback { target } => {
                    let keep = usize::try_from(target.journal_len).unwrap_or(usize::MAX);
                    events.retain(|(event_index, _)| *event_index < keep);
                    if let Some(prompt_index) = target.remove_prompt_index {
                        remove_prompt_appended(&mut events, prompt_index);
                    }
                }
                _ => events.push((index, event)),
            }
        }
        snapshot.apply_events(events.into_iter().map(|(_, event)| event));
        snapshot
    }

    /// Tool calls accumulated across all turns, in emission order.
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.turns.iter().flat_map(CompletionTurn::tool_calls)
    }

    /// Assistant text accumulated across all turns, in emission order.
    pub fn assistant_texts(&self) -> impl Iterator<Item = &str> {
        self.turns.iter().flat_map(CompletionTurn::assistant_texts)
    }

    /// Tool results accumulated across all turns, in emission order.
    pub fn tool_results(&self) -> impl Iterator<Item = &ToolResult> {
        self.turns.iter().flat_map(CompletionTurn::tool_results)
    }

    /// Thinking blocks accumulated across all turns, in emission order.
    pub fn thinking_blocks(&self) -> impl Iterator<Item = &ThinkingBlock> {
        self.turns.iter().flat_map(CompletionTurn::thinking_blocks)
    }

    /// Aggregate usage and cost from journaled provider turn metadata.
    #[must_use]
    pub fn metrics(&self) -> SessionMetrics {
        let mut metrics = SessionMetrics::default();
        for metadata in self.turns.iter().filter_map(|turn| turn.metadata.as_ref()) {
            metrics.add_turn_metadata(metadata);
        }
        metrics
    }

    /// Apply one durable journal event to this snapshot.
    pub fn apply_event(&mut self, event: &SessionJournalEvent) {
        match event {
            SessionJournalEvent::Timeline(event) => self.apply_timeline_event(event),
            SessionJournalEvent::PromptAppended { prompt } => {
                self.prompts.push(prompt.clone());
                self.prompt_turns.push(PromptTurn {
                    turn: self.turn,
                    prompt: prompt.clone(),
                });
                self.current_prompt = Some(prompt.clone());
                self.status = SessionStatus::BuildingRequest;
            }
            SessionJournalEvent::ProviderRequestRecorded { .. } => {
                self.status = SessionStatus::StreamingProvider;
            }
            SessionJournalEvent::ProviderThinkingBlockRecorded { turn, block } => {
                CompletionTurn::entry(&mut self.turns, *turn)
                    .items
                    .push(TurnItem::Thinking(block.clone()));
                self.status = SessionStatus::StreamingProvider;
            }
            SessionJournalEvent::ProviderTurnMetadataRecorded { turn, metadata } => {
                CompletionTurn::entry(&mut self.turns, *turn).metadata = Some(metadata.clone());
                self.status = SessionStatus::StreamingProvider;
            }
            SessionJournalEvent::MiddlewareStateUpdated { namespace, state } => {
                self.middleware_state
                    .insert(namespace.clone(), state.clone());
            }
            SessionJournalEvent::ExternalRequestResponded {
                request_id,
                response,
            } => self.respond_external_request(request_id, response),
            SessionJournalEvent::ExternalRequestCanceled { request_id, reason } => {
                self.cancel_external_request(request_id, reason);
            }
            SessionJournalEvent::Suspended { suspension } => {
                self.status = SessionStatus::Suspended(suspension.clone());
            }
            SessionJournalEvent::WorkCanceled { work_id, reason } => {
                self.cancel_work(work_id, reason);
            }
            SessionJournalEvent::Interrupted => {
                self.status = SessionStatus::Suspended(Suspension::Interrupted {
                    work: self.pending_work.clone(),
                });
            }
            SessionJournalEvent::TurnInterrupted { turn } => {
                self.turn = self.turn.max(turn + 1);
                self.status = SessionStatus::Suspended(Suspension::Interrupted {
                    work: self.pending_work.clone(),
                });
            }
            SessionJournalEvent::TurnFinished { turn } => {
                self.turn = self.turn.max(turn + 1);
                self.status = SessionStatus::Idle;
            }
            SessionJournalEvent::MasksMutated { .. } => {}
            SessionJournalEvent::Rollback { .. } => {}
            SessionJournalEvent::Finished => {
                self.status = SessionStatus::Finished;
            }
            SessionJournalEvent::Failed {
                message,
                authentication_failure,
            } => {
                self.status = SessionStatus::Failed {
                    message: message.clone(),
                    authentication_failure: authentication_failure.clone(),
                };
            }
        }
    }

    fn apply_timeline_event(&mut self, event: &AgentTimelineEvent) {
        if event.kind == TimelineEventKind::SessionStatusChanged {
            if let Some(status) = event.status {
                self.apply_timeline_status(status, event);
            }
            return;
        }

        match &event.content {
            TimelineContent::ToolCall { call } => self.record_tool_call(event.turn, call),
            TimelineContent::ToolResult { result } => self.record_tool_result(event.turn, result),
            TimelineContent::ExternalRequest { request } => self.record_external_request(request),
            TimelineContent::TextDelta { delta } if channel_is(event, CHANNEL_ASSISTANT_TEXT) => {
                self.record_assistant_text_delta(event.turn, delta);
                self.status = SessionStatus::StreamingProvider;
            }
            TimelineContent::TextSnapshot { text } if channel_is(event, CHANNEL_ASSISTANT_TEXT) => {
                self.record_assistant_text_snapshot(event.turn, text);
                self.status = SessionStatus::StreamingProvider;
            }
            TimelineContent::TextDelta { .. } | TimelineContent::TextSnapshot { .. }
                if channel_is(event, CHANNEL_THINKING) =>
            {
                self.status = SessionStatus::StreamingProvider;
            }
            TimelineContent::Empty
            | TimelineContent::TextDelta { .. }
            | TimelineContent::TextSnapshot { .. }
            | TimelineContent::ContentRef { .. }
            | TimelineContent::Json { .. } => {}
        }
    }

    fn apply_timeline_status(&mut self, status: TimelineStatus, event: &AgentTimelineEvent) {
        self.status = match status {
            TimelineStatus::Idle => SessionStatus::Idle,
            TimelineStatus::BuildingRequest => SessionStatus::BuildingRequest,
            TimelineStatus::StreamingProvider => SessionStatus::StreamingProvider,
            TimelineStatus::RetryingProvider => retrying_provider_status(event),
            TimelineStatus::EvaluatingToolCall => SessionStatus::EvaluatingToolCall,
            TimelineStatus::RunningTool => SessionStatus::RunningTool,
            TimelineStatus::AwaitingToolResults => {
                SessionStatus::Suspended(Suspension::AwaitingToolResults {
                    work: self.pending_work.clone(),
                })
            }
            TimelineStatus::AwaitingExternalInput => {
                let request = self
                    .external_requests
                    .iter()
                    .rev()
                    .find(|request| matches!(request.status, ExternalRequestStatus::Pending))
                    .cloned();
                request.map_or(SessionStatus::RunningTool, |request| {
                    SessionStatus::Suspended(Suspension::AwaitingExternalInput {
                        request: Box::new(request),
                    })
                })
            }
            TimelineStatus::Interrupted => SessionStatus::Suspended(Suspension::Interrupted {
                work: self.pending_work.clone(),
            }),
            TimelineStatus::Finished => SessionStatus::Finished,
            TimelineStatus::Failed => SessionStatus::Failed {
                message: timeline_failure_message(event),
                authentication_failure: None,
            },
            TimelineStatus::Pending | TimelineStatus::Resolved | TimelineStatus::Canceled => {
                self.status.clone()
            }
        };
    }

    fn record_assistant_text_delta(&mut self, turn: u64, delta: &str) {
        CompletionTurn::entry(&mut self.turns, turn).append_assistant_text(delta);
    }

    fn record_assistant_text_snapshot(&mut self, turn: u64, text: &str) {
        CompletionTurn::entry(&mut self.turns, turn).set_assistant_text_snapshot(text);
    }

    fn record_tool_call(&mut self, turn: u64, call: &ToolCall) {
        let turn_record = CompletionTurn::entry(&mut self.turns, turn);
        if !turn_record
            .items
            .iter()
            .any(|item| matches!(item, TurnItem::ToolCall(existing) if existing.id == call.id))
        {
            turn_record.items.push(TurnItem::ToolCall(call.clone()));
        }
        if !self.pending_work.iter().any(|work| work.id == call.id) {
            self.pending_work.push(PendingWork {
                id: call.id.clone(),
                turn,
                tool_call: Some(call.clone()),
                status: PendingWorkStatus::Pending,
            });
        }
        self.status = SessionStatus::EvaluatingToolCall;
    }

    fn record_tool_result(&mut self, turn: u64, result: &ToolResult) {
        let turn_record = CompletionTurn::entry(&mut self.turns, turn);
        if !turn_record.items.iter().any(|item| {
            matches!(item, TurnItem::ToolResult(existing) if existing.call_id == result.call_id)
        }) {
            turn_record.items.push(TurnItem::ToolResult(result.clone()));
        }
        if let Some(work) = self
            .pending_work
            .iter_mut()
            .rev()
            .find(|work| work.id == result.call_id)
        {
            work.status = PendingWorkStatus::Finished(result.clone());
        } else {
            self.pending_work.push(PendingWork {
                id: result.call_id.clone(),
                turn,
                tool_call: None,
                status: PendingWorkStatus::Finished(result.clone()),
            });
        }
        self.status = SessionStatus::RunningTool;
    }

    fn record_external_request(&mut self, request: &ExternalRequest) {
        upsert_external_request(&mut self.external_requests, request.clone());
        match &request.status {
            ExternalRequestStatus::Pending => {
                self.status = SessionStatus::Suspended(Suspension::AwaitingExternalInput {
                    request: Box::new(request.clone()),
                });
            }
            ExternalRequestStatus::Responded(_) => self.status = SessionStatus::RunningTool,
            ExternalRequestStatus::Canceled(_) => {}
        }
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

    /// Apply durable journal events to this snapshot in order.
    pub fn apply_events<'a>(&mut self, events: impl IntoIterator<Item = &'a SessionJournalEvent>) {
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
    /// Provider retry backoff is in progress.
    RetryingProvider {
        /// One-based attempt that failed.
        attempt: u32,
        /// Total configured attempts.
        max_attempts: u32,
        /// Delay before the next attempt, in milliseconds.
        delay_ms: u64,
        /// User-facing retry message.
        message: String,
    },
    /// Tool call middleware is being evaluated.
    EvaluatingToolCall,
    /// A local tool executor is running.
    RunningTool,
    /// Runtime is waiting for external input or pending work.
    Suspended(Suspension),
    /// Session completed normally.
    Finished,
    /// Session failed with a durable error message.
    Failed {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        authentication_failure: Option<AuthenticationFailure>,
    },
}

/// Ordered durable session journal event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
pub enum SessionJournalEvent {
    /// Cursor-assigned timeline event.
    Timeline(AgentTimelineEvent),
    /// User prompt was appended to the conversation.
    PromptAppended { prompt: String },
    /// Provider request was recorded before dispatch.
    ProviderRequestRecorded { request: RequestRecord },
    /// Provider emitted a replayable thinking block.
    ProviderThinkingBlockRecorded { turn: u64, block: ThinkingBlock },
    /// Provider surfaced response metadata for the turn.
    ProviderTurnMetadataRecorded {
        turn: u64,
        metadata: ProviderTurnMetadata,
    },
    /// Middleware namespace state was replaced wholesale.
    MiddlewareStateUpdated { namespace: String, state: Value },
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
    /// Provider turn was interrupted before normal completion.
    TurnInterrupted { turn: u64 },
    /// Provider turn finished.
    TurnFinished { turn: u64 },
    /// Persistent replacement masks were atomically removed and added.
    MasksMutated {
        cursor: TimelineCursor,
        removed_mask_ids: Vec<SessionMaskId>,
        added_masks: Vec<SessionMask>,
    },
    /// Session state was logically rolled back to an earlier checkpoint.
    Rollback { target: SessionRollbackTarget },
    /// Session completed normally.
    Finished,
    /// Session failed.
    Failed {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        authentication_failure: Option<AuthenticationFailure>,
    },
}

/// Input used to resume a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResumeInput {
    /// Continue from the existing durable checkpoint without adding input.
    Continue,
    /// Resume with a new user prompt.
    Prompt(String),
    /// Resume with multiple adjacent user prompts.
    Prompts(Vec<String>),
    /// Resume with externally completed tool results.
    ToolResults(Vec<ToolResult>),
    /// Resume with an answer to a pending external request.
    ExternalResponse {
        request_id: ExternalRequestId,
        response: ExternalResponse,
    },
}

/// Durable rollback target stored as an append-only replay barrier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRollbackTarget {
    /// Physical journal length to keep while replaying this rollback barrier.
    pub journal_len: u64,
    /// Last visible timeline cursor preserved by this rollback.
    pub after_cursor: Option<TimelineCursor>,
    /// Next provider turn after applying the rollback.
    pub next_turn: u64,
    /// Prompt index to remove from the replayed journal, used when editing a user prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove_prompt_index: Option<u64>,
}

fn remove_prompt_appended(events: &mut Vec<(usize, &SessionJournalEvent)>, prompt_index: u64) {
    let mut seen = 0_u64;
    let Some(position) = events.iter().position(|(_, event)| {
        if matches!(event, SessionJournalEvent::PromptAppended { .. }) {
            let matches = seen == prompt_index;
            seen += 1;
            matches
        } else {
            false
        }
    }) else {
        return;
    };
    events.remove(position);
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

fn channel_is(event: &AgentTimelineEvent, channel: &str) -> bool {
    event
        .channel
        .as_ref()
        .is_some_and(|candidate| candidate.as_str() == channel)
}

fn timeline_failure_message(event: &AgentTimelineEvent) -> String {
    match &event.content {
        TimelineContent::TextDelta { delta } => delta.clone(),
        TimelineContent::TextSnapshot { text } => text.clone(),
        TimelineContent::Json { value } => value.to_string(),
        TimelineContent::Empty
        | TimelineContent::ToolCall { .. }
        | TimelineContent::ToolResult { .. }
        | TimelineContent::ExternalRequest { .. }
        | TimelineContent::ContentRef { .. } => "timeline status failed".to_owned(),
    }
}

fn retrying_provider_status(event: &AgentTimelineEvent) -> SessionStatus {
    let TimelineContent::Json { value } = &event.content else {
        return SessionStatus::RetryingProvider {
            attempt: 0,
            max_attempts: 0,
            delay_ms: 0,
            message: String::new(),
        };
    };
    SessionStatus::RetryingProvider {
        attempt: value
            .get("attempt")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or_default(),
        max_attempts: value
            .get("maxAttempts")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or_default(),
        delay_ms: value
            .get("delayMs")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        message: value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    }
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
    use crate::{
        InferenceProviderSettings, ModelApi, ThinkingBlockKind,
        timeline::{
            CHANNEL_EXTERNAL_REQUEST, PendingTimelineEvent, PendingTimelineItemId, TimelineChannel,
            TimelineContent, TimelineCursor, TimelineEventKind, TimelineItemId,
        },
    };

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
                cache_read_tokens: Some(11),
                cache_write_tokens: Some(7),
                ..Default::default()
            }),
            cost: None,
            resume: json!({ "response_id": "resp-1" }),
        }
    }

    fn assigned(cursor: u64, pending: PendingTimelineEvent) -> SessionJournalEvent {
        SessionJournalEvent::Timeline(pending.with_cursor(TimelineCursor::new(cursor)))
    }

    fn inference_settings() -> InferenceSettings {
        InferenceSettings {
            provider: InferenceProviderSettings {
                key: "test".to_owned(),
                api: ModelApi::OpenAiResponses,
                url: "https://provider.example/v1".to_owned(),
                model_id: "test-model".to_owned(),
                model_cost: None,
                headers: BTreeMap::new(),
                oauth: None,
                reasoning_effort: Some("medium".to_owned()),
            },
        }
    }

    #[test]
    fn session_record_serializes_inference_settings_field() {
        let mut record = SessionRecord::new("session-1");
        record.inference_settings = Some(inference_settings());

        let serialized = serde_json::to_value(record).expect("session record serializes");

        assert!(serialized.get("inference_settings").is_some());
        assert!(serialized.get("runtime_settings").is_none());
    }

    #[test]
    fn durable_record_summary_defaults_to_ready_coordination() {
        let summary = SessionRecord::new("session-1").summary();
        assert_eq!(
            summary.coordination,
            crate::SessionCoordinationState::default()
        );
        assert_eq!(
            summary.coordination.phase,
            crate::SessionCoordinationPhase::Ready
        );
        assert_eq!(summary.progress.phase, crate::SessionProgressPhase::Idle);
        assert!(!summary.progress.running);
        assert_eq!(
            summary.capabilities.prompt_disposition,
            crate::SessionPromptDisposition::Immediate
        );
        assert!(!summary.capabilities.can_respond_external_request);
        assert!(!summary.capabilities.can_interrupt);
    }

    #[test]
    fn missing_cursor_high_water_falls_back_to_maximum_durable_cursor() {
        let mut record = SessionRecord::new("legacy");
        record.journal.push(assigned(
            8,
            PendingTimelineEvent::assistant_text_snapshot(
                record.id.clone(),
                0,
                PendingTimelineItemId::New,
                "durable",
            ),
        ));
        let mut serialized = serde_json::to_value(record).unwrap();
        serialized
            .as_object_mut()
            .unwrap()
            .remove("timeline_cursor_high_water");

        let restored: SessionRecord = serde_json::from_value(serialized).unwrap();

        assert_eq!(restored.timeline_cursor_high_water, TimelineCursor::ZERO);
        assert_eq!(
            restored.timeline_cursor_high_water(),
            TimelineCursor::new(8)
        );
    }

    #[test]
    fn runtime_settings_field_does_not_populate_inference_settings() {
        let serialized = serde_json::to_value(SessionRecord::new("session-1"))
            .expect("session record serializes");
        let mut object = serialized
            .as_object()
            .expect("session record object")
            .clone();
        object.remove("inference_settings");
        object.insert(
            "runtime_settings".to_owned(),
            serde_json::to_value(inference_settings()).expect("settings serialize"),
        );

        let restored: SessionRecord =
            serde_json::from_value(Value::Object(object)).expect("old record deserializes");

        assert!(restored.inference_settings.is_none());
    }

    #[test]
    fn session_record_bundle_is_optional_and_legacy_json_loads_as_none() {
        let record = SessionRecord::new("session-1");
        let serialized = serde_json::to_value(&record).unwrap();
        assert!(serialized.get("agent_definition").is_none());

        let restored: SessionRecord = serde_json::from_value(serialized).unwrap();
        assert_eq!(restored.agent_definition, None);
    }

    #[test]
    fn session_record_bundle_round_trips_outside_the_journal() {
        let bundle = AgentDefinitionBundle::new(
            "selected",
            vec!["apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: selected\nspec: {}\n".to_owned()],
        );
        let mut request = super::super::SessionCreateRequest::new("session-1");
        request.agent_definition = Some(bundle.clone());
        let record = request.materialize();

        let restored: SessionRecord =
            serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();

        assert_eq!(restored.agent_definition, Some(bundle));
        assert!(restored.journal.is_empty());
    }

    #[test]
    fn legacy_failed_status_and_journal_event_remain_readable() {
        let status: SessionStatus = serde_json::from_str(r#"{"Failed":{"message":"legacy"}}"#)
            .expect("legacy failed status");
        let event: SessionJournalEvent = serde_json::from_str(r#"{"Failed":{"message":"legacy"}}"#)
            .expect("legacy failed journal event");

        assert_eq!(
            status,
            SessionStatus::Failed {
                message: "legacy".to_owned(),
                authentication_failure: None,
            }
        );
        assert_eq!(
            event,
            SessionJournalEvent::Failed {
                message: "legacy".to_owned(),
                authentication_failure: None,
            }
        );
    }

    #[test]
    fn authentication_failure_round_trips_through_failed_journal_status() {
        let failure = AuthenticationFailure {
            target: crate::AuthTarget::mcp_server("configured-mcp"),
            reason: crate::AuthenticationFailureReason::CredentialRejected,
            mcp_authorization: Some(Box::new(crate::McpAuthorizationContext {
                endpoint: "https://mcp.example/rpc".to_owned(),
                http_status: 401,
                resource_metadata_url: None,
                scope: Some("tools.read".to_owned()),
                error: Some("invalid_token".to_owned()),
                error_description: None,
                error_uri: None,
                realm: None,
            })),
        };
        let event = SessionJournalEvent::Failed {
            message: "authentication failed".to_owned(),
            authentication_failure: Some(failure.clone()),
        };
        let serialized = serde_json::to_string(&event).expect("serialize failed event");
        let restored: SessionJournalEvent =
            serde_json::from_str(&serialized).expect("deserialize failed event");
        let snapshot = SessionSnapshot::from_journal(SessionId::new("session-1"), &[restored]);

        assert_eq!(
            snapshot.status,
            SessionStatus::Failed {
                message: "authentication failed".to_owned(),
                authentication_failure: Some(failure),
            }
        );
    }

    fn provider_fact_events(
        session_id: &SessionId,
        call: &ToolCall,
        result: &ToolResult,
    ) -> Vec<SessionJournalEvent> {
        vec![
            SessionJournalEvent::PromptAppended {
                prompt: "summarize".to_owned(),
            },
            assigned(
                1,
                PendingTimelineEvent::assistant_text_delta(
                    session_id.clone(),
                    0,
                    PendingTimelineItemId::New,
                    "hello",
                ),
            ),
            assigned(
                2,
                PendingTimelineEvent::thinking_delta(
                    session_id.clone(),
                    0,
                    PendingTimelineItemId::New,
                    "think",
                ),
            ),
            SessionJournalEvent::ProviderThinkingBlockRecorded {
                turn: 0,
                block: sample_thinking_block(),
            },
            assigned(
                3,
                PendingTimelineEvent::tool_call(
                    session_id.clone(),
                    0,
                    PendingTimelineItemId::New,
                    call.clone(),
                ),
            ),
            assigned(
                4,
                PendingTimelineEvent::tool_result(
                    session_id.clone(),
                    0,
                    PendingTimelineItemId::New,
                    TimelineItemId::from_first_cursor(TimelineCursor::new(3)),
                    result.clone(),
                ),
            ),
            SessionJournalEvent::ProviderTurnMetadataRecorded {
                turn: 0,
                metadata: sample_turn_metadata(),
            },
            SessionJournalEvent::TurnFinished { turn: 0 },
            SessionJournalEvent::Finished,
        ]
    }

    #[test]
    fn snapshot_rebuilds_provider_facts_from_journal() {
        let id = SessionId::new("session-1");
        let call = ToolCall::new("call-1", "read", json!({ "path": "README.md" }));
        let result = ToolResult::new("call-1", "read", "contents");

        let snapshot =
            SessionSnapshot::from_journal(id.clone(), &provider_fact_events(&id, &call, &result));

        assert_eq!(snapshot.current_prompt.as_deref(), Some("summarize"));
        assert_eq!(snapshot.prompts, ["summarize"]);
        assert_eq!(
            snapshot.turns,
            [CompletionTurn {
                turn: 0,
                metadata: Some(sample_turn_metadata()),
                items: vec![
                    TurnItem::AssistantText("hello".to_owned()),
                    TurnItem::Thinking(sample_thinking_block()),
                    TurnItem::ToolCall(call.clone()),
                    TurnItem::ToolResult(result.clone()),
                ],
            }]
        );
        assert_eq!(snapshot.assistant_texts().collect::<Vec<_>>(), ["hello"]);
        assert_eq!(snapshot.tool_calls().collect::<Vec<_>>(), [&call]);
        assert_eq!(snapshot.tool_results().collect::<Vec<_>>(), [&result]);
        let metrics = snapshot.metrics();
        assert_eq!(metrics.total_tokens, 7);
        assert_eq!(metrics.total_input_tokens, 23);
        assert_eq!(metrics.context_tokens, 25);
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
    }

    #[test]
    fn timeline_external_request_projects_lifecycle_state() {
        let id = SessionId::new("session-1");
        let request = ExternalRequest {
            id: ExternalRequestId::new("request-1"),
            kind: ExternalRequestKind::Permission,
            tool_call_id: Some("call-1".to_owned()),
            prompt: "Allow?".to_owned(),
            payload: json!({ "allow": true }),
            status: ExternalRequestStatus::Pending,
        };
        let pending = PendingTimelineEvent::new(
            id.clone(),
            0,
            PendingTimelineItemId::New,
            TimelineEventKind::ExternalRequestOpened,
            Some(TimelineChannel::from(CHANNEL_EXTERNAL_REQUEST)),
            TimelineContent::ExternalRequest {
                request: request.clone(),
            },
        );

        let snapshot = SessionSnapshot::from_journal(id, &[assigned(1, pending)]);

        assert_eq!(snapshot.external_requests, std::slice::from_ref(&request));
        assert_eq!(
            snapshot.status,
            SessionStatus::Suspended(Suspension::AwaitingExternalInput {
                request: Box::new(request),
            })
        );
    }

    #[test]
    fn middleware_state_fold_replaces_namespace_wholesale() {
        let events = vec![
            SessionJournalEvent::MiddlewareStateUpdated {
                namespace: "agents".to_owned(),
                state: json!({ "loaded": ["a"], "stale": true }),
            },
            SessionJournalEvent::MiddlewareStateUpdated {
                namespace: "cost".to_owned(),
                state: json!({ "tokens": 10 }),
            },
            SessionJournalEvent::MiddlewareStateUpdated {
                namespace: "agents".to_owned(),
                state: json!({ "loaded": ["a", "b"] }),
            },
        ];

        let snapshot = SessionSnapshot::from_journal(SessionId::new("session-1"), &events);

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
            if let SessionJournalEvent::MiddlewareStateUpdated { namespace, state } = event {
                incremental.insert(namespace.clone(), state.clone());
            }
        }
        assert_eq!(snapshot.middleware_state, incremental);
    }

    #[test]
    fn middleware_state_fold_does_not_change_status() {
        let mut snapshot = SessionSnapshot::new(SessionId::new("session-1"));
        snapshot.apply_event(&SessionJournalEvent::MiddlewareStateUpdated {
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
        snapshot.apply_event(&SessionJournalEvent::MiddlewareStateUpdated {
            namespace: "permissions".to_owned(),
            state: json!({ "grants": ["shell"] }),
        });

        let value = serde_json::to_value(&snapshot).expect("serialize snapshot");
        let restored: SessionSnapshot = serde_json::from_value(value).expect("restore snapshot");

        assert_eq!(restored.middleware_state, snapshot.middleware_state);
    }
}
