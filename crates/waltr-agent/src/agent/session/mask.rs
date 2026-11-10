//! Durable replacement masks for provider-visible session context.

#![allow(missing_docs)]

use std::{collections::HashSet, error::Error, fmt};

use serde::{Deserialize, Serialize};

use crate::{TimelineCursor, TimelineItemId};

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
    /// Explicit canonical source items; absent in legacy prefix masks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_ids: Option<Vec<TimelineItemId>>,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMaskAddition {
    pub key: String,
    pub selector: ThroughCursor,
    pub item_ids: Option<Vec<TimelineItemId>>,
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
            item_ids: None,
            replacement,
        }
    }

    /// Select an exact nonempty set of replay-consistent source items at a completed boundary.
    pub fn items(
        key: impl Into<String>,
        boundary: TimelineCursor,
        item_ids: Vec<TimelineItemId>,
        replacement: ReplacementUserContent,
    ) -> Self {
        Self {
            key: key.into(),
            selector: ThroughCursor::new(boundary),
            item_ids: Some(item_ids),
            replacement,
        }
    }
}

/// Explicit active-mask subtraction resolved before additions are applied.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMaskSubtraction {
    pub ids: Vec<SessionMaskId>,
    pub keys: Vec<String>,
    pub overlaps: Vec<ThroughCursor>,
}

/// Atomic durable mask mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMaskMutation {
    /// Latest effective raw timeline boundary expected by the caller.
    pub expected_timeline_boundary: Option<TimelineCursor>,
    /// Optional exact context precondition evaluated inside the same storage transaction.
    pub expected_context: Option<super::ContextIdentity>,
    /// Pinned source plus complete known administrative state for disjoint sibling mutations.
    pub expected_completed_source: Option<super::ContextIdentity>,
    /// Required alongside `expected_completed_source`; never silently refreshed from storage.
    pub expected_active_masks: Option<Vec<(String, SessionMaskId)>>,
    /// Opaque metadata values observed before the dependent mask edits.
    pub metadata_predicates: Vec<super::MetadataPredicate>,
    /// Stable key and exact active identity expected before any subtraction or addition.
    pub expected_masks: Vec<(String, Option<SessionMaskId>)>,
    pub subtract: SessionMaskSubtraction,
    pub add: Vec<SessionMaskAddition>,
}

