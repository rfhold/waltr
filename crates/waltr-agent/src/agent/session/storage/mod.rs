//! Agent session storage interface.

#![allow(async_fn_in_trait, missing_docs)]

mod filesystem;
mod memory;
mod opendal;
mod postgresql;
mod s3;
mod scylla;
#[cfg(test)]
mod revisions;

use std::{collections::HashSet, error::Error, fmt, future::Future};

pub use filesystem::FilesystemSessionStore;
pub use memory::MemorySessionStore;
pub use opendal::OpenDalSessionStore;
pub use postgresql::{opendal_postgresql_session_store, run_postgresql_migrations};
pub use s3::{S3SessionStore, S3SessionWriteMode};
pub use scylla::ScyllaSessionStore;

use crate::timeline::{
    AgentTimelineEvent, PendingTimelineEvent, PendingTimelineItemId, TimelineContent,
    TimelineCursor, TimelineDirection, TimelineItemId, TimelineItemText, TimelineQuery,
};
use serde_json::Value;

use super::{
    InferenceSettings, SessionCreateRequest, SessionMaskError, SessionMaskMutation,
    SessionMaskMutationOutcome,
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
    /// Legacy records remain readable but cannot authorize execution without an incarnation.
    ExecutionCompatibility {
        id: SessionId,
    },
    /// Execution assumptions changed, or this is a different durable incarnation.
    StaleExecution {
        expected: super::ExecutionToken,
        actual: super::ExecutionToken,
    },
    /// The requested session does not exist.
    NotFound {
        id: SessionId,
    },
    /// A session with the requested id already exists.
    AlreadyExists {
        id: SessionId,
    },
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
    /// A mask mutation failed validation or its pinned timeline precondition.
    MaskMutation {
        error: SessionMaskError,
    },
    MetadataMutation {
        error: super::MetadataError,
    },
}

impl fmt::Display for SessionStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExecutionCompatibility { id } => write!(
                formatter,
                "session {} has no durable incarnation; execution token unavailable for legacy record",
                id.as_str()
            ),
            Self::StaleExecution { expected, actual } => write!(
                formatter,
                "stale session execution token: expected {expected:?}, actual {actual:?}"
            ),
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
            Self::MaskMutation { error } => {
                write!(formatter, "session mask mutation failed: {error}")
            }
            Self::MetadataMutation { error } => error.fmt(formatter),
        }
    }
}

impl Error for SessionStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MaskMutation { error } => Some(error),
            Self::MetadataMutation { error } => Some(error),
            _ => None,
        }
    }
}

impl From<SessionMaskError> for SessionStoreError {
    fn from(error: SessionMaskError) -> Self {
        Self::MaskMutation { error }
    }
}

