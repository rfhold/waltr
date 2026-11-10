//! Opaque host metadata and atomic history mutations. Values never confer execution authority.

#![allow(missing_docs)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use crate::TimelineItemId;
use super::{
    ContextIdentity, Revision, SessionId, SessionMaskMutation, SessionMaskMutationOutcome,
    SessionRecord,
};

/// Opaque annotations indexed by session-local canonical item identity.
pub type ItemMetadata = BTreeMap<TimelineItemId, BTreeMap<String, Value>>;

pub const MAX_METADATA_KEY_BYTES: usize = 128;
pub const MAX_METADATA_VALUE_BYTES: usize = 65_536;
pub const MAX_METADATA_KEYS: usize = 128;
pub const MAX_METADATA_ITEMS: usize = 4096;
pub const MAX_METADATA_MUTATION_ENTRIES: usize = 8192;
pub const MAX_METADATA_BYTES: usize = 1024 * 1024;
pub const MAX_SESSION_MUTATION_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum MetadataTarget {
    Session,
    Item {
        session_id: SessionId,
        item_id: TimelineItemId,
    },
}

/// `None` expects absence; `Some(Value::Null)` expects a present JSON null.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataPredicate {
    pub target: MetadataTarget,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present_value")]
    pub expected: Option<Value>,
}

/// `None` removes a key; `Some(Value::Null)` stores a JSON null.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataUpdate {
    pub target: MetadataTarget,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present_value")]
    pub value: Option<Value>,
}

fn present_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

/// One store transaction, using the backend's existing complete-record mutation mechanism.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionMutation {
    /// Exact durable record incarnation, including before any completed history exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_incarnation: Option<String>,
    pub expected_revision: Option<Revision>,
    pub expected_context: Option<ContextIdentity>,
    pub predicates: Vec<MetadataPredicate>,
    pub updates: Vec<MetadataUpdate>,
    pub masks: Option<SessionMaskMutation>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionMutationOutcome {
    pub record: SessionRecord,
    pub masks: Option<SessionMaskMutationOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataError {
    InvalidKey,
    Bounds,
    UnknownItem(TimelineItemId),
    DuplicateUpdate,
    Precondition,
}

impl std::fmt::Display for MetadataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "metadata mutation rejected: {self:?}")
    }
}
impl std::error::Error for MetadataError {}

pub(super) fn validate_key(key: &str) -> Result<(), MetadataError> {
    if key.is_empty()
        || key.len() > MAX_METADATA_KEY_BYTES
        || key.starts_with('_')
        || key.starts_with("waltr.")
        || key.starts_with("engine.")
        || key.chars().any(char::is_control)
    {
        return Err(MetadataError::InvalidKey);
    }
    Ok(())
}

fn validate_value(value: &Option<Value>) -> Result<(), MetadataError> {
    if !super::context::json_within_limit(value, MAX_METADATA_VALUE_BYTES) {
        return Err(MetadataError::Bounds);
    }
    Ok(())
}

pub(super) fn validate_record(record: &SessionRecord) -> Result<(), MetadataError> {
    if record.metadata.item_values.len() > MAX_METADATA_ITEMS
        || !super::context::json_within_limit(
            &(&record.metadata.values, &record.metadata.item_values),
            MAX_METADATA_BYTES,
        )
    {
        return Err(MetadataError::Bounds);
    }
    for values in std::iter::once(&record.metadata.values).chain(record.metadata.item_values.values()) {
        if values.len() > MAX_METADATA_KEYS {
            return Err(MetadataError::Bounds);
        }
        for (key, value) in values {
            validate_key(key)?;
            validate_value(&Some(value.clone()))?;
        }
    }
    Ok(())
}

pub(super) fn set_value(
    record: &mut SessionRecord,
    key: String,
    value: Value,
) -> Result<(), MetadataError> {
    validate_set(&key, &value)?;
    if record.metadata.values.get(&key) == Some(&value) {
        return Ok(());
    }
    let mut updated = record.clone();
    apply(
        &mut updated,
        &SessionMutation {
            updates: vec![MetadataUpdate {
                target: MetadataTarget::Session,
                key,
                value: Some(value),
            }],
            ..Default::default()
        },
    )?;
    updated.advance_revision(false);
    *record = updated;
    Ok(())
}

