//! Validated projection used to copy completed session context.

use std::{collections::HashMap, error::Error, fmt};

use crate::timeline::{
    AgentTimelineEvent, CHANNEL_ASSISTANT_TEXT, CHANNEL_TOOL_CALL, CHANNEL_TOOL_RESULT,
    CHANNEL_USER_TEXT, TimelineContent, TimelineCursor, TimelineEventKind, TimelineItemId,
};

use super::{SessionId, SessionJournalEvent, SessionRecord, materialize::completed_turn_at};

/// Opaque, validated context copied through an exact completed-turn boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCopy {
    source_id: SessionId,
    boundary: TimelineCursor,
    completed_turn: u64,
    projected_journal: Vec<SessionJournalEvent>,
    item_metadata: super::ItemMetadata,
}

impl SessionCopy {
    /// Project replayable context through an exact completed-turn cursor.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the cursor is absent, falls inside unfinished work, or the
    /// selected durable history has invalid identities or tool-result linkage.
    pub fn from_record(
        record: &SessionRecord,
        boundary: TimelineCursor,
    ) -> Result<Self, SessionCopyError> {
        super::metadata::validate_record(record).map_err(|error| SessionCopyError::InvalidSource(error.to_string()))?;
        let effective = record.effective_journal_events();
        let mut latest_cursor = None;
        let mut boundary_exists = false;
        let mut selected = None;

        for (index, event) in effective.iter().enumerate() {
            match event {
                SessionJournalEvent::Timeline(event) => {
                    if event.session_id != record.id {
                        return Err(SessionCopyError::InvalidSource(
                            "timeline event belongs to a different source session".to_owned(),
                        ));
                    }
                    if latest_cursor.is_some_and(|cursor| event.cursor <= cursor) {
                        return Err(SessionCopyError::InvalidSource(
                            "timeline cursors must be strictly increasing".to_owned(),
                        ));
                    }
                    latest_cursor = Some(event.cursor);
                    boundary_exists |= event.cursor == boundary;
                }
                SessionJournalEvent::TurnFinished { turn } if latest_cursor == Some(boundary) => {
                    selected = Some((index, *turn));
                    break;
                }
                _ => {}
            }
        }

        let Some((selected_index, completed_turn)) = selected else {
            return Err(if boundary_exists {
                SessionCopyError::IncompleteBoundary { boundary }
            } else {
                SessionCopyError::BoundaryNotFound { boundary }
            });
        };
        if completed_turn == u64::MAX {
            return Err(SessionCopyError::InvalidSource(
                "completed turn must allow a following destination turn".to_owned(),
            ));
        }
        if boundary.get() == u64::MAX {
            return Err(SessionCopyError::InvalidSource(
                "copy boundary must allow a following timeline cursor".to_owned(),
            ));
        }

        let projected_journal =
            project_journal(&record.id, &effective[..=selected_index], completed_turn)?;
        let ids = projected_journal.iter().filter_map(|event| match event {
            SessionJournalEvent::Timeline(event) => Some(&event.item_id), _ => None,
        }).collect::<std::collections::HashSet<_>>();
        let item_metadata = record.metadata.item_values.iter()
            .filter(|(id, _)| ids.contains(id))
            .map(|(id, values)| (id.clone(), values.clone())).collect();
        Ok(Self {
            source_id: record.id.clone(),
            boundary,
            completed_turn,
            projected_journal,
            item_metadata,
        })
    }

    /// Project mask-effective replayable context through an exact completed-turn cursor.
    ///
    /// # Errors
    ///
    /// Returns the same validation errors as [`Self::from_record`].
    pub fn from_record_effective(
        record: &SessionRecord,
        boundary: TimelineCursor,
    ) -> Result<Self, SessionCopyError> {
        let mut copy = Self::from_record(record, boundary)?;
        for mask in record
            .active_masks()
            .into_iter()
            .filter(|mask| mask.selector.cursor <= boundary)
        {
            if let Some(ids) = &mask.item_ids {
                apply_item_replacement(
                    &mut copy.projected_journal,
                    ids,
                    Some(&mask.replacement.text),
                );
                continue;
            }
            let selected_turn =
                completed_turn_at(record, mask.selector.cursor).ok_or_else(|| {
                    SessionCopyError::InvalidSource(
                        "active mask has no effective completed-turn boundary".to_owned(),
                    )
                })?;
            apply_prefix_replacement(
                &mut copy.projected_journal,
                selected_turn,
                &mask.replacement.text,
            );
        }
        copy.retain_metadata();
        Ok(copy)
    }

    /// Source cursor copied through by this projection.
    #[must_use]
    pub const fn boundary(&self) -> TimelineCursor {
        self.boundary
    }

    /// Source session from which this projection was validated.
    #[must_use]
    pub const fn source_id(&self) -> &SessionId {
        &self.source_id
    }