impl From<super::MetadataError> for SessionStoreError {
    fn from(error: super::MetadataError) -> Self {
        Self::MetadataMutation { error }
    }
}

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

    /// Append with an incarnation-bound execution token and return committed facts and cursors.
    fn append_timeline(
        &self,
        id: &SessionId,
        expected_token: super::ExecutionToken,
        events: Vec<PendingTimelineEvent>,
    ) -> impl Future<Output = SessionStoreResult<super::TimelineAppendReceipt>> + Send;

    /// Append internal journal events if the durable execution token matches.
    fn append_journal(
        &self,
        id: &SessionId,
        expected_token: super::ExecutionToken,
        events: Vec<SessionJournalEvent>,
    ) -> impl Future<Output = SessionStoreResult<super::SessionCommitReceipt>> + Send;

    /// Atomically subtract and add persistent replacement masks.
    fn mutate_masks(
        &self,
        id: &SessionId,
        mutation: SessionMaskMutation,
    ) -> impl Future<Output = SessionStoreResult<SessionMaskMutationOutcome>> + Send {
        async move {
            self.mutate(
                id,
                super::SessionMutation {
                    masks: Some(mutation),
                    ..Default::default()
                },
            )
            .await?
            .masks
            .ok_or_else(|| SessionStoreError::Backend {
                message: "missing mask outcome".into(),
            })
        }
    }

    /// Atomically evaluate metadata/source predicates and commit metadata plus dependent masks.
    /// Backends without complete-record atomic mutation must explicitly reject this operation.
    fn mutate(
        &self,
        _id: &SessionId,
        _mutation: super::SessionMutation,
    ) -> impl Future<Output = SessionStoreResult<super::SessionMutationOutcome>> + Send {
        async {
            Err(SessionStoreError::Backend {
                message: "atomic session mutation unsupported".into(),
            })
        }
    }

    /// Read bounded opaque values without exposing provider or engine metadata as authority.
    fn metadata(
        &self,
        id: &SessionId,
        target: super::MetadataTarget,
    ) -> impl Future<Output = SessionStoreResult<std::collections::BTreeMap<String, Value>>> + Send {
        async move {
            let record = self.load(id).await?;
            Ok(record.metadata_values(&target)?.cloned().unwrap_or_default())
        }
    }

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

    /// Set the agent-definition bundle; actual changes advance both revision domains.
    fn set_agent_definition(
        &self,
        id: &SessionId,
        agent_definition: Option<serde_json::Value>,
    ) -> impl Future<Output = SessionStoreResult<SessionRecord>> + Send;

    /// Set cycle configuration only if execution still matches, including for a no-op.
    /// Implementations must check atomically; unsupported stores fail closed.
    fn set_agent_definition_if_execution(
        &self,
        _id: &SessionId,
        _expected: super::ExecutionToken,
        _agent_definition: Option<Value>,
    ) -> impl Future<Output = SessionStoreResult<SessionRecord>> + Send {
        async {
            Err(SessionStoreError::Backend {
                message: "conditional execution configuration unsupported".into(),
            })
        }
    }

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

pub(super) fn decode_record(bytes: &[u8]) -> SessionStoreResult<SessionRecord> {
    let record = serde_json::from_slice(bytes).map_err(|error| SessionStoreError::Backend {
        message: error.to_string(),
    })?;
    super::metadata::validate_record(&record)?;
    Ok(record)
}