pub(super) fn validate_set(key: &str, value: &Value) -> Result<(), MetadataError> {
    validate_key(key)?;
    validate_value(&Some(value.clone()))
}

fn canonical_item_ids(record: &SessionRecord) -> Result<BTreeSet<TimelineItemId>, MetadataError> {
    if record.journal.len() > 100_000 {
        return Err(MetadataError::Bounds);
    }
    let mut first = BTreeMap::new();
    for event in record.timeline_events() {
        if event.session_id == record.id {
            first.entry(&event.item_id).or_insert(event.cursor);
        }
    }
    Ok(first.into_iter().filter_map(|(id, cursor)| {
        (TimelineItemId::from_first_cursor(cursor) == *id).then(|| id.clone())
    }).collect())
}

fn validate_target(record: &SessionRecord, target: &MetadataTarget) -> Result<(), MetadataError> {
    let ids = match target {
        MetadataTarget::Session => BTreeSet::new(),
        MetadataTarget::Item { .. } => canonical_item_ids(record)?,
    };
    validate_target_with_ids(record, target, &ids)
}

fn validate_target_with_ids(
    record: &SessionRecord,
    target: &MetadataTarget,
    ids: &BTreeSet<TimelineItemId>,
) -> Result<(), MetadataError> {
    if let MetadataTarget::Item { session_id, item_id: id } = target {
        if session_id != &record.id || !ids.contains(id) {
            return Err(MetadataError::UnknownItem(id.clone()));
        }
    }
    Ok(())
}

impl SessionRecord {
    /// Bounded opaque values; the target's session namespace is checked before reading items.
    pub fn metadata_values(
        &self,
        target: &MetadataTarget,
    ) -> Result<Option<&BTreeMap<String, Value>>, MetadataError> {
        validate_record(self)?;
        validate_target(self, target)?;
        Ok(match target {
            MetadataTarget::Session => Some(&self.metadata.values),
            MetadataTarget::Item { item_id, .. } => self.metadata.item_values.get(item_id),
        })
    }

    /// Read metadata only for a currently effective canonical item in this session.
    pub fn item_metadata_values(
        &self,
        id: &TimelineItemId,
    ) -> Result<Option<&BTreeMap<String, Value>>, MetadataError> {
        self.metadata_values(&MetadataTarget::Item {
            session_id: self.id.clone(),
            item_id: id.clone(),
        })
    }
}

pub(super) fn apply(
    record: &mut SessionRecord,
    mutation: &SessionMutation,
) -> Result<(), MetadataError> {
    if mutation
        .predicates
        .len()
        .saturating_add(mutation.updates.len())
        > MAX_METADATA_MUTATION_ENTRIES
        || !super::context::json_within_limit(
            &(&mutation.predicates, &mutation.updates),
            MAX_METADATA_BYTES,
        )
    {
        return Err(MetadataError::Bounds);
    }
    let has_items = mutation
        .predicates
        .iter()
        .map(|predicate| &predicate.target)
        .chain(mutation.updates.iter().map(|update| &update.target))
        .any(|target| matches!(target, MetadataTarget::Item { .. }));
    let ids = if has_items {
        canonical_item_ids(record)?
    } else {
        BTreeSet::new()
    };
    for predicate in &mutation.predicates {
        validate_target_with_ids(record, &predicate.target, &ids)?;
        validate_key(&predicate.key)?;
        validate_value(&predicate.expected)?;
        let actual = match &predicate.target {
            MetadataTarget::Session => record.metadata.values.get(&predicate.key),
            MetadataTarget::Item { item_id: id, .. } => record
                .metadata
                .item_values
                .get(id)
                .and_then(|v| v.get(&predicate.key)),
        };
        if actual != predicate.expected.as_ref() {
            return Err(MetadataError::Precondition);
        }
    }
    let mut seen = BTreeSet::new();
    for update in &mutation.updates {
        validate_target_with_ids(record, &update.target, &ids)?;
        validate_key(&update.key)?;
        validate_value(&update.value)?;
        if !seen.insert((&update.target, &update.key)) {
            return Err(MetadataError::DuplicateUpdate);
        }
    }
    for update in &mutation.updates {
        let values = match &update.target {
            MetadataTarget::Session => &mut record.metadata.values,
            MetadataTarget::Item { item_id: id, .. } => {
                record.metadata.item_values.entry(id.clone()).or_default()
            }
        };
        if let Some(value) = &update.value {
            values.insert(update.key.clone(), value.clone());
        } else {
            values.remove(&update.key);
        }
    }
    record
        .metadata
        .item_values
        .retain(|_, values| !values.is_empty());
    validate_record(record)?;
    if !mutation.updates.is_empty() {
        record.touch_metadata();
    }
    Ok(())
}

