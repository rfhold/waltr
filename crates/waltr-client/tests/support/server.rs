#![allow(dead_code)]

pub fn server_endpoint() -> String {
    waltr_server::test_support::setup_server().base_url()
}

pub async fn server_endpoint_with_seeded_timeline(
    seed_text: bool,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    let sessions = waltr_agent::AgentSessions::new(waltr_agent::MemorySessionStore::new());
    let session = sessions.create().await?;
    let session_id = session.id().clone();
    if seed_text {
        let events = vec![
            waltr_agent::PendingTimelineEvent::user_text_snapshot(
                session_id.clone(),
                1,
                "user-1",
                "hello",
            ),
            waltr_agent::PendingTimelineEvent::assistant_text_snapshot(
                session_id.clone(),
                1,
                "assistant-1",
                "hi there",
            ),
        ];
        sessions.append_timeline(&session_id, events).await?;
    }

    let server = waltr_server::test_support::setup_server_with_seeded_sessions(sessions);
    Ok((server.base_url(), session_id.as_str().to_owned()))
}
