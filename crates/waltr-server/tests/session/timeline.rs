use crate::support::grpc::session_client;
use waltr::grpc::waltr::session::v1::{
    CreateSessionRequest, GetTimelineRequest, TimelineDirection, TimelinePresentation,
    TimelineProjectionMode, TimelineQuery, WatchTimelineRequest, projected_timeline_event,
    timeline_content, watch_timeline_response,
};
use waltr_agent::{
    InferenceProviderSettings, InferenceSettings, MemorySessionStore, ModelApi,
    PendingTimelineEvent, PendingTimelineItemId, ReplacementUserContent, SessionCoordinationReason,
    SessionCoordinator, SessionCreateRequest as AgentCreateRequest, SessionId, SessionJournalEvent,
    SessionMaskAddition, SessionMaskMutation, SessionStore, ThroughCursor, TimelineCursor,
};
use waltr_server::test_support::{setup_server_with_seeded_sessions, setup_server_with_sessions};

#[tokio::test]
async fn get_timeline_returns_persisted_events() -> Result<(), Box<dyn std::error::Error>> {
    let (server, sessions) = setup_server_with_sessions();
    let mut client = session_client(server).await?;
    let session = client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");
    let session_id = SessionId::new(session.id.clone());

    sessions
        .append_timeline(
            &session_id,
            vec![
                PendingTimelineEvent::user_text_snapshot(
                    session_id.clone(),
                    1,
                    waltr_agent::PendingTimelineItemId::New,
                    "hello",
                ),
                PendingTimelineEvent::assistant_text_snapshot(
                    session_id.clone(),
                    1,
                    waltr_agent::PendingTimelineItemId::New,
                    "hi there",
                ),
            ],
        )
        .await?;

    let events = client
        .get_timeline(GetTimelineRequest {
            session_id: session.id.clone(),
            projection_mode: 0,
            query: Some(TimelineQuery {
                direction: TimelineDirection::Forward as i32,
                cursor: None,
                limit: 10,
                channels: Vec::new(),
            }),
        })
        .await?
        .into_inner()
        .events;

    assert_eq!(events.len(), 2);
    assert_eq!(events[0].session_id, session.id);
    assert_eq!(events[0].item_id, "i1");
    assert_eq!(text_snapshot(&events[0]), "hello");
    assert_eq!(events[1].item_id, "i2");
    assert_eq!(text_snapshot(&events[1]), "hi there");

    Ok(())
}

#[tokio::test]
async fn watch_timeline_replays_history_then_live_events() -> Result<(), Box<dyn std::error::Error>>
{
    let (server, sessions) = setup_server_with_sessions();
    let mut create_client = session_client(server).await?;
    let mut watch_client = session_client(server).await?;
    let session = create_client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");
    let session_id = SessionId::new(session.id.clone());
    let seeded = sessions
        .append_timeline(
            &session_id,
            vec![PendingTimelineEvent::user_text_snapshot(
                session_id.clone(),
                1,
                waltr_agent::PendingTimelineItemId::New,
                "hello",
            )],
        )
        .await?;

    let mut watch = watch_client
        .watch_timeline(WatchTimelineRequest {
            session_id: session.id.clone(),
            projection_mode: 0,
            after_cursor: Some(seeded[0].cursor.get()),
            channels: vec!["assistant_text".to_owned()],
            initial_limit: None,
        })
        .await?
        .into_inner();

    sessions
        .append_timeline(
            &session_id,
            vec![PendingTimelineEvent::assistant_text_snapshot(
                session_id.clone(),
                1,
                waltr_agent::PendingTimelineItemId::New,
                "hi there",
            )],
        )
        .await?;

    let update = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let update = watch
                .message()
                .await?
                .expect("watch stream should emit timeline event");
            if let Some(watch_timeline_response::Update::Event(event)) = update.update {
                return Ok::<_, tonic::Status>((update.sequence, event));
            }
        }
    })
    .await??;
    let (sequence, event) = update;
    assert!(sequence > 0);
    assert_eq!(event.item_id, "i2");
    assert_eq!(text_snapshot(&event), "hi there");

    Ok(())
}

