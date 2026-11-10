use super::*;
use futures_util::{Stream, stream};
use std::{
    pin::Pin,
    sync::{Arc, Mutex},
};
use tonic::{Request, Response, Status};
use wire::routine_service_server::{RoutineService, RoutineServiceServer};

type Responses<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Parallel,
    BadDigest,
    Short,
    TrailingError,
    Expired,
    LostAppend,
    BlockedAppend,
    NoEof,
    BlockedRead,
    StreamsOutput,
}
#[derive(Default)]
struct State {
    open: Option<wire::InteractionOpen>,
    upload: Vec<u8>,
    chunks: Vec<usize>,
    order: Vec<&'static str>,
    committed: bool,
    read: bool,
    terminal: bool,
    canceled: bool,
    closes: usize,
    text_offsets: Vec<u64>,
    streams: Vec<wire::StreamOpen>,
    stream_chunks: Vec<wire::StreamChunk>,
    stream_closes: Vec<wire::StreamClose>,
    output: Option<tokio::sync::mpsc::Sender<Result<wire::InteractResponse, Status>>>,
}
#[derive(Clone)]
struct Service {
    state: Arc<Mutex<State>>,
    wake: Arc<tokio::sync::Notify>,
    mode: Mode,
    accepted_chunk_bytes: u32,
}
fn digest(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|b| {
            [
                char::from(DIGITS[usize::from(b >> 4)]),
                char::from(DIGITS[usize::from(b & 15)]),
            ]
        })
        .collect()
}
fn descriptor(mode: Mode) -> wire::BlobDescriptor {
    wire::BlobDescriptor {
        blob_id: "output".into(),
        metadata: Some(wire::BlobMetadata {
            display_name: "result.bin".into(),
            content_type: "application/octet-stream".into(),
            bytes: 3,
            sha256: if mode == Mode::BadDigest {
                vec![0; 32]
            } else {
                digest(&[1, 2, 3])
            },
            image: None,
        }),
        remaining_validity_ms: Some(if mode == Mode::Expired { 30 } else { 900000 }),
    }
}
fn response(event: wire::output_event::Event) -> Result<wire::InteractResponse, Status> {
    Ok(wire::InteractResponse {
        output: Some(wire::OutputEvent { event: Some(event) }),
    })
}
impl Service {
    async fn terminal(&self, status: i32) {
        let (output, send) = {
            let mut state = self.state.lock().unwrap();
            let send = !state.terminal;
            state.terminal = true;
            (state.output.take(), send)
        };
        if let Some(output) = output {
            if send {
                let _sent = output
                    .send(response(wire::output_event::Event::Terminal(
                        wire::InteractionTerminal {
                            status,
                            cleanup_complete: true,
                            ..Default::default()
                        },
                    )))
                    .await;
            }
            if self.mode == Mode::NoEof && status == 3 {
                self.state.lock().unwrap().output = Some(output);
            }
        }
    }
}
#[tonic::async_trait]
impl RoutineService for Service {
    type WatchDefinitionsStream = Responses<wire::WatchDefinitionsResponse>;
    type WatchInvocationsStream = Responses<wire::WatchInvocationsResponse>;
    type InteractStream = Responses<wire::InteractResponse>;
    type WatchLiveOutputStream = Responses<wire::WatchLiveOutputResponse>;
    type ReadBlobStream = Responses<wire::ReadBlobResponse>;
    async fn watch_definitions(
        &self,
        _: Request<wire::WatchDefinitionsRequest>,
    ) -> Result<Response<Self::WatchDefinitionsStream>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn start_invocation(
        &self,
        _: Request<wire::StartInvocationRequest>,
    ) -> Result<Response<wire::StartInvocationResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn list_invocations(
        &self,
        _: Request<wire::ListInvocationsRequest>,
    ) -> Result<Response<wire::ListInvocationsResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn get_invocation(
        &self,
        _: Request<wire::GetInvocationRequest>,
    ) -> Result<Response<wire::GetInvocationResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn watch_invocations(
        &self,
        _: Request<wire::WatchInvocationsRequest>,
    ) -> Result<Response<Self::WatchInvocationsStream>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn list_operations(
        &self,
        _: Request<wire::ListOperationsRequest>,
    ) -> Result<Response<wire::ListOperationsResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn cancel_invocation(
        &self,
        _: Request<wire::CancelInvocationRequest>,
    ) -> Result<Response<wire::CancelInvocationResponse>, Status> {
        {
            let mut state = self.state.lock().unwrap();
            state.canceled = true;
            state.order.push("cancel");
        }
        self.terminal(5).await;
        self.wake.notify_one();
        Ok(Response::new(wire::CancelInvocationResponse { status: 5 }))
    }
    async fn read_text_result(
        &self,
        request: Request<wire::ReadTextResultRequest>,
    ) -> Result<Response<wire::ReadTextResultResponse>, Status> {
        let request = request.into_inner();
        self.state.lock().unwrap().text_offsets.push(request.offset);
        let text = durable_text();
        let offset = request.offset as usize;
        let end = if offset == 0 {
            2
        } else {
            (offset + 65536).min(text.len())
        };
        Ok(Response::new(wire::ReadTextResultResponse {
            data: text.as_bytes()[offset..end].to_vec(),
            next_offset: end as u64,
            eof: end == text.len(),
        }))
    }
    async fn interact(
        &self,
        request: Request<tonic::Streaming<wire::InteractRequest>>,
    ) -> Result<Response<Self::InteractStream>, Status> {
        let mut input = request.into_inner();
        let Some(wire::interact_request::Frame::Open(open)) =
            input.next().await.transpose()?.and_then(|x| x.frame)
        else {
            return Err(Status::invalid_argument("open first"));
        };
        {
            let mut state = self.state.lock().unwrap();
            assert!(state.open.is_none(), "no admission retry");
            state.open = Some(open);
            state.order.push("open");
        }
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        self.state.lock().unwrap().output = Some(tx.clone());
        tx.send(response(wire::output_event::Event::Accepted(
            wire::InteractionAccepted {
                invocation_id: "invocation".into(),
                revision: "opaque-9007199254740993".into(),
                limits: Some(wire::ServerLimits {
                    chunk_bytes: self.accepted_chunk_bytes,
                    ..limits()
                }),
            },
        )))
        .await
        .unwrap();
        self.state.lock().unwrap().order.push("accepted");
        if self.mode == Mode::StreamsOutput {
            for (port, id, format, payload) in [
                (
                    "download",
                    "audio-out",
                    wire::stream_open::Format::Pcm16le(wire::Pcm16Le {
                        sample_rate: 24000,
                        channels: 1,
                    }),
                    wire::stream_chunk::Payload::Audio(vec![1, 2, 3, 4]),
                ),
                (
                    "transcript",
                    "text-out",
                    wire::stream_open::Format::Text(wire::TextFormat {}),
                    wire::stream_chunk::Payload::Text(wire::TextRevision {
                        segment_id: "text".into(),
                        mutation: 1,
                        text: " exact final\n ".into(),
                        committed: true,
                    }),
                ),
            ] {
                tx.send(response(wire::output_event::Event::Open(
                    wire::StreamOpen {
                        port: port.into(),
                        stream_id: id.into(),
                        format: Some(format),
                    },
                )))
                .await
                .unwrap();
                tx.send(response(wire::output_event::Event::Chunk(
                    wire::StreamChunk {
                        stream_id: id.into(),
                        sequence: 0,
                        payload: Some(payload),
                    },
                )))
                .await
                .unwrap();
                tx.send(response(wire::output_event::Event::Close(
                    wire::StreamClose {
                        stream_id: id.into(),
                        next_sequence: 1,
                    },
                )))
                .await
                .unwrap();
            }
        } else {
            tx.send(response(wire::output_event::Event::Blob(
                wire::BlobOutput {
                    port: "download".into(),
                    blob: Some(descriptor(self.mode)),
                    output_grant: Some(wire::BinaryOutputCredentials {
                        invocation_id: "invocation".into(),
                        port: "download".into(),
                        blob_id: "output".into(),
                        generation: "generation".into(),
                        secret: vec![7; 32],
                    }),
                },
            )))
            .await
            .unwrap();
        }
        let service = self.clone();
        tokio::spawn(async move {
            loop {
                if service.state.lock().unwrap().canceled {
                    break;
                }
                let event = tokio::select! {
                    value = input.next() => value,
                    () = service.wake.notified() => continue,
                };
                match event {
                    Some(Ok(wire::InteractRequest {
                        frame: Some(wire::interact_request::Frame::Input(event)),
                    })) => match event.event {
                        Some(wire::input_event::Event::Blob(_)) => {
                            let mut state = service.state.lock().unwrap();
                            assert!(state.committed, "binding must follow commit");
                            state.order.push("bind");
                        }
                        Some(wire::input_event::Event::Open(open)) => {
                            service.state.lock().unwrap().streams.push(open)
                        }
                        Some(wire::input_event::Event::Chunk(chunk)) => {
                            service.state.lock().unwrap().stream_chunks.push(chunk)
                        }
                        Some(wire::input_event::Event::Close(close)) => {
                            service.state.lock().unwrap().stream_closes.push(close)
                        }
                        Some(wire::input_event::Event::Finish(_)) => {
                            service.state.lock().unwrap().order.push("finish");
                            service.terminal(3).await;
                            break;
                        }
                        Some(wire::input_event::Event::Cancel(_)) => {
                            {
                                let mut state = service.state.lock().unwrap();
                                state.canceled = true;
                                state.order.push("cancel-input");
                            }
                            service.wake.notify_waiters();
                            service.terminal(5).await;
                            break;
                        }
                        _ => {}
                    },
                    None | Some(Err(_)) => {
                        service.terminal(5).await;
                        break;
                    }
                    _ => {}
                }
            }
            drop(tx);
        });
        Ok(Response::new(Box::pin(stream::unfold(
            rx,
            |mut rx| async { rx.recv().await.map(|value| (value, rx)) },
        ))))
    }
    async fn open_attachment(
        &self,
        _: Request<wire::OpenAttachmentRequest>,
    ) -> Result<Response<wire::OpenAttachmentResponse>, Status> {
        Err(Status::unimplemented("native only"))
    }
    async fn send_input_batch(
        &self,
        _: Request<wire::SendInputBatchRequest>,
    ) -> Result<Response<wire::SendInputBatchResponse>, Status> {
        Err(Status::unimplemented("native only"))
    }
    async fn watch_live_output(
        &self,
        _: Request<wire::WatchLiveOutputRequest>,
    ) -> Result<Response<Self::WatchLiveOutputStream>, Status> {
        Err(Status::unimplemented("native only"))
    }
    async fn renew_attachment(
        &self,
        _: Request<wire::RenewAttachmentRequest>,
    ) -> Result<Response<wire::RenewAttachmentResponse>, Status> {
        Err(Status::unimplemented("native only"))
    }
    async fn close_attachment(
        &self,
        _: Request<wire::CloseAttachmentRequest>,
    ) -> Result<Response<wire::CloseAttachmentResponse>, Status> {
        Err(Status::unimplemented("native only"))
    }
    async fn begin_blob_upload(
        &self,
        request: Request<wire::BeginBlobUploadRequest>,
    ) -> Result<Response<wire::BeginBlobUploadResponse>, Status> {
        let request = request.into_inner();
        let mut state = self.state.lock().unwrap();
        assert!(state.order.contains(&"accepted"));
        let Some(wire::input_declaration::Format::Binary(declaration)) =
            &state.open.as_ref().unwrap().inputs[0].format
        else {
            panic!()
        };
        assert_eq!(request.declared_bytes, declaration.declared_bytes);
        assert_eq!(request.display_name, declaration.display_name);
        assert_eq!(request.content_type, declaration.content_type);
        assert_eq!(request.port, "input");
        state.order.push("begin");
        Ok(Response::new(wire::BeginBlobUploadResponse {
            upload_id: "upload".into(),
            chunk_bytes: 4096,
        }))
    }
    async fn append_blob_upload(
        &self,
        request: Request<wire::AppendBlobUploadRequest>,
    ) -> Result<Response<wire::AppendBlobUploadResponse>, Status> {
        let request = request.into_inner();
        {
            let mut state = self.state.lock().unwrap();
            assert!(request.data.len() <= 4096);
            assert_eq!(request.sequence as usize, state.chunks.len());
            assert_eq!(request.offset as usize, state.upload.len());
            state.order.push("append");
            state.chunks.push(request.data.len());
            state.upload.extend(&request.data);
        }
        self.wake.notify_one();
        if self.mode == Mode::LostAppend {
            return Err(Status::unavailable("ambiguous append"));
        }
        while (self.mode == Mode::Parallel && !self.state.lock().unwrap().read)
            || (self.mode == Mode::BlockedAppend && !self.state.lock().unwrap().canceled)
        {
            self.wake.notified().await;
        }
        Ok(Response::new(wire::AppendBlobUploadResponse {
            accepted_sequence: request.sequence,
            next_offset: request.offset + request.data.len() as u64,
        }))
    }
    async fn commit_blob_upload(
        &self,
        request: Request<wire::CommitBlobUploadRequest>,
    ) -> Result<Response<wire::CommitBlobUploadResponse>, Status> {
        let request = request.into_inner();
        let mut state = self.state.lock().unwrap();
        assert_eq!(request.bytes, state.upload.len() as u64);
        assert_eq!(request.next_sequence, state.chunks.len() as u64);
        state.committed = true;
        state.order.push("commit");
        let Some(wire::input_declaration::Format::Binary(declaration)) =
            &state.open.as_ref().unwrap().inputs[0].format
        else {
            panic!()
        };
        Ok(Response::new(wire::CommitBlobUploadResponse {
            blob: Some(wire::BlobDescriptor {
                blob_id: "input-blob".into(),
                metadata: Some(wire::BlobMetadata {
                    display_name: declaration.display_name.clone(),
                    content_type: declaration.content_type.clone(),
                    bytes: request.bytes,
                    sha256: digest(&state.upload),
                    image: None,
                }),
                remaining_validity_ms: None,
            }),
        }))
    }
    async fn cancel_blob_upload(
        &self,
        _: Request<wire::CancelBlobUploadRequest>,
    ) -> Result<Response<wire::CancelBlobUploadResponse>, Status> {
        self.state.lock().unwrap().order.push("cancel-upload");
        Ok(Response::new(wire::CancelBlobUploadResponse {}))
    }
    async fn open_blob_read(
        &self,
        request: Request<wire::OpenBlobReadRequest>,
    ) -> Result<Response<wire::OpenBlobReadResponse>, Status> {
        let request = request.into_inner();
        let Some(wire::open_blob_read_request::Access::OutputGrant(grant)) = request.access else {
            panic!("output grant required")
        };
        assert_eq!(grant.secret, vec![7; 32]);
        assert_eq!(grant.port, "download");
        Ok(Response::new(wire::OpenBlobReadResponse {
            credentials: Some(wire::BlobReaderCredentials {
                invocation_id: "invocation".into(),
                reader_id: "reader".into(),
                generation: "generation".into(),
                secret: vec![8; 32],
            }),
            blob: Some({
                let mut blob = descriptor(self.mode);
                // A later descriptor must never renew the event-pinned lease.
                blob.remaining_validity_ms = Some(900_000);
                blob
            }),
        }))
    }
    async fn read_blob(
        &self,
        _: Request<wire::ReadBlobRequest>,
    ) -> Result<Response<Self::ReadBlobStream>, Status> {
        {
            let mut state = self.state.lock().unwrap();
            state.read = true;
            state.order.push("read");
        }
        self.wake.notify_one();
        let data = if self.mode == Mode::Short {
            vec![1, 2]
        } else {
            vec![1, 2, 3]
        };
        let first = stream::iter([Ok(wire::ReadBlobResponse {
            data,
            sequence: 0,
            eof: !matches!(self.mode, Mode::Expired | Mode::BlockedRead),
        })]);
        let source: Self::ReadBlobStream = if matches!(self.mode, Mode::Expired | Mode::BlockedRead)
        {
            Box::pin(first.chain(stream::pending()))
        } else if self.mode == Mode::TrailingError {
            Box::pin(first.chain(stream::iter([Err(Status::unavailable("unclean end"))])))
        } else {
            Box::pin(first)
        };
        Ok(Response::new(source))
    }
    async fn close_blob_read(
        &self,
        _: Request<wire::CloseBlobReadRequest>,
    ) -> Result<Response<wire::CloseBlobReadResponse>, Status> {
        self.state.lock().unwrap().closes += 1;
        Ok(Response::new(wire::CloseBlobReadResponse {}))
    }
}
struct Fixture {
    service: Service,
    transport: NativeGrpcTransport,
    credentials: NativeCredentialProvider,
    stop: tokio::sync::oneshot::Sender<()>,
    server: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
}
impl Fixture {
    async fn new(mode: Mode) -> Self {
        Self::with_chunk_bytes(mode, 65536).await
    }
    async fn with_chunk_bytes(mode: Mode, accepted_chunk_bytes: u32) -> Self {
        let service = Service {
            state: Arc::default(),
            wake: Arc::default(),
            mode,
            accepted_chunk_bytes,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let incoming = stream::unfold(listener, |listener| async {
            Some((listener.accept().await.map(|(socket, _)| socket), listener))
        });
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let router = tonic::transport::Server::builder()
            .add_service(RoutineServiceServer::new(service.clone()));
        let server = tokio::spawn(router.serve_with_incoming_shutdown(incoming, async {
            let _stopped = stopped.await;
        }));
        let credentials = NativeCredentialProvider::default();
        let transport =
            NativeGrpcTransport::with_credential_provider(endpoint, credentials.clone());
        Self {
            service,
            transport,
            credentials,
            stop,
            server,
        }
    }
    async fn finish(self) {
        let _stopped = self.stop.send(());
        timeout(Duration::from_secs(2), self.server)
            .await
            .expect("fixture server must join")
            .unwrap()
            .unwrap();
    }
    async fn run(
        &self,
        request: MediaRequest,
        cancel: watch::Receiver<bool>,
    ) -> Result<(String, bool), String> {
        execute(
            request,
            self.transport.clone(),
            self.credentials.clone(),
            self.credentials.credential_lease().generation(),
            cancel,
        )
        .await
    }
}
fn request(dir: &Scratch) -> MediaRequest {
    let path = dir.path("input.bin");
    fs::write(&path, vec![9; 70000]).unwrap();
    let mut d = definition(RoutineMedia::File);
    d.ports.push(RoutinePort {
        name: "download".into(),
        direction: RoutineDirection::Output,
        media: RoutineMedia::File,
        required: true,
        max_bytes: RoutineRevision(64 * 1024 * 1024),
        formats: vec!["application/octet-stream".into()],
    });
    MediaRequest::Interaction {
        id: 1,
        definition: Box::new(d),
        limits: Box::new(limits().into()),
        entrypoint: "live".into(),
        source: None,
        parameters: " { }\n".into(),
        plan: Box::new(LocalPlan {
            inputs: vec![selection(path, "application/octet-stream")],
            saves: [("download".into(), dir.path("saved.bin"))].into(),
            ..Default::default()
        }),
    }
}
fn durable_text() -> String {
    format!("😀{}\n ", "x".repeat(70000))
}

async fn narrowed_stream_input(media: RoutineMedia, accepted_chunk_bytes: u32) {
    let dir = Scratch::new();
    let fixture = Fixture::with_chunk_bytes(Mode::Parallel, accepted_chunk_bytes).await;
    let mut request = request(&dir);
    let MediaRequest::Interaction {
        definition: d,
        plan,
        limits: catalog,
        ..
    } = &mut request
    else {
        panic!()
    };
    assert_eq!(catalog.chunk_bytes, 65536);
    d.ports[0].media = media;
    d.ports[0].formats = vec![
        if media == RoutineMedia::Text {
            "text"
        } else {
            "pcm16le"
        }
        .into(),
    ];
    plan.inputs[0].mime = None;
    let text = format!("  {}\n ", "é😀".repeat(12000));
    if media == RoutineMedia::Text {
        plan.inputs[0].path = None;
        plan.inputs[0].text = Some(text.clone());
    } else {
        plan.inputs[0].pcm24k_mono = true;
    }
    let (_cancel, canceled) = watch::channel(false);
    let result = timeout(Duration::from_secs(2), fixture.run(request, canceled))
        .await
        .expect("negotiated stream must finish or reject promptly");
    if accepted_chunk_bytes < 256 {
        assert!(result.unwrap_err().contains("too small"));
        let state = fixture.service.state.lock().unwrap();
        assert!(state.stream_chunks.is_empty());
        assert!(state.canceled);
    } else {
        result.unwrap();
        let state = fixture.service.state.lock().unwrap();
        assert!(state.order.contains(&"finish"));
        assert!(!state.canceled);
        assert_eq!(state.streams.len(), 1);
        assert_eq!(state.stream_closes.len(), 1);
        assert_eq!(
            state.stream_closes[0].next_sequence,
            state.stream_chunks.len() as u64
        );
        assert!(state.stream_chunks.len() > 1);
        let mut received_text = String::new();
        let mut received_audio = Vec::new();
        for (sequence, chunk) in state.stream_chunks.iter().enumerate() {
            assert_eq!(chunk.sequence, sequence as u64);
            match chunk.payload.as_ref().unwrap() {
                wire::stream_chunk::Payload::Text(revision) => {
                    // The producer reserves 128 bytes for protobuf and stream metadata.
                    assert!(revision.text.len() + 128 <= accepted_chunk_bytes as usize);
                    assert_eq!(
                        revision.committed,
                        sequence + 1 == state.stream_chunks.len()
                    );
                    received_text.push_str(&revision.text);
                }
                wire::stream_chunk::Payload::Audio(bytes) => {
                    assert!(bytes.len() <= accepted_chunk_bytes as usize);
                    assert_eq!(bytes.len() % 2, 0);
                    received_audio.extend_from_slice(bytes);
                }
            }
        }
        if media == RoutineMedia::Text {
            assert_eq!(received_text, text);
        } else {
            assert_eq!(received_audio, vec![9; 70000]);
        }
    }
    assert!(dir.clean());
    fixture.finish().await;
}

#[tokio::test]
async fn native_v2_accepted_4096_chunks_override_catalog_65536_for_utf8_text() {
    narrowed_stream_input(RoutineMedia::Text, 4096).await;
}

#[tokio::test]
async fn native_v2_accepted_4096_chunks_override_catalog_65536_for_pcm() {
    narrowed_stream_input(RoutineMedia::Audio, 4096).await;
}

#[tokio::test]
async fn native_v2_tiny_accepted_chunks_reject_without_sending_or_panicking() {
    narrowed_stream_input(RoutineMedia::Text, 128).await;
}
#[tokio::test]
async fn native_v2_raw_audio_save_and_exact_final_text_wait_for_clean_completion() {
    let dir = Scratch::new();
    let fixture = Fixture::new(Mode::StreamsOutput).await;
    let mut request = request(&dir);
    let MediaRequest::Interaction {
        definition: d,
        plan,
        ..
    } = &mut request
    else {
        panic!()
    };
    d.ports[1].media = RoutineMedia::Audio;
    d.ports[1].formats = vec!["pcm16le".into()];
    d.ports.push(RoutinePort {
        name: "transcript".into(),
        direction: RoutineDirection::Output,
        media: RoutineMedia::Text,
        required: true,
        max_bytes: RoutineRevision(65536),
        formats: vec!["text".into()],
    });
    plan.selected_text = Some("transcript".into());
    let (_cancel, canceled) = watch::channel(false);
    let (text, truncated) = fixture.run(request, canceled).await.unwrap();
    assert_eq!(text, " exact final\n ");
    assert!(!truncated);
    assert_eq!(fs::read(dir.path("saved.bin")).unwrap(), [1, 2, 3, 4]);
    assert!(dir.clean());
    fixture.finish().await;
}
#[tokio::test]
async fn native_v2_cancel_closes_an_active_blocked_binary_reader() {
    let dir = Scratch::new();
    let fixture = Fixture::new(Mode::BlockedRead).await;
    let (cancel, canceled) = watch::channel(false);
    let signal = async {
        while !fs::read_dir(&dir.0).unwrap().any(|entry| {
            entry
                .unwrap()
                .path()
                .join("content")
                .metadata()
                .is_ok_and(|metadata| metadata.len() == 3)
        }) {
            tokio::task::yield_now().await;
        }
        cancel.send_replace(true);
    };
    let (result, ()) = timeout(Duration::from_secs(2), async {
        tokio::join!(fixture.run(request(&dir), canceled), signal)
    })
    .await
    .expect("active reader cancellation must join promptly");
    assert!(result.is_err());
    assert_eq!(fixture.service.state.lock().unwrap().closes, 1);
    assert!(!std::path::Path::new(&dir.path("saved.bin")).exists());
    assert!(dir.clean());
    fixture.finish().await;
}
#[tokio::test]
async fn native_v2_file_image_declarations_are_pinned_not_content_policy_proof() {
    for (media, mime) in [
        (RoutineMedia::File, "application/pdf"),
        (RoutineMedia::File, "text/plain"),
        (RoutineMedia::File, "application/x-private-format"),
        (RoutineMedia::Image, "image/png"),
        (RoutineMedia::Image, "image/jpeg"),
        (RoutineMedia::Image, "image/webp"),
    ] {
        let dir = Scratch::new();
        let fixture = Fixture::new(Mode::Parallel).await;
        let mut request = request(&dir);
        let MediaRequest::Interaction {
            definition: d,
            plan,
            ..
        } = &mut request
        else {
            panic!()
        };
        d.ports[0].media = media;
        d.ports[0].formats = vec![mime.into()];
        plan.inputs[0].mime = Some(mime.into());
        let (_cancel, canceled) = watch::channel(false);
        fixture.run(request, canceled).await.unwrap();
        {
            let state = fixture.service.state.lock().unwrap();
            let Some(wire::input_declaration::Format::Binary(declaration)) =
                &state.open.as_ref().unwrap().inputs[0].format
            else {
                panic!()
            };
            assert_eq!(declaration.content_type, mime);
            assert_eq!(
                matches!(
                    declaration.purpose,
                    Some(wire::binary_input_declaration::Purpose::Image(_))
                ),
                media == RoutineMedia::Image
            );
        }
        fixture.finish().await;
    }
}
#[tokio::test]
async fn native_v2_text_and_explicit_pcm_files_close_streams_before_finish_input() {
    for media in [RoutineMedia::Text, RoutineMedia::Audio] {
        let dir = Scratch::new();
        let fixture = Fixture::new(Mode::Parallel).await;
        let mut request = request(&dir);
        let MediaRequest::Interaction {
            definition: d,
            plan,
            ..
        } = &mut request
        else {
            panic!()
        };
        d.ports[0].media = media;
        d.ports[0].formats = vec![
            if media == RoutineMedia::Text {
                "text"
            } else {
                "pcm16le"
            }
            .into(),
        ];
        plan.inputs[0].mime = None;
        if media == RoutineMedia::Text {
            plan.inputs[0].path = None;
            plan.inputs[0].text = Some(durable_text());
        } else {
            plan.inputs[0].pcm24k_mono = true;
        }
        let (_cancel, canceled) = watch::channel(false);
        fixture.run(request, canceled).await.unwrap();
        {
            let state = fixture.service.state.lock().unwrap();
            assert_eq!(state.streams.len(), 1);
            assert_eq!(state.stream_closes.len(), 1);
            assert_eq!(
                state.stream_closes[0].next_sequence,
                state.stream_chunks.len() as u64
            );
            assert!(state.order.contains(&"finish"));
            if media == RoutineMedia::Text {
                let text: String = state
                    .stream_chunks
                    .iter()
                    .map(|chunk| {
                        let Some(wire::stream_chunk::Payload::Text(text)) = &chunk.payload else {
                            panic!()
                        };
                        assert!(text.text.len() <= 65536);
                        text.text.as_str()
                    })
                    .collect();
                assert_eq!(text, durable_text());
            } else {
                let Some(wire::stream_open::Format::Pcm16le(pcm)) = &state.streams[0].format else {
                    panic!()
                };
                assert_eq!((pcm.sample_rate, pcm.channels), (24000, 1));
                let bytes: Vec<u8> = state
                    .stream_chunks
                    .iter()
                    .flat_map(|chunk| {
                        let Some(wire::stream_chunk::Payload::Audio(bytes)) = &chunk.payload else {
                            panic!()
                        };
                        assert!(bytes.len() <= 65536);
                        bytes.clone()
                    })
                    .collect();
                assert_eq!(bytes, vec![9; 70000]);
            }
        }
        assert!(dir.clean());
        fixture.finish().await;
    }
}
#[tokio::test]
async fn native_v2_parallel_download_negotiated_upload_commit_then_bind() {
    let dir = Scratch::new();
    let fixture = Fixture::new(Mode::Parallel).await;
    let (_cancel, canceled) = watch::channel(false);
    fixture.run(request(&dir), canceled).await.unwrap();
    assert_eq!(fs::read(dir.path("saved.bin")).unwrap(), [1, 2, 3]);
    {
        let state = fixture.service.state.lock().unwrap();
        assert_eq!(state.upload, vec![9; 70000]);
        assert_eq!(state.chunks.len(), 18);
        assert_eq!(state.closes, 1);
        let selection = state.open.as_ref().unwrap().selection.as_ref().unwrap();
        assert_eq!(selection.parameters.as_ref().unwrap().json, b" { }\n");
        assert!(!format!("{:?}", state.open).contains(&dir.0.to_string_lossy().to_string()));
        assert!(
            state.order.iter().position(|s| *s == "read").unwrap()
                < state.order.iter().position(|s| *s == "commit").unwrap()
        );
        assert_eq!(
            &state.order[state.order.len() - 3..],
            &["commit", "bind", "finish"]
        );
    }
    assert!(dir.clean());
    fixture.finish().await;
}
#[tokio::test]
async fn native_v2_bad_digest_length_unclean_eof_and_absolute_expiry_remove_partials() {
    for mode in [
        Mode::BadDigest,
        Mode::Short,
        Mode::TrailingError,
        Mode::Expired,
    ] {
        let dir = Scratch::new();
        let fixture = Fixture::new(mode).await;
        let (_cancel, canceled) = watch::channel(false);
        assert!(
            timeout(Duration::from_secs(2), fixture.run(request(&dir), canceled))
                .await
                .expect("failure and event-pinned expiry must stay bounded")
                .is_err()
        );
        assert!(!std::path::Path::new(&dir.path("saved.bin")).exists());
        assert!(dir.clean());
        assert_eq!(fixture.service.state.lock().unwrap().closes, 1);
        fixture.finish().await;
    }
}
#[tokio::test]
async fn native_v2_ambiguous_append_is_not_retried_and_upload_is_canceled() {
    let dir = Scratch::new();
    let fixture = Fixture::new(Mode::LostAppend).await;
    let (_cancel, canceled) = watch::channel(false);
    assert!(fixture.run(request(&dir), canceled).await.is_err());
    {
        let state = fixture.service.state.lock().unwrap();
        assert_eq!(state.chunks.len(), 1);
        assert!(state.order.contains(&"cancel-upload"));
        assert!(!state.committed);
    }
    assert!(dir.clean());
    fixture.finish().await;
}
#[tokio::test]
async fn native_v2_independent_cancel_unblocks_producer_and_joins_reader() {
    let dir = Scratch::new();
    let fixture = Fixture::new(Mode::BlockedAppend).await;
    let (cancel, canceled) = watch::channel(false);
    let signal = async {
        loop {
            if !fixture.service.state.lock().unwrap().chunks.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        cancel.send_replace(true);
    };
    let (result, ()) = tokio::join!(fixture.run(request(&dir), canceled), signal);
    assert!(result.is_err());
    assert!(fixture.service.state.lock().unwrap().canceled);
    assert!(!std::path::Path::new(&dir.path("saved.bin")).exists());
    assert!(dir.clean());
    fixture.finish().await;
}
#[tokio::test]
async fn native_v2_terminal_without_clean_eof_cannot_publish_a_save() {
    let dir = Scratch::new();
    let fixture = Fixture::new(Mode::NoEof).await;
    let (cancel, canceled) = watch::channel(false);
    let signal = async {
        while !fixture.service.state.lock().unwrap().terminal {
            tokio::task::yield_now().await;
        }
        cancel.send_replace(true);
    };
    let (result, ()) = tokio::join!(fixture.run(request(&dir), canceled), signal);
    assert!(result.is_err());
    assert!(!std::path::Path::new(&dir.path("saved.bin")).exists());
    assert!(dir.clean());
    fixture.finish().await;
}
#[tokio::test]
async fn native_v2_generation_change_during_upload_cannot_publish_or_retry() {
    let dir = Scratch::new();
    let fixture = Fixture::new(Mode::BlockedAppend).await;
    let (_cancel, canceled) = watch::channel(false);
    let signal = async {
        while fixture.service.state.lock().unwrap().chunks.is_empty() {
            tokio::task::yield_now().await;
        }
        let _lease = fixture.credentials.clear_bearer_token();
    };
    let (result, ()) = tokio::join!(fixture.run(request(&dir), canceled), signal);
    assert!(result.is_err());
    assert!(!std::path::Path::new(&dir.path("saved.bin")).exists());
    assert!(dir.clean());
    assert_eq!(fixture.service.state.lock().unwrap().chunks.len(), 1);
    fixture.service.state.lock().unwrap().canceled = true;
    fixture.service.wake.notify_waiters();
    fixture.finish().await;
}
#[tokio::test]
async fn native_v2_durable_text_uses_utf8_byte_offsets_and_saves_full_not_preview() {
    let dir = Scratch::new();
    let fixture = Fixture::new(Mode::Parallel).await;
    let text = durable_text();
    let descriptor = RoutineTextResult {
        id: "text".into(),
        name: "result".into(),
        sha256: hex(&digest(text.as_bytes())),
        utf8_bytes: RoutineRevision(text.len() as u64),
        encoded_charge: RoutineRevision(100000),
        helper_id: None,
    };
    let (_cancel, canceled) = watch::channel(false);
    let (preview, truncated) = fixture
        .run(
            MediaRequest::Text {
                id: 2,
                invocation: "invocation".into(),
                descriptors: vec![descriptor],
                selected: "text".into(),
                destination: Some(dir.path("text.txt")),
            },
            canceled,
        )
        .await
        .unwrap();
    assert!(truncated);
    assert!(preview.len() <= PREVIEW);
    assert!(preview.starts_with('😀'));
    assert_eq!(fs::read_to_string(dir.path("text.txt")).unwrap(), text);
    assert_eq!(
        fixture.service.state.lock().unwrap().text_offsets,
        [0, 2, 65538]
    );
    assert!(dir.clean());
    fixture.finish().await;
}
