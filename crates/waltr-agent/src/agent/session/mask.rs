//! Durable replacement masks for provider-visible session context.

#![allow(missing_docs)]

use std::{collections::HashSet, error::Error, fmt};

use serde::{Deserialize, Serialize};

use crate::TimelineCursor;

use super::{Revision, SessionJournalEvent, SessionRecord};

/// Maximum active replacement masks retained by one session.
pub const MAX_ACTIVE_SESSION_MASKS: usize = 64;
/// Maximum additions or subtraction selectors accepted in one mutation.
pub const MAX_SESSION_MASK_MUTATION_ITEMS: usize = 64;
/// Maximum UTF-8 bytes in a stable mask key.
pub const MAX_SESSION_MASK_KEY_BYTES: usize = 128;
/// Maximum UTF-8 bytes in replacement user content.
pub const MAX_SESSION_MASK_REPLACEMENT_BYTES: usize = 256 * 1024;

/// Opaque durable replacement-mask identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionMaskId(String);

impl SessionMaskId {
    pub(crate) fn generate() -> Self {
        Self(format!("mask_{}", uuid::Uuid::now_v7()))
    }

    /// Borrow the opaque identity.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Prefix selector covering replayable source items through one timeline cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThroughCursor {
    /// Inclusive timeline boundary.
    pub cursor: TimelineCursor,
}

impl ThroughCursor {
    /// Select source items through `cursor`.
    #[must_use]
    pub const fn new(cursor: TimelineCursor) -> Self {
        Self { cursor }
    }
}

/// Synthetic user content replacing selected source context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplacementUserContent {
    /// Replacement text presented to providers as a user prompt.
    pub text: String,
}

impl ReplacementUserContent {
    /// Create replacement user content.
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// One active durable replacement mask.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMask {
    /// Opaque durable identity.
    pub id: SessionMaskId,
    /// Stable caller-defined identity used for explicit replacement/removal.
    pub key: String,
    /// Selected source prefix.
    pub selector: ThroughCursor,
    /// Synthetic user replacement.
    pub replacement: ReplacementUserContent,
}

/// One rollback-effective mask paired with the cursor of its activation mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveSessionMask {
    /// Durable replacement mask.
    pub mask: SessionMask,
    /// Globally ordered administrative cursor that activated the mask.
    pub mutation_cursor: TimelineCursor,
}

/// Replacement mask requested for addition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMaskAddition {
    pub key: String,
    pub selector: ThroughCursor,
    pub replacement: ReplacementUserContent,
}

impl SessionMaskAddition {
    /// Create one prefix replacement addition.
    pub fn new(
        key: impl Into<String>,
        selector: ThroughCursor,
        replacement: ReplacementUserContent,
    ) -> Self {
        Self {
            key: key.into(),
            selector,
            replacement,
        }
    }
}

/// Explicit active-mask subtraction resolved before additions are applied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionMaskSubtraction {
    pub ids: Vec<SessionMaskId>,
    pub keys: Vec<String>,
    pub overlaps: Vec<ThroughCursor>,
}

/// Atomic durable mask mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMaskMutation {
    /// Latest effective raw timeline boundary expected by the caller.
    pub expected_timeline_boundary: Option<TimelineCursor>,
    pub subtract: SessionMaskSubtraction,
    pub add: Vec<SessionMaskAddition>,
}

impl SessionMaskMutation {
    /// Create a mutation pinned to the latest effective timeline boundary.
    #[must_use]
    pub const fn new(expected_timeline_boundary: Option<TimelineCursor>) -> Self {
        Self {
            expected_timeline_boundary,
            subtract: SessionMaskSubtraction {
                ids: Vec::new(),
                keys: Vec::new(),
                overlaps: Vec::new(),
            },
            add: Vec::new(),
        }
    }
}

/// Committed result of one atomic mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMaskMutationOutcome {
    pub revision: Revision,
    /// Administrative session cursor consumed by this mutation.
    pub cursor: TimelineCursor,
    pub removed_mask_ids: Vec<SessionMaskId>,
    pub added_masks: Vec<SessionMask>,
}

