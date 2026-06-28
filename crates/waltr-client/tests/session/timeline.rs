#![allow(clippy::future_not_send)]

use waltr_client::{ClientCommand, ClientRuntime, ClientTimelineDirection, TimelineContentView};

use crate::support::{
    server::server_endpoint_with_seeded_timeline,
    snapshots::{dispatch_stream_command, next_snapshot_until, no_sleep},
};

#[tokio::test]
async fn request_session_timeline_loads_seeded_events_through_client_state() {
    let (endpoint, session_id) = server_endpoint_with_seeded_timeline(true)
        .await
        .expect("seeded server should start");
    let mut stream = ClientRuntime::for_endpoint(endpoint).into_state_stream(no_sleep);

    wait_for_system_ready(&mut stream).await;
    dispatch_stream_command(
        &mut stream,
        ClientCommand::RequestSessionTimeline {
            session_id: session_id.clone(),
            direction: ClientTimelineDirection::Backward,
            cursor: None,
            limit: 10,
            channels: Vec::new(),
        },
    )
    .await
    .expect("timeline request should dispatch");

    let snapshot = next_snapshot_until(&mut stream, |snapshot| {
        snapshot
            .sessions
            .timelines
            .get(&session_id)
            .is_some_and(|timeline| timeline.load.status == "ready" && timeline.events.len() == 2)
    })
    .await;
    let timeline = snapshot
        .sessions
        .timelines
        .get(&session_id)
        .expect("timeline should be present");

    assert_eq!(timeline.load.error, None);
    assert_eq!(timeline.events[0].item_id, "user-1");
    assert_eq!(text_snapshot(&timeline.events[0].content), "hello");
    assert_eq!(timeline.events[1].item_id, "assistant-1");
    assert_eq!(text_snapshot(&timeline.events[1].content), "hi there");
}

#[tokio::test]
async fn timeline_watch_replays_seeded_events_through_client_state() {
    let (endpoint, session_id) = server_endpoint_with_seeded_timeline(true)
        .await
        .expect("seeded server should start");
    let mut stream = ClientRuntime::for_endpoint(endpoint).into_state_stream(no_sleep);

    wait_for_system_ready(&mut stream).await;
    dispatch_stream_command(
        &mut stream,
        ClientCommand::RegisterSessionTimelineWatchInterest {
            session_id: session_id.clone(),
            after_cursor: Some(0),
            channels: vec!["assistant_text".to_owned()],
            initial_limit: None,
        },
    )
    .await
    .expect("timeline watch interest should dispatch");

    let snapshot = next_snapshot_until(&mut stream, |snapshot| {
        snapshot
            .sessions
            .timelines
            .get(&session_id)
            .is_some_and(|timeline| {
                timeline.watch.status == "received"
                    && timeline
                        .events
                        .iter()
                        .any(|event| event.item_id == "assistant-1")
            })
    })
    .await;
    let timeline = snapshot
        .sessions
        .timelines
        .get(&session_id)
        .expect("timeline should be present");

    assert_eq!(timeline.watch.error, None);
    assert_eq!(timeline.events.len(), 1);
    assert_eq!(timeline.events[0].item_id, "assistant-1");
    assert_eq!(text_snapshot(&timeline.events[0].content), "hi there");

    let released = dispatch_stream_command(
        &mut stream,
        ClientCommand::ReleaseSessionTimelineWatchInterest { session_id },
    )
    .await
    .expect("timeline watch interest should release");
    assert!(
        released
            .sessions
            .timelines
            .values()
            .all(|timeline| timeline.watch.status == "idle")
    );
}

#[tokio::test]
async fn timeline_watch_keeps_distinct_request_params_active() {
    let (endpoint, session_id) = server_endpoint_with_seeded_timeline(true)
        .await
        .expect("seeded server should start");
    let mut stream = ClientRuntime::for_endpoint(endpoint).into_state_stream(no_sleep);

    wait_for_system_ready(&mut stream).await;
    dispatch_stream_command(
        &mut stream,
        ClientCommand::RegisterSessionTimelineWatchInterest {
            session_id: session_id.clone(),
            after_cursor: Some(0),
            channels: vec!["assistant_text".to_owned()],
            initial_limit: None,
        },
    )
    .await
    .expect("assistant timeline watch interest should dispatch");
    next_snapshot_until(&mut stream, |snapshot| {
        snapshot
            .sessions
            .timelines
            .get(&session_id)
            .is_some_and(|timeline| {
                timeline
                    .events
                    .iter()
                    .any(|event| event.item_id == "assistant-1")
            })
    })
    .await;

    dispatch_stream_command(
        &mut stream,
        ClientCommand::RegisterSessionTimelineWatchInterest {
            session_id: session_id.clone(),
            after_cursor: Some(0),
            channels: vec!["user_text".to_owned()],
            initial_limit: Some(1),
        },
    )
    .await
    .expect("user timeline watch interest should dispatch");

    let snapshot = next_snapshot_until(&mut stream, |snapshot| {
        snapshot
            .sessions
            .timelines
            .get(&session_id)
            .is_some_and(|timeline| {
                timeline
                    .events
                    .iter()
                    .any(|event| event.item_id == "assistant-1")
                    && timeline
                        .events
                        .iter()
                        .any(|event| event.item_id == "user-1")
            })
    })
    .await;
    let timeline = snapshot
        .sessions
        .timelines
        .get(&session_id)
        .expect("timeline should be present");

    assert_eq!(timeline.events.len(), 2);
    assert_eq!(timeline.watch.error, None);
}

#[tokio::test]
async fn submit_user_message_failure_is_reported_as_client_operation() {
    let (endpoint, session_id) = server_endpoint_with_seeded_timeline(false)
        .await
        .expect("seeded server should start");
    let mut stream = ClientRuntime::for_endpoint(endpoint).into_state_stream(no_sleep);

    wait_for_system_ready(&mut stream).await;
    dispatch_stream_command(
        &mut stream,
        ClientCommand::SubmitUserMessage {
            session_id: session_id.clone(),
            text: "hello".to_owned(),
        },
    )
    .await
    .expect("submit command should dispatch");

    let snapshot = next_snapshot_until(&mut stream, |snapshot| {
        snapshot.sessions.submit_user_message.status == "failed"
    })
    .await;

    assert_eq!(
        snapshot.sessions.submit_user_message.key.as_deref(),
        Some("op-0")
    );
    assert!(
        snapshot.sessions.submit_user_message.error.is_some(),
        "failure should retain the transport error"
    );
}

async fn wait_for_system_ready(stream: &mut waltr_client::ClientStateStream) {
    next_snapshot_until(stream, |snapshot| {
        snapshot.status.ready && snapshot.status.watch.status == "received"
    })
    .await;
}

fn text_snapshot(content: &TimelineContentView) -> &str {
    let TimelineContentView::TextSnapshot { text } = content else {
        panic!("expected text snapshot content: {content:?}");
    };
    text
}