    /// Select original canonical items, rejecting partial tool pairs and dangling parent links.
    ///
    /// # Errors
    /// Rejects unknown/duplicate IDs, empty selections, and selections exceeding 4096 items.
    pub fn selected(
        record: &SessionRecord,
        boundary: TimelineCursor,
        ids: &[TimelineItemId],
    ) -> Result<Self, SessionCopyError> {
        let selected: std::collections::HashSet<_> = ids.iter().collect();
        if ids.is_empty()
            || ids.len() > 4096
            || selected.len() != ids.len()
            || record.journal.len() > 100_000
            || !super::context::json_within_limit(&record.journal, 16 * 1024 * 1024)
        {
            return Err(SessionCopyError::InvalidSource(
                "invalid selected item set".into(),
            ));
        }
        let mut copy = Self::from_record(record, boundary)?;
        let events = copy.timeline();
        let known: std::collections::HashSet<_> =
            events.iter().map(|event| &event.item_id).collect();
        if !selected.is_subset(&known) {
            return Err(SessionCopyError::InvalidSource(
                "unknown selected item".into(),
            ));
        }
        let calls: HashMap<_, _> = events
            .iter()
            .filter_map(|event| match &event.content {
                TimelineContent::ToolCall { call } => Some((&call.id, &event.item_id)),
                _ => None,
            })
            .collect();
        let results = events
            .iter()
            .filter_map(|event| match &event.content {
                TimelineContent::ToolResult { result } => Some(&result.call_id),
                _ => None,
            })
            .collect::<std::collections::HashSet<_>>();
        let mut user_turns = HashMap::new();
        for event in &events {
            let included = selected.contains(&event.item_id);
            if let TimelineContent::ToolCall { call } = &event.content
                && included
                && !results.contains(&call.id)
            {
                return Err(SessionCopyError::InvalidSource(
                    "selected tool call has no result".into(),
                ));
            }
            if let TimelineContent::ToolResult { result } = &event.content
                && calls
                    .get(&result.call_id)
                    .is_none_or(|id| selected.contains(id) != included)
            {
                return Err(SessionCopyError::InvalidSource(
                    "selection splits a tool call and result".into(),
                ));
            }
            if included
                && event
                    .parent_id
                    .as_ref()
                    .is_some_and(|id| !selected.contains(id))
            {
                return Err(SessionCopyError::InvalidSource(
                    "selection omits a parent item".into(),
                ));
            }
            if !included
                && event
                    .parent_id
                    .as_ref()
                    .is_some_and(|id| selected.contains(id))
            {
                return Err(SessionCopyError::InvalidSource(
                    "selection splits a parent and child".into(),
                ));
            }
            if event
                .channel
                .as_ref()
                .is_some_and(|channel| channel.as_str() == CHANNEL_USER_TEXT)
                && user_turns
                    .insert(event.turn, included)
                    .is_some_and(|previous| previous != included)
            {
                return Err(SessionCopyError::InvalidSource(
                    "selection splits prompts in one turn".into(),
                ));
            }
        }
        let excluded = known
            .into_iter()
            .filter(|id| !selected.contains(id))
            .cloned()
            .collect::<Vec<_>>();
        apply_item_replacement(&mut copy.projected_journal, &excluded, None);
        copy.retain_metadata();
        Ok(copy)
    }

    /// Opaque item values for only the canonical items retained by this projection.
    #[must_use]
    pub fn item_metadata(&self) -> super::ItemMetadata {
        let ids = self.projected_journal.iter().filter_map(|event| match event {
            SessionJournalEvent::Timeline(event) => Some(&event.item_id), _ => None,
        }).collect::<std::collections::HashSet<_>>();
        self.item_metadata.iter().filter(|(id, _)| ids.contains(id))
            .map(|(id, values)| (id.clone(), values.clone())).collect()
    }

    fn retain_metadata(&mut self) {
        let ids = self.projected_journal.iter().filter_map(|event| match event {
            SessionJournalEvent::Timeline(event) => Some(event.item_id.clone()), _ => None,
        }).collect::<std::collections::HashSet<_>>();
        self.item_metadata.retain(|id, _| ids.contains(id));
    }

    /// Privacy-filtered canonical items, retaining first cursors, IDs and tool linkage.
    /// Text updates are coalesced; the replay journal remains unchanged.
    #[must_use]
    pub fn timeline(&self) -> Vec<AgentTimelineEvent> {
        let mut items: Vec<AgentTimelineEvent> = Vec::new();
        let mut positions = HashMap::new();
        for event in &self.projected_journal {
            let SessionJournalEvent::Timeline(event) = event else {
                continue;
            };
            let position = *positions.entry(event.item_id.clone()).or_insert_with(|| {
                items.push(event.clone());
                items.len() - 1
            });
            let item = &mut items[position];
            if item.cursor == event.cursor {
                continue;
            }
            let content = match (&item.content, &event.content) {
                (
                    TimelineContent::TextDelta { delta: text }
                    | TimelineContent::TextSnapshot { text },
                    TimelineContent::TextDelta { delta },
                ) => TimelineContent::TextSnapshot {
                    text: format!("{text}{delta}"),
                },
                (_, content) => content.clone(),
            };
            let cursor = item.cursor;
            *item = event.clone();
            item.cursor = cursor;
            item.content = content;
        }
        items
    }

    pub(crate) const fn next_turn(&self) -> u64 {
        self.completed_turn + 1
    }

    pub(crate) fn materialize(self, destination_id: &SessionId) -> Vec<SessionJournalEvent> {
        self.projected_journal
            .into_iter()
            .map(|mut event| {
                if let SessionJournalEvent::Timeline(timeline) = &mut event {
                    timeline.session_id = destination_id.clone();
                }
                event
            })
            .collect()
    }

    pub(crate) fn encoded_len(&self) -> Result<usize, serde_json::Error> {
        serde_json::to_vec(&(&self.projected_journal, self.item_metadata())).map(|encoded| encoded.len())
    }

    pub(crate) fn context_counts(&self) -> (usize, usize) {
        let snapshot =
            super::SessionSnapshot::from_journal(self.source_id.clone(), &self.projected_journal);
        let turns = snapshot
            .prompt_turns
            .len()
            .saturating_add(snapshot.turns.len());
        let items = snapshot.prompt_turns.len().saturating_add(
            snapshot
                .turns
                .iter()
                .map(|turn| turn.items.len())
                .sum::<usize>(),
        );
        (turns, items)
    }
}