impl SessionMaskMutation {
    /// Create a mutation pinned to the latest effective timeline boundary.
    #[must_use]
    pub const fn new(expected_timeline_boundary: Option<TimelineCursor>) -> Self {
        Self {
            expected_timeline_boundary,
            expected_context: None,
            expected_completed_source: None,
            expected_active_masks: None,
            metadata_predicates: Vec::new(),
            expected_masks: Vec::new(),
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
    ContextMismatch,
    MaskIdentityMismatch {
        key: String,
    },
    InvalidItems,
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
            Self::ContextMismatch => formatter.write_str("exact context snapshot mismatch"),
            Self::MaskIdentityMismatch { key } => {
                write!(formatter, "active mask identity mismatch: {key}")
            }
            Self::InvalidItems => formatter
                .write_str("invalid, empty, oversized, or replay-inconsistent item selection"),
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
    let mutation_items = mutation
        .subtract
        .ids
        .len()
        .saturating_add(mutation.subtract.keys.len())
        .saturating_add(mutation.subtract.overlaps.len())
        .saturating_add(mutation.add.len())
        .saturating_add(mutation.expected_masks.len());
    if mutation_items > MAX_SESSION_MASK_MUTATION_ITEMS {
        return Err(SessionMaskError::MutationTooLarge {
            items: mutation_items,
            limit: MAX_SESSION_MASK_MUTATION_ITEMS,
        });
    }
    super::metadata::apply(
        &mut record.clone(),
        &super::SessionMutation {
            predicates: mutation.metadata_predicates.clone(),
            ..Default::default()
        },
    )
    .map_err(|_| SessionMaskError::ContextMismatch)?;
    if mutation
        .expected_context
        .as_ref()
        .is_some_and(|context| !context.matches(record))
    {
        return Err(SessionMaskError::ContextMismatch);
    }
    let original_masks = record.active_masks();
    if let Some(context) = &mutation.expected_completed_source {
        let actual = original_masks
            .iter()
            .map(|mask| (mask.key.clone(), mask.id.clone()))
            .collect::<std::collections::BTreeMap<_, _>>();
        let expected = mutation.expected_active_masks.as_ref().map(|masks| {
            masks
                .iter()
                .cloned()
                .collect::<std::collections::BTreeMap<_, _>>()
        });
        if !context.matches_completed_source(record)
            || mutation.expected_active_masks.as_ref().is_none_or(|masks| {
                masks.len() > MAX_ACTIVE_SESSION_MASKS
                    || expected
                        .as_ref()
                        .is_none_or(|expected| expected.len() != masks.len())
            })
            || expected.as_ref() != Some(&actual)
        {
            return Err(SessionMaskError::ContextMismatch);
        }
    } else if mutation.expected_active_masks.is_some() {
        return Err(SessionMaskError::ContextMismatch);
    }
    for (key, expected) in &mutation.expected_masks {
        if original_masks
            .iter()
            .find(|mask| &mask.key == key)
            .map(|mask| &mask.id)
            != expected.as_ref()
        {
            return Err(SessionMaskError::MaskIdentityMismatch { key: key.clone() });
        }
    }
    let actual_boundary = record.timeline_events().last().map(|event| event.cursor);
    if actual_boundary != mutation.expected_timeline_boundary {
        return Err(SessionMaskError::BoundaryMismatch {
            expected: mutation.expected_timeline_boundary,
            actual: actual_boundary,
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
        if let Some(ids) = &addition.item_ids {
            super::SessionCopy::selected(record, addition.selector.cursor, ids)
                .map_err(|_| SessionMaskError::InvalidItems)?;
        }
    }

    let ids = mutation.subtract.ids.iter().collect::<HashSet<_>>();
    let keys = mutation
        .subtract
        .keys
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut active = original_masks;
    let mut removed_mask_ids = Vec::new();
    active.retain(|mask| {
        let remove = ids.contains(&mask.id)
            || keys.contains(mask.key.as_str())
            || mutation.subtract.overlaps.iter().any(|selector| {
                mask.item_ids.as_ref().map_or_else(
                    || selectors_overlap(mask.selector, *selector),
                    |ids| items_overlap_prefix(record, ids, selector.cursor),
                )
            });
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
            .find(|mask| masks_overlap(record, mask, addition))
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
            item_ids: addition.item_ids.clone(),
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

fn masks_overlap(record: &SessionRecord, left: &SessionMask, right: &SessionMaskAddition) -> bool {
    match (&left.item_ids, &right.item_ids) {
        (Some(left), Some(right)) => {
            let right = right.iter().collect::<HashSet<_>>();
            left.iter().any(|id| right.contains(id))
        }
        (Some(ids), None) => items_overlap_prefix(record, ids, right.selector.cursor),
        (None, Some(ids)) => items_overlap_prefix(record, ids, left.selector.cursor),
        (None, None) => selectors_overlap(left.selector, right.selector),
    }
}

fn items_overlap_prefix(
    record: &SessionRecord,
    ids: &[TimelineItemId],
    cursor: TimelineCursor,
) -> bool {
    let ids = ids.iter().collect::<HashSet<_>>();
    record
        .timeline_events()
        .any(|event| event.cursor <= cursor && ids.contains(&event.item_id))
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
        MemorySessionStore, PendingTimelineEvent, PendingTimelineItemId, SessionContextMode,
        SessionCopy, SessionCreateRequest, SessionStore, SessionStoreError, TimelineContent,
    };

    async fn sibling_source() -> (
        MemorySessionStore,
        SessionRecord,
        super::super::ContextIdentity,
        serde_json::Value,
    ) {
        let store = MemorySessionStore::new();
        let (record, context, checkpoint) = populate_sibling_source(&store).await;
        (store, record, context, checkpoint)
    }

    async fn populate_sibling_source(
        store: &impl SessionStore,
    ) -> (
        SessionRecord,
        super::super::ContextIdentity,
        serde_json::Value,
    ) {
        let id = super::super::SessionId::new("siblings");
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        store
            .append_timeline(
                &id,
                store.load(&id).await.unwrap().execution_token().unwrap(),
                vec![
                    PendingTimelineEvent::assistant_text_snapshot(
                        id.clone(),
                        0,
                        PendingTimelineItemId::New,
                        "left original",
                    ),
                    PendingTimelineEvent::assistant_text_snapshot(
                        id.clone(),
                        0,
                        PendingTimelineItemId::New,
                        "right original",
                    ),
                ],
            )
            .await
            .unwrap();
        store
            .append_journal(
                &id,
                store.load(&id).await.unwrap().execution_token().unwrap(),
                vec![SessionJournalEvent::TurnFinished { turn: 0 }],
            )
            .await
            .unwrap();
        let checkpoint = serde_json::json!({"opaque": "checkpoint"});
        let record = store
            .set_metadata_value_conditionally(
                &id,
                "host.checkpoint".into(),
                checkpoint.clone(),
                super::super::MetadataSetCondition::Unconditional,
            )
            .await
            .unwrap()
            .record;
        let context = super::super::ContextIdentity::capture(
            &record, TimelineCursor::new(2), "pinned-authority").unwrap();
        (record, context, checkpoint)
    }

    fn sibling_mutation(
        context: &super::super::ContextIdentity,
        checkpoint: &serde_json::Value,
        key: &str,
        cursor: u64,
        known: Vec<(String, SessionMaskId)>,
    ) -> SessionMaskMutation {
        let mut mutation = SessionMaskMutation::new(Some(context.boundary()));
        mutation.expected_completed_source = Some(context.clone());
        mutation.expected_active_masks = Some(known);
        mutation.metadata_predicates.push(super::super::MetadataPredicate {
            target: super::super::MetadataTarget::Session,
            key: "host.checkpoint".into(), expected: Some(checkpoint.clone()),
        });
        mutation.expected_masks = vec![(key.into(), None)];
        mutation.add.push(SessionMaskAddition::items(
            key,
            context.boundary(),
            vec![TimelineItemId::from_first_cursor(TimelineCursor::new(
                cursor,
            ))],
            ReplacementUserContent::new(format!("{key} summary")),
        ));
        mutation
    }

    #[tokio::test]
    async fn disjoint_siblings_apply_with_original_source_and_exact_known_administrative_successors()
     {
        let (store, record, context, checkpoint) = sibling_source().await;
        let first = store
            .mutate_masks(
                &record.id,
                sibling_mutation(&context, &checkpoint, "left", 1, Vec::new()),
            )
            .await
            .unwrap();
        let known = first
            .added_masks
            .iter()
            .map(|mask| (mask.key.clone(), mask.id.clone()))
            .collect();
        let second = store
            .mutate_masks(
                &record.id,
                sibling_mutation(&context, &checkpoint, "right", 2, known),
            )
            .await
            .unwrap();
        assert_eq!(second.added_masks.len(), 1);
        let current = store.load(&record.id).await.unwrap();
        assert_eq!(current.active_masks().len(), 2);
        assert!(!context.matches(&current));
        assert!(context.matches_completed_source(&current));
        assert_eq!(
            current.metadata.values["host.checkpoint"],
            checkpoint
        );
        let effective = current.materialize_context(SessionContextMode::Effective);
        assert_eq!(
            effective
                .prompt_turns
                .iter()
                .map(|prompt| prompt.prompt.as_str())
                .collect::<Vec<_>>(),
            ["left summary", "right summary"]
        );
        assert!(effective.turns.is_empty());
        let rollback_len = current.journal.len() as u64;
        let left = current
            .active_masks()
            .into_iter()
            .find(|mask| mask.key == "left")
            .unwrap();
        let known = current
            .active_masks()
            .iter()
            .map(|mask| (mask.key.clone(), mask.id.clone()))
            .collect();
        let mut replacement = sibling_mutation(
            &context,
            &checkpoint,
            "left",
            1,
            known,
        );
        replacement.expected_masks = vec![("left".into(), Some(left.id.clone()))];
        replacement.subtract.keys.push("left".into());
        replacement.add[0].replacement = ReplacementUserContent::new("left replacement");
        store.mutate_masks(&record.id, replacement).await.unwrap();
        let replaced = store.load(&record.id).await.unwrap();
        let copy = SessionCopy::from_record_effective(&replaced, context.boundary()).unwrap();
        let mut request = SessionCreateRequest::new("durable-helper");
        request.start = super::super::SessionStart::Copy {
            copy,
            messages: Vec::new(),
        };
        let helper_record = store.create(request).await.unwrap();
        assert!(
            !serde_json::to_string(&helper_record)
                .unwrap()
                .contains("original")
        );
        let helper = helper_record.materialize_context(SessionContextMode::Raw);
        assert_eq!(
            helper
                .prompt_turns
                .iter()
                .map(|prompt| prompt.prompt.as_str())
                .collect::<Vec<_>>(),
            ["left replacement", "right summary"]
        );
        let original = SessionCopy::selected(
            &replaced,
            context.boundary(),
            &[TimelineItemId::from_first_cursor(TimelineCursor::new(1))],
        )
        .unwrap();
        assert!(original.timeline().iter().any(|event| event.item_id == TimelineItemId::from_first_cursor(TimelineCursor::new(1)) && matches!(&event.content, TimelineContent::TextSnapshot { text } if text == "left original")));
        assert!(context.matches_completed_source(&replaced));
        store
            .append_journal(
                &record.id,
                replaced.execution_token().unwrap(),
                vec![SessionJournalEvent::Rollback {
                    target: SessionRollbackTarget {
                        journal_len: rollback_len,
                        after_cursor: Some(context.boundary()),
                        next_turn: 1,
                        remove_prompt_index: None,
                    },
                }],
            )
            .await
            .unwrap();
        let rolled_back = store.load(&record.id).await.unwrap();
        assert_eq!(
            rolled_back
                .active_masks()
                .iter()
                .find(|mask| mask.key == "left")
                .unwrap()
                .id,
            left.id
        );
        assert!(!context.matches_completed_source(&rolled_back));
        let mut request = SessionCreateRequest::new("rollback-helper");
        request.start = super::super::SessionStart::Copy {
            copy: SessionCopy::from_record_effective(&rolled_back, context.boundary()).unwrap(),
            messages: Vec::new(),
        };
        let helper = store
            .create(request)
            .await
            .unwrap()
            .materialize_context(SessionContextMode::Raw);
        assert_eq!(
            helper
                .prompt_turns
                .iter()
                .map(|prompt| prompt.prompt.as_str())
                .collect::<Vec<_>>(),
            ["left summary", "right summary"]
        );
    }

    #[tokio::test]
    async fn filesystem_context_preconditions_compare_the_pre_transaction_revision() {
        let directory = tempfile::tempdir().unwrap();
        let store = super::super::FilesystemSessionStore::new(directory.path());
        let (record, context, checkpoint) = populate_sibling_source(&store).await;
        let mut first = sibling_mutation(&context, &checkpoint, "left", 1, Vec::new());
        first.expected_completed_source = None;
        first.expected_active_masks = None;
        first.expected_context = Some(context.clone());
        let committed = store.mutate_masks(&record.id, first.clone()).await.unwrap();
        let before = store.load(&record.id).await.unwrap();
        assert!(!context.matches(&before));
        assert!(store.mutate_masks(&record.id, first).await.is_err());
        assert_eq!(store.load(&record.id).await.unwrap(), before);
        let known = committed
            .added_masks
            .iter()
            .map(|mask| (mask.key.clone(), mask.id.clone()))
            .collect();
        store
            .mutate_masks(
                &record.id,
                sibling_mutation(&context, &checkpoint, "right", 2, known),
            )
            .await
            .unwrap();
        let current = store.load(&record.id).await.unwrap();
        assert_eq!(current.active_masks().len(), 2);
        assert!(context.matches_completed_source(&current));
    }

    #[tokio::test]
    async fn unseen_sibling_or_newer_checkpoint_is_explicit_conflict_and_preserves_committed_effects()
     {
        let (store, record, context, checkpoint) = sibling_source().await;
        let left = sibling_mutation(&context, &checkpoint, "left", 1, Vec::new());
        let right = sibling_mutation(&context, &checkpoint, "right", 2, Vec::new());
        let (left, right) = tokio::join!(
            store.mutate_masks(&record.id, left),
            store.mutate_masks(&record.id, right)
        );
        assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
        assert_eq!(
            store.load(&record.id).await.unwrap().active_masks().len(),
            1
        );
        let committed = left.or(right).unwrap();
        let (key, cursor) = if committed.added_masks[0].key == "left" {
            ("right", 2)
        } else {
            ("left", 1)
        };
        let known = committed
            .added_masks
            .iter()
            .map(|mask| (mask.key.clone(), mask.id.clone()))
            .collect();
        let pending = sibling_mutation(&context, &checkpoint, key, cursor, known);
        store
            .set_metadata_value(
                &record.id,
                "host.checkpoint".into(),
                serde_json::json!({"newer":"checkpoint"}),
            )
            .await
            .unwrap();
        let before = store.load(&record.id).await.unwrap();
        assert!(store.mutate_masks(&record.id, pending).await.is_err());
        assert_eq!(store.load(&record.id).await.unwrap(), before);
    }

    #[tokio::test]
    async fn sibling_source_identity_rejects_revision_rollback_incarnation_agent_and_inference_drift()
     {
        let (_store, record, context, checkpoint) = sibling_source().await;
        let mut cases = Vec::new();
        let mut changed = record.clone();
        changed.revision = changed.revision.next();
        cases.push(changed);
        let mut changed = record.clone();
        changed.metadata.incarnation = Some("another incarnation".into());
        cases.push(changed);
        let mut changed = record.clone();
        changed.agent_definition = Some(crate::agent::session::opaque_test_configuration(
            "changed",
            vec!["changed".into()],
        ));
        cases.push(changed);
        let mut changed = record.clone();
        changed.inference_settings = Some(crate::InferenceSettings {
            provider: crate::InferenceProviderSettings {
                key: "changed".into(),
                api: crate::ModelApi::OpenAiResponses,
                url: "https://unused.invalid".into(),
                model_id: "changed".into(),
                model_cost: None,
                headers: Default::default(),
                oauth: None,
                reasoning_effort: None,
            },
        });
        cases.push(changed);
        let mut changed = record.clone();
        changed.append_journal_event(SessionJournalEvent::Rollback {
            target: SessionRollbackTarget {
                journal_len: record.journal.len() as u64,
                after_cursor: Some(context.boundary()),
                next_turn: 1,
                remove_prompt_index: None,
            },
        });
        cases.push(changed);
        for mut changed in cases {
            assert!(!context.matches_completed_source(&changed));
            let before = changed.clone();
            assert_eq!(
                mutate_record_masks(
                    &mut changed,
                    record.revision.next(),
                    &sibling_mutation(&context, &checkpoint, "left", 1, Vec::new())
                ),
                Err(SessionMaskError::ContextMismatch)
            );
            assert_eq!(changed, before);
        }
    }

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
        let raw = record.materialize_context(SessionContextMode::Raw);
        assert_eq!(raw.prompt_turns[0].prompt, "original question");
        assert!(matches!(
            &record.timeline_events().last().unwrap().content,
            TimelineContent::TextSnapshot { text } if text == "original answer"
        ));
        let effective = record.materialize_context(SessionContextMode::Effective);
        assert_eq!(effective.prompt_turns[0].prompt, "summary");
        assert!(effective.turns.is_empty());

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
                store.load(&id).await.unwrap().execution_token().unwrap(),
                vec![SessionJournalEvent::PromptAppended {
                    prompt: "question".to_owned(),
                }],
            )
            .await
            .unwrap();
        store
            .append_timeline(
                &id,
                store.load(&id).await.unwrap().execution_token().unwrap(),
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
                store.load(&id).await.unwrap().execution_token().unwrap(),
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