/// Typed replacement-mask validation or precondition failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionMaskError {
    BoundaryMismatch {
        expected: Option<TimelineCursor>,
        actual: Option<TimelineCursor>,
    },
    SelectorNotCompletedBoundary {
        cursor: TimelineCursor,
    },
    EmptyKey,
    KeyTooLong {
        bytes: usize,
        limit: usize,
    },
    EmptyReplacement,
    ReplacementTooLarge {
        bytes: usize,
        limit: usize,
    },
    MutationTooLarge {
        items: usize,
        limit: usize,
    },
    TooManyActiveMasks {
        masks: usize,
        limit: usize,
    },
    DuplicateKey {
        key: String,
    },
    ReplacementOverlap {
        left: ThroughCursor,
        right: ThroughCursor,
    },
    CursorExhausted,
}

impl fmt::Display for SessionMaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BoundaryMismatch { expected, actual } => write!(
                formatter,
                "latest effective timeline boundary mismatch: expected {expected:?}, actual {actual:?}"
            ),
            Self::SelectorNotCompletedBoundary { cursor } => write!(
                formatter,
                "mask selector cursor {} is not an effective completed-turn boundary",
                cursor.get()
            ),
            Self::EmptyKey => formatter.write_str("mask key must not be empty"),
            Self::KeyTooLong { bytes, limit } => {
                write!(
                    formatter,
                    "mask key exceeds limit ({bytes} > {limit} bytes)"
                )
            }
            Self::EmptyReplacement => formatter.write_str("mask replacement must not be empty"),
            Self::ReplacementTooLarge { bytes, limit } => write!(
                formatter,
                "mask replacement exceeds limit ({bytes} > {limit} bytes)"
            ),
            Self::MutationTooLarge { items, limit } => {
                write!(formatter, "mask mutation exceeds limit ({items} > {limit})")
            }
            Self::TooManyActiveMasks { masks, limit } => {
                write!(formatter, "active masks exceed limit ({masks} > {limit})")
            }
            Self::DuplicateKey { key } => write!(formatter, "duplicate active mask key: {key}"),
            Self::ReplacementOverlap { left, right } => write!(
                formatter,
                "replacement masks overlap at prefix cursors {} and {}",
                left.cursor.get(),
                right.cursor.get()
            ),
            Self::CursorExhausted => formatter.write_str("session cursor space is exhausted"),
        }
    }
}

impl Error for SessionMaskError {}

pub(super) fn mutate_record_masks(
    record: &mut SessionRecord,
    revision: Revision,
    mutation: &SessionMaskMutation,
) -> Result<SessionMaskMutationOutcome, SessionMaskError> {
    let actual_boundary = record.timeline_events().last().map(|event| event.cursor);
    if actual_boundary != mutation.expected_timeline_boundary {
        return Err(SessionMaskError::BoundaryMismatch {
            expected: mutation.expected_timeline_boundary,
            actual: actual_boundary,
        });
    }

    let mutation_items = mutation
        .subtract
        .ids
        .len()
        .saturating_add(mutation.subtract.keys.len())
        .saturating_add(mutation.subtract.overlaps.len())
        .saturating_add(mutation.add.len());
    if mutation_items > MAX_SESSION_MASK_MUTATION_ITEMS {
        return Err(SessionMaskError::MutationTooLarge {
            items: mutation_items,
            limit: MAX_SESSION_MASK_MUTATION_ITEMS,
        });
    }

    for selector in mutation
        .subtract
        .overlaps
        .iter()
        .chain(mutation.add.iter().map(|addition| &addition.selector))
    {
        validate_completed_boundary(record, *selector)?;
    }
    for addition in &mutation.add {
        validate_addition(addition)?;
    }

    let ids = mutation.subtract.ids.iter().collect::<HashSet<_>>();
    let keys = mutation
        .subtract
        .keys
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut active = record.active_masks();
    let mut removed_mask_ids = Vec::new();
    active.retain(|mask| {
        let remove = ids.contains(&mask.id)
            || keys.contains(mask.key.as_str())
            || mutation
                .subtract
                .overlaps
                .iter()
                .any(|selector| selectors_overlap(mask.selector, *selector));
        if remove {
            removed_mask_ids.push(mask.id.clone());
        }
        !remove
    });

    let mut added_masks = Vec::with_capacity(mutation.add.len());
    for addition in &mutation.add {
        if active
            .iter()
            .chain(added_masks.iter())
            .any(|mask| mask.key == addition.key)
        {
            return Err(SessionMaskError::DuplicateKey {
                key: addition.key.clone(),
            });
        }
        if let Some(overlap) = active
            .iter()
            .chain(added_masks.iter())
            .find(|mask| selectors_overlap(mask.selector, addition.selector))
        {
            return Err(SessionMaskError::ReplacementOverlap {
                left: overlap.selector,
                right: addition.selector,
            });
        }
        added_masks.push(SessionMask {
            id: SessionMaskId::generate(),
            key: addition.key.clone(),
            selector: addition.selector,
            replacement: addition.replacement.clone(),
        });
    }
    let active_count = active.len().saturating_add(added_masks.len());
    if active_count > MAX_ACTIVE_SESSION_MASKS {
        return Err(SessionMaskError::TooManyActiveMasks {
            masks: active_count,
            limit: MAX_ACTIVE_SESSION_MASKS,
        });
    }
    let high_water = record.timeline_cursor_high_water();
    if high_water.get() == u64::MAX {
        return Err(SessionMaskError::CursorExhausted);
    }
    let cursor = high_water.next();
    record.append_journal_event(SessionJournalEvent::MasksMutated {
        cursor,
        removed_mask_ids: removed_mask_ids.clone(),
        added_masks: added_masks.clone(),
    });
    record.touch_activity();
    Ok(SessionMaskMutationOutcome {
        revision,
        cursor,
        removed_mask_ids,
        added_masks,
    })
}