pub(super) fn apply_prefix_replacement(
    journal: &mut Vec<SessionJournalEvent>,
    selected_turn: u64,
    replacement: &str,
) {
    let mut projected = Vec::with_capacity(journal.len());
    let mut turn = 0_u64;
    let mut inserted = false;
    for event in journal.drain(..) {
        let selected = match &event {
            SessionJournalEvent::PromptAppended { .. } => turn <= selected_turn,
            SessionJournalEvent::Timeline(event) => event.turn <= selected_turn,
            SessionJournalEvent::ProviderThinkingBlockRecorded { turn, .. }
            | SessionJournalEvent::ProviderTurnMetadataRecorded { turn, .. } => {
                *turn <= selected_turn
            }
            _ => false,
        };
        if selected {
            if !inserted {
                projected.push(SessionJournalEvent::PromptAppended {
                    prompt: replacement.to_owned(),
                });
                inserted = true;
            }
        } else {
            projected.push(event.clone());
        }
        if let SessionJournalEvent::TurnFinished { turn: finished } = event {
            turn = turn.max(finished.saturating_add(1));
        }
    }
    *journal = projected;
}

pub(super) fn apply_item_replacement(
    journal: &mut Vec<SessionJournalEvent>,
    ids: &[TimelineItemId],
    replacement: Option<&str>,
) {
    let selected: std::collections::HashSet<_> = ids.iter().collect();
    let affected_turns: std::collections::HashSet<_> = journal
        .iter()
        .filter_map(|event| match event {
            SessionJournalEvent::Timeline(event) if selected.contains(&event.item_id) => {
                Some(event.turn)
            }
            _ => None,
        })
        .collect();
    let user_turns: std::collections::HashSet<_> = journal
        .iter()
        .filter_map(|event| match event {
            SessionJournalEvent::Timeline(event)
                if selected.contains(&event.item_id)
                    && event
                        .channel
                        .as_ref()
                        .is_some_and(|channel| channel.as_str() == CHANNEL_USER_TEXT) =>
            {
                Some(event.turn)
            }
            _ => None,
        })
        .collect();
    let mut turn = 0;
    let mut inserted = false;
    let mut projected = Vec::with_capacity(journal.len());
    for event in journal.drain(..) {
        let remove = match &event {
            SessionJournalEvent::Timeline(event) => selected.contains(&event.item_id),
            SessionJournalEvent::PromptAppended { .. } => user_turns.contains(&turn),
            SessionJournalEvent::ProviderThinkingBlockRecorded { turn, .. }
            | SessionJournalEvent::ProviderTurnMetadataRecorded { turn, .. } => {
                affected_turns.contains(turn)
            }
            _ => false,
        };
        if remove {
            if !inserted
                && matches!(
                    event,
                    SessionJournalEvent::Timeline(_) | SessionJournalEvent::PromptAppended { .. }
                )
            {
                if let Some(text) = replacement {
                    projected.push(SessionJournalEvent::PromptAppended {
                        prompt: text.to_owned(),
                    });
                }
                inserted = true;
            }
        } else {
            projected.push(event.clone());
        }
        if let SessionJournalEvent::TurnFinished { turn: finished } = event {
            turn = turn.max(finished.saturating_add(1));
        }
    }
    *journal = projected;
}

/// Failure to derive a session copy from durable history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCopyError {
    /// No effective timeline event has the requested cursor.
    BoundaryNotFound {
        /// Requested source cursor.
        boundary: TimelineCursor,
    },
    /// The requested cursor is followed by more work before its turn completes.
    IncompleteBoundary {
        /// Requested source cursor.
        boundary: TimelineCursor,
    },
    /// A copied item's first occurrence does not use the identity derived from its cursor.
    NonCanonicalItemIdentity {
        /// Malformed copied item identity.
        item_id: TimelineItemId,
        /// Cursor of the item's first copied occurrence.
        first_cursor: TimelineCursor,
    },
    /// A copied item references itself as its parent.
    SelfParent {
        /// Self-parented copied item identity.
        item_id: TimelineItemId,
    },
    /// The selected source journal violates durable replay invariants.
    InvalidSource(String),
}

impl fmt::Display for SessionCopyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BoundaryNotFound { boundary } => {
                write!(
                    formatter,
                    "timeline cursor {} is not effective",
                    boundary.get()
                )
            }
            Self::IncompleteBoundary { boundary } => write!(
                formatter,
                "timeline cursor {} is not a completed-turn boundary",
                boundary.get()
            ),
            Self::NonCanonicalItemIdentity {
                item_id,
                first_cursor,
            } => write!(
                formatter,
                "timeline item {item_id} must be derived from its first cursor {}",
                first_cursor.get()
            ),
            Self::SelfParent { item_id } => {
                write!(formatter, "timeline item {item_id} must not parent itself")
            }
            Self::InvalidSource(message) => formatter.write_str(message),
        }
    }
}

impl Error for SessionCopyError {}

