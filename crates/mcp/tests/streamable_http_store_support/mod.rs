use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use mcp::{McpPrincipalId, server::*};
use serde_json::{Value, json};

pub const fn test_config() -> StreamableHttpSessionStoreConfig {
    StreamableHttpSessionStoreConfig {
        session_ttl: Duration::from_secs(2),
        max_sessions: 100,
        max_sessions_per_principal: 2,
        max_events_per_session: 12,
        max_pending_bytes_per_session: 4096,
        max_event_bytes: 1024,
        delivery_batch_size: 4,
        cleanup_batch_size: 2,
        delivery_lease: Duration::from_millis(100),
        initialization_claim_lease: Duration::from_millis(100),
    }
}

#[allow(clippy::too_many_lines)]
pub async fn assert_store_contract(
    first: Arc<dyn StreamableHttpSessionStore>,
    reopened: Arc<dyn StreamableHttpSessionStore>,
) {
    let now = SystemTime::now();
    let alice = owner("alice");
    let bob = owner("bob");

    let persistent = key("persistent", alice.clone());
    let created = create(&*first, persistent.clone(), now).await;
    let operation_id = created.initialization_operation_id;
    assert!(matches!(
        reopened.inspect_session(request(persistent.clone(), now)).await.unwrap(),
        StreamableHttpInspectSession::Session(session)
            if session.initialization_operation_id == operation_id
                && session.protocol_version == "2025-11-25"
    ));
    assert_eq!(
        reopened
            .inspect_session(request(key("persistent", bob.clone()), now))
            .await
            .unwrap(),
        StreamableHttpInspectSession::NotFound
    );
    assert!(matches!(
        reopened
            .inspect_session(request(key("persistent", owner("alice")), now))
            .await
            .unwrap(),
        StreamableHttpInspectSession::Session(_)
    ));
    let mut mismatch = request(persistent.clone(), now);
    mismatch.presented_protocol_version = Some("other".to_owned());
    assert_eq!(
        first.inspect_session(mismatch).await.unwrap(),
        StreamableHttpInspectSession::ProtocolVersionMismatch
    );

    let initialization_claim = claim(&*reopened, persistent.clone(), now).await;
    assert_eq!(initialization_claim.1, operation_id);
    let emitted = vec![json!({"initialization": "atomic"})];
    assert!(matches!(
        reopened.complete_initialization(StreamableHttpCompleteInitializationRequest {
            session: request(persistent.clone(), now),
            claim_id: initialization_claim.0.clone(),
            messages: emitted.clone(),
        }).await.unwrap(),
        StreamableHttpCompleteInitialization::Completed(events) if events[0].message == emitted[0]
    ));
    assert!(matches!(
        first
            .complete_initialization(StreamableHttpCompleteInitializationRequest {
                session: request(persistent.clone(), now),
                claim_id: initialization_claim.0,
                messages: vec![json!("must-not-duplicate")],
            })
            .await
            .unwrap(),
        StreamableHttpCompleteInitialization::AlreadyCompleted
    ));

    for operation in [
        reopened
            .append_events(StreamableHttpAppendEventsRequest {
                session: request(key("persistent", bob.clone()), now),
                messages: vec![json!("wrong-owner")],
            })
            .await
            .unwrap()
            == StreamableHttpAppendEvents::NotFound,
        reopened
            .prepare_delivery(StreamableHttpPrepareDeliveryRequest {
                session: request(key("persistent", bob.clone()), now),
                acknowledged_event_id: None,
            })
            .await
            .unwrap()
            == StreamableHttpPrepareDelivery::NotFound,
        reopened
            .delete_session(request(key("persistent", bob.clone()), now))
            .await
            .unwrap()
            == StreamableHttpDeleteSession::NotFound,
    ] {
        assert!(operation);
    }
    assert!(matches!(
        first.inspect_session(request(persistent.clone(), now)).await.unwrap(),
        StreamableHttpInspectSession::Session(session) if session.pending_event_count == 1
    ));

    let sequence_key = key("sequences", StreamableHttpSessionOwner::Anonymous);
    initialize(&*first, sequence_key.clone(), now, Vec::new()).await;
    let StreamableHttpAppendEvents::Events(appended) = first
        .append_events(StreamableHttpAppendEventsRequest {
            session: request(sequence_key.clone(), now),
            messages: (1..=11).map(|number| json!(number)).collect(),
        })
        .await
        .unwrap()
    else {
        panic!("append failed")
    };
    assert_eq!(
        appended
            .iter()
            .map(|event| event.id.sequence)
            .collect::<Vec<_>>(),
        (1..=11).collect::<Vec<_>>()
    );

    let first_delivery = delivery(&*reopened, sequence_key.clone(), now, None).await;
    assert_eq!(sequences(&first_delivery), [1, 2, 3, 4]);
    assert!(matches!(
        first.inspect_session(request(sequence_key.clone(), now)).await.unwrap(),
        StreamableHttpInspectSession::Session(session) if session.pending_event_count == 11
    ));
    assert!(matches!(
        first
            .prepare_delivery(StreamableHttpPrepareDeliveryRequest {
                session: request(sequence_key.clone(), now),
                acknowledged_event_id: None,
            })
            .await
            .unwrap(),
        StreamableHttpPrepareDelivery::DeliveryBusy { .. }
    ));
    let replay = delivery(
        &*first,
        sequence_key.clone(),
        now + Duration::from_millis(101),
        None,
    )
    .await;
    assert_eq!(replay, first_delivery);
    assert!(matches!(
        first
            .prepare_delivery(StreamableHttpPrepareDeliveryRequest {
                session: request(sequence_key.clone(), now + Duration::from_millis(102)),
                acknowledged_event_id: Some(first_delivery[1].id.clone()),
            })
            .await
            .unwrap(),
        StreamableHttpPrepareDelivery::DeliveryBusy { .. }
    ));
    assert!(matches!(
        first.inspect_session(request(sequence_key.clone(), now)).await.unwrap(),
        StreamableHttpInspectSession::Session(session) if session.pending_event_count == 9
    ));
    let second = delivery(
        &*first,
        sequence_key.clone(),
        now + Duration::from_millis(202),
        None,
    )
    .await;
    assert_eq!(sequences(&second), [3, 4, 5, 6]);
    let third = delivery(
        &*first,
        sequence_key.clone(),
        now + Duration::from_millis(203),
        Some(second.last().unwrap().id.clone()),
    )
    .await;
    assert_eq!(sequences(&third), [7, 8, 9, 10]);

    let before_invalid = inspect(&*first, sequence_key.clone(), now).await;
    for cursor in [
        StreamableHttpEventId {
            stream_id: "wrong".to_owned(),
            sequence: 1,
        },
        StreamableHttpEventId {
            stream_id: third[0].id.stream_id.clone(),
            sequence: 99,
        },
        StreamableHttpEventId {
            stream_id: third[0].id.stream_id.clone(),
            sequence: 1,
        },
    ] {
        assert_eq!(
            first
                .prepare_delivery(StreamableHttpPrepareDeliveryRequest {
                    session: request(sequence_key.clone(), now),
                    acknowledged_event_id: Some(cursor),
                })
                .await
                .unwrap(),
            StreamableHttpPrepareDelivery::InvalidCursor
        );
    }
    assert_eq!(
        inspect(&*first, sequence_key.clone(), now).await,
        before_invalid
    );

    let full_ack = key("full-ack", StreamableHttpSessionOwner::Anonymous);
    initialize(
        &*first,
        full_ack.clone(),
        now,
        vec![json!("one"), json!("two")],
    )
    .await;
    let delivered = delivery(&*first, full_ack.clone(), now, None).await;
    assert!(matches!(
        first
            .prepare_delivery(StreamableHttpPrepareDeliveryRequest {
                session: request(full_ack.clone(), now + Duration::from_millis(1)),
                acknowledged_event_id: Some(delivered.last().unwrap().id.clone()),
            })
            .await
            .unwrap(),
        StreamableHttpPrepareDelivery::Idle { .. }
    ));
    assert!(matches!(
        inspect(&*first, full_ack, now).await,
        StreamableHttpSession {
            pending_event_count: 0,
            pending_event_bytes: 0,
            ..
        }
    ));

    let concurrency = key("concurrency", StreamableHttpSessionOwner::Anonymous);
    initialize(&*first, concurrency.clone(), now, Vec::new()).await;
    let (left, right) = tokio::join!(
        first.append_events(StreamableHttpAppendEventsRequest {
            session: request(concurrency.clone(), now),
            messages: vec![json!("a"), json!("b")],
        }),
        reopened.append_events(StreamableHttpAppendEventsRequest {
            session: request(concurrency.clone(), now),
            messages: vec![json!("c"), json!("d")],
        })
    );
    let mut ranges = [
        event_sequences(left.unwrap()),
        event_sequences(right.unwrap()),
    ];
    ranges.sort_by_key(|range| range[0]);
    assert_eq!(ranges, [vec![1, 2], vec![3, 4]]);
    let (left, right) = tokio::join!(
        first.prepare_delivery(StreamableHttpPrepareDeliveryRequest {
            session: request(concurrency.clone(), now),
            acknowledged_event_id: None,
        }),
        reopened.prepare_delivery(StreamableHttpPrepareDeliveryRequest {
            session: request(concurrency.clone(), now),
            acknowledged_event_id: None,
        })
    );
    let outcomes = [left.unwrap(), right.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, StreamableHttpPrepareDelivery::Events { .. }))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, StreamableHttpPrepareDelivery::DeliveryBusy { .. }))
            .count(),
        1
    );

    let race = key("append-delete", StreamableHttpSessionOwner::Anonymous);
    initialize(&*first, race.clone(), now, Vec::new()).await;
    let (append, delete) = tokio::join!(
        first.append_events(StreamableHttpAppendEventsRequest {
            session: request(race.clone(), now),
            messages: vec![json!("racing")],
        }),
        reopened.delete_session(request(race.clone(), now)),
    );
    assert!(matches!(
        append.unwrap(),
        StreamableHttpAppendEvents::Events(_) | StreamableHttpAppendEvents::NotFound
    ));
    assert_eq!(delete.unwrap(), StreamableHttpDeleteSession::Deleted);
    assert_eq!(
        first.inspect_session(request(race, now)).await.unwrap(),
        StreamableHttpInspectSession::NotFound
    );

    let delivery_race = key("delivery-delete", StreamableHttpSessionOwner::Anonymous);
    initialize(&*first, delivery_race.clone(), now, vec![json!("racing")]).await;
    let (delivery, deletion) = tokio::join!(
        first.prepare_delivery(StreamableHttpPrepareDeliveryRequest {
            session: request(delivery_race.clone(), now),
            acknowledged_event_id: None,
        }),
        reopened.delete_session(request(delivery_race, now)),
    );
    assert!(matches!(
        delivery.unwrap(),
        StreamableHttpPrepareDelivery::Events { .. } | StreamableHttpPrepareDelivery::NotFound
    ));
    assert_eq!(deletion.unwrap(), StreamableHttpDeleteSession::Deleted);

    let reclaim = key("reclaim", StreamableHttpSessionOwner::Anonymous);
    let created = create(&*first, reclaim.clone(), now).await;
    let first_claim = claim(&*first, reclaim.clone(), now).await;
    assert!(matches!(
        reopened
            .claim_initialization(request(reclaim.clone(), now))
            .await
            .unwrap(),
        StreamableHttpClaimInitialization::InitializationInProgress { .. }
    ));
    let second_claim = claim(
        &*reopened,
        reclaim.clone(),
        now + Duration::from_millis(101),
    )
    .await;
    assert_ne!(first_claim.0, second_claim.0);
    assert_eq!(second_claim.1, created.initialization_operation_id);
    assert_eq!(
        first
            .abandon_initialization(StreamableHttpAbandonInitializationRequest {
                session: request(reclaim.clone(), now + Duration::from_millis(101)),
                claim_id: second_claim.0,
            })
            .await
            .unwrap(),
        StreamableHttpAbandonInitialization::Abandoned
    );

    let quota_owner = owner("quota");
    let (one, two) = tokio::join!(
        first.create_session(create_request(key("quota-1", quota_owner.clone()), now)),
        reopened.create_session(create_request(key("quota-2", quota_owner.clone()), now)),
    );
    assert!(matches!(
        one.unwrap(),
        StreamableHttpCreateSession::Created(_)
    ));
    assert!(matches!(
        two.unwrap(),
        StreamableHttpCreateSession::Created(_)
    ));
    assert_eq!(
        first
            .create_session(create_request(key("quota-3", quota_owner), now))
            .await
            .unwrap(),
        StreamableHttpCreateSession::CapacityExceeded
    );

    let capacity = key("event-capacity", StreamableHttpSessionOwner::Anonymous);
    initialize(&*first, capacity.clone(), now, Vec::new()).await;
    let oversized = "x".repeat(1025);
    assert_eq!(
        first
            .append_events(StreamableHttpAppendEventsRequest {
                session: request(capacity.clone(), now),
                messages: vec![json!(oversized)],
            })
            .await,
        Err(StreamableHttpSessionStoreError::InvalidData)
    );
    let (one, two) = tokio::join!(
        first.append_events(StreamableHttpAppendEventsRequest {
            session: request(capacity.clone(), now),
            messages: (0..7).map(|_| json!("a")).collect(),
        }),
        reopened.append_events(StreamableHttpAppendEventsRequest {
            session: request(capacity.clone(), now),
            messages: (0..7).map(|_| json!("b")).collect(),
        }),
    );
    assert_eq!(
        [one.unwrap(), two.unwrap()]
            .iter()
            .filter(|outcome| matches!(outcome, StreamableHttpAppendEvents::Events(_)))
            .count(),
        1
    );
    assert!(matches!(
        inspect(&*first, capacity, now).await,
        StreamableHttpSession {
            pending_event_count: 7,
            ..
        }
    ));

    let expired = key("000-expired", StreamableHttpSessionOwner::Anonymous);
    initialize(&*first, expired.clone(), now, vec![json!("expires")]).await;
    let after_expiry = now + Duration::from_secs(2);
    assert_eq!(
        first
            .inspect_session(request(expired.clone(), after_expiry))
            .await
            .unwrap(),
        StreamableHttpInspectSession::NotFound
    );
    assert_eq!(
        first
            .append_events(StreamableHttpAppendEventsRequest {
                session: request(expired.clone(), after_expiry),
                messages: vec![json!("inert")],
            })
            .await
            .unwrap(),
        StreamableHttpAppendEvents::NotFound
    );
    assert_eq!(
        first
            .prepare_delivery(StreamableHttpPrepareDeliveryRequest {
                session: request(expired.clone(), after_expiry),
                acknowledged_event_id: None,
            })
            .await
            .unwrap(),
        StreamableHttpPrepareDelivery::NotFound
    );
    let cleanup = first.cleanup_expired(after_expiry, 1).await.unwrap();
    assert_eq!(cleanup.sessions_deleted, 1);
    assert_eq!(cleanup.events_deleted, 1);
    assert_eq!(
        first
            .cleanup_expired(after_expiry, 1)
            .await
            .unwrap()
            .sessions_deleted,
        1
    );
}