#[tokio::test]
async fn watch_timeline_replays_after_cursor_before_initial_limit()
-> Result<(), Box<dyn std::error::Error>> {
    let (server, sessions) = setup_server_with_sessions();
    let mut create_client = session_client(server).await?;
    let mut watch_client = session_client(server).await?;
    let session = create_client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");
    let session_id = SessionId::new(session.id.clone());
    let seeded = sessions
        .append_timeline(
            &session_id,
            vec![
                PendingTimelineEvent::user_text_snapshot(
                    session_id.clone(),
                    1,
                    waltr_agent::PendingTimelineItemId::New,
                    "one",
                ),
                PendingTimelineEvent::user_text_snapshot(
                    session_id.clone(),
                    1,
                    waltr_agent::PendingTimelineItemId::New,
                    "two",
                ),
                PendingTimelineEvent::user_text_snapshot(
                    session_id.clone(),
                    1,
                    waltr_agent::PendingTimelineItemId::New,
                    "three",
                ),
            ],
        )
        .await?;

    let mut watch = watch_client
        .watch_timeline(WatchTimelineRequest {
            session_id: session.id.clone(),
            projection_mode: 0,
            after_cursor: Some(seeded[0].cursor.get()),
            channels: Vec::new(),
            initial_limit: Some(1),
        })
        .await?
        .into_inner();

    let events = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut events = Vec::new();
        while events.len() < 2 {
            let update = watch
                .message()
                .await?
                .expect("watch stream should emit timeline event");
            if let Some(watch_timeline_response::Update::Event(event)) = update.update {
                events.push(event);
            }
        }
        Ok::<_, tonic::Status>(events)
    })
    .await??;

    assert_eq!(events[0].item_id, "i2");
    assert_eq!(events[1].item_id, "i3");

    Ok(())
}

#[tokio::test]
async fn watch_timeline_pages_large_after_cursor_catchup_without_initial_gap()
-> Result<(), Box<dyn std::error::Error>> {
    let (server, sessions) = setup_server_with_sessions();
    let mut create_client = session_client(server).await?;
    let mut watch_client = session_client(server).await?;
    let session = create_client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");
    let session_id = SessionId::new(session.id.clone());
    let seeded = sessions
        .append_timeline(
            &session_id,
            (0..=1005)
                .map(|index| {
                    PendingTimelineEvent::user_text_snapshot(
                        session_id.clone(),
                        1,
                        waltr_agent::PendingTimelineItemId::New,
                        format!("message {index}"),
                    )
                })
                .collect(),
        )
        .await?;

    let mut watch = watch_client
        .watch_timeline(WatchTimelineRequest {
            session_id: session.id.clone(),
            projection_mode: 0,
            after_cursor: Some(seeded[0].cursor.get()),
            channels: Vec::new(),
            initial_limit: Some(1),
        })
        .await?
        .into_inner();

    let events = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut events = Vec::new();
        while events.len() < 1005 {
            let update = watch
                .message()
                .await?
                .expect("watch stream should emit timeline event");
            if let Some(watch_timeline_response::Update::Event(event)) = update.update {
                events.push(event);
            }
        }
        Ok::<_, tonic::Status>(events)
    })
    .await??;

    assert_eq!(
        events.first().map(|event| event.item_id.as_str()),
        Some("i2")
    );
    assert_eq!(
        events.last().map(|event| event.item_id.as_str()),
        Some("i1006")
    );
    assert!(
        events
            .windows(2)
            .all(|pair| pair[1].cursor == pair[0].cursor + 1)
    );

    Ok(())
}

