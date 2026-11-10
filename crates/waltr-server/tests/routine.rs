//! Production Routine discovery and pre-admission contracts.

mod support;

use std::{collections::BTreeMap, time::Duration};

use tonic::Code;
use waltr::grpc::waltr::routine::v2::{
    InvocationSelection, ListInvocationsRequest, StartContext, StartInvocationRequest,
    StructuredValue, Trigger, WatchDefinitionsRequest, WatchInvocationsRequest,
};
use waltr_agent::{
    InferenceProviderSettings, InferenceSettings, MemorySessionStore, ModelApi,
    PendingTimelineEvent, PendingTimelineItemId, SessionCoordinator, SessionCreateRequest,
    SessionId, SessionJournalEvent, SessionStore,
};
use waltr_server::test_support::{setup_server, setup_server_with_seeded_sessions};

use crate::support::grpc::routine_client;

#[tokio::test]
async fn definition_watch_starts_with_complete_global_snapshot()
-> Result<(), Box<dyn std::error::Error>> {
    waltr_server::isolate_application_test!();
    let mut client = routine_client(setup_server()).await?;
    let mut stream = client
        .watch_definitions(WatchDefinitionsRequest {})
        .await?
        .into_inner();
    let response = stream.message().await?.expect("initial complete catalog");
    assert!(!response.catalog_revision.is_empty());
    assert_eq!(response.definitions.len(), 12);
    let fork = response
        .definitions
        .iter()
        .find(|definition| definition.name == "fork")
        .unwrap();
    assert!(!fork.revision.is_empty());
    let manual = fork
        .entrypoints
        .iter()
        .find(|entry| entry.name == "manual")
        .unwrap();
    assert_eq!(manual.trigger, Trigger::Manual as i32);
    assert_eq!(manual.start_contexts, [StartContext::Session as i32]);
    let parameters: serde_json::Value =
        serde_json::from_slice(&fork.parameters.as_ref().unwrap().schema_json)?;
    assert_eq!(parameters["required"], serde_json::json!(["direction"]));
    let compact = response
        .definitions
        .iter()
        .find(|definition| definition.name == "compact-and-continue")
        .unwrap();
    assert!(
        compact
            .entrypoints
            .iter()
            .any(|entry| entry.name == "context-pressure"
                && entry.trigger == Trigger::Lifecycle as i32
                && entry.start_contexts.is_empty())
    );
    assert!(
        response
            .definitions
            .iter()
            .flat_map(|definition| &definition.entrypoints)
            .filter(|entry| entry.trigger == Trigger::Schedule as i32)
            .all(|entry| entry.start_contexts.is_empty())
    );
    Ok(())
}