pub const fn global_expiry_admission_config() -> StreamableHttpSessionStoreConfig {
    StreamableHttpSessionStoreConfig {
        session_ttl: Duration::from_millis(10),
        max_sessions: 2,
        max_sessions_per_principal: 1,
        cleanup_batch_size: 1,
        ..test_config()
    }
}

pub async fn assert_global_admission_ignores_retained_expired(
    store: &dyn StreamableHttpSessionStore,
) {
    let now = SystemTime::now();
    for id in ["a-cleaned-expired", "z-retained-expired"] {
        assert!(matches!(
            store
                .create_session(create_request(
                    key(id, StreamableHttpSessionOwner::Anonymous),
                    now,
                ))
                .await
                .unwrap(),
            StreamableHttpCreateSession::Created(_)
        ));
    }
    assert!(matches!(
        store
            .create_session(create_request(
                key("global-fresh", StreamableHttpSessionOwner::Anonymous),
                now + Duration::from_millis(10),
            ))
            .await
            .unwrap(),
        StreamableHttpCreateSession::Created(_)
    ));
}

pub const fn principal_expiry_admission_config() -> StreamableHttpSessionStoreConfig {
    StreamableHttpSessionStoreConfig {
        session_ttl: Duration::from_millis(10),
        max_sessions: 10,
        max_sessions_per_principal: 1,
        cleanup_batch_size: 1,
        ..test_config()
    }
}