fn validate_addition(addition: &SessionMaskAddition) -> Result<(), SessionMaskError> {
    if addition.key.is_empty() {
        return Err(SessionMaskError::EmptyKey);
    }
    if addition.key.len() > MAX_SESSION_MASK_KEY_BYTES {
        return Err(SessionMaskError::KeyTooLong {
            bytes: addition.key.len(),
            limit: MAX_SESSION_MASK_KEY_BYTES,
        });
    }
    if addition.replacement.text.is_empty() {
        return Err(SessionMaskError::EmptyReplacement);
    }
    if addition.replacement.text.len() > MAX_SESSION_MASK_REPLACEMENT_BYTES {
        return Err(SessionMaskError::ReplacementTooLarge {
            bytes: addition.replacement.text.len(),
            limit: MAX_SESSION_MASK_REPLACEMENT_BYTES,
        });
    }
    Ok(())
}

fn validate_completed_boundary(
    record: &SessionRecord,
    selector: ThroughCursor,
) -> Result<(), SessionMaskError> {
    let mut latest = None;
    for event in record.effective_journal_events() {
        match event {
            SessionJournalEvent::Timeline(event) => latest = Some(event.cursor),
            SessionJournalEvent::TurnFinished { .. } if latest == Some(selector.cursor) => {
                return Ok(());
            }
            _ => {}
        }
    }
    Err(SessionMaskError::SelectorNotCompletedBoundary {
        cursor: selector.cursor,
    })
}