fn project_journal(
    source_id: &SessionId,
    selected: &[&SessionJournalEvent],
    completed_turn: u64,
) -> Result<Vec<SessionJournalEvent>, SessionCopyError> {
    let mut projected = Vec::new();
    let mut source_items = HashMap::new();
    for event in selected {
        let SessionJournalEvent::Timeline(timeline) = event else {
            continue;
        };
        if source_items.contains_key(&timeline.item_id) {
            continue;
        }
        if timeline.item_id != TimelineItemId::from_first_cursor(timeline.cursor) {
            return Err(SessionCopyError::NonCanonicalItemIdentity {
                item_id: timeline.item_id.clone(),
                first_cursor: timeline.cursor,
            });
        }
        source_items.insert(timeline.item_id.clone(), timeline.cursor);
    }

    let mut projected_items = std::collections::HashSet::new();
    let mut tool_calls = HashMap::new();
    let mut tool_results = std::collections::HashSet::new();

    for event in selected {
        match event {
            SessionJournalEvent::PromptAppended { .. }
            | SessionJournalEvent::TurnFinished { .. } => projected.push((*event).clone()),
            SessionJournalEvent::Timeline(timeline)
                if matches!(&timeline.content, TimelineContent::ContentRef { .. })
                    && timeline.channel.as_ref().is_some_and(|channel| {
                        matches!(channel.as_str(), CHANNEL_USER_TEXT | CHANNEL_ASSISTANT_TEXT)
                    }) =>
            {
                return Err(SessionCopyError::InvalidSource(
                    "copied text is stored in an unresolved content reference".to_owned(),
                ));
            }
            SessionJournalEvent::Timeline(timeline) if replayable_timeline(timeline) => {
                if timeline.session_id != *source_id || timeline.turn > completed_turn {
                    return Err(SessionCopyError::InvalidSource(
                        "copied timeline identity extends outside the selected source".to_owned(),
                    ));
                }
                if timeline.parent_id.as_ref() == Some(&timeline.item_id) {
                    return Err(SessionCopyError::SelfParent {
                        item_id: timeline.item_id.clone(),
                    });
                }
                projected_items.insert(timeline.item_id.clone());
                if let Some(parent_id) = &timeline.parent_id
                    && !projected_items.contains(parent_id)
                {
                    return Err(SessionCopyError::InvalidSource(
                        "timeline parent must precede its copied child".to_owned(),
                    ));
                }
                match &timeline.content {
                    TimelineContent::ToolCall { call } => {
                        if tool_calls
                            .insert(call.id.clone(), timeline.item_id.clone())
                            .is_some()
                        {
                            return Err(SessionCopyError::InvalidSource(
                                "copied tool-call ids must be unique".to_owned(),
                            ));
                        }
                    }
                    TimelineContent::ToolResult { result } => {
                        let Some(call_item) = tool_calls.get(&result.call_id) else {
                            return Err(SessionCopyError::InvalidSource(
                                "copied tool result has no preceding tool call".to_owned(),
                            ));
                        };
                        if timeline.parent_id.as_ref() != Some(call_item) {
                            return Err(SessionCopyError::InvalidSource(
                                "copied tool result parent does not match its tool call".to_owned(),
                            ));
                        }
                        if !tool_results.insert(result.call_id.clone()) {
                            return Err(SessionCopyError::InvalidSource(
                                "copied tool results must be unique per tool call".to_owned(),
                            ));
                        }
                    }
                    TimelineContent::TextDelta { .. } | TimelineContent::TextSnapshot { .. } => {}
                    TimelineContent::Empty
                    | TimelineContent::ContentRef { .. }
                    | TimelineContent::Json { .. } => unreachable!("timeline was allowlisted"),
                }
                projected.push((*event).clone());
            }
            SessionJournalEvent::Timeline(_)
            | SessionJournalEvent::ProviderRequestRecorded { .. }
            | SessionJournalEvent::SandboxDispatchStarted { .. }
            | SessionJournalEvent::SandboxCallRecorded { .. }
            | SessionJournalEvent::ProviderThinkingBlockRecorded { .. }
            | SessionJournalEvent::ProviderTurnMetadataRecorded { .. }
            | SessionJournalEvent::MiddlewareStateUpdated { .. }
            | SessionJournalEvent::Suspended { .. }
            | SessionJournalEvent::WorkCanceled { .. }
            | SessionJournalEvent::Interrupted
            | SessionJournalEvent::TurnInterrupted { .. }
            | SessionJournalEvent::MasksMutated { .. }
            | SessionJournalEvent::Rollback { .. }
            | SessionJournalEvent::Finished
            | SessionJournalEvent::Failed { .. } => {}
        }
    }
    Ok(projected)
}

