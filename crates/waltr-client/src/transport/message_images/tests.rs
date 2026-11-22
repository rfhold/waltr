use std::sync::{Arc, Mutex};

use futures_util::stream;
use tokio::{net::TcpListener, sync::Notify, task::JoinHandle, time::timeout};
use tonic::Response;
use waltr::grpc::waltr::session::v1::{self as wire, session_service_server::*};

use super::*;
use crate::{NativeGrpcTransport, WaltrTransport as _};

const WAIT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq)]
enum Call {
    Begin(i32, u32),
    Write(u32, usize),
    Seal,
    Submit(Vec<(bool, String)>),
    Abort,
    Read(String, u32),
}

#[derive(Clone, Copy)]
enum Mode {
    Normal,
    ExpiredBearer,
    RejectWrite,
    HoldWrite,
    HoldSubmit,
    HoldRead,
    EmptyChunk,
    OversizeChunk,
}

#[derive(Clone)]
struct Mock {
    route: Option<String>,
    mode: Mode,
    read_payload: Vec<u8>,
    calls: Arc<Mutex<Vec<Call>>>,
    paused: Arc<Notify>,
}

impl Mock {
    fn new(mode: Mode, read_payload: Vec<u8>) -> Self {
        Self {
            route: None,
            mode,
            read_payload,
            calls: Arc::default(),
            paused: Arc::default(),
        }
    }

    fn record<T>(&self, request: &Request<T>, call: Call) {
        assert_eq!(
            request
                .metadata()
                .get(waltr::destination::HEADER)
                .and_then(|value| value.to_str().ok()),
            self.route.as_deref()
        );
        self.calls.lock().unwrap().push(call);
    }
}

macro_rules! image_service {
    ($($method:ident($request:ident) -> $response:ident),* $(,)?) => {
        #[tonic::async_trait]
        impl SessionService for Mock {
            $(async fn $method(&self, _: Request<wire::$request>) -> Result<Response<wire::$response>, Status> {
                Err(Status::unimplemented("not used"))
            })*

            type WatchSessionsStream = stream::Empty<Result<wire::WatchSessionsResponse, Status>>;
            async fn watch_sessions(&self, _: Request<wire::WatchSessionsRequest>) -> Result<Response<Self::WatchSessionsStream>, Status> {
                Err(Status::unimplemented("not used"))
            }

            type WatchTimelineStream = stream::Empty<Result<wire::WatchTimelineResponse, Status>>;
            async fn watch_timeline(&self, _: Request<wire::WatchTimelineRequest>) -> Result<Response<Self::WatchTimelineStream>, Status> {
                Err(Status::unimplemented("not used"))
            }

            async fn begin_message_image_upload(&self, request: Request<BeginMessageImageUploadRequest>) -> Result<Response<wire::BeginMessageImageUploadResponse>, Status> {
                let message = request.get_ref();
                assert_eq!(message.session_id, "same-session");
                self.record(&request, Call::Begin(message.format, message.size_bytes));
                Ok(Response::new(wire::BeginMessageImageUploadResponse { upload_id: "staged-secret".into() }))
            }

            async fn write_message_image_chunk(&self, request: Request<WriteMessageImageChunkRequest>) -> Result<Response<wire::WriteMessageImageChunkResponse>, Status> {
                let message = request.get_ref();
                assert_eq!(message.session_id, "same-session");
                assert_eq!(message.upload_id, "staged-secret");
                self.record(&request, Call::Write(message.offset, message.data.len()));
                match self.mode {
                    Mode::ExpiredBearer => {
                        use prost::Message as _;
                        use waltr::grpc::waltr::auth::v1::{
                            AuthenticationDomain, AuthenticationFailure, AuthenticationFailureReason,
                        };
                        let failure = AuthenticationFailure {
                            version: 1,
                            domain: AuthenticationDomain::WaltrAccess as i32,
                            reason: AuthenticationFailureReason::CredentialRequired as i32,
                            target: None,
                            mcp_authorization: None,
                        };
                        Err(Status::with_details(Code::Unauthenticated, "expired", failure.encode_to_vec().into()))
                    }
                    Mode::RejectWrite => Err(Status::invalid_argument("staged-secret unsafe remote error")),
                    Mode::HoldWrite => {
                        self.paused.notify_one();
                        std::future::pending().await
                    }
                    _ => Ok(Response::new(wire::WriteMessageImageChunkResponse {})),
                }
            }

            async fn seal_message_image_upload(&self, request: Request<SealMessageImageUploadRequest>) -> Result<Response<wire::SealMessageImageUploadResponse>, Status> {
                assert_eq!(request.get_ref().upload_id, "staged-secret");
                self.record(&request, Call::Seal);
                Ok(Response::new(wire::SealMessageImageUploadResponse {}))
            }

            async fn abort_message_image_upload(&self, request: Request<AbortMessageImageUploadRequest>) -> Result<Response<wire::AbortMessageImageUploadResponse>, Status> {
                assert_eq!(request.get_ref().upload_id, "staged-secret");
                self.record(&request, Call::Abort);
                Ok(Response::new(wire::AbortMessageImageUploadResponse {}))
            }

            async fn submit_user_message(&self, request: Request<SubmitUserMessageRequest>) -> Result<Response<wire::SubmitUserMessageResponse>, Status> {
                let message = request.get_ref();
                assert_eq!(message.session_id, "same-session");
                self.record(&request, Call::Submit(message.parts.iter().map(|part| match part.part.as_ref().unwrap() {
                    Part::Text(text) => (false, text.clone()),
                    Part::ImageUploadId(id) => (true, id.clone()),
                }).collect()));
                if matches!(self.mode, Mode::HoldSubmit) {
                    self.paused.notify_one();
                    std::future::pending().await
                } else {
                    Ok(Response::new(wire::SubmitUserMessageResponse {}))
                }
            }

            type ReadMessageImageStream = stream::Iter<std::vec::IntoIter<Result<wire::ReadMessageImageResponse, Status>>>;
            async fn read_message_image(&self, request: Request<ReadMessageImageRequest>) -> Result<Response<Self::ReadMessageImageStream>, Status> {
                let message = request.get_ref();
                assert_eq!(message.session_id, "same-session");
                self.record(&request, Call::Read(message.item_id.clone(), message.part_index));
                if matches!(self.mode, Mode::HoldRead) {
                    self.paused.notify_one();
                    std::future::pending().await
                } else {
                    let frames = match self.mode {
                        Mode::EmptyChunk => vec![Vec::new()],
                        Mode::OversizeChunk => vec![vec![0; CHUNK_BYTES + 1]],
                        _ => self.read_payload.chunks(CHUNK_BYTES).map(<[u8]>::to_vec).collect(),
                    };
                    Ok(Response::new(stream::iter(frames.into_iter().map(|data| Ok(wire::ReadMessageImageResponse { data })).collect::<Vec<_>>())))
                }
            }
        }
    };
}

