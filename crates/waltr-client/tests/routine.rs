//! UI-like Routine flows over the authoritative gRPC contract.

mod support;

use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_core::Stream;
use futures_util::StreamExt;
use waltr::grpc::waltr::{
    routine::v1::{
        GetRoutineInvocationRequest, GetRoutineInvocationResponse, ListRoutineInvocationsRequest,
        ListRoutineInvocationsResponse, ManualRoutineAccepted, ManualRoutineDefinition,
        RoutineInputDefinition, RoutineInvocation, RoutineInvocationDetail,
        RoutineInvocationSnapshot, RoutineInvocationStatus, RoutineResultDefinition, RoutineScope,
        RoutineTask, RoutineTaskStatus, RoutineTriggerKind, RoutineValidationFailure,
        RoutineValue as ProtoRoutineValue, RoutineValueType, StartManualRoutineRequest,
        StartManualRoutineResponse, WatchManualRoutinesRequest, WatchManualRoutinesResponse,
        WatchRoutineInvocationsRequest, WatchRoutineInvocationsResponse,
        routine_service_server::{RoutineService, RoutineServiceServer},
        routine_value, start_manual_routine_response, watch_routine_invocations_response,
    },
    status::v1::{
        SystemStatus, WatchStatusRequest, WatchStatusResponse,
        status_service_server::{StatusService, StatusServiceServer},
    },
};
use waltr_client::{
    ClientCommand, ClientRoutineValue, ClientRoutineValueType, ClientRuntime, WaltrTransport,
    NativeGrpcTransport,
};

use support::snapshots::{dispatch_stream_command, next_snapshot_until, no_sleep};

type ResponseStream<T> = Pin<Box<dyn Stream<Item = Result<T, tonic::Status>> + Send>>;

#[derive(Clone, Default)]
struct TestRoutineService {
    definition_opens: Arc<AtomicUsize>,
    invocation_opens: Arc<AtomicUsize>,
    invocation_filters: Arc<Mutex<Vec<std::collections::HashMap<String, String>>>>,
    list_filters: Arc<Mutex<Vec<std::collections::HashMap<String, String>>>>,
}

#[tonic::async_trait]
impl RoutineService for TestRoutineService {
    type WatchManualRoutinesStream = ResponseStream<WatchManualRoutinesResponse>;
    type WatchRoutineInvocationsStream = ResponseStream<WatchRoutineInvocationsResponse>;

    async fn watch_manual_routines(
        &self,
        request: tonic::Request<WatchManualRoutinesRequest>,
    ) -> Result<tonic::Response<Self::WatchManualRoutinesStream>, tonic::Status> {
        self.definition_opens.fetch_add(1, Ordering::SeqCst);
        let request_id = request.into_inner().request_id;
        let initial = futures_util::stream::once(async move {
            Ok(WatchManualRoutinesResponse {
                request_id,
                routines: vec![definition("revision-1")],
                digest: "digest-1".to_owned(),
            })
        })
        .chain(futures_util::stream::pending());
        Ok(tonic::Response::new(Box::pin(initial)))
    }

    async fn start_manual_routine(
        &self,
        request: tonic::Request<StartManualRoutineRequest>,
    ) -> Result<tonic::Response<StartManualRoutineResponse>, tonic::Status> {
        let request = request.into_inner();
        let outcome = if request.inputs.is_empty() {
            start_manual_routine_response::Outcome::ValidationFailure(RoutineValidationFailure {
                message: "inputs are invalid".to_owned(),
                violations: vec![waltr::grpc::waltr::routine::v1::RoutineFieldViolation {
                    field_path: "inputs.direction".to_owned(),
                    message: "required".to_owned(),
                }],
            })
        } else {
            start_manual_routine_response::Outcome::Accepted(ManualRoutineAccepted {
                invocation_id: "invocation-2".to_owned(),
            })
        };
        Ok(tonic::Response::new(StartManualRoutineResponse {
            request_id: request.request_id,
            outcome: Some(outcome),
        }))
    }

    async fn list_routine_invocations(
        &self,
        request: tonic::Request<ListRoutineInvocationsRequest>,
    ) -> Result<tonic::Response<ListRoutineInvocationsResponse>, tonic::Status> {
        let request = request.into_inner();
        assert_eq!(request.before_admission_sequence, Some(10));
        self.list_filters
            .lock()
            .unwrap()
            .push(request.metadata_filters.clone());
        Ok(tonic::Response::new(ListRoutineInvocationsResponse {
            request_id: request.request_id,
            invocations: vec![summary(9, "invocation-older", "revision-old")],
            older_page_token: None,
        }))
    }