fn replayable_timeline(event: &AgentTimelineEvent) -> bool {
    if !matches!(
        event.kind,
        TimelineEventKind::ItemStarted
            | TimelineEventKind::ContentDelta
            | TimelineEventKind::ItemUpdated
            | TimelineEventKind::ItemFinished
    ) {
        return false;
    }
    match &event.content {
        TimelineContent::TextDelta { .. } | TimelineContent::TextSnapshot { .. } => {
            event.channel.as_ref().is_some_and(|channel| {
                matches!(channel.as_str(), CHANNEL_USER_TEXT | CHANNEL_ASSISTANT_TEXT)
            })
        }
        TimelineContent::ToolCall { .. } => event
            .channel
            .as_ref()
            .is_some_and(|channel| channel.as_str() == CHANNEL_TOOL_CALL),
        TimelineContent::ToolResult { .. } => event
            .channel
            .as_ref()
            .is_some_and(|channel| channel.as_str() == CHANNEL_TOOL_RESULT),
        TimelineContent::Empty
        | TimelineContent::ContentRef { .. }
        | TimelineContent::Json { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        PendingTimelineEvent, PendingTimelineItemId, SessionCreateRequest, SessionStart,
        SessionStartMessage, TimelineEventKind, TimelineItemId, TimelineStatus, ToolCall,
        ToolResult, ToolResultContent, ToolResultContentBlock, ToolResultImage,
        agent::session::SessionRollbackTarget,
    };

    fn assigned(cursor: u64, pending: PendingTimelineEvent) -> SessionJournalEvent {
        SessionJournalEvent::Timeline(pending.with_cursor(TimelineCursor::new(cursor)))
    }

    fn completed_source() -> SessionRecord {
        let mut record = SessionRecord::new("source");
        let call = ToolCall::new("call-1", "inspect", json!({ "path": "image.png" }));
        let result = ToolResult::from_content(
            "call-1",
            "inspect",
            ToolResultContent::from_blocks(vec![
                ToolResultContentBlock::Text {
                    text: "result".to_owned(),
                },
                ToolResultContentBlock::Image(
                    ToolResultImage::new("iVBORw0KGgo=", "image/png").unwrap(),
                ),
            ])
            .unwrap(),
            false,
        );
        let mut status = PendingTimelineEvent::new(
            record.id.clone(),
            0,
            PendingTimelineItemId::New,
            TimelineEventKind::SessionStatusChanged,
            None,
            TimelineContent::Empty,
        );
        status.status = Some(TimelineStatus::Idle);
        record.journal = vec![
            SessionJournalEvent::PromptAppended {
                prompt: "question".to_owned(),
            },
            assigned(
                1,
                PendingTimelineEvent::user_text_snapshot(
                    record.id.clone(),
                    0,
                    PendingTimelineItemId::New,
                    "question",
                ),
            ),
            assigned(
                2,
                PendingTimelineEvent::thinking_delta(
                    record.id.clone(),
                    0,
                    PendingTimelineItemId::New,
                    "private reasoning",
                ),
            ),
            assigned(
                4,
                PendingTimelineEvent::assistant_text_snapshot(
                    record.id.clone(),
                    0,
                    PendingTimelineItemId::New,
                    "answer",
                ),
            ),
            assigned(
                7,
                PendingTimelineEvent::tool_call(
                    record.id.clone(),
                    0,
                    PendingTimelineItemId::New,
                    call,
                ),
            ),
            assigned(
                9,
                PendingTimelineEvent::tool_result(
                    record.id.clone(),
                    0,
                    PendingTimelineItemId::New,
                    TimelineItemId::from_first_cursor(TimelineCursor::new(7)),
                    result,
                ),
            ),
            SessionJournalEvent::ProviderThinkingBlockRecorded {
                turn: 0,
                block: crate::ThinkingBlock {
                    kind: crate::ThinkingBlockKind::OpenAiResponses,
                    text: "private reasoning".to_owned(),
                    payload: json!({ "provider": "opaque" }),
                },
            },
            assigned(10, status),
            SessionJournalEvent::TurnFinished { turn: 0 },
            assigned(
                11,
                PendingTimelineEvent::assistant_text_delta(
                    record.id.clone(),
                    1,
                    PendingTimelineItemId::New,
                    "active partial",
                ),
            ),
        ];
        record
    }

    #[test]
    fn copy_preserves_replay_identities_and_allocates_after_exact_boundary() {
        let source = completed_source();
        let copy = SessionCopy::from_record(&source, TimelineCursor::new(10)).unwrap();
        let mut request = SessionCreateRequest::new("destination");
        request.start = SessionStart::Copy {
            copy,
            messages: vec![SessionStartMessage::user("continue").unwrap()],
        };

        let record = request.materialize();
        let timeline = record.timeline_events().collect::<Vec<_>>();
        assert_eq!(record.revision, super::super::Revision::ZERO);
        assert_eq!(record.timeline_cursor_high_water(), TimelineCursor::new(11));
        assert_eq!(
            timeline
                .iter()
                .map(|event| event.cursor.get())
                .collect::<Vec<_>>(),
            [1, 4, 7, 9, 11]
        );
        assert_eq!(
            timeline
                .iter()
                .map(|event| event.item_id.as_str())
                .collect::<Vec<_>>(),
            ["i1", "i4", "i7", "i9", "i11"]
        );
        assert!(
            timeline
                .iter()
                .all(|event| event.session_id.as_str() == "destination")
        );
        assert_eq!(timeline[3].parent_id.as_ref(), Some(&timeline[2].item_id));
        let TimelineContent::ToolResult { result } = &timeline[3].content else {
            panic!("copied tool result");
        };
        assert_eq!(result.content.images().count(), 1);
        assert_eq!(record.snapshot().prompt_turns[1].turn, 1);
        assert!(record.snapshot().thinking_blocks().next().is_none());
    }

    #[test]
    fn original_selection_ignores_effective_masks_and_retains_tool_pairs() {
        let mut source = completed_source();
        source.journal.pop();
        for event in &mut source.journal {
            if let SessionJournalEvent::Timeline(event) = event
                && event.item_id.as_str() == "i4"
            {
                event.content = TimelineContent::TextSnapshot {
                    text: "界".repeat(1000),
                };
            }
        }
        let ids =
            [7, 9].map(|cursor| TimelineItemId::from_first_cursor(TimelineCursor::new(cursor)));
        let mut mutation = crate::SessionMaskMutation::new(Some(TimelineCursor::new(10)));
        mutation.add.push(crate::SessionMaskAddition::items(
            "tools",
            TimelineCursor::new(10),
            ids.to_vec(),
            crate::ReplacementUserContent::new("active tool summary"),
        ));
        let revision = source.revision.next();
        super::super::mask::mutate_record_masks(&mut source, revision, &mutation).unwrap();
        let selected = SessionCopy::selected(
            &source,
            TimelineCursor::new(10),
            &ids,
        )
        .unwrap();
        assert_eq!(
            selected
                .timeline()
                .iter()
                .map(|event| event.item_id.as_str())
                .collect::<Vec<_>>(),
            ["i7", "i9"]
        );
        let mut request = SessionCreateRequest::new("helper");
        request.start = SessionStart::Copy {
            copy: selected,
            messages: Vec::new(),
        };
        let helper = request.materialize();
        let timeline = helper.timeline_events().collect::<Vec<_>>();
        assert_eq!(timeline[1].parent_id.as_ref(), Some(&timeline[0].item_id));
    }

    #[test]
    fn selected_items_retain_tool_pair_ids_and_reject_split_unknown_or_private_items() {
        let source = completed_source();
        let ids =
            [7, 9].map(|cursor| TimelineItemId::from_first_cursor(TimelineCursor::new(cursor)));
        let selected = SessionCopy::selected(&source, TimelineCursor::new(10), &ids).unwrap();
        let events = selected.timeline();
        assert_eq!(
            events
                .iter()
                .map(|event| event.item_id.as_str())
                .collect::<Vec<_>>(),
            ["i7", "i9"]
        );
        assert_eq!(events[1].parent_id.as_ref(), Some(&events[0].item_id));
        for ids in [
            vec![ids[0].clone()],
            vec![ids[1].clone()],
            vec![ids[0].clone(), ids[0].clone()],
            vec![TimelineItemId::from_first_cursor(TimelineCursor::new(2))],
            vec![TimelineItemId::from_first_cursor(TimelineCursor::new(99))],
            Vec::new(),
        ] {
            assert!(SessionCopy::selected(&source, TimelineCursor::new(10), &ids).is_err());
        }
    }

    #[test]
    fn item_masks_share_effective_materialization_copy_helper_replay_and_rollback() {
        use crate::{
            ReplacementUserContent, SessionContextMode, SessionMaskAddition, SessionMaskMutation,
        };
        let mut source = completed_source();
        source.journal.pop();
        let journal_len = source.journal.len();
        let raw = source.materialize_context(SessionContextMode::Raw);
        let mut mutation = SessionMaskMutation::new(Some(TimelineCursor::new(10)));
        mutation.add.push(SessionMaskAddition::items(
            "tools",
            TimelineCursor::new(10),
            [7, 9]
                .map(|cursor| TimelineItemId::from_first_cursor(TimelineCursor::new(cursor)))
                .to_vec(),
            ReplacementUserContent::new("tool summary"),
        ));
        let next_revision = source.revision.next();
        super::super::mask::mutate_record_masks(&mut source, next_revision, &mutation).unwrap();
        let effective = source.materialize_context(SessionContextMode::Effective);
        assert!(
            effective
                .turns
                .iter()
                .all(|turn| turn.tool_calls().next().is_none()
                    && turn.tool_results().next().is_none()
                    && turn.thinking_blocks().next().is_none())
        );
        let copy = SessionCopy::from_record_effective(&source, TimelineCursor::new(10)).unwrap();
        assert!(copy.timeline().iter().all(|event| !matches!(
            event.content,
            TimelineContent::ToolCall { .. } | TimelineContent::ToolResult { .. }
        )));
        let mut request = SessionCreateRequest::new("helper");
        request.start = SessionStart::Copy {
            copy,
            messages: Vec::new(),
        };
        let helper = request.materialize();
        let helper_context = helper.materialize_context(SessionContextMode::Raw);
        assert_eq!(effective, helper_context);
        assert_eq!(source.materialize_context(SessionContextMode::Raw), raw);
        let encoded = serde_json::to_string(&helper).unwrap();
        assert!(!encoded.contains("private reasoning"));
        assert!(!encoded.contains("image/png"));
        assert!(encoded.contains("tool summary"));
        source.append_journal_event(SessionJournalEvent::Rollback {
            target: SessionRollbackTarget {
                journal_len: journal_len as u64,
                remove_prompt_index: None,
                after_cursor: Some(TimelineCursor::new(10)),
                next_turn: 1,
            },
        });
        assert_eq!(
            source.materialize_context(SessionContextMode::Effective),
            raw
        );
    }

    #[test]
    fn explicit_item_overlap_and_expected_mask_id_fail_atomically() {
        use crate::{
            ReplacementUserContent, SessionMaskAddition, SessionMaskError, SessionMaskMutation,
        };
        let mut source = completed_source();
        source.journal.pop();
        let make = |key, ids: Vec<u64>| {
            SessionMaskAddition::items(
                key,
                TimelineCursor::new(10),
                ids.into_iter()
                    .map(|cursor| TimelineItemId::from_first_cursor(TimelineCursor::new(cursor)))
                    .collect(),
                ReplacementUserContent::new("summary"),
            )
        };
        let mut mutation = SessionMaskMutation::new(Some(TimelineCursor::new(10)));
        mutation.add.push(make("tools", vec![7, 9]));
        let result = super::super::mask::mutate_record_masks(
            &mut source,
            crate::Revision::new(1),
            &mutation,
        )
        .unwrap();
        let before = source.clone();
        mutation.add = vec![make("overlap", vec![7, 9])];
        assert!(matches!(
            super::super::mask::mutate_record_masks(
                &mut source,
                crate::Revision::new(2),
                &mutation
            ),
            Err(SessionMaskError::ReplacementOverlap { .. })
        ));
        assert_eq!(source, before);
        mutation.add = vec![make("separate", vec![4])];
        super::super::mask::mutate_record_masks(&mut source, crate::Revision::new(2), &mutation)
            .unwrap();
        let before = source.clone();
        mutation.expected_masks = vec![("tools".into(), None)];
        mutation.subtract.keys = vec!["tools".into()];
        mutation.add = vec![make("tools", vec![7, 9])];
        assert!(matches!(
            super::super::mask::mutate_record_masks(
                &mut source,
                crate::Revision::new(3),
                &mutation
            ),
            Err(SessionMaskError::MaskIdentityMismatch { .. })
        ));
        assert_eq!(source, before);
        mutation.expected_masks = vec![("tools".into(), Some(result.added_masks[0].id.clone()))];
        super::super::mask::mutate_record_masks(&mut source, crate::Revision::new(3), &mutation)
            .unwrap();
        assert_ne!(
            source
                .active_masks()
                .iter()
                .find(|mask| mask.key == "tools")
                .unwrap()
                .id,
            result.added_masks[0].id
        );
    }

    #[test]
    fn legacy_prefix_masks_round_trip_without_item_selector_field() {
        use crate::{
            ReplacementUserContent, SessionContextMode, SessionMaskAddition, SessionMaskMutation,
            ThroughCursor,
        };
        let mut source = completed_source();
        source.journal.pop();
        let mut mutation = SessionMaskMutation::new(Some(TimelineCursor::new(10)));
        mutation.add.push(SessionMaskAddition::new(
            "prefix",
            ThroughCursor::new(TimelineCursor::new(10)),
            ReplacementUserContent::new("legacy summary"),
        ));
        super::super::mask::mutate_record_masks(&mut source, crate::Revision::new(1), &mutation)
            .unwrap();
        let mut json = serde_json::to_value(&source).unwrap();
        json["metadata"]
            .as_object_mut()
            .unwrap()
            .remove("incarnation");
        assert!(!serde_json::to_string(&json).unwrap().contains("item_ids"));
        let legacy: SessionRecord = serde_json::from_value(json).unwrap();
        assert_eq!(
            legacy.materialize_context(SessionContextMode::Effective),
            source.materialize_context(SessionContextMode::Effective)
        );
        assert_eq!(
            SessionCopy::from_record_effective(&legacy, TimelineCursor::new(10)).unwrap(),
            SessionCopy::from_record_effective(&source, TimelineCursor::new(10)).unwrap()
        );
    }

    #[test]
    fn copy_rejects_inside_and_active_partial_boundaries_without_clamping() {
        let source = completed_source();
        assert!(matches!(
            SessionCopy::from_record(&source, TimelineCursor::new(4)),
            Err(SessionCopyError::IncompleteBoundary { .. })
        ));
        assert!(matches!(
            SessionCopy::from_record(&source, TimelineCursor::new(11)),
            Err(SessionCopyError::IncompleteBoundary { .. })
        ));
        assert!(matches!(
            SessionCopy::from_record(&source, TimelineCursor::new(99)),
            Err(SessionCopyError::BoundaryNotFound { .. })
        ));
    }

    #[test]
    fn copy_rejects_item_identity_that_can_collide_after_the_boundary() {
        let mut source = completed_source();
        let event = source
            .journal
            .iter_mut()
            .find_map(|event| match event {
                SessionJournalEvent::Timeline(event) if event.cursor == TimelineCursor::new(4) => {
                    Some(event)
                }
                _ => None,
            })
            .unwrap();
        event.item_id = TimelineItemId::from_first_cursor(TimelineCursor::new(11));

        assert!(matches!(
            SessionCopy::from_record(&source, TimelineCursor::new(10)),
            Err(SessionCopyError::NonCanonicalItemIdentity {
                item_id,
                first_cursor: TimelineCursor(4),
            }) if item_id.as_str() == "i11"
        ));
    }

    #[test]
    fn canonical_items_coalesce_streamed_text_and_final_snapshot_without_changing_replay() {
        let mut source = SessionRecord::new("streamed-copy");
        let item_id = TimelineItemId::from_first_cursor(TimelineCursor::new(1));
        for (cursor, kind, content) in [
            (
                1,
                TimelineEventKind::ContentDelta,
                TimelineContent::TextDelta {
                    delta: "first ".into(),
                },
            ),
            (
                2,
                TimelineEventKind::ContentDelta,
                TimelineContent::TextDelta {
                    delta: "second".into(),
                },
            ),
            (
                3,
                TimelineEventKind::ItemFinished,
                TimelineContent::TextSnapshot {
                    text: "first second".into(),
                },
            ),
        ] {
            source.journal.push(assigned(
                cursor,
                PendingTimelineEvent::new(
                    source.id.clone(),
                    0,
                    if cursor == 1 {
                        PendingTimelineItemId::New
                    } else {
                        PendingTimelineItemId::Existing(item_id.clone())
                    },
                    kind,
                    Some(crate::TimelineChannel::from(CHANNEL_ASSISTANT_TEXT)),
                    content,
                ),
            ));
        }
        source
            .journal
            .push(SessionJournalEvent::TurnFinished { turn: 0 });
        let copy = SessionCopy::from_record(&source, TimelineCursor::new(3)).unwrap();
        assert_eq!(copy.projected_journal, source.journal);
        let items = copy.timeline();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item_id, item_id);
        assert_eq!(items[0].cursor, TimelineCursor::new(1));
        assert!(
            matches!(&items[0].content, TimelineContent::TextSnapshot { text } if text == "first second")
        );
        let replay = copy.materialize(&SessionId::new("destination"));
        assert_eq!(replay.len(), source.journal.len());
    }

    #[test]
    fn copy_preserves_existing_item_after_excluded_canonical_introduction() {
        let mut source = SessionRecord::new("excluded-introduction-source");
        let item_id = TimelineItemId::from_first_cursor(TimelineCursor::new(1));
        source.journal = vec![
            assigned(
                1,
                PendingTimelineEvent::new(
                    source.id.clone(),
                    0,
                    PendingTimelineItemId::New,
                    TimelineEventKind::ItemStarted,
                    None,
                    TimelineContent::Empty,
                ),
            ),
            assigned(
                2,
                PendingTimelineEvent::new(
                    source.id.clone(),
                    0,
                    PendingTimelineItemId::Existing(item_id.clone()),
                    TimelineEventKind::ItemFinished,
                    Some(crate::TimelineChannel::from(CHANNEL_ASSISTANT_TEXT)),
                    TimelineContent::TextSnapshot {
                        text: "preserved".to_owned(),
                    },
                ),
            ),
            SessionJournalEvent::TurnFinished { turn: 0 },
        ];
        let mut request = SessionCreateRequest::new("excluded-introduction-destination");
        request.start = SessionStart::Copy {
            copy: SessionCopy::from_record(&source, TimelineCursor::new(2)).unwrap(),
            messages: Vec::new(),
        };

        let record = request.materialize();
        let timeline = record.timeline_events().collect::<Vec<_>>();

        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline[0].cursor, TimelineCursor::new(2));
        assert_eq!(timeline[0].item_id, item_id);
        assert!(matches!(
            &timeline[0].content,
            TimelineContent::TextSnapshot { text } if text == "preserved"
        ));
    }

    #[test]
    fn copy_rejects_self_parent_tool_result_with_valid_call_linkage() {
        let mut source = SessionRecord::new("self-parent-source");
        let item_id = TimelineItemId::from_first_cursor(TimelineCursor::new(1));
        source.journal = vec![
            assigned(
                1,
                PendingTimelineEvent::tool_call(
                    source.id.clone(),
                    0,
                    PendingTimelineItemId::New,
                    ToolCall::new("call-1", "lookup", json!({})),
                ),
            ),
            assigned(
                2,
                PendingTimelineEvent::tool_result(
                    source.id.clone(),
                    0,
                    PendingTimelineItemId::Existing(item_id.clone()),
                    item_id.clone(),
                    ToolResult::new("call-1", "lookup", "result"),
                ),
            ),
            SessionJournalEvent::TurnFinished { turn: 0 },
        ];

        assert!(matches!(
            SessionCopy::from_record(&source, TimelineCursor::new(2)),
            Err(SessionCopyError::SelfParent { item_id: rejected }) if rejected == item_id
        ));
    }

    #[test]
    fn copy_applies_rollback_before_boundary_selection() {
        let mut source = completed_source();
        source.journal.truncate(9);
        let rollback_len = source.journal.len() as u64;
        source.journal.extend([
            assigned(
                12,
                PendingTimelineEvent::assistant_text_snapshot(
                    source.id.clone(),
                    1,
                    PendingTimelineItemId::New,
                    "rolled back",
                ),
            ),
            SessionJournalEvent::TurnFinished { turn: 1 },
            SessionJournalEvent::Rollback {
                target: SessionRollbackTarget {
                    journal_len: rollback_len,
                    after_cursor: Some(TimelineCursor::new(10)),
                    next_turn: 1,
                    remove_prompt_index: None,
                },
            },
            assigned(
                14,
                PendingTimelineEvent::assistant_text_snapshot(
                    source.id.clone(),
                    1,
                    PendingTimelineItemId::New,
                    "kept retry",
                ),
            ),
            SessionJournalEvent::TurnFinished { turn: 1 },
        ]);

        assert!(matches!(
            SessionCopy::from_record(&source, TimelineCursor::new(12)),
            Err(SessionCopyError::BoundaryNotFound { .. })
        ));
        assert_eq!(
            SessionCopy::from_record(&source, TimelineCursor::new(14))
                .unwrap()
                .boundary(),
            TimelineCursor::new(14)
        );
    }

    #[test]
    fn effective_copy_replaces_selected_prefix_without_changing_raw_copy() {
        let mut source = completed_source();
        let mut mutation = super::super::SessionMaskMutation::new(Some(TimelineCursor::new(11)));
        mutation.add.push(super::super::SessionMaskAddition::new(
            "context.compaction-prefix",
            super::super::ThroughCursor::new(TimelineCursor::new(10)),
            super::super::ReplacementUserContent::new("compact summary"),
        ));
        super::super::mask::mutate_record_masks(
            &mut source,
            super::super::Revision::new(1),
            &mutation,
        )
        .unwrap();

        let raw = SessionCopy::from_record(&source, TimelineCursor::new(10)).unwrap();
        let effective =
            SessionCopy::from_record_effective(&source, TimelineCursor::new(10)).unwrap();
        let mut raw_request = SessionCreateRequest::new("raw-destination");
        raw_request.start = SessionStart::Copy {
            copy: raw,
            messages: Vec::new(),
        };
        let mut effective_request = SessionCreateRequest::new("effective-destination");
        effective_request.start = SessionStart::Copy {
            copy: effective,
            messages: Vec::new(),
        };

        assert_eq!(raw_request.materialize().snapshot().prompts, ["question"]);
        let effective_snapshot = effective_request.materialize().snapshot();
        assert_eq!(effective_snapshot.prompts, ["compact summary"]);
        assert!(effective_snapshot.turns.is_empty());
    }

    #[test]
    fn effective_copy_ignores_masks_newer_than_requested_boundary() {
        let mut source = SessionRecord::new("boundary-source");
        source.journal = vec![
            SessionJournalEvent::PromptAppended {
                prompt: "first question".to_owned(),
            },
            assigned(
                1,
                PendingTimelineEvent::assistant_text_snapshot(
                    source.id.clone(),
                    0,
                    PendingTimelineItemId::New,
                    "first answer",
                ),
            ),
            SessionJournalEvent::TurnFinished { turn: 0 },
            SessionJournalEvent::PromptAppended {
                prompt: "second question".to_owned(),
            },
            assigned(
                2,
                PendingTimelineEvent::assistant_text_snapshot(
                    source.id.clone(),
                    1,
                    PendingTimelineItemId::New,
                    "second answer",
                ),
            ),
            SessionJournalEvent::TurnFinished { turn: 1 },
        ];
        let mut mutation = super::super::SessionMaskMutation::new(Some(TimelineCursor::new(2)));
        mutation.add.push(super::super::SessionMaskAddition::new(
            "later-summary",
            super::super::ThroughCursor::new(TimelineCursor::new(2)),
            super::super::ReplacementUserContent::new("future-derived summary"),
        ));
        super::super::mask::mutate_record_masks(
            &mut source,
            super::super::Revision::new(1),
            &mutation,
        )
        .unwrap();

        let copy = SessionCopy::from_record_effective(&source, TimelineCursor::new(1)).unwrap();
        let mut request = SessionCreateRequest::new("earlier-destination");
        request.start = SessionStart::Copy {
            copy,
            messages: Vec::new(),
        };
        let snapshot = request.materialize().snapshot();

        assert_eq!(snapshot.prompts, ["first question"]);
        assert_eq!(
            snapshot.turns[0].assistant_texts().collect::<Vec<_>>(),
            ["first answer"]
        );
        assert!(
            !snapshot
                .prompts
                .iter()
                .any(|prompt| prompt.contains("future-derived"))
        );
    }
}
