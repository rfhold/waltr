//! Agent session storage interface.

#![allow(async_fn_in_trait, missing_docs)]

mod filesystem;
mod memory;
mod opendal;
mod postgresql;
mod s3;
mod scylla;

use std::{
    collections::{BTreeMap, HashSet},
    error::Error,
    fmt,
    future::Future,
};

pub use filesystem::FilesystemSessionStore;
pub use memory::MemorySessionStore;
pub use opendal::OpenDalSessionStore;
pub use postgresql::{opendal_postgresql_session_store, run_postgresql_migrations};
pub use s3::{S3SessionStore, S3SessionWriteMode};
pub use scylla::ScyllaSessionStore;

use crate::{
    definition::AgentDefinitionBundle,
    timeline::{
        AgentTimelineEvent, PendingTimelineEvent, PendingTimelineItemId, TimelineContent,
        TimelineCursor, TimelineDirection, TimelineItemId, TimelineItemText, TimelineQuery,
    },
};
use serde_json::Value;

use super::{
    SessionCreateRequest,
    types::{
        MetadataSetCondition, MetadataSetOutcome, Revision, SessionId, SessionJournalEvent,
        SessionRecord, SessionSummary,
    },
};

/// Result type returned by session stores.
pub type SessionStoreResult<T> = Result<T, SessionStoreError>;

/// Storage failure returned by session stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStoreError {
    /// The requested session does not exist.
    NotFound { id: SessionId },
    /// A session with the requested id already exists.
    AlreadyExists { id: SessionId },
    /// The caller expected a stale revision.
    StaleRevision {
        id: SessionId,
        expected: Revision,
        actual: Revision,
    },
    /// The record being saved belongs to a different session.
    SessionIdMismatch {
        expected: SessionId,
        actual: SessionId,
    },
    /// The in-process store lock was poisoned.
    LockPoisoned,
    /// The backend returned an implementation-specific error.
    Backend { message: String },
    /// The requested timeline item does not exist in the session timeline.
    TimelineItemNotFound {
        /// Session containing the timeline.
        id: SessionId,
        /// Missing item identity.
        item_id: TimelineItemId,
    },
    /// The requested timeline content cannot be reconstructed by this store contract.
    TimelineContentUnavailable {
        /// Session containing the timeline.
        id: SessionId,
        /// Timeline item identity.
        item_id: TimelineItemId,
        /// Human-readable reason.
        reason: String,
    },
}

impl fmt::Display for SessionStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { id } => write!(formatter, "session not found: {}", id.as_str()),
            Self::AlreadyExists { id } => {
                write!(formatter, "session already exists: {}", id.as_str())
            }
            Self::StaleRevision {
                id,
                expected,
                actual,
            } => write!(
                formatter,
                "stale session revision for {}: expected {}, actual {}",
                id.as_str(),
                expected.get(),
                actual.get()
            ),
            Self::SessionIdMismatch { expected, actual } => write!(
                formatter,
                "session id mismatch: expected {}, actual {}",
                expected.as_str(),
                actual.as_str()
            ),
            Self::LockPoisoned => formatter.write_str("session store lock poisoned"),
            Self::Backend { message } => {
                write!(formatter, "session store backend error: {message}")
            }
            Self::TimelineItemNotFound { id, item_id } => write!(
                formatter,
                "timeline item not found in {}: {}",
                id.as_str(),
                item_id.as_str()
            ),
            Self::TimelineContentUnavailable {
                id,
                item_id,
                reason,
            } => write!(
                formatter,
                "timeline content unavailable in {} for {}: {reason}",
                id.as_str(),
                item_id.as_str()
            ),
        }
    }
}

impl Error for SessionStoreError {}

/// Durable agent session storage contract.
pub trait SessionStore: Send + Sync {
    /// Create a complete session record if its id is absent.
    fn create(
        &self,
        request: SessionCreateRequest,
    ) -> impl Future<Output = SessionStoreResult<SessionRecord>> + Send;

    /// Load a session record.
    fn load(
        &self,
        id: &SessionId,
    ) -> impl Future<Output = SessionStoreResult<SessionRecord>> + Send;

    /// Delete a session record if present.
    fn delete(&self, id: &SessionId) -> impl Future<Output = SessionStoreResult<()>> + Send;

    /// Delete every session record without decoding stored data.
    fn wipe(&self) -> impl Future<Output = SessionStoreResult<()>> + Send;

    /// List known sessions, newest activity first.
    fn list(&self) -> impl Future<Output = SessionStoreResult<Vec<SessionSummary>>> + Send;