    async fn get_routine_invocation(
        &self,
        request: tonic::Request<GetRoutineInvocationRequest>,
    ) -> Result<tonic::Response<GetRoutineInvocationResponse>, tonic::Status> {
        let request = request.into_inner();
        Ok(tonic::Response::new(GetRoutineInvocationResponse {
            request_id: request.request_id,
            invocation: Some(RoutineInvocationDetail {
                summary: Some(summary(10, &request.invocation_id, "revision-pinned")),
                tasks: vec![RoutineTask {
                    name: "create".to_owned(),
                    r#type: "sessionCreate".to_owned(),
                    status: RoutineTaskStatus::Succeeded as i32,
                    ..RoutineTask::default()
                }],
                results: Vec::new(),
                events: Vec::new(),
            }),
        }))
    }

    async fn watch_routine_invocations(
        &self,
        request: tonic::Request<WatchRoutineInvocationsRequest>,
    ) -> Result<tonic::Response<Self::WatchRoutineInvocationsStream>, tonic::Status> {
        self.invocation_opens.fetch_add(1, Ordering::SeqCst);
        let request = request.into_inner();
        assert_eq!(request.snapshot_size, 50);
        self.invocation_filters
            .lock()
            .unwrap()
            .push(request.metadata_filters.clone());
        let invocation_id = request
            .metadata_filters
            .get("waltr.routine.source_session_id")
            .map_or_else(
                || "invocation-1".to_owned(),
                |session_id| format!("invocation-{session_id}"),
            );
        let initial = futures_util::stream::once(async move {
            Ok(WatchRoutineInvocationsResponse {
                request_id: request.request_id,
                update: Some(watch_routine_invocations_response::Update::Snapshot(
                    RoutineInvocationSnapshot {
                        invocations: vec![summary(10, &invocation_id, "revision-pinned")],
                        through_sequence: 1,
                        older_page_token: Some(10),
                    },
                )),
            })
        })
        .chain(futures_util::stream::pending());
        Ok(tonic::Response::new(Box::pin(initial)))
    }
}

#[derive(Clone, Default)]
struct ReadyStatusService;

#[tonic::async_trait]
impl StatusService for ReadyStatusService {
    type WatchStatusStream = ResponseStream<WatchStatusResponse>;

    async fn watch_status(
        &self,
        _: tonic::Request<WatchStatusRequest>,
    ) -> Result<tonic::Response<Self::WatchStatusStream>, tonic::Status> {
        let stream = futures_util::stream::once(async {
            Ok(WatchStatusResponse {
                status: Some(SystemStatus { ready: true }),
            })
        })
        .chain(futures_util::stream::pending());
        Ok(tonic::Response::new(Box::pin(stream)))
    }
}

