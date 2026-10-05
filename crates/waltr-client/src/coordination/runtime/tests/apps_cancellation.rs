use super::*;
use crate::{McpAppEffect, McpAppEvent, McpAppView};
use waltr::grpc::waltr::session::v1::{
    self as proto,
    session_service_server::{SessionService, SessionServiceServer},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct DropSignal(Arc<AtomicBool>);
impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[derive(Clone)]
struct AppsService {
    started: Arc<tokio::sync::Notify>,
    close_called: Arc<AtomicBool>,
    call_dropped: Arc<AtomicBool>,
}

macro_rules! apps_service {
    ($($method:ident($request:ident) -> $response:ident),* $(,)?) => {
        #[tonic::async_trait]
        impl SessionService for AppsService {
            $(async fn $method(&self, _: tonic::Request<proto::$request>) -> Result<tonic::Response<proto::$response>, tonic::Status> {
                Err(tonic::Status::unimplemented("not used"))
            })*
            type WatchSessionsStream = futures_util::stream::Empty<Result<proto::WatchSessionsResponse, tonic::Status>>;
            async fn watch_sessions(&self, _: tonic::Request<proto::WatchSessionsRequest>) -> Result<tonic::Response<Self::WatchSessionsStream>, tonic::Status> {
                Err(tonic::Status::unimplemented("not used"))
            }
            type WatchTimelineStream = futures_util::stream::Empty<Result<proto::WatchTimelineResponse, tonic::Status>>;
            async fn watch_timeline(&self, _: tonic::Request<proto::WatchTimelineRequest>) -> Result<tonic::Response<Self::WatchTimelineStream>, tonic::Status> {
                Err(tonic::Status::unimplemented("not used"))
            }
            async fn call_session_mcp_app_tool(&self, _: tonic::Request<proto::CallSessionMcpAppToolRequest>) -> Result<tonic::Response<proto::CallSessionMcpAppToolResponse>, tonic::Status> {
                self.started.notify_one();
                std::future::pending().await
            }
            async fn close_session_mcp_app(&self, request: tonic::Request<proto::CloseSessionMcpAppRequest>) -> Result<tonic::Response<proto::CloseSessionMcpAppResponse>, tonic::Status> {
                assert_eq!(request.into_inner().app_id, "app-a");
                assert!(self.call_dropped.load(Ordering::SeqCst), "call future must drop before remote close");
                self.close_called.store(true, Ordering::SeqCst);
                Ok(tonic::Response::new(proto::CloseSessionMcpAppResponse {}))
            }
        }
    };
}
apps_service! {
    open_session_mcp_app(OpenSessionMcpAppRequest) -> OpenSessionMcpAppResponse,
    list_agents(ListAgentsRequest) -> ListAgentsResponse,
    create_session(CreateSessionRequest) -> CreateSessionResponse,
    list_sessions(ListSessionsRequest) -> ListSessionsResponse,
    delete_session(DeleteSessionRequest) -> DeleteSessionResponse,
    wipe_sessions(WipeSessionsRequest) -> WipeSessionsResponse,
    update_session_inference_settings(UpdateSessionInferenceSettingsRequest) -> UpdateSessionInferenceSettingsResponse,
    update_session_agent(UpdateSessionAgentRequest) -> UpdateSessionAgentResponse,
    get_timeline(GetTimelineRequest) -> GetTimelineResponse,
    submit_user_message(SubmitUserMessageRequest) -> SubmitUserMessageResponse,
    respond_external_request(RespondExternalRequestRequest) -> RespondExternalRequestResponse,
    interrupt_session(InterruptSessionRequest) -> InterruptSessionResponse,
    rollback_session(RollbackSessionRequest) -> RollbackSessionResponse,
    continue_session(ContinueSessionRequest) -> ContinueSessionResponse,
    search_session_templates(SearchSessionTemplatesRequest) -> SearchSessionTemplatesResponse,
}

fn seed_app(stream: &mut ClientStateStream, id: &str) {
    stream.runtime.reducer.apps_state_mut().current.insert(
        id.to_owned(),
        McpAppView {
            app_id: id.to_owned(),
            html: "html".to_owned(),
            tool_json: "{}".to_owned(),
            arguments_json: "{}".to_owned(),
            result_json: "{}".to_owned(),
            resource_meta_json: "{}".to_owned(),
        },
    );
}

fn call(stream: &mut ClientStateStream, app_id: &str, request_id: &str) {
    stream
        .begin_command(ClientCommand::CallSessionMcpAppTool {
            app_id: app_id.to_owned(),
            name: "refresh".to_owned(),
            arguments_json: "{}".to_owned(),
            request_id: request_id.to_owned(),
        })
        .unwrap();
}

#[tokio::test]
async fn close_preempts_blocked_transport_call_before_remote_close() {
    let service = AppsService {
        started: Arc::new(tokio::sync::Notify::new()),
        close_called: Arc::new(AtomicBool::new(false)),
        call_dropped: Arc::new(AtomicBool::new(false)),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let incoming = futures_util::stream::unfold(listener, |listener| async move {
        let accepted = listener.accept().await.map(|(socket, _)| socket);
        Some((accepted, listener))
    });
    let server_service = service.clone();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(SessionServiceServer::new(server_service))
            .serve_with_incoming_shutdown(incoming, async {
                stop_rx.await.ok();
            })
            .await
            .unwrap();
    });
    let mut stream = ready_state_stream();
    stream.runtime.executor = GrpcEffectExecutor::for_endpoint(endpoint);
    seed_app(&mut stream, "app-a");
    seed_app(&mut stream, "app-b");
    call(&mut stream, "app-a", "blocked");
    let queued = stream.effects.pop().unwrap();
    let mut in_flight = stream.start_effect(queued);
    let guard = DropSignal(Arc::clone(&service.call_dropped));
    let future = in_flight.future;
    in_flight.future = async move {
        let _guard = guard;
        future.await
    }
    .boxed_local();
    stream.in_flight = Some(in_flight);
    call(&mut stream, "app-a", "queued-a");
    call(&mut stream, "app-b", "queued-b");

    let event = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        stream.next_or_dispatch(async {
            service.started.notified().await;
            Some(ClientCommandRequest::new(
                ClientCommand::CloseSessionMcpApp {
                    app_id: "app-a".to_owned(),
                },
            ))
        }),
    )
    .await
    .expect("close command must preempt blocked RPC");
    assert!(matches!(event, ClientStateStreamEvent::Snapshot(_)));
    assert!(service.call_dropped.load(Ordering::SeqCst));
    assert!(stream.in_flight.is_none());
    assert!(!stream.effects.iter().any(|queued| matches!(&queued.effect, ClientEffect::McpApp(McpAppEffect::Call { request, .. }) if request.app_id == "app-a")));
    assert!(stream.effects.iter().any(|queued| matches!(&queued.effect, ClientEffect::McpApp(McpAppEffect::Call { request, .. }) if request.app_id == "app-b")));
    tokio::time::timeout(std::time::Duration::from_secs(3), stream.next())
        .await
        .expect("remote close must not wait for tool completion");
    assert!(service.close_called.load(Ordering::SeqCst));
    assert!(
        stream
            .runtime
            .reducer
            .state()
            .apps
            .requests
            .values()
            .any(|op| op.kind == crate::McpAppOperationKind::Close && op.status == "ready")
    );
    assert!(
        stream
            .runtime
            .reducer
            .state()
            .apps
            .current
            .contains_key("app-b")
    );
    let update = stream
        .runtime
        .reducer
        .apply(ClientEvent::McpApp(McpAppEvent::Called {
            request_id: "blocked".to_owned(),
            app_id: "app-a".to_owned(),
            result_json: "stale".to_owned(),
        }));
    assert!(update.effects().is_empty());
    assert!(
        !stream
            .runtime
            .reducer
            .state()
            .apps
            .requests
            .contains_key("blocked")
    );
    stop_tx.send(()).ok();
    server.abort();
}