    /// Append pending timeline events and return them with assigned cursors.
    fn append_timeline(
        &self,
        id: &SessionId,
        expected_revision: Revision,
        events: Vec<PendingTimelineEvent>,
    ) -> impl Future<Output = SessionStoreResult<Vec<AgentTimelineEvent>>> + Send;

    /// Append internal journal events if the revision matches.
    fn append_journal(
        &self,
        id: &SessionId,
        expected_revision: Revision,
        events: Vec<SessionJournalEvent>,
    ) -> impl Future<Output = SessionStoreResult<Revision>> + Send;

    /// Set a host-defined metadata value and return the updated record.
    fn set_metadata_value(
        &self,
        id: &SessionId,
        key: String,
        value: Value,
    ) -> impl Future<Output = SessionStoreResult<SessionRecord>> + Send {
        async move {
            self.set_metadata_value_conditionally(
                id,
                key,
                value,
                MetadataSetCondition::Unconditional,
            )
            .await
            .map(|outcome| outcome.record)
        }
    }

    /// Atomically set metadata according to `condition`.
    fn set_metadata_value_conditionally(
        &self,
        id: &SessionId,
        key: String,
        value: Value,
        condition: MetadataSetCondition,
    ) -> impl Future<Output = SessionStoreResult<MetadataSetOutcome>> + Send;

    /// Remove a host-defined metadata value and return the updated record.
    fn remove_metadata_value(
        &self,
        id: &SessionId,
        key: String,
    ) -> impl Future<Output = SessionStoreResult<SessionRecord>> + Send;

    /// Set inference settings and return the updated record.
    fn set_inference_settings(
        &self,
        id: &SessionId,
        inference_settings: Option<InferenceSettings>,
    ) -> impl Future<Output = SessionStoreResult<SessionRecord>> + Send;

    /// Set the agent-definition bundle on the latest complete record without changing revision.
    fn set_agent_definition(
        &self,
        id: &SessionId,
        agent_definition: Option<AgentDefinitionBundle>,
    ) -> impl Future<Output = SessionStoreResult<SessionRecord>> + Send;

    /// Read timeline events using a bounded store-neutral query.
    fn timeline(
        &self,
        id: &SessionId,
        query: TimelineQuery,
    ) -> impl Future<Output = SessionStoreResult<Vec<AgentTimelineEvent>>> + Send;

    /// Reconstruct text-like content for one timeline item.
    fn timeline_item_text(
        &self,
        id: &SessionId,
        item_id: &TimelineItemId,
    ) -> impl Future<Output = SessionStoreResult<TimelineItemText>> + Send;

    /// Read timeline events after `cursor`, or from the start when no cursor is supplied.
    fn timeline_after(
        &self,
        id: &SessionId,
        cursor: Option<TimelineCursor>,
        limit: usize,
    ) -> impl Future<Output = SessionStoreResult<Vec<AgentTimelineEvent>>> + Send {
        self.timeline(id, TimelineQuery::forward(cursor, limit))
    }

    /// Read timeline events before `cursor`, preserving persisted order.
    fn timeline_before(
        &self,
        id: &SessionId,
        cursor: Option<TimelineCursor>,
        limit: usize,
    ) -> impl Future<Output = SessionStoreResult<Vec<AgentTimelineEvent>>> + Send {
        self.timeline(id, TimelineQuery::backward(cursor, limit))
    }
}

pub(super) fn append_timeline_to_record(
    record: &mut SessionRecord,
    id: &SessionId,
    events: Vec<PendingTimelineEvent>,
) -> SessionStoreResult<Vec<AgentTimelineEvent>> {
    let mut cursor = record.timeline_cursor_high_water();
    let mut effective_item_ids = record
        .timeline_events()
        .map(|event| event.item_id.clone())
        .collect::<HashSet<_>>();
    let mut assigned = Vec::with_capacity(events.len());
    for event in events {
        if event.session_id != *id {
            return Err(SessionStoreError::SessionIdMismatch {
                expected: id.clone(),
                actual: event.session_id,
            });
        }
        cursor = cursor.next();
        if let PendingTimelineItemId::Existing(item_id) = &event.item_id
            && !effective_item_ids.contains(item_id)
        {
            return Err(SessionStoreError::TimelineItemNotFound {
                id: id.clone(),
                item_id: item_id.clone(),
            });
        }
        if let Some(parent_id) = &event.parent_id
            && !effective_item_ids.contains(parent_id)
        {
            return Err(SessionStoreError::TimelineItemNotFound {
                id: id.clone(),
                item_id: parent_id.clone(),
            });
        }
        let event = event.with_cursor(cursor);
        effective_item_ids.insert(event.item_id.clone());
        assigned.push(event);
    }
    record.append_journal_events(assigned.iter().cloned().map(SessionJournalEvent::Timeline));
    record.touch_activity();
    Ok(assigned)
}

