use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use waltr::grpc::waltr::session::v1 as s;

pub(super) const PROMPT: &str =
    "Hold the parent response until the installed title Routine commits";
pub(super) struct ParentGate {
    pub entered: tokio::sync::Semaphore,
    pub release: tokio::sync::Semaphore,
    pub requests: AtomicUsize,
}

fn title(row: &s::SessionSummary) -> Option<&str> {
    use prost_types::value::Kind;
    match &row.metadata.as_ref()?.fields.get("title")?.kind {
        Some(Kind::StringValue(value)) => Some(value),
        _ => None,
    }
}

pub(super) async fn run(address: std::net::SocketAddr, bearer: &str, provider: &ScriptedProvider) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let gate = Arc::new(ParentGate {
            entered: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
            requests: AtomicUsize::new(0),
        });
        *provider.overlap_gate.lock().unwrap() = Some(gate.clone());
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let authorization = format!("Bearer {bearer}");
        let header = Some(("authorization", authorization.as_str()));
        let mut sessions = s::session_service_client::SessionServiceClient::new(channel.clone());
        let id = sessions
            .create_session(request(
                s::CreateSessionRequest {
                    agent_selection: Some(s::AgentSelectionRequest {
                        agent_name: "routine-title".into(),
                    }),
                    ..Default::default()
                },
                header,
            ))
            .await
            .unwrap()
            .into_inner()
            .session
            .unwrap()
            .id;
        let mut watch = sessions
            .watch_sessions(request(s::WatchSessionsRequest {}, header))
            .await
            .unwrap()
            .into_inner();
        let mut routines = p::routine_service_client::RoutineServiceClient::new(channel);
        let mut invocations = routines
            .watch_invocations(request(
                p::WatchInvocationsRequest {
                    limit: 100,
                    ..Default::default()
                },
                header,
            ))
            .await
            .unwrap()
            .into_inner();
        sessions
            .submit_user_message(request(
                s::SubmitUserMessageRequest {
                    session_id: id.clone(),
                    text: PROMPT.into(),
                },
                header,
            ))
            .await
            .unwrap();
        gate.entered.acquire().await.unwrap().forget();
        let mut revision = 0;
        loop {
            let rows = match watch.message().await.unwrap().unwrap().event.unwrap() {
                s::watch_sessions_response::Event::Snapshot(snapshot) => snapshot.sessions,
                s::watch_sessions_response::Event::Session(row) => vec![row],
                _ => Vec::new(),
            };
            let mut committed = false;
            for row in rows.into_iter().filter(|row| row.id == id) {
                assert!(row.revision >= revision, "watch revisions never regress");
                revision = row.revision;
                committed |= title(&row) == Some("Verified normal helper");
            }
            if committed {
                break;
            }
        }
        // The installed JS invocation must finish its context transaction, not just its helper.
        loop {
            let update = invocations.message().await.unwrap().unwrap();
            if update.invocations.iter().any(|row| {
                row.routine == "title"
                    && row.status == p::InvocationStatus::Succeeded as i32
                    && row.source_session.as_deref() == Some(id.as_str())
            }) {
                break;
            }
        }
        let committed_revision = revision;
        gate.release.add_permits(1);
        loop {
            let rows = match watch.message().await.unwrap().unwrap().event.unwrap() {
                s::watch_sessions_response::Event::Snapshot(snapshot) => snapshot.sessions,
                s::watch_sessions_response::Event::Session(row) => vec![row],
                _ => Vec::new(),
            };
            let mut ready = false;
            for row in rows.into_iter().filter(|row| row.id == id) {
                assert!(row.revision >= revision);
                revision = row.revision;
                assert_eq!(title(&row), Some("Verified normal helper"));
                ready |= row
                    .coordination
                    .as_ref()
                    .is_some_and(|state| state.phase == s::SessionCoordinationPhase::Ready as i32);
            }
            if ready && revision > committed_revision {
                break;
            }
        }
        let listed = sessions
            .list_sessions(request(s::ListSessionsRequest {}, header))
            .await
            .unwrap()
            .into_inner()
            .sessions;
        let row = listed.iter().find(|row| row.id == id).unwrap();
        assert_eq!(title(row), Some("Verified normal helper"));
        assert!(row.revision >= revision);
        let timeline = sessions
            .get_timeline(request(
                s::GetTimelineRequest {
                    session_id: id.clone(),
                    projection_mode: 0,
                    query: Some(s::TimelineQuery {
                        direction: s::TimelineDirection::Forward as i32,
                        cursor: None,
                        limit: 100,
                        channels: Vec::new(),
                    }),
                },
                header,
            ))
            .await
            .unwrap()
            .into_inner()
            .events;
        assert!(
            timeline.iter().any(|event| {
                event.content.as_ref().is_some_and(|content|
            matches!(&content.content, Some(s::timeline_content::Content::TextSnapshot(text))
                if text.text == "Verified normal helper"))
            }),
            "complete parent output persisted"
        );
        assert!(
            timeline.iter().any(
                |event| event.kind == s::TimelineEventKind::ItemFinished as i32
                    && event.status == Some(s::TimelineStatus::Finished as i32)
            ),
            "parent finished normally"
        );
        assert!(
            !timeline
                .iter()
                .any(|event| event.status == Some(s::TimelineStatus::Failed as i32))
        );
        assert_eq!(gate.requests.load(Ordering::SeqCst), 1);
        *provider.overlap_gate.lock().unwrap() = None;
    })
    .await
    .expect("bounded installed title/parent overlap");
}