#[tokio::test]
async fn unrelated_commands_and_other_app_close_preserve_in_flight_call() {
    let mut stream = ready_state_stream();
    seed_app(&mut stream, "app-a");
    seed_app(&mut stream, "app-b");
    call(&mut stream, "app-b", "blocked-b");
    let queued = stream.effects.pop().unwrap();
    let (in_flight, started, release) = controlled_in_flight(
        queued,
        ExecutedEffect::Events(vec![ClientEvent::McpApp(McpAppEvent::Called {
            request_id: "blocked-b".to_owned(),
            app_id: "app-b".to_owned(),
            result_json: "fresh".to_owned(),
        })]),
    );
    stream.in_flight = Some(in_flight);
    stream
        .next_or_dispatch(async {
            started.await.ok()?;
            Some(ClientCommandRequest::new(
                ClientCommand::RegisterConfigWatchInterest,
            ))
        })
        .await;
    assert!(stream.in_flight.is_some());
    stream
        .next_or_dispatch(async {
            Some(ClientCommandRequest::new(
                ClientCommand::CloseSessionMcpApp {
                    app_id: "app-a".to_owned(),
                },
            ))
        })
        .await;
    assert!(stream.in_flight.is_some());
    release
        .send(())
        .expect("another app's call must stay alive");
    stream.next().await;
    assert_eq!(
        stream.runtime.reducer.state().apps.requests["blocked-b"]
            .result_json
            .as_deref(),
        Some("fresh")
    );
}