pub async fn assert_principal_admission_ignores_retained_expired(
    store: &dyn StreamableHttpSessionStore,
) {
    let now = SystemTime::now();
    let principal = owner("retained-expired-owner");
    for session in [
        key("a-cleaned-expired", StreamableHttpSessionOwner::Anonymous),
        key("z-retained-expired", principal.clone()),
    ] {
        assert!(matches!(
            store
                .create_session(create_request(session, now))
                .await
                .unwrap(),
            StreamableHttpCreateSession::Created(_)
        ));
    }
    assert!(matches!(
        store
            .create_session(create_request(
                key("principal-fresh", principal),
                now + Duration::from_millis(10),
            ))
            .await
            .unwrap(),
        StreamableHttpCreateSession::Created(_)
    ));
}

pub fn owner(value: &str) -> StreamableHttpSessionOwner {
    StreamableHttpSessionOwner::Authenticated(McpPrincipalId::new(value).expect("valid principal"))
}

pub fn key(id: &str, owner: StreamableHttpSessionOwner) -> StreamableHttpSessionKey {
    StreamableHttpSessionKey {
        session_id: id.to_owned(),
        owner,
    }
}

pub fn request(key: StreamableHttpSessionKey, now: SystemTime) -> StreamableHttpSessionRequest {
    StreamableHttpSessionRequest {
        key,
        presented_protocol_version: Some("2025-11-25".to_owned()),
        now,
    }
}

