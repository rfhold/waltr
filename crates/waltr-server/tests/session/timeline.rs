use crate::support::grpc::session_client;
use waltr::grpc::waltr::session::v1::{
    CreateSessionRequest, GetTimelineRequest, TimelineDirection, TimelineQuery,
    WatchTimelineRequest, timeline_content,
};
use waltr_agent::{PendingTimelineEvent, SessionId};
use waltr_server::test_support::setup_server_with_sessions;

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
                PendingTimelineEvent::user_text_snapshot(session_id.clone(), 1, "user-1", "hello"),
                PendingTimelineEvent::assistant_text_snapshot(
                    session_id.clone(),
                    1,
                    "assistant-1",
                    "hi there",
                ),
            ],
        )
        .await?;

    let events = client
        .get_timeline(GetTimelineRequest {
            session_id: session.id.clone(),
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
    assert_eq!(events[0].item_id, "user-1");
    assert_eq!(text_snapshot(&events[0]), "hello");
    assert_eq!(events[1].item_id, "assistant-1");
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
                "user-1",
                "hello",
            )],
        )
        .await?;

    let mut watch = watch_client
        .watch_timeline(WatchTimelineRequest {
            session_id: session.id.clone(),
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
                "assistant-1",
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
            if update.event.is_some() {
                return Ok::<_, tonic::Status>(update);
            }
        }
    })
    .await??;
    assert!(update.sequence > 0);
    let event = update.event.expect("timeline event should be present");
    assert_eq!(event.item_id, "assistant-1");
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
                PendingTimelineEvent::user_text_snapshot(session_id.clone(), 1, "user-1", "one"),
                PendingTimelineEvent::user_text_snapshot(session_id.clone(), 1, "user-2", "two"),
                PendingTimelineEvent::user_text_snapshot(session_id.clone(), 1, "user-3", "three"),
            ],
        )
        .await?;

    let mut watch = watch_client
        .watch_timeline(WatchTimelineRequest {
            session_id: session.id.clone(),
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
            if let Some(event) = update.event {
                events.push(event);
            }
        }
        Ok::<_, tonic::Status>(events)
    })
    .await??;

    assert_eq!(events[0].item_id, "user-2");
    assert_eq!(events[1].item_id, "user-3");

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
                        format!("user-{index}"),
                        format!("message {index}"),
                    )
                })
                .collect(),
        )
        .await?;

    let mut watch = watch_client
        .watch_timeline(WatchTimelineRequest {
            session_id: session.id.clone(),
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
            if let Some(event) = update.event {
                events.push(event);
            }
        }
        Ok::<_, tonic::Status>(events)
    })
    .await??;

    assert_eq!(
        events.first().map(|event| event.item_id.as_str()),
        Some("user-1")
    );
    assert_eq!(
        events.last().map(|event| event.item_id.as_str()),
        Some("user-1005")
    );
    assert!(
        events
            .windows(2)
            .all(|pair| pair[1].cursor == pair[0].cursor + 1)
    );

    Ok(())
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
