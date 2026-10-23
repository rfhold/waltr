use super::{EngineHealth, EventBus, EventSource, engine::RoutineEngine, host::*, interaction};
use crate::resource_catalog::routine::InteractionLimits;
use crate::{
    persistence::{
        DatabaseConfig, Persistence,
        routine_invocation::{RoutineInvocationStatus, RoutineTriggerKind},
    },
    resource_catalog::AgentDefinitionState,
    routine_interaction_service::RoutineInteractionService,
};
use waltr::grpc::waltr::routine::v1::{
    self as wire,
    routine_interaction_service_client::RoutineInteractionServiceClient,
    routine_interaction_service_server::{
        RoutineInteractionService as Api, RoutineInteractionServiceServer,
    },
};
use waltr_agent::*;
use std::{collections::BTreeMap, num::NonZeroU64, sync::Arc, time::Duration};
use tokio_stream::{
    StreamExt as _,
    wrappers::{ReceiverStream, TcpListenerStream},
};
use tonic::Request;
use wire::{interact_request::Event as In, interact_response::Event as Out};
#[path = "capability_tests.rs"]
mod capability_tests;
#[path = "failure_tests.rs"]
mod failure_tests;
#[path = "identity_tests.rs"]
mod identity_tests;
#[path = "more_tests.rs"]
mod more_tests;
#[path = "ownership_tests.rs"]
mod ownership_tests;
#[path = "session_fixture.rs"]
mod session_fixture;
#[path = "telemetry_tests.rs"]
mod telemetry_tests;

struct NoHost;
impl RoutineHost for NoHost {
    fn capture_latest_completed<'a>(&'a self, _: &'a SessionId) -> CaptureFuture<'a> {
        panic!("unused")
    }
    fn acquire_pause<'a>(
        &'a self,
        _: &'a SessionId,
        _: SessionCoordinationReason,
    ) -> PauseFuture<'a> {
        panic!("unused")
    }
    fn capture_paused_effective<'a>(&'a self, _: &'a RoutinePause) -> PauseCaptureFuture<'a> {
        panic!("unused")
    }
    fn apply_paused_mask<'a>(
        &'a self,
        _: &'a RoutinePause,
        _: SessionMaskMutation,
    ) -> PauseMaskFuture<'a> {
        panic!("unused")
    }
    fn arm_paused_continuation<'a>(
        &'a self,
        _: &'a RoutinePause,
        _: TimelineCursor,
    ) -> PauseUnitFuture<'a> {
        panic!("unused")
    }
    fn release_pause<'a>(&'a self, _: &'a RoutinePause) -> PauseUnitFuture<'a> {
        panic!("unused")
    }
    fn load_inference<'a>(&'a self, _: &'a SessionId) -> HostFuture<'a, InferenceSettings> {
        panic!("unused")
    }
    fn create_child(&self, _: SessionCreateRequest) -> HostFuture<'_, ()> {
        panic!("unused")
    }
    fn submit_child(&self, _: SessionId, _: NonZeroU64) -> HostFuture<'_, ChildRunFuture> {
        panic!("unused")
    }
    fn create_destination(&self, _: SessionCreateRequest) -> HostFuture<'_, ()> {
        panic!("unused")
    }
    fn start_destination(&self, _: SessionId) -> HostFuture<'_, ()> {
        panic!("unused")
    }
    fn set_metadata_if_absent<'a>(
        &'a self,
        _: &'a SessionId,
        _: String,
        _: serde_json::Value,
    ) -> HostFuture<'a, bool> {
        panic!("unused")
    }
    fn interrupt<'a>(&'a self, _: &'a SessionId) -> HostFuture<'a, ()> {
        panic!("unused")
    }
}

const TEXT: &str = "apiVersion: waltr.dev/v1alpha1\nkind: Routine\nmetadata: {name: echo}\nspec:\n  description: Echo\n  triggers:\n    - name: live\n      interaction:\n        inputPorts:\n          - {name: input, required: true, format: {type: text}}\n        outputPorts:\n          - {name: output, format: {type: text}}\n  tasks:\n    - name: collect\n      taskSpec:\n        collect: {stream: '$(interaction.inputs.input)', maxBytes: 1024, separator: ' '}\n  outputs:\n    - {port: output, streamFrom: '$(interaction.inputs.input)'}\n";