pub fn create_request(
    key: StreamableHttpSessionKey,
    now: SystemTime,
) -> StreamableHttpCreateSessionRequest {
    StreamableHttpCreateSessionRequest {
        key,
        protocol_version: "2025-11-25".to_owned(),
        now,
    }
}

async fn create(
    store: &dyn StreamableHttpSessionStore,
    key: StreamableHttpSessionKey,
    now: SystemTime,
) -> StreamableHttpSession {
    let StreamableHttpCreateSession::Created(session) = store
        .create_session(create_request(key, now))
        .await
        .unwrap()
    else {
        panic!("create failed")
    };
    session
}

async fn claim(
    store: &dyn StreamableHttpSessionStore,
    key: StreamableHttpSessionKey,
    now: SystemTime,
) -> (String, String) {
    let StreamableHttpClaimInitialization::Claimed {
        claim_id,
        operation_id,
    } = store.claim_initialization(request(key, now)).await.unwrap()
    else {
        panic!("claim failed")
    };
    (claim_id, operation_id)
}

pub async fn initialize(
    store: &dyn StreamableHttpSessionStore,
    key: StreamableHttpSessionKey,
    now: SystemTime,
    messages: Vec<Value>,
) {
    create(store, key.clone(), now).await;
    let (claim_id, _) = claim(store, key.clone(), now).await;
    assert!(matches!(
        store
            .complete_initialization(StreamableHttpCompleteInitializationRequest {
                session: request(key, now),
                claim_id,
                messages,
            })
            .await
            .unwrap(),
        StreamableHttpCompleteInitialization::Completed(_)
    ));
}

async fn inspect(
    store: &dyn StreamableHttpSessionStore,
    key: StreamableHttpSessionKey,
    now: SystemTime,
) -> StreamableHttpSession {
    let StreamableHttpInspectSession::Session(session) =
        store.inspect_session(request(key, now)).await.unwrap()
    else {
        panic!("inspect failed")
    };
    session
}

async fn delivery(
    store: &dyn StreamableHttpSessionStore,
    key: StreamableHttpSessionKey,
    now: SystemTime,
    cursor: Option<StreamableHttpEventId>,
) -> Vec<StreamableHttpEvent> {
    let StreamableHttpPrepareDelivery::Events { events, .. } = store
        .prepare_delivery(StreamableHttpPrepareDeliveryRequest {
            session: request(key, now),
            acknowledged_event_id: cursor,
        })
        .await
        .unwrap()
    else {
        panic!("delivery failed")
    };
    events
}

fn sequences(events: &[StreamableHttpEvent]) -> Vec<u64> {
    events.iter().map(|event| event.id.sequence).collect()
}

fn event_sequences(outcome: StreamableHttpAppendEvents) -> Vec<u64> {
    let StreamableHttpAppendEvents::Events(events) = outcome else {
        panic!("append failed")
    };
    sequences(&events)
}
