//! Shared dual-domain storage contract tests.

use super::*;
use crate::{ExecutionSequence, MetadataTarget, MetadataUpdate, SessionMutation};
use serde_json::json;

async fn assert_conditional_configuration<S: SessionStore>(store: S) {
    let id = SessionId::new("conditional-cycle-config");
    let initial = store
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .unwrap();
    let token = initial.execution_token().unwrap();
    assert_eq!(
        store
            .set_agent_definition_if_execution(&id, token.clone(), None)
            .await
            .unwrap(),
        initial
    );
    let metadata = store
        .set_metadata_value(&id, "title".into(), json!("latest"))
        .await
        .unwrap();
    let configured = store
        .set_agent_definition_if_execution(&id, token.clone(), Some(json!({"cycle":true})))
        .await
        .unwrap();
    assert_eq!(configured.metadata_value("title"), Some(&json!("latest")));
    assert_eq!(configured.revision, metadata.revision.next());
    assert_eq!(
        configured.execution_sequence.get(),
        initial.execution_sequence.get() + 1
    );
    assert!(matches!(
        store
            .set_agent_definition_if_execution(&id, token, configured.agent_definition.clone())
            .await,
        Err(SessionStoreError::StaleExecution { .. })
    ));
    assert_eq!(store.load(&id).await.unwrap(), configured);
    assert_eq!(
        store
            .set_agent_definition_if_execution(
                &id,
                configured.execution_token().unwrap(),
                configured.agent_definition.clone()
            )
            .await
            .unwrap(),
        configured
    );
    let old = configured.execution_token().unwrap();
    store.delete(&id).await.unwrap();
    store
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .unwrap();
    let recreated = store
        .set_agent_definition(&id, configured.agent_definition.clone())
        .await
        .unwrap();
    assert_eq!(old.sequence, recreated.execution_sequence);
    assert!(matches!(
        store
            .set_agent_definition_if_execution(&id, old, recreated.agent_definition.clone())
            .await,
        Err(SessionStoreError::StaleExecution { .. })
    ));
    assert_eq!(store.load(&id).await.unwrap(), recreated);
}