image_service! {
    open_session_mcp_app(OpenSessionMcpAppRequest) -> OpenSessionMcpAppResponse,
    call_session_mcp_app_tool(CallSessionMcpAppToolRequest) -> CallSessionMcpAppToolResponse,
    close_session_mcp_app(CloseSessionMcpAppRequest) -> CloseSessionMcpAppResponse,
    list_agents(ListAgentsRequest) -> ListAgentsResponse,
    create_session(CreateSessionRequest) -> CreateSessionResponse,
    list_sessions(ListSessionsRequest) -> ListSessionsResponse,
    delete_session(DeleteSessionRequest) -> DeleteSessionResponse,
    wipe_sessions(WipeSessionsRequest) -> WipeSessionsResponse,
    update_session_inference_settings(UpdateSessionInferenceSettingsRequest) -> UpdateSessionInferenceSettingsResponse,
    update_session_agent(UpdateSessionAgentRequest) -> UpdateSessionAgentResponse,
    get_timeline(GetTimelineRequest) -> GetTimelineResponse,
    interrupt_session(InterruptSessionRequest) -> InterruptSessionResponse,
    rollback_session(RollbackSessionRequest) -> RollbackSessionResponse,
    continue_session(ContinueSessionRequest) -> ContinueSessionResponse,
    search_session_templates(SearchSessionTemplatesRequest) -> SearchSessionTemplatesResponse,
}

struct Server(JoinHandle<()>);

impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn serve(mock: Mock) -> (Server, NativeGrpcTransport) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let incoming = stream::unfold(listener, |listener| async move {
        Some((listener.accept().await.map(|(socket, _)| socket), listener))
    });
    let mut transport = NativeGrpcTransport::new(endpoint);
    if let Some(route) = mock.route.as_deref() {
        transport = transport.for_destination(route).unwrap();
    }
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(SessionServiceServer::new(mock))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    (Server(server), transport)
}

fn image(size: usize) -> MessageInputPart {
    MessageInputPart::Image {
        format: MessageImageFormatView::Png,
        bytes: vec![42; size],
    }
}