async fn setup(
    source: &str,
    limits: InteractionLimits,
) -> (
    RoutineInteractionService,
    RoutineEngine,
    EventBus,
    Persistence,
    std::path::PathBuf,
) {
    let (service, engine, bus, persistence, root, _) =
        setup_full(source, limits, session_fixture::adapters()).await;
    (service, engine, bus, persistence, root)
}

async fn setup_full(
    source: &str,
    limits: InteractionLimits,
    adapters: Arc<dyn interaction::Adapters>,
) -> (
    RoutineInteractionService,
    RoutineEngine,
    EventBus,
    Persistence,
    std::path::PathBuf,
    AgentDefinitionState,
) {
    setup_resources(source, limits, adapters, &[]).await
}

async fn setup_resources(
    source: &str,
    limits: InteractionLimits,
    adapters: Arc<dyn interaction::Adapters>,
    extra_resources: &[(&str, String)],
) -> (
    RoutineInteractionService,
    RoutineEngine,
    EventBus,
    Persistence,
    std::path::PathBuf,
    AgentDefinitionState,
) {
    let root = crate::test_support::temp_root();
    let resources = root.join(".waltr/resources");
    for (path, content) in extra_resources {
        let path = resources.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    for directory in ["mcp-servers", "routines"] {
        std::fs::create_dir_all(resources.join(directory)).unwrap();
    }
    std::fs::write(resources.join("mcp-servers/waltr.yaml"), "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata: {name: waltr}\nspec: {transport: streamableHttp, url: 'https://unused.example', path: /mcp}\n").unwrap();
    std::fs::write(resources.join("routines/echo.yaml"), source).unwrap();
    let loader = crate::test_support::default_catalog_loader();
    let materializer = crate::catalog::CatalogMaterializer::load(&loader)
        .await
        .unwrap();
    let state = AgentDefinitionState::load_unified(root.clone(), materializer).unwrap();
    let persistence = Persistence::initialize(DatabaseConfig::for_tests().unwrap())
        .await
        .unwrap();
    let bus = EventBus::new(32);
    let engine = RoutineEngine::start_with_host_and_store(
        bus.subscribe(),
        state.clone(),
        Arc::new(NoHost),
        persistence.routine_invocation_store(),
    );
    let mut health = engine.health();
    health
        .wait_for(|v| matches!(v, EngineHealth::Running))
        .await
        .unwrap();
    let service = RoutineInteractionService::with_adapters(
        state.clone(),
        engine.manual_handle(),
        adapters,
        limits,
    )
    .unwrap();
    (service, engine, bus, persistence, root, state)
}
fn open() -> wire::OpenInteraction {
    wire::OpenInteraction {
        routine_name: "echo".into(),
        trigger_name: "live".into(),
        source_session_id: None,
        params: vec![],
        request_id: Some("correlation".into()),
    }
}
fn text_input(text: &str) -> wire::InteractionInput {
    wire::InteractionInput {
        port_name: "input".into(),
        content: Some(wire::interaction_input::Content::Text(text.into())),
    }
}
fn request(event: In) -> wire::InteractRequest {
    wire::InteractRequest { event: Some(event) }
}
fn opened() -> In {
    In::InputOpened(wire::InteractionInputOpened {
        port_name: "input".into(),
        stream_id: "s".into(),
        format: Some(wire::InteractionStreamFormat {
            content: Some(wire::interaction_stream_format::Content::Text(
                wire::InteractionTextFormat {},
            )),
        }),
    })
}
fn chunk(committed: bool) -> In {
    In::InputChunk(wire::InteractionInputChunk {
        stream_id: "s".into(),
        content: Some(wire::interaction_input_chunk::Content::Text(
            wire::TextUpdate {
                segment_id: "a".into(),
                operation: 1,
                text: "hello".into(),
                committed,
            },
        )),
    })
}
fn closed() -> In {
    In::InputClosed(wire::InteractionInputClosed {
        stream_id: "s".into(),
        status: 1,
        failure: None,
    })
}

#[tokio::test]
async fn run_commits_before_acceptance_and_uses_interaction_history() {
    let (service, engine, _bus, persistence, root) =
        setup(TEXT, InteractionLimits::default()).await;
    let mut stream = service
        .run(Request::new(wire::RunInteractionRequest {
            open: Some(open()),
            inputs: vec![text_input("hello")],
        }))
        .await
        .unwrap()
        .into_inner();
    let first = stream.next().await.unwrap().unwrap();
    let Some(Out::Accepted(accepted)) = first.event else {
        panic!("acceptance first")
    };
    let store = persistence.routine_invocation_store();
    let detail = store.detail(&accepted.invocation_id).await.unwrap();
    assert_eq!(detail.summary.trigger_kind, RoutineTriggerKind::Interaction);
    assert_eq!(detail.summary.correlation_id, "correlation");
    let mut body = String::new();
    let terminal = loop {
        let message = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        match message.event.unwrap() {
            Out::OutputChunk(v) => {
                if let Some(wire::interaction_output_chunk::Content::Text(v)) = v.content {
                    body.push_str(&v.text);
                }
            }
            Out::Completed(v) => break v,
            _ => (),
        }
    };
    assert_eq!(
        terminal.status,
        wire::RoutineInvocationStatus::Succeeded as i32,
        "{terminal:?}"
    );
    assert_eq!(body, "hello");
    assert_eq!(
        store
            .detail(&accepted.invocation_id)
            .await
            .unwrap()
            .summary
            .status,
        RoutineInvocationStatus::Succeeded
    );
    engine.shutdown().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn unary_validation_has_no_history() {
    let (service, engine, _bus, persistence, root) =
        setup(TEXT, InteractionLimits::default()).await;
    for inputs in [
        vec![],
        vec![text_input("one"), text_input("two")],
        vec![wire::InteractionInput {
            port_name: "bad".into(),
            content: Some(wire::interaction_input::Content::Text("bad".into())),
        }],
    ] {
        let mut stream = service
            .run(Request::new(wire::RunInteractionRequest {
                open: Some(open()),
                inputs,
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(matches!(
            stream.next().await.unwrap().unwrap().event,
            Some(Out::ValidationFailure(_))
        ));
        assert!(stream.next().await.is_none());
    }
    assert!(
        persistence
            .routine_invocation_store()
            .list(None, 100, &BTreeMap::new())
            .await
            .unwrap()
            .is_empty()
    );
    engine.shutdown().await;
    std::fs::remove_dir_all(root).unwrap();
}

struct Loopback {
    client: RoutineInteractionServiceClient<tonic::transport::Channel>,
    stop: tokio_util::sync::CancellationToken,
    task: tokio::task::JoinHandle<()>,
}
impl Loopback {
    async fn start(service: RoutineInteractionService) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = tokio_util::sync::CancellationToken::new();
        let shutdown = stop.clone();
        let task = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(
                    RoutineInteractionServiceServer::new(service)
                        .max_decoding_message_size(32 * 1024 * 1024),
                )
                .serve_with_incoming_shutdown(
                    TcpListenerStream::new(listener),
                    shutdown.cancelled(),
                )
                .await
                .unwrap();
        });
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let client = RoutineInteractionServiceClient::new(channel);
        Self { client, stop, task }
    }
    async fn shutdown(self) {
        self.stop.cancel();
        self.task.await.unwrap();
    }
}

#[tokio::test]
async fn interact_outputs_before_input_close_and_cancel_after_input_complete() {
    let (service, engine, _bus, persistence, root, _) = setup_full(
        TEXT,
        InteractionLimits::default(),
        Arc::new(ownership_tests::Hold),
    )
    .await;
    let mut server = Loopback::start(service).await;
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    tx.send(request(In::Open(open()))).await.unwrap();
    let mut output = server
        .client
        .interact(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    let Some(Out::Accepted(accepted)) = output.message().await.unwrap().unwrap().event else {
        panic!()
    };
    assert_eq!(
        persistence
            .routine_invocation_store()
            .detail(&accepted.invocation_id)
            .await
            .unwrap()
            .summary
            .trigger_kind,
        RoutineTriggerKind::Interaction
    );
    tx.send(request(opened())).await.unwrap();
    tx.send(request(chunk(true))).await.unwrap();
    loop {
        if matches!(
            tokio::time::timeout(Duration::from_secs(5), output.message())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .event,
            Some(Out::OutputChunk(_))
        ) {
            break;
        }
    }
    // Input is still open at the observed chunk, proving no complete-input output barrier.
    tx.send(request(closed())).await.unwrap();
    tx.send(request(In::InputCompleted(
        wire::InteractionInputCompleted {},
    )))
    .await
    .unwrap();
    tx.send(request(In::Cancel(wire::InteractionCancel {})))
        .await
        .unwrap();
    let terminal = loop {
        if let Some(Out::Completed(v)) = output.message().await.unwrap().unwrap().event {
            break v;
        }
    };
    assert_eq!(
        terminal.status,
        wire::RoutineInvocationStatus::Interrupted as i32,
        "{terminal:?}"
    );
    assert_eq!(
        persistence
            .routine_invocation_store()
            .detail(&accepted.invocation_id)
            .await
            .unwrap()
            .summary
            .status,
        RoutineInvocationStatus::Interrupted
    );
    drop(output);
    drop(tx);
    server.shutdown().await;
    engine.shutdown().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn missing_required_stream_is_failed_durably() {
    let (service, engine, _bus, persistence, root) =
        setup(TEXT, InteractionLimits::default()).await;
    let mut server = Loopback::start(service).await;
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    tx.send(request(In::Open(open()))).await.unwrap();
    let mut output = server
        .client
        .interact(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    let Some(Out::Accepted(accepted)) = output.message().await.unwrap().unwrap().event else {
        panic!()
    };
    tx.send(request(In::InputCompleted(
        wire::InteractionInputCompleted {},
    )))
    .await
    .unwrap();
    let terminal = loop {
        if let Some(Out::Completed(v)) =
            tokio::time::timeout(Duration::from_secs(5), output.message())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .event
        {
            break v;
        }
    };
    assert_eq!(
        terminal.status,
        wire::RoutineInvocationStatus::Failed as i32
    );
    assert_eq!(
        persistence
            .routine_invocation_store()
            .detail(&accepted.invocation_id)
            .await
            .unwrap()
            .summary
            .status,
        RoutineInvocationStatus::Failed
    );
    drop(output);
    drop(tx);
    tokio::time::timeout(Duration::from_secs(5), engine.shutdown())
        .await
        .unwrap();
    server.shutdown().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn capacity_sixteen_is_shared_and_shutdown_joins_unpolled_streams() {
    let (service, engine, _bus, persistence, root) =
        setup(TEXT, InteractionLimits::default()).await;
    let mut server = Loopback::start(service.clone()).await;
    let mut streams = Vec::new();
    let mut ids = Vec::new();
    for _ in 0..16 {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        tx.send(request(In::Open(open()))).await.unwrap();
        let mut output = server
            .client
            .interact(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();
        let Some(Out::Accepted(v)) = output.message().await.unwrap().unwrap().event else {
            panic!()
        };
        ids.push(v.invocation_id);
        streams.push((tx, output));
    }
    let error = service
        .run(Request::new(wire::RunInteractionRequest {
            open: Some(open()),
            inputs: vec![text_input("hello")],
        }))
        .await
        .err()
        .unwrap();
    assert_eq!(error.code(), tonic::Code::ResourceExhausted);
    tokio::time::timeout(Duration::from_secs(5), engine.shutdown())
        .await
        .unwrap();
    for id in ids {
        assert_eq!(
            persistence
                .routine_invocation_store()
                .detail(&id)
                .await
                .unwrap()
                .summary
                .status,
            RoutineInvocationStatus::Interrupted
        );
    }
    let error = service
        .run(Request::new(wire::RunInteractionRequest {
            open: Some(open()),
            inputs: vec![text_input("hello")],
        }))
        .await
        .err()
        .unwrap();
    assert_eq!(error.code(), tonic::Code::Unavailable);
    drop(streams);
    server.shutdown().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn malformed_transition_matrix_is_failed_and_never_strands_shutdown() {
    let (service, engine, _bus, persistence, root) =
        setup(TEXT, InteractionLimits::default()).await;
    let mut server = Loopback::start(service).await;
    let mut failed = wire::InteractionInputClosed {
        stream_id: "s".into(),
        status: 3,
        failure: None,
    };
    let missing_failure = In::InputClosed(failed.clone());
    failed.failure = Some(wire::RoutineFailure {
        code: "client".into(),
        message: "failed".into(),
    });
    let cases = vec![
        vec![In::Open(open())],
        vec![chunk(true)],
        vec![opened(), opened()],
        vec![
            opened(),
            In::InputCompleted(wire::InteractionInputCompleted {}),
        ],
        vec![opened(), closed(), chunk(true)],
        vec![opened(), missing_failure],
        vec![opened(), In::InputClosed(failed)],
        vec![
            opened(),
            chunk(false),
            closed(),
            In::InputCompleted(wire::InteractionInputCompleted {}),
        ],
    ];
    for events in cases {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        tx.send(request(In::Open(open()))).await.unwrap();
        let mut output = server
            .client
            .interact(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();
        let Some(Out::Accepted(v)) = output.message().await.unwrap().unwrap().event else {
            panic!()
        };
        for event in events {
            tx.send(request(event)).await.unwrap();
        }
        let terminal = loop {
            let value = tokio::time::timeout(Duration::from_secs(5), output.message())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if let Some(Out::OutputClosed(v)) = &value.event {
                assert_eq!(v.failure.is_some(), v.status == 3);
            }
            if let Some(Out::Completed(v)) = value.event {
                break v;
            }
        };
        assert_eq!(
            terminal.status,
            wire::RoutineInvocationStatus::Failed as i32,
            "{terminal:?}"
        );
        assert!(terminal.failure.is_some());
        assert_eq!(
            persistence
                .routine_invocation_store()
                .detail(&v.invocation_id)
                .await
                .unwrap()
                .summary
                .status,
            RoutineInvocationStatus::Failed,
            "{terminal:?}"
        );
        drop(tx);
        drop(output);
    }
    tokio::time::timeout(Duration::from_secs(5), engine.shutdown())
        .await
        .unwrap();
    server.shutdown().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn normal_halfclose_succeeds_and_disconnect_interrupts() {
    let (service, engine, _bus, persistence, root) =
        setup(TEXT, InteractionLimits::default()).await;
    let mut server = Loopback::start(service).await;
    let mut abandoned_id = None;
    for abandon in [false, true] {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        tx.send(request(In::Open(open()))).await.unwrap();
        let mut output = server
            .client
            .interact(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();
        let Some(Out::Accepted(v)) = output.message().await.unwrap().unwrap().event else {
            panic!()
        };
        if abandon {
            abandoned_id = Some(v.invocation_id);
            drop(output);
            drop(tx);
            continue;
        }
        tx.send(request(opened())).await.unwrap();
        tx.send(request(chunk(true))).await.unwrap();
        tx.send(request(closed())).await.unwrap();
        drop(tx);
        let terminal = loop {
            if let Some(Out::Completed(v)) =
                tokio::time::timeout(Duration::from_secs(5), output.message())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
                    .event
            {
                break v;
            }
        };
        assert_eq!(
            terminal.status,
            wire::RoutineInvocationStatus::Succeeded as i32,
            "{terminal:?}"
        );
    }
    tokio::time::timeout(Duration::from_secs(5), engine.shutdown())
        .await
        .unwrap();
    assert_eq!(
        persistence
            .routine_invocation_store()
            .detail(&abandoned_id.unwrap())
            .await
            .unwrap()
            .summary
            .status,
        RoutineInvocationStatus::Interrupted
    );
    server.shutdown().await;
    std::fs::remove_dir_all(root).unwrap();
}
