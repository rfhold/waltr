//! Server Routine contract integration tests.

mod support;

use std::time::Duration;

use waltr::grpc::waltr::routine::v1::{
    ListRoutineInvocationsRequest, RoutineInputValue, RoutineScope, RoutineValue, RoutineValueType,
    StartManualRoutineRequest, WatchManualRoutinesRequest, WatchRoutineInvocationsRequest,
    routine_value, start_manual_routine_response, watch_routine_invocations_response,
};
use waltr_server::test_support::setup_server;
use tokio_stream::StreamExt as _;
use tonic::Code;

use crate::support::grpc::routine_client;

fn start_request(source_session_id: &str, direction: &str) -> StartManualRoutineRequest {
    StartManualRoutineRequest {
        request_id: "start-request".to_owned(),
        routine_name: "fork".to_owned(),
        trigger_name: "manual".to_owned(),
        source_session_id: source_session_id.to_owned(),
        inputs: vec![RoutineInputValue {
            name: "direction".to_owned(),
            value: Some(RoutineValue {
                value: Some(routine_value::Value::StringValue(direction.to_owned())),
            }),
        }],
    }
}

#[tokio::test]
async fn definition_watch_starts_with_complete_global_snapshot()
-> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut stream = routine_client(server)
        .await?
        .watch_manual_routines(WatchManualRoutinesRequest {
            request_id: "watch-definitions".to_owned(),
        })
        .await?
        .into_inner();
    let response = stream
        .message()
        .await?
        .expect("initial definition snapshot");

    assert_eq!(response.request_id, "watch-definitions");
    assert!(!response.digest.is_empty());
    assert_eq!(response.routines.len(), 1);
    let routine = &response.routines[0];
    assert_eq!(routine.routine_name, "fork");
    assert_eq!(routine.trigger_name, "manual");
    assert_eq!(routine.scope, RoutineScope::Session as i32);
    assert!(!routine.routine_revision.is_empty());
    assert_eq!(routine.inputs[0].r#type, RoutineValueType::String as i32);
    assert_eq!(routine.results[0].name, "session-id");
    Ok(())
}

#[tokio::test]
async fn pre_admission_validation_and_source_failure_create_no_history()
-> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = routine_client(server).await?;
    let invalid = client
        .start_manual_routine(start_request("missing", " \n\t"))
        .await?
        .into_inner();
    let Some(start_manual_routine_response::Outcome::ValidationFailure(invalid)) = invalid.outcome
    else {
        panic!("structured input validation expected")
    };
    assert_eq!(invalid.violations[0].field_path, "inputs.direction");

    let missing = client
        .start_manual_routine(start_request("missing", "continue"))
        .await?
        .into_inner();
    let Some(start_manual_routine_response::Outcome::ValidationFailure(missing)) = missing.outcome
    else {
        panic!("structured source validation expected")
    };
    assert_eq!(missing.violations[0].field_path, "source_session_id");

    for (mut request, expected_field) in [
        (
            {
                let mut request = start_request("source", "continue");
                request.routine_name = "missing".to_owned();
                request
            },
            "routine_name",
        ),
        (
            {
                let mut request = start_request("source", "continue");
                request.trigger_name = "missing".to_owned();
                request
            },
            "trigger_name",
        ),
    ] {
        request.request_id = format!("invalid-{expected_field}");
        let response = client.start_manual_routine(request).await?.into_inner();
        let Some(start_manual_routine_response::Outcome::ValidationFailure(failure)) =
            response.outcome
        else {
            panic!("structured definition validation expected")
        };
        assert_eq!(failure.violations[0].field_path, expected_field);
    }

    let history = client
        .list_routine_invocations(ListRoutineInvocationsRequest {
            request_id: "list".to_owned(),
            page_size: 10,
            before_admission_sequence: None,
            metadata_filters: Default::default(),
        })
        .await?
        .into_inner();
    assert!(history.invocations.is_empty());
    assert!(history.older_page_token.is_none());
    Ok(())
}

#[tokio::test]
async fn invocation_watch_initial_snapshot_is_bounded_and_cursor_is_checked()
-> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = routine_client(server).await?;
    let mut stream = client
        .watch_routine_invocations(WatchRoutineInvocationsRequest {
            request_id: "history-watch".to_owned(),
            after_sequence: None,
            snapshot_size: 1,
            metadata_filters: Default::default(),
        })
        .await?
        .into_inner();
    let initial = stream.message().await?.expect("initial history snapshot");
    let Some(watch_routine_invocations_response::Update::Snapshot(snapshot)) = initial.update
    else {
        panic!("snapshot update expected")
    };
    assert!(snapshot.invocations.is_empty());
    assert_eq!(snapshot.through_sequence, 0);

    let error = client
        .watch_routine_invocations(WatchRoutineInvocationsRequest {
            request_id: "bad-cursor".to_owned(),
            after_sequence: Some(u64::MAX),
            snapshot_size: 1,
            metadata_filters: Default::default(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::InvalidArgument);
    Ok(())
}

#[tokio::test]
async fn invocation_metadata_filters_are_bounded_and_validated()
-> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = routine_client(server).await?;
    let error = client
        .list_routine_invocations(ListRoutineInvocationsRequest {
            request_id: "invalid-filter".to_owned(),
            page_size: 1,
            before_admission_sequence: None,
            metadata_filters: [("x".repeat(257), "value".to_owned())]
                .into_iter()
                .collect(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::InvalidArgument);

    let error = client
        .watch_routine_invocations(WatchRoutineInvocationsRequest {
            request_id: "too-many-filters".to_owned(),
            after_sequence: None,
            snapshot_size: 1,
            metadata_filters: (0..33)
                .map(|index| (format!("key-{index}"), "value".to_owned()))
                .collect(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::InvalidArgument);
    Ok(())
}

#[tokio::test]
async fn definition_watch_stays_open_without_periodic_duplicate_snapshots()
-> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut stream = routine_client(server)
        .await?
        .watch_manual_routines(WatchManualRoutinesRequest {
            request_id: "stable-definitions".to_owned(),
        })
        .await?
        .into_inner();
    assert!(stream.message().await?.is_some());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), stream.next())
            .await
            .is_err()
    );
    Ok(())
}