async fn endpoint() -> (String, TestRoutineService, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let incoming = futures_util::stream::unfold(listener, |listener| async move {
        let item = listener.accept().await.map(|(stream, _)| stream);
        Some((item, listener))
    });
    let service = TestRoutineService::default();
    let server_service = service.clone();
    let task = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(StatusServiceServer::new(ReadyStatusService))
            .add_service(RoutineServiceServer::new(server_service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    (format!("http://{address}"), service, task)
}

#[tokio::test]
async fn multiple_interests_share_one_physical_stream_per_routine_watch() {
    let (endpoint, service, task) = endpoint().await;
    let mut stream = ClientRuntime::for_endpoint(endpoint).into_state_stream(no_sleep);
    for command in [
        ClientCommand::RegisterRoutineDefinitionWatchInterest,
        ClientCommand::RegisterRoutineDefinitionWatchInterest,
        ClientCommand::RegisterRoutineInvocationWatchInterest,
        ClientCommand::RegisterRoutineInvocationWatchInterest,
    ] {
        dispatch_stream_command(&mut stream, command).await.unwrap();
    }
    let snapshot = next_snapshot_until(&mut stream, |snapshot| {
        snapshot.routines.definitions.watch.status == "received"
            && snapshot.routines.history.watch.status == "received"
    })
    .await;

    assert_eq!(service.definition_opens.load(Ordering::SeqCst), 1);
    assert_eq!(service.invocation_opens.load(Ordering::SeqCst), 1);
    assert_eq!(snapshot.routines.definitions.digest, "digest-1");
    let definition = &snapshot.routines.definitions.definitions[0];
    assert_eq!(definition.routine_revision, "revision-1");
    assert_eq!(definition.inputs[0].r#type, ClientRoutineValueType::String);
    assert_eq!(
        definition.inputs[0].default_value,
        Some(ClientRoutineValue::String("north".to_owned()))
    );
    assert_eq!(snapshot.routines.history.through_sequence, Some(1));
    task.abort();
}

#[tokio::test]
async fn session_history_receives_initial_snapshot_as_the_only_child_watch() {
    let (endpoint, service, task) = endpoint().await;
    let mut stream = ClientRuntime::for_endpoint(endpoint).into_state_stream(no_sleep);
    dispatch_stream_command(
        &mut stream,
        ClientCommand::RegisterSessionRoutineInvocationWatchInterest {
            session_id: "session-only".to_owned(),
        },
    )
    .await
    .unwrap();

    let snapshot = next_snapshot_until(&mut stream, |snapshot| {
        snapshot
            .routines
            .session_histories
            .get("session-only")
            .is_some_and(|history| history.watch.status == "received")
    })
    .await;

    assert_eq!(service.invocation_opens.load(Ordering::SeqCst), 1);
    assert_eq!(snapshot.routines.history.watch.status, "idle");
    assert_eq!(
        snapshot.routines.session_histories["session-only"].recent[0].invocation_id,
        "invocation-session-only"
    );
    assert_eq!(
        service.invocation_filters.lock().unwrap().as_slice(),
        &[std::collections::HashMap::from([(
            "waltr.routine.source_session_id".to_owned(),
            "session-only".to_owned(),
        )])]
    );
    task.abort();
}

#[tokio::test]
async fn list_transport_serializes_non_empty_metadata_filters() {
    let (endpoint, service, task) = endpoint().await;
    let filters = std::collections::BTreeMap::from([(
        "waltr.routine.source_session_id".to_owned(),
        "session-filter".to_owned(),
    )]);

    NativeGrpcTransport::new(endpoint)
        .list_routine_invocations("filtered-page".to_owned(), 25, Some(10), filters)
        .await
        .unwrap();

    assert_eq!(
        service.list_filters.lock().unwrap().as_slice(),
        &[std::collections::HashMap::from([(
            "waltr.routine.source_session_id".to_owned(),
            "session-filter".to_owned(),
        )])]
    );
    task.abort();
}

#[tokio::test]
async fn start_validation_history_pagination_and_pinned_detail_are_shell_neutral() {
    let (endpoint, service, task) = endpoint().await;
    let mut stream = ClientRuntime::for_endpoint(endpoint).into_state_stream(no_sleep);
    dispatch_stream_command(
        &mut stream,
        ClientCommand::RegisterRoutineInvocationWatchInterest,
    )
    .await
    .unwrap();
    next_snapshot_until(&mut stream, |snapshot| {
        snapshot.routines.history.watch.status == "received"
    })
    .await;

    dispatch_stream_command(
        &mut stream,
        ClientCommand::StartManualRoutine {
            operation_key: "accepted".to_owned(),
            request_id: "start-1".to_owned(),
            routine_name: "fork".to_owned(),
            trigger_name: "manual".to_owned(),
            source_session_id: "source".to_owned(),
            inputs: std::collections::BTreeMap::from([(
                "direction".to_owned(),
                ClientRoutineValue::String("preserve this".to_owned()),
            )]),
        },
    )
    .await
    .unwrap();
    let snapshot = next_snapshot_until(&mut stream, |snapshot| {
        snapshot
            .routines
            .starts
            .get("accepted")
            .is_some_and(|start| start.status == "ready")
    })
    .await;
    assert_eq!(
        snapshot.routines.starts["accepted"]
            .invocation_id
            .as_deref(),
        Some("invocation-2")
    );

    dispatch_stream_command(
        &mut stream,
        ClientCommand::StartManualRoutine {
            operation_key: "invalid".to_owned(),
            request_id: "start-2".to_owned(),
            routine_name: "fork".to_owned(),
            trigger_name: "manual".to_owned(),
            source_session_id: "source".to_owned(),
            inputs: Default::default(),
        },
    )
    .await
    .unwrap();
    let snapshot = next_snapshot_until(&mut stream, |snapshot| {
        snapshot
            .routines
            .starts
            .get("invalid")
            .is_some_and(|start| start.status == "failed")
    })
    .await;
    assert!(snapshot.routines.starts["invalid"].invocation_id.is_none());
    assert!(
        snapshot.routines.starts["invalid"]
            .validation_failure
            .is_some()
    );

    dispatch_stream_command(
        &mut stream,
        ClientCommand::LoadOlderRoutineInvocations {
            request_id: "page-1".to_owned(),
            page_size: 25,
        },
    )
    .await
    .unwrap();
    let snapshot = next_snapshot_until(&mut stream, |snapshot| {
        snapshot.routines.history.pagination.status == "ready"
    })
    .await;
    assert_eq!(snapshot.routines.history.recent.len(), 2);
    assert!(snapshot.routines.history.older_page_token.is_none());
    assert_eq!(
        service.list_filters.lock().unwrap().as_slice(),
        &[std::collections::HashMap::new()]
    );

    dispatch_stream_command(
        &mut stream,
        ClientCommand::LoadRoutineInvocationDetail {
            request_id: "detail-1".to_owned(),
            invocation_id: "invocation-1".to_owned(),
        },
    )
    .await
    .unwrap();
    let snapshot = next_snapshot_until(&mut stream, |snapshot| {
        snapshot
            .routines
            .details
            .get("invocation-1")
            .is_some_and(|detail| detail.status == "ready")
    })
    .await;
    let detail = snapshot.routines.details["invocation-1"]
        .detail
        .as_ref()
        .unwrap();
    assert_eq!(detail.summary.routine_revision, "revision-pinned");
    assert_eq!(detail.tasks[0].name, "create");
    task.abort();
}

#[tokio::test]
async fn session_histories_use_independent_filtered_streams_and_clear_on_final_release() {
    let (endpoint, service, task) = endpoint().await;
    let mut stream = ClientRuntime::for_endpoint(endpoint).into_state_stream(no_sleep);
    for session_id in ["session-a", "session-b"] {
        dispatch_stream_command(
            &mut stream,
            ClientCommand::RegisterSessionRoutineInvocationWatchInterest {
                session_id: session_id.to_owned(),
            },
        )
        .await
        .unwrap();
    }
    dispatch_stream_command(
        &mut stream,
        ClientCommand::RegisterRoutineInvocationWatchInterest,
    )
    .await
    .unwrap();

    let snapshot = next_snapshot_until(&mut stream, |snapshot| {
        snapshot.routines.history.watch.status == "received"
            && snapshot.routines.session_histories.len() == 2
            && snapshot
                .routines
                .session_histories
                .values()
                .all(|history| history.watch.status == "received")
    })
    .await;

    assert_eq!(
        snapshot.routines.session_histories["session-a"].recent[0].invocation_id,
        "invocation-session-a"
    );
    assert_eq!(
        snapshot.routines.session_histories["session-b"].recent[0].invocation_id,
        "invocation-session-b"
    );
    assert_eq!(
        snapshot.routines.history.recent[0].invocation_id,
        "invocation-1"
    );
    let filters = service.invocation_filters.lock().unwrap().clone();
    assert!(filters.contains(&std::collections::HashMap::new()));
    assert!(filters.contains(&std::collections::HashMap::from([(
        "waltr.routine.source_session_id".to_owned(),
        "session-a".to_owned(),
    )])));
    assert!(filters.contains(&std::collections::HashMap::from([(
        "waltr.routine.source_session_id".to_owned(),
        "session-b".to_owned(),
    )])));

    let released = dispatch_stream_command(
        &mut stream,
        ClientCommand::ReleaseSessionRoutineInvocationWatchInterest {
            session_id: "session-a".to_owned(),
        },
    )
    .await
    .unwrap();
    assert!(
        !released
            .routines
            .session_histories
            .contains_key("session-a")
    );
    assert!(
        released
            .routines
            .session_histories
            .contains_key("session-b")
    );
    assert_eq!(released.routines.history.watch.status, "received");
    task.abort();
}

fn definition(revision: &str) -> ManualRoutineDefinition {
    ManualRoutineDefinition {
        routine_name: "fork".to_owned(),
        trigger_name: "manual".to_owned(),
        display_name: "Fork".to_owned(),
        description: "Fork a session".to_owned(),
        scope: RoutineScope::Session as i32,
        inputs: vec![RoutineInputDefinition {
            name: "direction".to_owned(),
            r#type: RoutineValueType::String as i32,
            required: true,
            default_value: Some(ProtoRoutineValue {
                value: Some(routine_value::Value::StringValue("north".to_owned())),
            }),
            ..RoutineInputDefinition::default()
        }],
        results: vec![RoutineResultDefinition {
            name: "session-id".to_owned(),
            r#type: RoutineValueType::String as i32,
        }],
        routine_revision: revision.to_owned(),
    }
}

fn summary(sequence: u64, id: &str, revision: &str) -> RoutineInvocation {
    RoutineInvocation {
        admission_sequence: sequence,
        invocation_id: id.to_owned(),
        routine_name: "fork".to_owned(),
        routine_revision: revision.to_owned(),
        trigger_name: "manual".to_owned(),
        trigger_kind: RoutineTriggerKind::Manual as i32,
        correlation_id: format!("request-{sequence}"),
        metadata: std::collections::HashMap::from([(
            "waltr.routine.source_session_id".to_owned(),
            "source".to_owned(),
        )]),
        status: RoutineInvocationStatus::Succeeded as i32,
        admitted_at_ms: 1,
        started_at_ms: Some(2),
        completed_at_ms: Some(3),
        updated_at_ms: 3,
        latest_sequence: 1,
        ..RoutineInvocation::default()
    }
}