#[tokio::test]
async fn effective_timeline_projects_synthetic_identity_and_explicit_pagination()
-> Result<(), Box<dyn std::error::Error>> {
    let (sessions, session_id) = seeded_completed_session().await?;
    apply_replacement(&sessions, &session_id, "compact summary").await?;
    let server = setup_server_with_seeded_sessions(sessions);
    let mut client = session_client(server).await?;

    let first = client
        .get_timeline(GetTimelineRequest {
            session_id: session_id.as_str().to_owned(),
            projection_mode: TimelineProjectionMode::Effective as i32,
            query: Some(TimelineQuery {
                direction: TimelineDirection::Forward as i32,
                cursor: None,
                limit: 1,
                channels: Vec::new(),
            }),
    })
        .await?
        .into_inner();
    assert!(first.events.is_empty());
    assert_eq!(first.projected_events.len(), 1);
    let first_projection = first.projection.expect("projection metadata");
    assert_eq!(first_projection.raw_page_boundary, Some(2));
    assert!(!first_projection.has_more);

    let projected = &first.projected_events[0];
    assert_eq!(
        projected.presentation,
        TimelinePresentation::MaskReplacement as i32
    );
    assert!(
        projected
            .mask_id
            .as_deref()
            .is_some_and(|id| !id.is_empty())
    );
    let Some(projected_timeline_event::Event::Synthetic(synthetic)) = &projected.event else {
        panic!("mask replacement must use synthetic wire identity")
    };
    assert_eq!(synthetic.cursor, 3);
    assert_eq!(text_content(synthetic.content.as_ref()), "compact summary");

    let raw = client
        .get_timeline(GetTimelineRequest {
            session_id: session_id.as_str().to_owned(),
            projection_mode: TimelineProjectionMode::Raw as i32,
            query: Some(TimelineQuery {
                direction: TimelineDirection::Forward as i32,
                cursor: None,
                limit: 10,
                channels: Vec::new(),
            }),
        })
        .await?
        .into_inner();
    assert_eq!(raw.events.len(), 2);
    assert!(raw.projected_events.is_empty());
    assert_eq!(raw.events[0].item_id, "i1");
    assert_eq!(raw.events[1].item_id, "i2");
    Ok(())
}

#[tokio::test]
async fn effective_watch_requeries_after_projection_invalidation()
-> Result<(), Box<dyn std::error::Error>> {
    let (sessions, session_id) = seeded_completed_session().await?;
    let server = setup_server_with_seeded_sessions(sessions.clone());
    let mut client = session_client(server).await?;
    let mut watch = client
        .watch_timeline(WatchTimelineRequest {
            session_id: session_id.as_str().to_owned(),
            after_cursor: None,
            channels: Vec::new(),
            initial_limit: Some(10),
            projection_mode: TimelineProjectionMode::Effective as i32,
        })
        .await?
        .into_inner();

    let initial = watch.message().await?.expect("initial projection reset");
    let Some(watch_timeline_response::Update::ProjectionReset(initial)) = initial.update else {
        panic!("effective watch must begin with an authoritative reset")
    };
    assert_eq!(initial.events.len(), 2);

    apply_replacement(&sessions, &session_id, "watch summary").await?;
    let reset = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let update = watch
                .message()
                .await?
                .expect("projection invalidation reset");
            if let Some(watch_timeline_response::Update::ProjectionReset(reset)) = update.update {
                return Ok::<_, tonic::Status>(reset);
            }
        }
    })
    .await??;
    assert_eq!(reset.events.len(), 1);
    assert!(matches!(
        reset.events[0].event,
        Some(projected_timeline_event::Event::Synthetic(_))
    ));
    assert_eq!(
        reset.events[0].presentation,
        TimelinePresentation::MaskReplacement as i32
    );
    Ok(())
}

#[tokio::test]
async fn effective_watch_emits_projected_durable_live_events()
-> Result<(), Box<dyn std::error::Error>> {
    let (sessions, session_id) = seeded_completed_session().await?;
    let server = setup_server_with_seeded_sessions(sessions.clone());
    let mut client = session_client(server).await?;
    let mut watch = client
        .watch_timeline(WatchTimelineRequest {
            session_id: session_id.as_str().to_owned(),
            after_cursor: None,
            channels: vec!["user_text".to_owned()],
            initial_limit: Some(10),
            projection_mode: TimelineProjectionMode::Effective as i32,
        })
        .await?
        .into_inner();
    let initial = watch.message().await?.expect("initial projection reset");
    assert!(matches!(
        initial.update,
        Some(watch_timeline_response::Update::ProjectionReset(_))
    ));

    sessions
        .append_timeline(
            &session_id,
            vec![PendingTimelineEvent::user_text_snapshot(
                session_id.clone(),
                1,
                PendingTimelineItemId::New,
                "later prompt",
            )],
        )
        .await?;
    let update = tokio::time::timeout(std::time::Duration::from_secs(2), watch.message())
        .await??
        .expect("projected live event");
    let Some(watch_timeline_response::Update::ProjectedEvent(projected)) = update.update else {
        panic!("effective watch must use the projected live variant")
    };
    assert_eq!(projected.presentation, TimelinePresentation::Durable as i32);
    assert!(projected.mask_id.is_none());
    let Some(projected_timeline_event::Event::Durable(event)) = projected.event else {
        panic!("durable append must retain durable identity")
    };
    assert_eq!(event.item_id, "i3");
    assert_eq!(text_snapshot(&event), "later prompt");
    Ok(())
}