#[tokio::test]
async fn pre_admission_validation_and_source_failure_create_no_history()
-> Result<(), Box<dyn std::error::Error>> {
    waltr_server::isolate_application_test!();
    let (sessions, source, pause) = paused_source().await?;
    let mut client = routine_client(setup_server_with_seeded_sessions(sessions.clone())).await?;
    let definitions = client
        .watch_definitions(WatchDefinitionsRequest {})
        .await?
        .into_inner()
        .message()
        .await?
        .unwrap()
        .definitions;
    let fork = definitions
        .iter()
        .find(|definition| definition.name == "fork")
        .unwrap();
    for (name, revision, entrypoint, source, parameters) in [
        (
            "fork",
            fork.revision.as_str(),
            "manual",
            Some("missing"),
            br#"{"direction":false}"#.as_slice(),
        ),
        (
            "fork",
            fork.revision.as_str(),
            "manual",
            Some("missing"),
            br#"{"direction":"continue"}"#.as_slice(),
        ),
        (
            "missing",
            fork.revision.as_str(),
            "manual",
            Some("source"),
            br#"{"direction":"continue"}"#.as_slice(),
        ),
        (
            "fork",
            fork.revision.as_str(),
            "missing",
            Some("source"),
            br#"{"direction":"continue"}"#.as_slice(),
        ),
        (
            "fork",
            "stale",
            "manual",
            Some("source"),
            br#"{"direction":"continue"}"#.as_slice(),
        ),
        (
            "fork",
            fork.revision.as_str(),
            "manual",
            None,
            br#"{"direction":"continue"}"#.as_slice(),
        ),
    ] {
        let error = client
            .start_invocation(StartInvocationRequest {
                selection: Some(InvocationSelection {
                    routine: name.into(),
                    revision: revision.into(),
                    entrypoint: entrypoint.into(),
                    source_session: source.map(str::to_owned),
                    parameters: Some(StructuredValue {
                        json: parameters.to_vec(),
                    }),
                    correlation: "validation".into(),
                }),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(
            matches!(
                error.code(),
                Code::InvalidArgument
                    | Code::FailedPrecondition
                    | Code::NotFound
                    | Code::PermissionDenied
            ),
            "{error:?}"
        );
    }
    let compact = definitions
        .iter()
        .find(|definition| definition.name == "compact-and-continue")
        .unwrap();
    let error = client
        .start_invocation(StartInvocationRequest {
            selection: Some(InvocationSelection {
                routine: compact.name.clone(),
                revision: compact.revision.clone(),
                entrypoint: "manual".into(),
                source_session: Some(source.as_str().into()),
                parameters: Some(StructuredValue {
                    json: b"{}".to_vec(),
                }),
                correlation: "busy-compaction".into(),
            }),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::FailedPrecondition);
    let history = client
        .list_invocations(ListInvocationsRequest {
            limit: 10,
            ..Default::default()
        })
        .await?
        .into_inner();
    assert!(history.invocations.is_empty());
    sessions.release_pause(&pause).await?;
    Ok(())
}

async fn paused_source() -> Result<
    (
        SessionCoordinator<MemorySessionStore>,
        SessionId,
        waltr_agent::SessionPauseLease,
    ),
    Box<dyn std::error::Error>,
> {
    let store = MemorySessionStore::new();
    let source = SessionId::new("compact-busy-source");
    let mut create = SessionCreateRequest::new(source.clone());
    create.inference_settings = Some(InferenceSettings {
        provider: InferenceProviderSettings {
            key: "test".into(),
            api: ModelApi::OpenAiResponses,
            url: "http://127.0.0.1".into(),
            model_id: "test-model".into(),
            model_cost: None,
            headers: BTreeMap::new(),
            oauth: None,
            reasoning_effort: None,
        },
    });
    create.agent_definition = Some(serde_json::to_value(
        waltr_server::agent_definition::load_default_agent(&[], "routine-compaction")?.bundle,
    )?);
    let created = store.create(create).await?;
    let commit = store
        .append_journal(
            &source,
            created.execution_token()?,
            vec![SessionJournalEvent::PromptAppended {
                prompt: "source prompt".into(),
            }],
        )
        .await?;
    let timeline = store
        .append_timeline(
            &source,
            commit.execution_token,
            vec![
                PendingTimelineEvent::user_text_snapshot(
                    source.clone(),
                    0,
                    PendingTimelineItemId::New,
                    "source prompt",
                ),
                PendingTimelineEvent::assistant_text_snapshot(
                    source.clone(),
                    0,
                    PendingTimelineItemId::New,
                    "source answer",
                ),
            ],
        )
        .await?;
    store
        .append_journal(
            &source,
            timeline.commit.execution_token,
            vec![SessionJournalEvent::TurnFinished { turn: 0 }],
        )
        .await?;
    let sessions = SessionCoordinator::new(store);
    let pause = sessions
        .acquire_pause_reject_busy(
            &source,
            waltr_agent::SessionCoordinationReason::new(
                "context-compaction",
                Some("compact-and-continue".into()),
                Some("manual".into()),
            )?,
        )
        .await?;
    Ok((sessions, source, pause))
}

#[tokio::test]
async fn invocation_watch_initial_snapshot_is_bounded_and_cursor_is_checked()
-> Result<(), Box<dyn std::error::Error>> {
    waltr_server::isolate_application_test!();
    let mut client = routine_client(setup_server()).await?;
    let initial = client
        .watch_invocations(WatchInvocationsRequest {
            limit: 1,
            ..Default::default()
        })
        .await?
        .into_inner()
        .message()
        .await?
        .unwrap();
    assert!(initial.snapshot_complete);
    assert!(initial.invocations.is_empty());
    let error = client
        .watch_invocations(WatchInvocationsRequest {
            after_cursor: "invalid-cursor".into(),
            limit: 1,
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::OutOfRange);
    Ok(())
}

#[tokio::test]
async fn invocation_filters_and_limits_are_bounded_and_validated()
-> Result<(), Box<dyn std::error::Error>> {
    waltr_server::isolate_application_test!();
    let mut client = routine_client(setup_server()).await?;
    let error = client
        .list_invocations(ListInvocationsRequest {
            source_session: Some("x".repeat(257)),
            limit: 1,
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::InvalidArgument);
    let error = client
        .watch_invocations(WatchInvocationsRequest {
            limit: 201,
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::InvalidArgument);
    Ok(())
}

#[tokio::test]
async fn definition_watch_stays_open_without_periodic_duplicate_snapshots()
-> Result<(), Box<dyn std::error::Error>> {
    waltr_server::isolate_application_test!();
    let mut client = routine_client(setup_server()).await?;
    let mut stream = client
        .watch_definitions(WatchDefinitionsRequest {})
        .await?
        .into_inner();
    assert!(stream.message().await?.is_some());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), stream.message())
            .await
            .is_err()
    );
    Ok(())
}