pub(super) const fn selectors_overlap(left: ThroughCursor, right: ThroughCursor) -> bool {
    left.cursor.get() > 0 && right.cursor.get() > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::session::SessionRollbackTarget;
    use crate::{
        MemorySessionStore, PendingTimelineEvent, PendingTimelineItemId, SessionCreateRequest,
        SessionStore, SessionStoreError, TimelineContent,
    };

    fn completed_record() -> SessionRecord {
        let mut record = SessionRecord::new("masked");
        record.append_journal_event(SessionJournalEvent::PromptAppended {
            prompt: "original question".to_owned(),
        });
        record.append_journal_event(SessionJournalEvent::Timeline(
            PendingTimelineEvent::user_text_snapshot(
                record.id.clone(),
                0,
                PendingTimelineItemId::New,
                "original question",
            )
            .with_cursor(TimelineCursor::new(1)),
        ));
        record.append_journal_event(SessionJournalEvent::Timeline(
            PendingTimelineEvent::assistant_text_snapshot(
                record.id.clone(),
                0,
                PendingTimelineItemId::New,
                "original answer",
            )
            .with_cursor(TimelineCursor::new(2)),
        ));
        record.append_journal_event(SessionJournalEvent::TurnFinished { turn: 0 });
        record
    }

    fn addition(key: &str, replacement: &str) -> SessionMaskAddition {
        SessionMaskAddition::new(
            key,
            ThroughCursor::new(TimelineCursor::new(2)),
            ReplacementUserContent::new(replacement),
        )
    }

    #[test]
    fn mask_is_rollback_effective_and_raw_timeline_is_unchanged() {
        let mut record = completed_record();
        let rollback_len = record.journal.len() as u64;
        let mut mutation = SessionMaskMutation::new(Some(TimelineCursor::new(2)));
        mutation
            .add
            .push(addition("context.compaction-prefix", "summary"));

        let outcome = mutate_record_masks(&mut record, Revision::new(1), &mutation).unwrap();

        assert_eq!(outcome.cursor, TimelineCursor::new(3));
        assert_eq!(record.active_masks(), outcome.added_masks);
        assert_eq!(
            record
                .timeline_events()
                .map(|event| event.cursor)
                .collect::<Vec<_>>(),
            [TimelineCursor::new(1), TimelineCursor::new(2)]
        );
        assert!(matches!(
            &record.timeline_events().last().unwrap().content,
            TimelineContent::TextSnapshot { text } if text == "original answer"
        ));

        record.append_journal_event(SessionJournalEvent::Rollback {
            target: SessionRollbackTarget {
                journal_len: rollback_len,
                after_cursor: Some(TimelineCursor::new(2)),
                next_turn: 1,
                remove_prompt_index: None,
            },
        });
        assert!(record.active_masks().is_empty());
    }

    #[test]
    fn subtraction_resolves_before_add_and_failed_overlap_is_atomic() {
        let mut record = completed_record();
        let mut initial = SessionMaskMutation::new(Some(TimelineCursor::new(2)));
        initial.add.push(addition("old", "old summary"));
        let old = mutate_record_masks(&mut record, Revision::new(1), &initial)
            .unwrap()
            .added_masks
            .remove(0);

        let unchanged = record.clone();
        let mut overlapping = SessionMaskMutation::new(Some(TimelineCursor::new(2)));
        overlapping.add.push(addition("new", "new summary"));
        assert!(matches!(
            mutate_record_masks(&mut record, Revision::new(2), &overlapping),
            Err(SessionMaskError::ReplacementOverlap { .. })
        ));
        assert_eq!(record, unchanged);

        let mut replacement = SessionMaskMutation::new(Some(TimelineCursor::new(2)));
        replacement.subtract.ids.push(old.id.clone());
        replacement.add.push(addition("new", "new summary"));
        let outcome = mutate_record_masks(&mut record, Revision::new(2), &replacement).unwrap();
        assert_eq!(outcome.removed_mask_ids, [old.id]);
        assert_eq!(record.active_masks()[0].key, "new");
        assert_eq!(outcome.cursor, TimelineCursor::new(4));
    }

    #[tokio::test]
    async fn memory_store_commits_one_atomic_mask_event() {
        let store = MemorySessionStore::new();
        let id = super::super::SessionId::new("stored-mask");
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        store
            .append_journal(
                &id,
                Revision::ZERO,
                vec![SessionJournalEvent::PromptAppended {
                    prompt: "question".to_owned(),
                }],
            )
            .await
            .unwrap();
        store
            .append_timeline(
                &id,
                Revision::new(1),
                vec![
                    PendingTimelineEvent::user_text_snapshot(
                        id.clone(),
                        0,
                        PendingTimelineItemId::New,
                        "question",
                    ),
                    PendingTimelineEvent::assistant_text_snapshot(
                        id.clone(),
                        0,
                        PendingTimelineItemId::New,
                        "answer",
                    ),
                ],
            )
            .await
            .unwrap();
        store
            .append_journal(
                &id,
                Revision::new(2),
                vec![SessionJournalEvent::TurnFinished { turn: 0 }],
            )
            .await
            .unwrap();

        let mut mutation = SessionMaskMutation::new(Some(TimelineCursor::new(2)));
        mutation.add.push(addition("summary", "compacted"));
        let outcome = store.mutate_masks(&id, mutation).await.unwrap();
        let record = store.load(&id).await.unwrap();
        assert_eq!(outcome.revision, Revision::new(4));
        assert_eq!(record.revision, Revision::new(4));
        assert!(matches!(
            record.journal.last(),
            Some(SessionJournalEvent::MasksMutated { cursor, .. })
                if *cursor == TimelineCursor::new(3)
        ));

        let before = record.clone();
        let error = store
            .mutate_masks(&id, SessionMaskMutation::new(Some(TimelineCursor::new(1))))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            SessionStoreError::MaskMutation {
                error: SessionMaskError::BoundaryMismatch { .. }
            }
        ));
        assert_eq!(store.load(&id).await.unwrap(), before);
    }
}