pub(super) async fn seeded_completed_session()
-> Result<(SessionCoordinator<MemorySessionStore>, SessionId), Box<dyn std::error::Error>> {
    let store = MemorySessionStore::new();
    let session_id = SessionId::new(format!("projected-{}", uuid::Uuid::new_v4()));
    let mut create = AgentCreateRequest::new(session_id.clone());
    create.inference_settings = Some(InferenceSettings {
        provider: InferenceProviderSettings {
            key: "test".to_owned(),
            api: ModelApi::OpenAiResponses,
            url: "http://localhost".to_owned(),
            model_id: "test-model".to_owned(),
            model_cost: None,
            headers: std::collections::BTreeMap::new(),
            oauth: None,
            reasoning_effort: None,
        },
    });
    create.agent_definition = Some(waltr_server::test_support::default_agent_definition().1);
    let created = store.create(create).await?;
    let revision = store
        .append_journal(
            &session_id,
            created.revision,
            vec![SessionJournalEvent::PromptAppended {
                prompt: "source prompt".to_owned(),
            }],
        )
        .await?;
    store
        .append_timeline(
            &session_id,
            revision,
            vec![
                PendingTimelineEvent::user_text_snapshot(
                    session_id.clone(),
                    0,
                    PendingTimelineItemId::New,
                    "source prompt",
                ),
                PendingTimelineEvent::assistant_text_snapshot(
                    session_id.clone(),
                    0,
                    PendingTimelineItemId::New,
                    "source answer",
                ),
            ],
        )
        .await?;
    let revision = store.load(&session_id).await?.revision;
    store
        .append_journal(
            &session_id,
            revision,
            vec![SessionJournalEvent::TurnFinished { turn: 0 }],
        )
        .await?;
    Ok((SessionCoordinator::new(store), session_id))
}

async fn apply_replacement(
    sessions: &SessionCoordinator<MemorySessionStore>,
    session_id: &SessionId,
    replacement: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let lease = sessions
        .acquire_pause_reject_busy(
            session_id,
            SessionCoordinationReason::new(
                "context-compaction",
                Some("compact-and-continue".to_owned()),
                Some("manual".to_owned()),
            )?,
        )
        .await?;
    let mut mutation = SessionMaskMutation::new(Some(TimelineCursor::new(2)));
    mutation.add.push(SessionMaskAddition::new(
        "context.compaction-prefix",
        ThroughCursor::new(TimelineCursor::new(2)),
        ReplacementUserContent::new(replacement),
    ));
    sessions
        .apply_paused_mask_mutation(&lease, mutation)
        .await?;
    sessions.release_pause(&lease).await?;
    Ok(())
}

fn text_content(content: Option<&waltr::grpc::waltr::session::v1::TimelineContent>) -> &str {
    match content.and_then(|content| content.content.as_ref()) {
        Some(timeline_content::Content::TextSnapshot(snapshot)) => &snapshot.text,
        other => panic!("expected text snapshot, got {other:?}"),
    }
}

fn text_snapshot(event: &waltr::grpc::waltr::session::v1::TimelineEvent) -> &str {
    let content = event
        .content
        .as_ref()
        .and_then(|content| content.content.as_ref())
        .expect("timeline content should be present");
    match content {
        timeline_content::Content::TextSnapshot(snapshot) => snapshot.text.as_str(),
        other => panic!("expected text snapshot, got {other:?}"),
    }
}