#[tokio::test]
async fn cancel_open_preserves_late_lease_for_cleanup() {
    let mut stream = ready_state_stream();
    stream
        .begin_command(ClientCommand::OpenSessionMcpApp {
            session_id: "s".to_owned(),
            call_id: "c".to_owned(),
            request_id: "open".to_owned(),
        })
        .unwrap();
    let queued = stream.effects.pop().unwrap();
    seed_app(&mut stream, "late");
    let app = stream
        .runtime
        .reducer
        .apps_state_mut()
        .current
        .remove("late")
        .unwrap();
    let (in_flight, started, release) = controlled_in_flight(
        queued,
        ExecutedEffect::Events(vec![ClientEvent::McpApp(McpAppEvent::Opened {
            request_id: "open".to_owned(),
            app,
        })]),
    );
    stream.in_flight = Some(in_flight);
    stream
        .next_or_dispatch(async {
            started.await.ok()?;
            Some(ClientCommandRequest::new(
                ClientCommand::CancelSessionMcpAppOpen {
                    request_id: "open".to_owned(),
                },
            ))
        })
        .await;
    assert!(stream.in_flight.is_some());
    release.send(()).expect("open must retain lease response");
    stream.next().await;
    assert!(stream.runtime.reducer.state().apps.current.is_empty());
    assert!(
        matches!(&stream.effects.last().unwrap().effect, ClientEffect::McpApp(McpAppEffect::Close { request, .. }) if request.app_id == "late")
    );
}

#[tokio::test]
async fn app_close_preserves_unrelated_non_app_effect() {
    let mut stream = ready_state_stream();
    seed_app(&mut stream, "app-a");
    let queued = QueuedEffect {
        access_generation: stream.runtime.reducer.state().access_generation,
        effect: ClientEffect::Config(ConfigEffect::SleepReconnect { delay_ms: 1 }),
    };
    let (in_flight, started, release) =
        controlled_in_flight(queued, ExecutedEffect::Events(Vec::new()));
    stream.in_flight = Some(in_flight);
    stream
        .next_or_dispatch(async {
            started.await.ok()?;
            Some(ClientCommandRequest::new(
                ClientCommand::CloseSessionMcpApp {
                    app_id: "app-a".to_owned(),
                },
            ))
        })
        .await;
    assert!(stream.in_flight.is_some());
    release
        .send(())
        .expect("close must not cancel unrelated non-app effects");
    stream.next().await;
    assert!(stream.in_flight.is_none());
    assert!(
        matches!(&stream.effects.last().unwrap().effect, ClientEffect::McpApp(McpAppEffect::Close { request, .. }) if request.app_id == "app-a")
    );
}