fn info(size_bytes: u32) -> MessageImageInfoView {
    MessageImageInfoView {
        format: MessageImageFormatView::Png,
        size_bytes,
    }
}

#[tokio::test]
async fn pinned_native_lane_uploads_in_order_and_uses_the_same_rpc_as_text() {
    let payload = vec![17; CHUNK_BYTES + 1];
    let mut mock = Mock::new(Mode::Normal, payload.clone());
    mock.route = Some(format!("remote@{}", "a".repeat(32)));
    let (_server, transport) = serve(mock.clone()).await;
    let lane = MessageImageLane::new(transport.clone());
    lane.update_access(0, true);
    lane.send(
        "same-session".into(),
        vec![
            MessageInputPart::Text("before".into()),
            image(CHUNK_BYTES + 3),
            MessageInputPart::Text("after".into()),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        lane.read(
            "same-session".into(),
            "i5".into(),
            1,
            info(u32::try_from(payload.len()).unwrap())
        )
        .await
        .unwrap(),
        payload
    );
    transport
        .submit_user_message("same-session".into(), "text only".into())
        .await
        .unwrap();
    assert_eq!(
        *mock.calls.lock().unwrap(),
        [
            Call::Begin(
                MessageImageFormat::Png as i32,
                u32::try_from(CHUNK_BYTES + 3).unwrap()
            ),
            Call::Write(0, CHUNK_BYTES),
            Call::Write(u32::try_from(CHUNK_BYTES).unwrap(), 3),
            Call::Seal,
            Call::Submit(vec![
                (false, "before".into()),
                (true, "staged-secret".into()),
                (false, "after".into())
            ]),
            Call::Read("i5".into(), 1),
            Call::Submit(vec![(false, "text only".into())]),
        ]
    );
}

#[tokio::test]
async fn image_send_reports_the_revision_from_clearing_the_rejected_bearer() {
    let mock = Mock::new(Mode::ExpiredBearer, vec![]);
    let (_server, transport) = serve(mock).await;
    let credentials = transport.credential_provider();
    credentials.replace_bearer_token("rejected").unwrap();
    let rejected_revision = credentials.credential_revision();
    let lane = MessageImageLane::new(transport);
    lane.update_access(0, true);

    let (result, expiry_revision) = timeout(
        WAIT,
        lane.send_with_access_expiry("same-session".into(), vec![image(7)]),
    )
    .await
    .unwrap();
    assert!(result.unwrap_err().is_waltr_login_required());
    assert!(!credentials.has_bearer_token());
    assert_ne!(expiry_revision, Some(rejected_revision));
    assert_eq!(expiry_revision, Some(credentials.credential_revision()));

    credentials.replace_bearer_token("replacement").unwrap();
    assert_ne!(expiry_revision, Some(credentials.credential_revision()));
}

#[tokio::test]
async fn rejected_or_canceled_staging_aborts_known_handles_without_admission() {
    for mode in [Mode::RejectWrite, Mode::HoldWrite] {
        let mock = Mock::new(mode, vec![]);
        let (_server, transport) = serve(mock.clone()).await;
        let lane = MessageImageLane::new(transport);
        lane.update_access(0, true);
        let task = tokio::spawn({
            let lane = lane.clone();
            async move { lane.send("same-session".into(), vec![image(7)]).await }
        });
        if matches!(mode, Mode::HoldWrite) {
            timeout(WAIT, mock.paused.notified()).await.unwrap();
            lane.cancel_send();
        }
        let error = timeout(WAIT, task).await.unwrap().unwrap().unwrap_err();
        assert!(!error.to_string().contains("staged-secret"));
        assert_eq!(
            *mock.calls.lock().unwrap(),
            [
                Call::Begin(MessageImageFormat::Png as i32, 7),
                Call::Write(0, 7),
                Call::Abort
            ]
        );
    }
}

#[tokio::test]
async fn canceled_admission_is_unconfirmed_and_never_retried_or_aborted() {
    let mock = Mock::new(Mode::HoldSubmit, vec![]);
    let (_server, transport) = serve(mock.clone()).await;
    let lane = MessageImageLane::new(transport);
    lane.update_access(0, true);
    let task = tokio::spawn({
        let lane = lane.clone();
        async move { lane.send("same-session".into(), vec![image(7)]).await }
    });
    timeout(WAIT, mock.paused.notified()).await.unwrap();
    lane.cancel_send();
    assert_eq!(
        timeout(WAIT, task).await.unwrap().unwrap().unwrap_err(),
        admission_unconfirmed()
    );
    assert_eq!(
        *mock.calls.lock().unwrap(),
        [
            Call::Begin(MessageImageFormat::Png as i32, 7),
            Call::Write(0, 7),
            Call::Seal,
            Call::Submit(vec![(true, "staged-secret".into())])
        ]
    );
}

#[tokio::test]
async fn replacing_the_native_lease_cancels_pending_work_without_retargeting() {
    let first = Mock::new(Mode::HoldWrite, vec![]);
    let (_first_server, first_transport) = serve(first.clone()).await;
    let credentials = first_transport.credential_provider();
    credentials.replace_bearer_token("old-generation").unwrap();
    let old = MessageImageLane::new(first_transport);
    old.update_access(0, true);
    let task = tokio::spawn({
        let lane = old.clone();
        async move { lane.send("same-session".into(), vec![image(7)]).await }
    });
    timeout(WAIT, first.paused.notified()).await.unwrap();
    credentials.replace_bearer_token("new-generation").unwrap();
    assert!(timeout(WAIT, task).await.unwrap().unwrap().is_err());
    assert!(
        old.send("same-session".into(), vec![image(7)])
            .await
            .is_err()
    );

    let second = Mock::new(Mode::Normal, vec![25; 7]);
    let (_second_server, second_transport) = serve(second.clone()).await;
    let new = MessageImageLane::new(second_transport);
    new.update_access(0, true);
    assert_eq!(
        new.read("same-session".into(), "i5".into(), 0, info(7))
            .await
            .unwrap(),
        vec![25; 7]
    );
    assert_eq!(
        *first.calls.lock().unwrap(),
        [
            Call::Begin(MessageImageFormat::Png as i32, 7),
            Call::Write(0, 7)
        ]
    );
    assert_eq!(*second.calls.lock().unwrap(), [Call::Read("i5".into(), 0)]);
}

#[tokio::test]
async fn revoked_runtime_hides_pending_reads_and_malformed_streams_return_no_bytes() {
    let mock = Mock::new(Mode::HoldRead, vec![]);
    let (_server, transport) = serve(mock.clone()).await;
    let runtime = crate::ClientRuntime::with_transport(transport);
    let lane = runtime.message_images();
    assert!(
        lane.read("same-session".into(), "i5".into(), 1, info(7))
            .await
            .is_err()
    );
    lane.update_access(0, true);
    let task = tokio::spawn({
        let lane = lane.clone();
        async move {
            lane.read("same-session".into(), "i5".into(), 1, info(7))
                .await
        }
    });
    timeout(WAIT, mock.paused.notified()).await.unwrap();
    drop(runtime);
    assert!(timeout(WAIT, task).await.unwrap().unwrap().is_err());
    assert!(
        lane.read("same-session".into(), "i5".into(), 1, info(7))
            .await
            .is_err()
    );

    for (mode, size) in [
        (Mode::Normal, 6),
        (Mode::Normal, 8),
        (Mode::EmptyChunk, 7),
        (Mode::OversizeChunk, 7),
    ] {
        let (_server, transport) = serve(Mock::new(mode, vec![49; 7])).await;
        let lane = MessageImageLane::new(transport);
        lane.update_access(0, true);
        assert!(
            lane.read("same-session".into(), "i5".into(), 1, info(size))
                .await
                .is_err()
        );
    }
}

#[test]
fn input_limits_apply_before_any_rpc() {
    assert!(validate_parts(&[]).is_err());
    assert!(validate_parts(&[MessageInputPart::Text(" \n".into())]).is_err());
    assert!(validate_parts(&[image(0)]).is_err());
    assert!(validate_parts(&[image(MAX_MESSAGE_IMAGE_BYTES + 1)]).is_err());
    assert!(
        validate_parts(&[
            image(MAX_MESSAGE_IMAGE_BYTES),
            image(MAX_MESSAGE_IMAGE_BYTES),
            image(1)
        ])
        .is_err()
    );
    assert!(validate_parts(&(0..5).map(|_| image(1)).collect::<Vec<_>>()).is_err());
    assert!(
        validate_parts(&[MessageInputPart::Text(
            "x".repeat(MAX_MESSAGE_TEXT_BYTES + 1)
        )])
        .is_err()
    );
    assert!(
        validate_parts(&[
            image(MAX_MESSAGE_IMAGE_BYTES),
            image(MAX_MESSAGE_IMAGE_BYTES)
        ])
        .is_ok()
    );
    assert!(
        !format!("{:?}", MessageInputPart::Text("draft-secret".into())).contains("draft-secret")
    );
}