#[tokio::test]
async fn conditional_configuration_checks_noop_and_preserves_metadata_memory_and_filesystem() {
    assert_conditional_configuration(MemorySessionStore::new()).await;
    let directory = tempfile::tempdir().unwrap();
    assert_conditional_configuration(FilesystemSessionStore::new(directory.path())).await;
    let store = FilesystemSessionStore::new(directory.path());
    let id = SessionId::new("conditional-config-legacy");
    let mut record = store
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .unwrap();
    let token = record.execution_token().unwrap();
    record.metadata.incarnation = None;
    let path = directory.path().join("conditional-config-legacy.json");
    let bytes = serde_json::to_vec(&record).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    assert!(matches!(
        store
            .set_agent_definition_if_execution(&id, token, None)
            .await,
        Err(SessionStoreError::ExecutionCompatibility { .. })
    ));
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

pub(super) async fn assert_revision_contract<S: SessionStore>(store: S) {
    let id = SessionId::new("dual-revision-contract");
    let created = store
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .unwrap();
    let initial = created.execution_token().unwrap();
    let title = store
        .mutate(
            &id,
            SessionMutation {
                expected_revision: Some(created.revision),
                updates: vec![MetadataUpdate {
                    target: MetadataTarget::Session,
                    key: "title".into(),
                    value: Some(json!("latest")),
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .record;
    assert_eq!(title.revision, created.revision.next());
    assert_eq!(title.execution_token().unwrap(), initial);
    assert!(matches!(
        store
            .mutate(
                &id,
                SessionMutation {
                    expected_revision: Some(created.revision),
                    ..Default::default()
                }
            )
            .await,
        Err(SessionStoreError::StaleRevision { .. })
    ));
    assert_eq!(store.load(&id).await.unwrap(), title);

    let appended = store
        .append_timeline(
            &id,
            initial.clone(),
            vec![PendingTimelineEvent::assistant_text_snapshot(
                id.clone(),
                0,
                PendingTimelineItemId::New,
                "answer",
            )],
        )
        .await
        .unwrap();
    assert_eq!(appended.commit.metadata.values["title"], json!("latest"));
    assert_eq!(appended.commit.revision, title.revision.next());
    assert_eq!(
        appended.commit.execution_token.sequence,
        ExecutionSequence::new(1)
    );
    assert_eq!(appended.events[0].cursor, TimelineCursor::new(1));
    let current = store.load(&id).await.unwrap();
    assert_eq!(appended.commit.metadata, current.metadata);
    assert!(matches!(
        store.append_journal(&id, initial, vec![]).await,
        Err(SessionStoreError::StaleExecution { .. })
    ));

    let item = MetadataTarget::Item {
        session_id: id.clone(),
        item_id: appended.events[0].item_id.clone(),
    };
    let annotated = store
        .mutate(
            &id,
            SessionMutation {
                updates: vec![MetadataUpdate {
                    target: item,
                    key: "opaque".into(),
                    value: Some(Value::Null),
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .record;
    assert_eq!(
        annotated.execution_token().unwrap(),
        current.execution_token().unwrap()
    );
    assert_eq!(annotated.journal, current.journal);

    let finished = store
        .append_journal(
            &id,
            annotated.execution_token().unwrap(),
            vec![SessionJournalEvent::TurnFinished { turn: 0 }],
        )
        .await
        .unwrap();
    let before = store.load(&id).await.unwrap();
    let mut masks = SessionMaskMutation::new(before.last_timeline_cursor());
    masks.add.push(crate::SessionMaskAddition::new(
        "opaque-mask",
        crate::ThroughCursor::new(TimelineCursor::new(1)),
        crate::ReplacementUserContent::new("replacement"),
    ));
    let combined = store
        .mutate(
            &id,
            SessionMutation {
                expected_revision: Some(before.revision),
                masks: Some(masks),
                updates: vec![MetadataUpdate {
                    target: MetadataTarget::Session,
                    key: "combined".into(),
                    value: Some(json!(true)),
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .record;
    assert_eq!(combined.revision, before.revision.next());
    assert_eq!(
        combined.execution_sequence.get(),
        before.execution_sequence.get() + 1
    );
    assert!(matches!(
        store
            .append_journal(&id, finished.execution_token, vec![])
            .await,
        Err(SessionStoreError::StaleExecution { .. })
    ));

    let predicate_only = store
        .mutate(&id, SessionMutation::default())
        .await
        .unwrap()
        .record;
    assert_eq!(predicate_only.revision, combined.revision.next());
    assert_eq!(
        predicate_only.execution_sequence,
        combined.execution_sequence
    );
    let failed = store
        .mutate(
            &id,
            SessionMutation {
                predicates: vec![crate::MetadataPredicate {
                    target: MetadataTarget::Session,
                    key: "missing".into(),
                    expected: Some(Value::Null),
                }],
                ..Default::default()
            },
        )
        .await;
    assert!(failed.is_err());
    assert_eq!(store.load(&id).await.unwrap(), predicate_only);

    let set = store
        .set_metadata_value(&id, "setter".into(), json!(true))
        .await
        .unwrap();
    assert_eq!(set.revision, predicate_only.revision.next());
    assert_eq!(set.execution_sequence, predicate_only.execution_sequence);
    let removed = store
        .remove_metadata_value(&id, "setter".into())
        .await
        .unwrap();
    assert_eq!(removed.revision, set.revision.next());
    assert_eq!(removed.execution_sequence, set.execution_sequence);
    let conditional = store
        .set_metadata_value_conditionally(
            &id,
            "title".into(),
            json!("ignored"),
            MetadataSetCondition::Absent,
        )
        .await
        .unwrap();
    assert!(!conditional.changed);
    assert_eq!(conditional.record, removed);

    let same = store
        .set_metadata_value(&id, "title".into(), json!("latest"))
        .await
        .unwrap();
    assert_eq!(same, removed);
    let unchanged = store.set_inference_settings(&id, None).await.unwrap();
    assert_eq!(unchanged, same);
    let unchanged = store.set_agent_definition(&id, None).await.unwrap();
    assert_eq!(unchanged, same);
    let unchanged = store
        .remove_metadata_value(&id, "absent".into())
        .await
        .unwrap();
    assert_eq!(unchanged, same);

    let old = same.execution_token().unwrap();
    let definition = store
        .set_agent_definition(&id, Some(json!({"opaque": 1})))
        .await
        .unwrap();
    assert_eq!(definition.revision, same.revision.next());
    assert_eq!(
        definition.execution_sequence.get(),
        same.execution_sequence.get() + 1
    );
    assert!(matches!(
        store.append_journal(&id, old, vec![]).await,
        Err(SessionStoreError::StaleExecution { .. })
    ));
    assert_eq!(
        store
            .set_agent_definition(&id, definition.agent_definition.clone())
            .await
            .unwrap(),
        definition
    );

    let old = definition.execution_token().unwrap();
    let settings = super::super::InferenceSettings {
        provider: crate::InferenceProviderSettings {
            key: "fixture".into(),
            api: crate::ModelApi::OpenAiResponses,
            url: String::new(),
            model_id: "fixture".into(),
            model_cost: None,
            headers: Default::default(),
            oauth: None,
            reasoning_effort: None,
        },
    };
    let configured = store
        .set_inference_settings(&id, Some(settings.clone()))
        .await
        .unwrap();
    assert_eq!(configured.revision, definition.revision.next());
    assert_eq!(
        configured.execution_sequence.get(),
        definition.execution_sequence.get() + 1
    );
    assert!(matches!(
        store.append_journal(&id, old, vec![]).await,
        Err(SessionStoreError::StaleExecution { .. })
    ));
    assert_eq!(
        store
            .set_inference_settings(&id, Some(settings))
            .await
            .unwrap(),
        configured
    );

    let before_rollback = configured.execution_token().unwrap();
    let rolled_back = store
        .append_journal(
            &id,
            before_rollback.clone(),
            vec![SessionJournalEvent::Rollback {
                target: super::super::SessionRollbackTarget {
                    journal_len: 0,
                    after_cursor: None,
                    next_turn: 0,
                    remove_prompt_index: None,
                },
            }],
        )
        .await
        .unwrap();
    assert_eq!(rolled_back.revision, configured.revision.next());
    assert!(matches!(
        store.append_journal(&id, before_rollback, vec![]).await,
        Err(SessionStoreError::StaleExecution { .. })
    ));

    let aba_id = SessionId::new("same-id-aba");
    let old = store
        .create(SessionCreateRequest::new(aba_id.clone()))
        .await
        .unwrap()
        .execution_token()
        .unwrap();
    store.delete(&aba_id).await.unwrap();
    let replacement = store
        .create(SessionCreateRequest::new(aba_id.clone()))
        .await
        .unwrap();
    assert_eq!(old.sequence, replacement.execution_sequence);
    assert_ne!(
        old.incarnation,
        replacement.metadata.incarnation.clone().unwrap()
    );
    assert!(matches!(
        store.append_journal(&aba_id, old, vec![]).await,
        Err(SessionStoreError::StaleExecution { .. })
    ));
    assert_eq!(store.load(&aba_id).await.unwrap(), replacement);
}

#[tokio::test]
async fn memory_dual_revision_receipts_settings_masks_and_aba() {
    assert_revision_contract(MemorySessionStore::new()).await;
}

#[tokio::test]
async fn filesystem_dual_revision_receipts_settings_masks_and_aba() {
    let directory = tempfile::tempdir().unwrap();
    assert_revision_contract(FilesystemSessionStore::new(directory.path())).await;
}

#[tokio::test]
async fn filesystem_legacy_seed_read_only_until_authorized_write_and_missing_incarnation() {
    let directory = tempfile::tempdir().unwrap();
    let store = FilesystemSessionStore::new(directory.path());
    let mut record = SessionRecord::new("legacy-sequence");
    record.revision = Revision::new(47);
    record
        .journal
        .push(SessionJournalEvent::MiddlewareStateUpdated {
            namespace: "opaque".into(),
            state: json!({"kept": true}),
        });
    let mut old = serde_json::to_value(&record).unwrap();
    old.as_object_mut().unwrap().remove("execution_sequence");
    let path = directory.path().join("legacy-sequence.json");
    let original = serde_json::to_vec_pretty(&old).unwrap();
    std::fs::write(&path, &original).unwrap();
    let loaded = store.load(&record.id).await.unwrap();
    assert_eq!(loaded.execution_sequence, ExecutionSequence::new(47));
    assert_eq!(loaded.journal, record.journal);
    assert_eq!(std::fs::read(&path).unwrap(), original);
    let receipt = store
        .append_journal(
            &record.id,
            loaded.execution_token().unwrap(),
            vec![SessionJournalEvent::Interrupted],
        )
        .await
        .unwrap();
    let persisted: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(persisted["execution_sequence"], json!(48));
    assert_eq!(receipt.revision, Revision::new(48));
    assert_eq!(
        store.load(&record.id).await.unwrap().journal[0],
        record.journal[0]
    );

    old["id"] = json!("legacy-no-incarnation");
    old["metadata"]
        .as_object_mut()
        .unwrap()
        .remove("incarnation");
    std::fs::write(
        directory.path().join("legacy-no-incarnation.json"),
        serde_json::to_vec(&old).unwrap(),
    )
    .unwrap();
    let legacy = store
        .load(&SessionId::new("legacy-no-incarnation"))
        .await
        .unwrap();
    assert_eq!(store.list().await.unwrap().len(), 2);
    let error = legacy.execution_token().unwrap_err();
    assert!(matches!(
        error,
        SessionStoreError::ExecutionCompatibility { .. }
    ));
    assert!(error.to_string().contains("no durable incarnation"));
}
