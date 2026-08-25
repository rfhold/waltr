use crate::support::grpc::session_client;
use waltr::grpc::waltr::session::v1::{
    AgentSelectionRequest, CreateSessionRequest, ListAgentsRequest, ListSessionsRequest,
    UpdateSessionAgentRequest, WatchSessionsRequest, watch_sessions_response,
};
use waltr_agent::{SessionCreateRequest, SessionId};
use waltr_server::test_support::{setup_server, setup_server_with_sessions};

#[tokio::test]
async fn list_agents_is_sorted_and_reports_default() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let response = session_client(server)
        .await?
        .list_agents(ListAgentsRequest {})
        .await?
        .into_inner();

    assert_eq!(response.default_agent_name, "coding");
    let names = response
        .agents
        .iter()
        .map(|agent| agent.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["coding", "configuration", "default"]);
    assert!(
        response
            .agents
            .iter()
            .all(|agent| agent.description.is_some())
    );
    Ok(())
}

#[tokio::test]
async fn create_selects_alternate_and_rejects_invalid_names()
-> Result<(), Box<dyn std::error::Error>> {
    let (server, sessions) = setup_server_with_sessions();
    let mut client = session_client(server).await?;
    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: Some(AgentSelectionRequest {
                agent_name: "default".to_owned(),
            }),
            ..CreateSessionRequest::default()
        })
        .await?
        .into_inner()
        .session
        .expect("created session");
    assert_eq!(created.agent_name, "default");
    let stored = sessions.load(&SessionId::new(&created.id)).await?;
    assert_eq!(
        stored.record.agent_definition.unwrap().agent_name,
        "default"
    );

    for name in ["", "  ", "missing", " default"] {
        let error = client
            .create_session(CreateSessionRequest {
                agent_selection: Some(AgentSelectionRequest {
                    agent_name: name.to_owned(),
                }),
                ..CreateSessionRequest::default()
            })
            .await
            .expect_err("invalid selection should fail");
        assert_eq!(error.code(), tonic::Code::InvalidArgument);
    }
    Ok(())
}

#[tokio::test]
async fn switch_updates_response_watch_list_and_durable_bundle()
-> Result<(), Box<dyn std::error::Error>> {
    let (server, sessions) = setup_server_with_sessions();
    let mut client = session_client(server).await?;
    let created = client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session");
    let mut watch = session_client(server)
        .await?
        .watch_sessions(WatchSessionsRequest {})
        .await?
        .into_inner();
    let _snapshot = watch.message().await?.expect("initial snapshot");

    let switched = client
        .update_session_agent(UpdateSessionAgentRequest {
            id: created.id.clone(),
            agent_selection: Some(AgentSelectionRequest {
                agent_name: "default".to_owned(),
            }),
        })
        .await?
        .into_inner()
        .session
        .expect("updated session");
    assert_eq!(switched.agent_name, "default");
    let update = watch.message().await?.expect("switch watch update");
    let watched = match update.event.expect("watch event") {
        watch_sessions_response::Event::Session(session) => session,
        event => panic!("expected session upsert, got {event:?}"),
    };
    assert_eq!(watched, switched);
    let listed = client
        .list_sessions(ListSessionsRequest {})
        .await?
        .into_inner()
        .sessions;
    assert_eq!(listed[0].agent_name, "default");
    let stored = sessions.load(&SessionId::new(&created.id)).await?;
    assert_eq!(
        stored.record.agent_definition.unwrap().agent_name,
        "default"
    );
    Ok(())
}

#[tokio::test]
async fn switch_validation_and_missing_session_have_precise_statuses()
-> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = session_client(server).await?;
    let created = client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session");

    for selection in [None, Some(""), Some("missing")] {
        let error = client
            .update_session_agent(UpdateSessionAgentRequest {
                id: created.id.clone(),
                agent_selection: selection.map(|name| AgentSelectionRequest {
                    agent_name: name.to_owned(),
                }),
            })
            .await
            .expect_err("invalid selection should fail");
        assert_eq!(error.code(), tonic::Code::InvalidArgument);
    }
    let error = client
        .update_session_agent(UpdateSessionAgentRequest {
            id: "missing-session".to_owned(),
            agent_selection: Some(AgentSelectionRequest {
                agent_name: "default".to_owned(),
            }),
        })
        .await
        .expect_err("missing session should fail");
    assert_eq!(error.code(), tonic::Code::NotFound);
    Ok(())
}

#[tokio::test]
async fn legacy_bundleless_summary_uses_effective_default() -> Result<(), Box<dyn std::error::Error>>
{
    let (server, sessions) = setup_server_with_sessions();
    let legacy = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await?;
    let mut client = session_client(server).await?;
    let listed = client
        .list_sessions(ListSessionsRequest {})
        .await?
        .into_inner()
        .sessions;
    let summary = listed
        .iter()
        .find(|summary| summary.id == legacy.id().as_str())
        .expect("legacy session summary");
    assert_eq!(summary.agent_name, "coding");
    Ok(())
}