pub(super) fn sorted_session_summaries(
    records: impl IntoIterator<Item = SessionRecord>,
) -> Vec<SessionSummary> {
    let mut summaries = records
        .into_iter()
        .map(|record| record.summary())
        .collect::<Vec<_>>();
    summaries.sort_by(|left, right| {
        right
            .last_activity_at
            .cmp(&left.last_activity_at)
            .then_with(|| right.id.cmp(&left.id))
    });
    summaries
}

pub(super) fn timeline_query_slice(
    record: &SessionRecord,
    query: &TimelineQuery,
) -> Vec<AgentTimelineEvent> {
    let limit = query.limit;
    if limit == 0 {
        return Vec::new();
    }

    match query.direction {
        TimelineDirection::Forward => record
            .timeline_events()
            .filter(|event| query.cursor.is_none_or(|cursor| event.cursor > cursor))
            .filter(|event| channel_matches(event, query.channels.as_deref()))
            .take(limit)
            .cloned()
            .collect(),
        TimelineDirection::Backward => {
            let events = record
                .timeline_events()
                .filter(|event| query.cursor.is_none_or(|cursor| event.cursor < cursor))
                .filter(|event| channel_matches(event, query.channels.as_deref()))
                .cloned()
                .collect::<Vec<_>>();
            let start = events.len().saturating_sub(limit);
            events[start..].to_vec()
        }
    }
}

pub(super) fn timeline_item_text_from_record(
    record: &SessionRecord,
    item_id: &TimelineItemId,
) -> SessionStoreResult<TimelineItemText> {
    let mut found = false;
    let mut text = String::new();
    let mut last_cursor = None;

    for event in record
        .timeline_events()
        .filter(|event| event.item_id == *item_id)
    {
        found = true;
        last_cursor = Some(event.cursor);
        match &event.content {
            TimelineContent::TextDelta { delta } => text.push_str(delta),
            TimelineContent::TextSnapshot { text: snapshot } => text.clone_from(snapshot),
            TimelineContent::ContentRef { content_id, .. } => {
                return Err(SessionStoreError::TimelineContentUnavailable {
                    id: record.id.clone(),
                    item_id: item_id.clone(),
                    reason: format!(
                        "content references are not implemented: {}",
                        content_id.as_str()
                    ),
                });
            }
            TimelineContent::Empty
            | TimelineContent::ToolCall { .. }
            | TimelineContent::ToolResult { .. }
            | TimelineContent::ExternalRequest { .. }
            | TimelineContent::Json { .. } => {}
        }
    }

    if !found {
        return Err(SessionStoreError::TimelineItemNotFound {
            id: record.id.clone(),
            item_id: item_id.clone(),
        });
    }

    Ok(TimelineItemText {
        item_id: item_id.clone(),
        bytes: text.len() as u64,
        text,
        last_cursor,
    })
}

fn channel_matches(
    event: &AgentTimelineEvent,
    channels: Option<&[crate::timeline::TimelineChannel]>,
) -> bool {
    channels.is_none_or(|channels| {
        event
            .channel
            .as_ref()
            .is_some_and(|channel| channels.contains(channel))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::session::SessionRollbackTarget;

    #[test]
    fn timeline_allocation_uses_persisted_high_water_across_cursor_gaps() {
        let id = SessionId::new("destination");
        let mut record = SessionRecord::new(id.clone());
        record.append_journal_event(SessionJournalEvent::Timeline(
            PendingTimelineEvent::assistant_text_snapshot(
                id.clone(),
                0,
                PendingTimelineItemId::New,
                "rolled back",
            )
            .with_cursor(TimelineCursor::new(9)),
        ));
        record.append_journal_event(SessionJournalEvent::Rollback {
            target: SessionRollbackTarget {
                journal_len: 0,
                after_cursor: None,
                next_turn: 0,
                remove_prompt_index: None,
            },
        });
        assert!(record.timeline_events().next().is_none());

        let assigned = append_timeline_to_record(
            &mut record,
            &id,
            vec![PendingTimelineEvent::user_text_snapshot(
                id.clone(),
                1,
                PendingTimelineItemId::New,
                "next",
            )],
        )
        .unwrap();

        assert_eq!(assigned[0].cursor, TimelineCursor::new(10));
        assert_eq!(assigned[0].item_id.as_str(), "i10");
        assert_eq!(record.timeline_cursor_high_water(), TimelineCursor::new(10));
    }
}