#[cfg(test)]
pub(super) async fn assert_store_contract<
    S: super::SessionStore + Clone + Send + Sync + 'static,
>(
    store: S,
    label: &str,
) {
    use crate::{
        PendingTimelineEvent, PendingTimelineItemId, SessionCreateRequest, SessionJournalEvent,
        TimelineCursor,
    };
    assert_incarnation_store_contract(&store, label).await;
    let id = SessionId::new(format!("metadata-{label}"));
    store
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .unwrap();
    let events = store
        .append_timeline(
            &id,
            store.load(&id).await.unwrap().execution_token().unwrap(),
            vec![PendingTimelineEvent::assistant_text_snapshot(
                id.clone(),
                0,
                PendingTimelineItemId::New,
                "raw evidence",
            )],
        )
        .await
        .unwrap()
        .events;
    store
        .append_journal(
            &id,
            store.load(&id).await.unwrap().execution_token().unwrap(),
            vec![SessionJournalEvent::TurnFinished { turn: 0 }],
        )
        .await
        .unwrap();
    let before = store.load(&id).await.unwrap();
    let target = MetadataTarget::Item {
        session_id: id.clone(),
        item_id: events[0].item_id.clone(),
    };
    let update = |target, key: &str, value| MetadataUpdate {
        target,
        key: key.into(),
        value,
    };
    let predicate = |target, key: &str, expected| MetadataPredicate {
        target,
        key: key.into(),
        expected,
    };
    let mutation = SessionMutation {
        expected_revision: Some(before.revision),
        predicates: vec![predicate(MetadataTarget::Session, "opaque", None)],
        updates: vec![
            update(MetadataTarget::Session, "opaque", Some(Value::Null)),
            update(
                target.clone(),
                "arbitrary.世界",
                Some(serde_json::json!({"schema": [1, null, "free"]})),
            ),
        ],
        ..Default::default()
    };
    let written = store.mutate(&id, mutation.clone()).await.unwrap().record;
    assert_eq!(written.journal, before.journal);
    assert_eq!(written.snapshot().turns, before.snapshot().turns);
    assert_eq!(written.metadata_value("opaque"), Some(&Value::Null));
    assert_eq!(
        written
            .item_metadata_values(&events[0].item_id)
            .unwrap()
            .unwrap()["arbitrary.世界"],
        serde_json::json!({"schema": [1, null, "free"]})
    );
    assert_eq!(
        serde_json::from_value::<SessionRecord>(serde_json::to_value(&written).unwrap()).unwrap(),
        written
    );
    assert!(store.mutate(&id, mutation).await.is_err());
    assert_eq!(store.load(&id).await.unwrap(), written);

    let mut mask = super::SessionMaskMutation::new(Some(TimelineCursor::new(1)));
    mask.add.push(super::SessionMaskAddition::new(
        "free-mask",
        super::ThroughCursor::new(TimelineCursor::new(1)),
        super::ReplacementUserContent::new("replacement"),
    ));
    let rejected = SessionMutation {
        predicates: vec![predicate(MetadataTarget::Session, "opaque", None)],
        updates: vec![update(
            MetadataTarget::Session,
            "partial",
            Some(Value::Bool(true)),
        )],
        masks: Some(mask.clone()),
        ..Default::default()
    };
    assert!(store.mutate(&id, rejected).await.is_err());
    assert_eq!(store.load(&id).await.unwrap(), written);
    let invalid_mask = SessionMutation {
        updates: vec![update(
            MetadataTarget::Session,
            "partial",
            Some(Value::Bool(true)),
        )],
        masks: Some(super::SessionMaskMutation::new(None)),
        ..Default::default()
    };
    assert!(store.mutate(&id, invalid_mask).await.is_err());
    assert_eq!(store.load(&id).await.unwrap(), written);
    let committed = store
        .mutate(
            &id,
            SessionMutation {
                predicates: vec![predicate(
                    MetadataTarget::Session,
                    "opaque",
                    Some(Value::Null),
                )],
                updates: vec![update(MetadataTarget::Session, "opaque", None)],
                masks: Some(mask),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(committed.record.metadata_value("opaque").is_none());
    assert_eq!(committed.record.active_masks().len(), 1);
    assert_eq!(committed.record.snapshot().turns, before.snapshot().turns);

    for update in [
        update(MetadataTarget::Session, "_private", Some(Value::Null)),
        update(
            MetadataTarget::Session,
            "waltr.authority",
            Some(Value::Null),
        ),
        update(
            MetadataTarget::Session,
            "engine.authority",
            Some(Value::Null),
        ),
        update(
            MetadataTarget::Session,
            &"x".repeat(MAX_METADATA_KEY_BYTES + 1),
            Some(Value::Null),
        ),
        update(
            MetadataTarget::Session,
            "large",
            Some(Value::String("x".repeat(MAX_METADATA_VALUE_BYTES))),
        ),
        update(
            MetadataTarget::Item {
                session_id: SessionId::new("foreign"),
                item_id: events[0].item_id.clone(),
            },
            "x",
            Some(Value::Null),
        ),
        update(
            MetadataTarget::Item {
                session_id: id.clone(),
                item_id: crate::TimelineItemId::from_first_cursor(TimelineCursor::new(999)),
            },
            "x",
            Some(Value::Null),
        ),
    ] {
        assert!(
            store
                .mutate(
                    &id,
                    SessionMutation {
                        updates: vec![update],
                        ..Default::default()
                    }
                )
                .await
                .is_err()
        );
        assert_eq!(store.load(&id).await.unwrap(), committed.record);
    }
    let context =
        ContextIdentity::capture(&committed.record, TimelineCursor::new(1), "host-authority")
            .unwrap();
    let mutation = SessionMutation {
        updates: vec![update(
            MetadataTarget::Session,
            "concurrent",
            Some(Value::Bool(true)),
        )],
        ..Default::default()
    };
    let append = async {
        store
            .append_journal(
                &id,
                committed.record.execution_token().unwrap(),
                vec![SessionJournalEvent::PromptAppended {
                    prompt: "concurrent journal".into(),
                }],
            )
            .await
            .unwrap();
    };
    let (changed, ()) = tokio::join!(store.mutate(&id, mutation), append);
    changed.unwrap();
    let current = store.load(&id).await.unwrap();
    assert_eq!(
        current.metadata_value("concurrent"),
        Some(&Value::Bool(true))
    );
    assert!(
        current
            .journal
            .contains(&SessionJournalEvent::PromptAppended {
                prompt: "concurrent journal".into()
            })
    );
    assert!(
        store
            .mutate(
                &id,
                SessionMutation {
                    expected_context: Some(context),
                    updates: vec![update(target, "stale", Some(Value::Null))],
                    ..Default::default()
                }
            )
            .await
            .is_err()
    );
    assert_eq!(store.load(&id).await.unwrap(), current);
}

#[cfg(test)]
async fn assert_incarnation_store_contract(store: &impl super::SessionStore, label: &str) {
    use super::{SessionCreateRequest, SessionMaskMutation};

    let id = SessionId::new(format!("metadata-incarnation-{label}"));
    let original = store.create(SessionCreateRequest::new(id.clone())).await.unwrap();
    let expected_incarnation = original.metadata.incarnation.clone().unwrap();
    let mut mutation = SessionMutation {
        expected_incarnation: Some(expected_incarnation.clone()),
        expected_revision: Some(original.revision),
        updates: vec![MetadataUpdate {
            target: MetadataTarget::Session,
            key: "opaque".into(),
            value: Some(Value::Bool(true)),
        }],
        ..Default::default()
    };
    store.delete(&id).await.unwrap();
    let mut request = SessionCreateRequest::new(id.clone());
    request.metadata_values.insert("incarnation".into(), Value::String(expected_incarnation));
    let recreated = store.create(request).await.unwrap();
    assert_eq!(recreated.revision, original.revision);
    assert_ne!(recreated.metadata.incarnation, original.metadata.incarnation);
    assert!(recreated.journal.is_empty());

    for masks in [None, Some(SessionMaskMutation::new(None))] {
        mutation.masks = masks;
        assert_eq!(
            store.mutate(&id, mutation.clone()).await.unwrap_err(),
            super::SessionStoreError::MetadataMutation { error: MetadataError::Precondition },
        );
        assert_eq!(store.load(&id).await.unwrap(), recreated);
    }

    mutation.expected_incarnation = recreated.metadata.incarnation.clone();
    mutation.masks = None;
    let committed = store.mutate(&id, mutation).await.unwrap().record;
    assert_eq!(committed.metadata_value("opaque"), Some(&Value::Bool(true)));
    assert_eq!(committed.journal, recreated.journal);
    assert!(committed.active_masks().is_empty());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PendingTimelineEvent, PendingTimelineItemId, SessionJournalEvent, TimelineCursor};

    fn record() -> SessionRecord {
        let mut record = SessionRecord::new("source");
        for cursor in [1, 2] {
            record.append_journal_event(SessionJournalEvent::Timeline(PendingTimelineEvent::assistant_text_snapshot(
                record.id.clone(), 0, PendingTimelineItemId::New, "raw").with_cursor(TimelineCursor::new(cursor))));
        }
        record.append_journal_event(SessionJournalEvent::TurnFinished { turn: 0 });
        record
    }

    #[test]
    fn serialized_predicates_and_updates_preserve_absence_versus_json_null() {
        for value in [None, Some(Value::Null), Some(serde_json::json!({"opaque": true}))] {
            let predicate = MetadataPredicate { target: MetadataTarget::Session, key: "free".into(), expected: value.clone() };
            let update = MetadataUpdate { target: MetadataTarget::Session, key: "free".into(), value };
            assert_eq!(serde_json::from_value::<MetadataPredicate>(serde_json::to_value(&predicate).unwrap()).unwrap(), predicate);
            assert_eq!(serde_json::from_value::<MetadataUpdate>(serde_json::to_value(&update).unwrap()).unwrap(), update);
        }
        assert!(serde_json::from_value::<MetadataTarget>(serde_json::json!({"Item": {
            "session_id": "source", "item_id": "i1", "unexpected": true
        }})).is_err());
    }

    #[test]
    fn incarnation_precondition_rejects_missing_identity_before_metadata_or_masks_change() {
        let mut record = SessionRecord::new("early-session");
        let expected_incarnation = record.metadata.incarnation.take();
        let original = record.clone();
        for masks in [None, Some(super::super::SessionMaskMutation::new(None))] {
            let mutation = SessionMutation {
                expected_incarnation: expected_incarnation.clone(),
                expected_revision: Some(record.revision),
                updates: vec![MetadataUpdate {
                    target: MetadataTarget::Session,
                    key: "opaque".into(),
                    value: Some(Value::Null),
                }],
                masks,
                ..Default::default()
            };
            assert_eq!(
                super::super::storage::mutate_on_record(&mut record, Revision::new(1), &mutation),
                Err(super::super::SessionStoreError::MetadataMutation { error: MetadataError::Precondition }),
            );
            assert_eq!(record, original);
        }
    }

    #[tokio::test]
    async fn delete_recreate_same_revision_rejects_stale_incarnation_without_history() {
        assert_incarnation_store_contract(&crate::MemorySessionStore::new(), "early-aba").await;
    }

    #[test]
    fn incarnation_precondition_is_optional_in_serialized_mutations() {
        let mutation = SessionMutation::default();
        let encoded = serde_json::to_value(&mutation).unwrap();
        assert!(encoded.get("expected_incarnation").is_none());
        assert_eq!(serde_json::from_value::<SessionMutation>(encoded).unwrap(), mutation);
        let mutation = SessionMutation {
            expected_incarnation: Some("durable-incarnation".into()),
            ..Default::default()
        };
        assert_eq!(serde_json::from_value::<SessionMutation>(serde_json::to_value(&mutation).unwrap()).unwrap(), mutation);
    }

    #[test]
    fn bounds_and_duplicate_updates_are_rejected_without_partial_record_mutation() {
        let original = record();
        let update = |key: String, value| MetadataUpdate { target: MetadataTarget::Session, key, value: Some(value) };
        let cases = vec![
            vec![update("same".into(), Value::Null), update("same".into(), Value::Bool(true))],
            (0..=MAX_METADATA_KEYS).map(|i| update(format!("key{i}"), Value::Null)).collect(),
            (0..32).map(|i| update(format!("key{i}"), Value::String("x".repeat(60_000)))).collect(),
            (0..=MAX_METADATA_MUTATION_ENTRIES).map(|i| update(format!("key{i}"), Value::Null)).collect(),
        ];
        for updates in cases {
            let mut changed = original.clone();
            assert!(super::super::storage::mutate_on_record(&mut changed, Revision::new(1), &SessionMutation { updates, ..Default::default() }).is_err());
            assert_eq!(changed, original);
        }
        let mut oversized = original.clone();
        oversized.metadata.item_values = (0..=MAX_METADATA_ITEMS).map(|i| (
            crate::TimelineItemId::from_first_cursor(TimelineCursor::new(i as u64 + 1)),
            BTreeMap::from([("key".into(), Value::Null)])
        )).collect();
        assert_eq!(validate_record(&oversized), Err(MetadataError::Bounds));
        assert!(oversized.metadata_values(&MetadataTarget::Session).is_err());
        let mut oversized = original.clone();
        oversized.metadata.values = (0..32).map(|i| (
            format!("key{i}"), Value::String("x".repeat(60_000))
        )).collect();
        assert_eq!(validate_record(&oversized), Err(MetadataError::Bounds));
        let mut reserved = original;
        reserved.metadata.values.insert("_reserved".into(), Value::Null);
        assert!(super::super::storage::decode_record(&serde_json::to_vec(&reserved).unwrap()).is_err());
    }

    #[test]
    fn annotations_follow_selected_copies_and_effective_projections_without_replay_edits() {
        let mut record = record();
        let original = record.journal.clone();
        let ids = [1, 2].map(|cursor| crate::TimelineItemId::from_first_cursor(TimelineCursor::new(cursor)));
        let mutation = SessionMutation { updates: ids.iter().map(|id| MetadataUpdate {
            target: MetadataTarget::Item { session_id: record.id.clone(), item_id: id.clone() }, key: "host.free".into(), value: Some(serde_json::json!({"arbitrary": id.as_str()})),
        }).collect(), ..Default::default() };
        super::super::storage::mutate_on_record(&mut record, Revision::new(1), &mutation).unwrap();
        assert_eq!(record.journal, original);
        let copy = super::super::SessionCopy::selected(&record, TimelineCursor::new(2), &ids[..1]).unwrap();
        assert_eq!(copy.item_metadata().len(), 1);
        let mut request = super::super::SessionCreateRequest::new("destination");
        request.start = super::super::SessionStart::Copy { copy, messages: Vec::new() };
        let destination = request.materialize();
        assert_eq!(destination.item_metadata_values(&ids[0]).unwrap(), record.item_metadata_values(&ids[0]).unwrap());
        assert!(destination.item_metadata_values(&ids[1]).is_err());
        let raw = record.materialize_context(super::super::SessionContextMode::Raw);
        assert_eq!(raw.item_metadata.len(), 2);
        let mut masks = super::super::SessionMaskMutation::new(Some(TimelineCursor::new(2)));
        masks.add.push(super::super::SessionMaskAddition::items("host-mask", TimelineCursor::new(2), vec![ids[0].clone()], super::super::ReplacementUserContent::new("masked")));
        super::super::storage::mutate_on_record(&mut record, Revision::new(2), &SessionMutation { masks: Some(masks), ..Default::default() }).unwrap();
        assert_eq!(record.materialize_context(super::super::SessionContextMode::Raw).item_metadata.len(), 2);
        assert_eq!(record.materialize_context(super::super::SessionContextMode::Effective).item_metadata.len(), 1);
        let effective = super::super::SessionCopy::from_record_effective(&record, TimelineCursor::new(2)).unwrap();
        assert!(!effective.item_metadata().contains_key(&ids[0]));
        let page = super::super::projection::project_timeline(&record, super::super::SessionTimelineMode::Effective, crate::TimelineQuery::forward(None, 100));
        assert!(!page.item_metadata.contains_key(&ids[0]));
        assert!(page.item_metadata.contains_key(&ids[1]));
        record.append_journal_event(SessionJournalEvent::Rollback { target: super::super::SessionRollbackTarget { journal_len: 0, after_cursor: None, next_turn: 0, remove_prompt_index: None } });
        assert!(record.item_metadata_values(&ids[0]).is_err());
        let before = record.clone();
        assert!(super::super::storage::mutate_on_record(&mut record, Revision::new(3), &mutation).is_err());
        assert_eq!(record, before);
    }
}