pub(super) fn mutate_on_record(
    record: &mut SessionRecord,
    revision: Revision,
    mutation: &super::SessionMutation,
) -> SessionStoreResult<super::SessionMutationOutcome> {
    if !super::context::json_within_limit(mutation, super::MAX_SESSION_MUTATION_BYTES) {
        return Err(super::MetadataError::Bounds.into());
    }
    let mask_predicates = mutation.masks.as_ref().map(|masks| &masks.metadata_predicates);
    if mutation.predicates.len().saturating_add(mutation.updates.len())
        .saturating_add(mask_predicates.map_or(0, Vec::len)) > super::MAX_METADATA_MUTATION_ENTRIES
        || !super::context::json_within_limit(
            &(&mutation.predicates, &mutation.updates, mask_predicates), super::MAX_METADATA_BYTES)
    {
        return Err(super::MetadataError::Bounds.into());
    }
    if let Some(expected) = &mutation.expected_incarnation
        && record.metadata.incarnation.as_ref() != Some(expected)
    {
        return Err(super::MetadataError::Precondition.into());
    }
    if let Some(expected) = mutation.expected_revision
        && record.revision != expected
    {
        return Err(SessionStoreError::StaleRevision {
            id: record.id.clone(),
            expected,
            actual: record.revision,
        });
    }
    if mutation.expected_context.as_ref().is_some_and(|identity| !identity.matches(record)) {
        return Err(SessionMaskError::ContextMismatch.into());
    }
    let mut updated = record.clone();
    // Mask/source predicates observe the same pre-update state as metadata predicates.
    let masks = mutation.masks.as_ref()
        .map(|masks| super::mask::mutate_record_masks(&mut updated, revision, masks))
        .transpose()?;
    let mut metadata = record.clone();
    super::metadata::apply(&mut metadata, mutation)?;
    updated.metadata.values = metadata.metadata.values;
    updated.metadata.item_values = metadata.metadata.item_values;
    if !mutation.updates.is_empty() {
        updated.touch_metadata();
    }
    updated.revision = revision;
    if mutation.masks.is_some() {
        updated.execution_sequence = record.execution_sequence.next();
    }
    *record = updated;
    Ok(super::SessionMutationOutcome {
        record: record.clone(),
        masks,
    })
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
pub(super) async fn fixture_token<S: SessionStore>(
    store: &S,
    id: &SessionId,
    sequence: u64,
) -> super::ExecutionToken {
    let mut token = store.load(id).await.unwrap().execution_token().unwrap();
    token.sequence = super::ExecutionSequence::new(sequence);
    token
}

#[cfg(test)]
pub(super) async fn assert_mask_store_contract<S>(store: S, label: &str)
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    super::metadata::assert_store_contract(store.clone(), label).await;
    let id = SessionId::new(format!("mask-contract-{label}"));
    store
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .unwrap();
    store
        .append_timeline(
            &id,
            fixture_token(&store, &id, 0).await,
            vec![PendingTimelineEvent::assistant_text_snapshot(
                id.clone(),
                0,
                PendingTimelineItemId::New,
                "answer",
            )],
        )
        .await
        .unwrap();
    store
        .append_journal(
            &id,
            fixture_token(&store, &id, 1).await,
            vec![SessionJournalEvent::TurnFinished { turn: 0 }],
        )
        .await
        .unwrap();
    let rollback_len = store.load(&id).await.unwrap().journal.len() as u64;
    let addition = || {
        crate::SessionMaskAddition::new(
            "summary",
            crate::ThroughCursor::new(TimelineCursor::new(1)),
            crate::ReplacementUserContent::new("compacted"),
        )
    };
    let mut add = SessionMaskMutation::new(Some(TimelineCursor::new(1)));
    add.add.push(addition());
    let added = store.mutate_masks(&id, add).await.unwrap();
    let mask_id = added.added_masks[0].id.clone();
    let persisted = store.load(&id).await.unwrap();
    assert_eq!(added.revision, persisted.revision);
    assert_eq!(added.cursor, persisted.timeline_cursor_high_water());
    assert_eq!(persisted.active_masks(), added.added_masks);

    let before_failure = persisted.clone();
    let error = store
        .mutate_masks(&id, SessionMaskMutation::new(None))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        SessionStoreError::MaskMutation {
            error: crate::SessionMaskError::BoundaryMismatch { .. }
        }
    ));
    assert_eq!(store.load(&id).await.unwrap(), before_failure);

    store
        .append_journal(
            &id,
            persisted.execution_token().unwrap(),
            vec![SessionJournalEvent::Rollback {
                target: super::SessionRollbackTarget {
                    journal_len: rollback_len,
                    after_cursor: Some(TimelineCursor::new(1)),
                    next_turn: 1,
                    remove_prompt_index: None,
                },
            }],
        )
        .await
        .unwrap();
    assert!(store.load(&id).await.unwrap().active_masks().is_empty());

    let mut subtract_absent = SessionMaskMutation::new(Some(TimelineCursor::new(1)));
    subtract_absent.subtract.ids.push(mask_id);
    let absent = store.mutate_masks(&id, subtract_absent).await.unwrap();
    assert!(absent.removed_mask_ids.is_empty());
    assert!(store.load(&id).await.unwrap().active_masks().is_empty());

    let mut readd = SessionMaskMutation::new(Some(TimelineCursor::new(1)));
    readd.add.push(addition());
    let readded = store.mutate_masks(&id, readd).await.unwrap();
    let mut subtract = SessionMaskMutation::new(Some(TimelineCursor::new(1)));
    subtract.subtract.keys.push("summary".to_owned());
    let subtracted = store.mutate_masks(&id, subtract).await.unwrap();
    assert_eq!(
        subtracted.removed_mask_ids,
        [readded.added_masks[0].id.clone()]
    );
    let persisted = store.load(&id).await.unwrap();
    assert_eq!(subtracted.revision, persisted.revision);
    assert_eq!(subtracted.cursor, persisted.timeline_cursor_high_water());
    assert!(persisted.active_masks().is_empty());

    let assigned = store
        .append_timeline(
            &id,
            persisted.execution_token().unwrap(),
            vec![PendingTimelineEvent::assistant_text_snapshot(
                id.clone(),
                1,
                PendingTimelineItemId::New,
                "after masks",
            )],
        )
        .await
        .unwrap();
    assert_eq!(assigned.events[0].cursor, subtracted.cursor.next());
    assert_eq!(
        store.load(&id).await.unwrap().timeline_cursor_high_water(),
        assigned.events[0].cursor
    );
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
