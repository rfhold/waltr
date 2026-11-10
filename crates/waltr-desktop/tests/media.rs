//! Finite native-handle fixtures and fake v2 service. No picker, file writes, or device effects.
use futures_util::{Stream, StreamExt, stream};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    pin::Pin,
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use waltr_client::{
    NativeGrpcTransport, RoutineInteractionTransport,
    interaction::{BlobReader, MediaTransfers, wire as w},
};
use waltr_desktop::{
    host::{Error, Observer},
    interaction::{Consumer, Progress},
    media::{self, BinaryInput, Frame, OutputSink},
    typing::{Guard, LocalFuture},
};
#[allow(
    unused_imports,
    unused_macros,
    reason = "shares catalog fixtures but has a media-capable service"
)]
mod support;

type Responses = Pin<Box<dyn Stream<Item = Result<w::InteractResponse, Status>> + Send>>;
#[derive(Default)]
struct State {
    accepted: bool,
    declarations: BTreeMap<String, w::BinaryInputDeclaration>,
    uploads: BTreeMap<String, Vec<u8>>,
    chunks: Vec<usize>,
    log: Vec<String>,
}
#[derive(Clone)]
struct Fake {
    mode: &'static str,
    state: Arc<Mutex<State>>,
}
fn event(event: w::output_event::Event) -> Result<w::InteractResponse, Status> {
    Ok(w::InteractResponse {
        output: Some(w::OutputEvent { event: Some(event) }),
    })
}
fn terminal(status: w::InvocationStatus) -> Result<w::InteractResponse, Status> {
    event(w::output_event::Event::Terminal(w::InteractionTerminal {
        status: status as i32,
        cleanup_complete: true,
        ..Default::default()
    }))
}
fn blob(
    id: &str,
    declaration: &w::BinaryInputDeclaration,
    size: u64,
    output: bool,
) -> w::BlobDescriptor {
    w::BlobDescriptor {
        blob_id: id.into(),
        metadata: Some(w::BlobMetadata {
            display_name: declaration.display_name.clone(),
            content_type: declaration.content_type.clone(),
            bytes: size,
            sha256: vec![1; 32],
            image: if declaration.content_type == "image/png" {
                Some(w::ImageInfo {
                    format: w::ImageFormat::Png as i32,
                    width: 1,
                    height: 1,
                })
            } else {
                None
            },
        }),
        remaining_validity_ms: output.then_some(900000),
    }
}
fn declaration(size: u64, image: bool) -> w::BinaryInputDeclaration {
    w::BinaryInputDeclaration {
        display_name: if image {
            "selected.png"
        } else {
            "selected.bin"
        }
        .into(),
        content_type: if image {
            "image/png"
        } else {
            "application/octet-stream"
        }
        .into(),
        declared_bytes: Some(size),
        expected_sha256: None,
        purpose: Some(if image {
            w::binary_input_declaration::Purpose::Image(w::ImageFormat::Png as i32)
        } else {
            w::binary_input_declaration::Purpose::File(w::FilePurpose {})
        }),
    }
}
impl Fake {
    async fn interact(
        &self,
        request: Request<tonic::Streaming<w::InteractRequest>>,
    ) -> Result<Response<Responses>, Status> {
        let mut input = request.into_inner();
        let Some(w::interact_request::Frame::Open(open)) = input.message().await?.unwrap().frame
        else {
            panic!("open");
        };
        assert_eq!(open.selection.unwrap().revision, "revision");
        {
            let mut state = self.state.lock().unwrap();
            state.log.push("open".into());
            for declaration in open.inputs {
                if let Some(w::input_declaration::Format::Binary(binary)) = declaration.format {
                    state.declarations.insert(declaration.port, binary);
                }
            }
        }
        let fake = self.clone();
        let prefix = stream::once(async move {
            if fake.mode == "gate" {
                assert!(
                    tokio::time::timeout(Duration::from_millis(20), input.message())
                        .await
                        .is_err()
                );
                assert!(fake.state.lock().unwrap().uploads.is_empty());
            }
            if fake.mode == "pending" {
                futures_util::future::pending::<()>().await;
            }
            fake.state.lock().unwrap().accepted = true;
            let accepted = event(w::output_event::Event::Accepted(w::InteractionAccepted {
                invocation_id: "invocation".into(),
                revision: "revision".into(),
                limits: Some(support::limits()),
            }));
            let mut prefix = vec![accepted];
            if fake.mode == "capture" {
                prefix.extend([
                    event(w::output_event::Event::Open(w::StreamOpen {
                        port: "clean-text".into(),
                        stream_id: "text".into(),
                        format: Some(w::stream_open::Format::Text(w::TextFormat {})),
                    })),
                    event(w::output_event::Event::Chunk(w::StreamChunk {
                        stream_id: "text".into(),
                        sequence: 0,
                        payload: Some(w::stream_chunk::Payload::Text(w::TextRevision {
                            segment_id: "one".into(),
                            mutation: 1,
                            text: "selected".into(),
                            committed: true,
                        })),
                    })),
                    event(w::output_event::Event::Close(w::StreamClose {
                        stream_id: "text".into(),
                        next_sequence: 1,
                    })),
                ]);
            }
            if fake.mode.starts_with("output") || fake.mode == "sink-block" {
                prefix.push(event(w::output_event::Event::Blob(w::BlobOutput {
                    port: "download".into(),
                    blob: Some(blob("output", &declaration(3, false), 3, true)),
                    output_grant: Some(w::BinaryOutputCredentials {
                        invocation_id: "invocation".into(),
                        port: "download".into(),
                        blob_id: "output".into(),
                        generation: "generation".into(),
                        secret: vec![1; 32],
                    }),
                })));
            }
            if fake.mode == "early" {
                prefix.push(terminal(w::InvocationStatus::Succeeded));
                return Box::pin(stream::iter(prefix)) as Responses;
            }
            let tail = stream::once(async move {
                while let Some(message) = input.message().await? {
                    let Some(w::interact_request::Frame::Input(input)) = message.frame else {
                        panic!("input");
                    };
                    match input.event.unwrap() {
                        w::input_event::Event::Blob(blob) => {
                            let mut state = fake.state.lock().unwrap();
                            assert!(state.log.contains(&format!("commit:{}", blob.port)));
                            state.log.push(format!("blob:{}", blob.port));
                        }
                        w::input_event::Event::Finish(_) => {
                            fake.state.lock().unwrap().log.push("finish".into());
                            return terminal(w::InvocationStatus::Succeeded);
                        }
                        w::input_event::Event::Cancel(_) => {
                            fake.state.lock().unwrap().log.push("cancel".into());
                            return terminal(w::InvocationStatus::Interrupted);
                        }
                        _ => (),
                    }
                }
                Err(Status::cancelled("input ended"))
            });
            Box::pin(stream::iter(prefix).chain(tail)) as Responses
        })
        .flatten();
        Ok(Response::new(Box::pin(prefix)))
    }
    async fn begin_blob_upload(
        &self,
        r: Request<w::BeginBlobUploadRequest>,
    ) -> Result<Response<w::BeginBlobUploadResponse>, Status> {
        let r = r.into_inner();
        {
            let mut state = self.state.lock().unwrap();
            assert!(state.accepted);
            let declared = state.declarations.get(&r.port).unwrap();
            assert_eq!(declared.declared_bytes, r.declared_bytes);
            assert_eq!(declared.display_name, r.display_name);
            state.log.push(format!("begin:{}", r.port));
            state.uploads.insert(r.port.clone(), Vec::new());
        }
        if self.mode == "begin-fail" {
            return Err(Status::unavailable("uncertain begin"));
        }
        if self.mode == "begin-block" {
            futures_util::future::pending::<()>().await;
        }
        Ok(Response::new(w::BeginBlobUploadResponse {
            upload_id: r.port,
            chunk_bytes: 7,
        }))
    }
    async fn append_blob_upload(
        &self,
        r: Request<w::AppendBlobUploadRequest>,
    ) -> Result<Response<w::AppendBlobUploadResponse>, Status> {
        let r = r.into_inner();
        {
            let mut state = self.state.lock().unwrap();
            assert!(r.data.len() <= 7);
            state.chunks.push(r.data.len());
            let upload = state.uploads.get_mut(&r.upload_id).unwrap();
            assert_eq!(upload.len() as u64, r.offset);
            upload.extend(&r.data);
        }
        if self.mode == "append-fail" {
            return Err(Status::unavailable("uncertain append"));
        }
        if self.mode == "append-block" {
            futures_util::future::pending::<()>().await;
        }
        Ok(Response::new(w::AppendBlobUploadResponse {
            accepted_sequence: r.sequence,
            next_offset: r.offset + r.data.len() as u64,
        }))
    }
    async fn commit_blob_upload(
        &self,
        r: Request<w::CommitBlobUploadRequest>,
    ) -> Result<Response<w::CommitBlobUploadResponse>, Status> {
        let r = r.into_inner();
        let descriptor = {
            let mut state = self.state.lock().unwrap();
            assert_eq!(r.bytes, state.uploads[&r.upload_id].len() as u64);
            state.log.push(format!("commit:{}", r.upload_id));
            blob(
                &r.upload_id,
                &state.declarations[&r.upload_id],
                r.bytes,
                false,
            )
        };
        if self.mode == "commit-fail" {
            return Err(Status::unavailable("uncertain commit"));
        }
        if self.mode == "commit-block" {
            futures_util::future::pending::<()>().await;
        }
        if self.mode == "image-rejected" {
            return Err(Status::invalid_argument("quarantine rejected image"));
        }
        Ok(Response::new(w::CommitBlobUploadResponse {
            blob: Some(descriptor),
        }))
    }
    async fn cancel_blob_upload(
        &self,
        _: Request<w::CancelBlobUploadRequest>,
    ) -> Result<Response<w::CancelBlobUploadResponse>, Status> {
        self.state.lock().unwrap().log.push("cancel-upload".into());
        Ok(Response::new(w::CancelBlobUploadResponse {}))
    }
    async fn open_blob_read(
        &self,
        r: Request<w::OpenBlobReadRequest>,
    ) -> Result<Response<w::OpenBlobReadResponse>, Status> {
        assert!(matches!(
            r.into_inner().access,
            Some(w::open_blob_read_request::Access::OutputGrant(_))
        ));
        self.state.lock().unwrap().log.push("open-reader".into());
        Ok(Response::new(w::OpenBlobReadResponse {
            blob: Some(blob("output", &declaration(3, false), 3, true)),
            credentials: Some(w::BlobReaderCredentials {
                invocation_id: "invocation".into(),
                reader_id: "reader".into(),
                generation: "generation".into(),
                secret: vec![2; 32],
            }),
        }))
    }
    async fn read_blob(
        &self,
        r: Request<w::ReadBlobRequest>,
    ) -> Result<
        Response<Pin<Box<dyn Stream<Item = Result<w::ReadBlobResponse, Status>> + Send>>>,
        Status,
    > {
        assert_eq!(r.into_inner().max_bytes, 65536);
        let mut chunks = vec![Ok(w::ReadBlobResponse {
            sequence: 0,
            data: vec![1, 2],
            eof: false,
        })];
        if self.mode == "output-fail" {
            chunks.push(Err(Status::data_loss("read failed")));
        } else if self.mode != "output-short" {
            chunks.push(Ok(w::ReadBlobResponse {
                sequence: 1,
                data: vec![3],
                eof: true,
            }));
        }
        Ok(Response::new(Box::pin(stream::iter(chunks))))
    }
    async fn close_blob_read(
        &self,
        _: Request<w::CloseBlobReadRequest>,
    ) -> Result<Response<w::CloseBlobReadResponse>, Status> {
        self.state.lock().unwrap().log.push("close-reader".into());
        Ok(Response::new(w::CloseBlobReadResponse {}))
    }
}

macro_rules! service {
    ($($method:ident: $request:ident => $response:ident),* $(,)?) => {
        #[tonic::async_trait]
        impl w::routine_service_server::RoutineService for Fake {
            type InteractStream = Responses;
            async fn interact(&self, r: Request<tonic::Streaming<w::InteractRequest>>) -> Result<Response<Responses>, Status> { self.interact(r).await }
            type WatchDefinitionsStream = Pin<Box<dyn Stream<Item=Result<w::WatchDefinitionsResponse, Status>> + Send>>;
            async fn watch_definitions(&self, _: Request<w::WatchDefinitionsRequest>) -> Result<Response<Self::WatchDefinitionsStream>, Status> { Err(Status::unimplemented("unused")) }
            type WatchInvocationsStream = Pin<Box<dyn Stream<Item=Result<w::WatchInvocationsResponse, Status>> + Send>>;
            async fn watch_invocations(&self, _: Request<w::WatchInvocationsRequest>) -> Result<Response<Self::WatchInvocationsStream>, Status> { Err(Status::unimplemented("unused")) }
            type WatchLiveOutputStream = Pin<Box<dyn Stream<Item=Result<w::WatchLiveOutputResponse, Status>> + Send>>;
            async fn watch_live_output(&self, _: Request<w::WatchLiveOutputRequest>) -> Result<Response<Self::WatchLiveOutputStream>, Status> { Err(Status::unimplemented("unused")) }
            type ReadBlobStream = Pin<Box<dyn Stream<Item=Result<w::ReadBlobResponse, Status>> + Send>>;
            async fn read_blob(&self, r: Request<w::ReadBlobRequest>) -> Result<Response<Self::ReadBlobStream>, Status> { self.read_blob(r).await }
            async fn begin_blob_upload(&self, r: Request<w::BeginBlobUploadRequest>) -> Result<Response<w::BeginBlobUploadResponse>, Status> { self.begin_blob_upload(r).await }
            async fn append_blob_upload(&self, r: Request<w::AppendBlobUploadRequest>) -> Result<Response<w::AppendBlobUploadResponse>, Status> { self.append_blob_upload(r).await }
            async fn commit_blob_upload(&self, r: Request<w::CommitBlobUploadRequest>) -> Result<Response<w::CommitBlobUploadResponse>, Status> { self.commit_blob_upload(r).await }
            async fn cancel_blob_upload(&self, r: Request<w::CancelBlobUploadRequest>) -> Result<Response<w::CancelBlobUploadResponse>, Status> { self.cancel_blob_upload(r).await }
            async fn open_blob_read(&self, r: Request<w::OpenBlobReadRequest>) -> Result<Response<w::OpenBlobReadResponse>, Status> { self.open_blob_read(r).await }
            async fn close_blob_read(&self, r: Request<w::CloseBlobReadRequest>) -> Result<Response<w::CloseBlobReadResponse>, Status> { self.close_blob_read(r).await }
            $(async fn $method(&self, _: Request<w::$request>) -> Result<Response<w::$response>, Status> { Err(Status::unimplemented("unused")) })*
        }
    }
}
service!(
    start_invocation: StartInvocationRequest => StartInvocationResponse,
    list_invocations: ListInvocationsRequest => ListInvocationsResponse,
    get_invocation: GetInvocationRequest => GetInvocationResponse,
    list_operations: ListOperationsRequest => ListOperationsResponse,
    cancel_invocation: CancelInvocationRequest => CancelInvocationResponse,
    read_text_result: ReadTextResultRequest => ReadTextResultResponse,
    open_attachment: OpenAttachmentRequest => OpenAttachmentResponse,
    send_input_batch: SendInputBatchRequest => SendInputBatchResponse,
    renew_attachment: RenewAttachmentRequest => RenewAttachmentResponse,
    close_attachment: CloseAttachmentRequest => CloseAttachmentResponse,
);

struct Server {
    transport: NativeGrpcTransport,
    fake: Fake,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn server(mode: &'static str) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = stream::unfold(listener, |listener| async {
        Some((listener.accept().await.map(|v| v.0), listener))
    });
    let fake = Fake {
        mode,
        state: Arc::default(),
    };
    let service = w::routine_service_server::RoutineServiceServer::new(fake.clone());
    let task = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    Server {
        transport: NativeGrpcTransport::new(format!("http://{addr}")),
        fake,
        task,
    }
}
fn invocation(image: bool, output: bool) -> waltr_desktop::interaction::Invocation {
    let mut invocation = support::invocation("finite");
    invocation.limits.definition.ports.clear();
    invocation
        .limits
        .definition
        .ports
        .push(waltr_client::RoutinePort {
            name: "file".into(),
            direction: waltr_client::RoutineDirection::Input,
            media: if image {
                waltr_client::RoutineMedia::Image
            } else {
                waltr_client::RoutineMedia::File
            },
            required: true,
            max_bytes: waltr_client::RoutineRevision(media::FILE_BYTES),
            formats: vec![
                if image {
                    "image/png"
                } else {
                    "application/octet-stream"
                }
                .into(),
            ],
        });
    if output {
        invocation
            .limits
            .definition
            .ports
            .push(waltr_client::RoutinePort {
                name: "download".into(),
                direction: waltr_client::RoutineDirection::Output,
                media: waltr_client::RoutineMedia::File,
                required: true,
                max_bytes: waltr_client::RoutineRevision(65536),
                formats: vec!["application/octet-stream".into()],
            });
    }
    invocation
}

#[derive(Default)]
struct SourceState {
    bytes: Vec<u8>,
    offset: usize,
    reads: Vec<usize>,
    finished: Option<bool>,
    joined: bool,
    worker_exited: Arc<std::sync::atomic::AtomicBool>,
}
struct Source {
    state: Rc<RefCell<SourceState>>,
    mode: &'static str,
    worker: Option<tokio::task::JoinHandle<()>>,
    stop: CancellationToken,
}
impl media::Source for Source {
    fn read(&mut self, max: usize) -> LocalFuture<'_, Result<Vec<u8>, Error>> {
        Box::pin(async move {
            self.state.borrow_mut().reads.push(max);
            if self.mode == "read-fail" {
                return Err(Error::Interaction);
            }
            if self.mode == "read-block" {
                let stop = self.stop.clone();
                let exited = self.state.borrow().worker_exited.clone();
                self.worker = Some(tokio::spawn(async move {
                    stop.cancelled().await;
                    exited.store(true, std::sync::atomic::Ordering::SeqCst);
                }));
                futures_util::future::pending::<()>().await;
            }
            let mut state = self.state.borrow_mut();
            let end = (state.offset + max).min(state.bytes.len());
            let bytes = state.bytes[state.offset..end].to_vec();
            state.offset = end;
            Ok(bytes)
        })
    }
    fn finish(&mut self, success: bool) -> LocalFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.stop.cancel();
            if let Some(worker) = self.worker.take() {
                worker.await.map_err(|_| Error::Interaction)?;
            }
            self.state.borrow_mut().joined = true;
            self.state.borrow_mut().finished = Some(success);
            Ok(())
        })
    }
}
fn input(
    bytes: Vec<u8>,
    declared: u64,
    image: bool,
    mode: &'static str,
) -> (BinaryInput, Rc<RefCell<SourceState>>) {
    let state = Rc::new(RefCell::new(SourceState {
        bytes,
        ..Default::default()
    }));
    let stop = CancellationToken::new();
    let worker = (mode == "prestarted").then(|| {
        let stop = stop.clone();
        let exited = state.borrow().worker_exited.clone();
        tokio::spawn(async move {
            stop.cancelled().await;
            tokio::task::yield_now().await;
            exited.store(true, std::sync::atomic::Ordering::SeqCst);
        })
    });
    (
        BinaryInput {
            port: "file".into(),
            declaration: declaration(declared, image),
            source: Box::new(Source {
                state: state.clone(),
                mode,
                worker,
                stop,
            }),
        },
        state,
    )
}
#[derive(Default)]
struct SinkState {
    bytes: Vec<u8>,
    reader: Option<BlobReader>,
    finished: Option<bool>,
    blocked: bool,
}
struct Sink {
    transfers: MediaTransfers,
    state: RefCell<SinkState>,
    block: bool,
}
impl OutputSink for Sink {
    fn supports(&self, port: &waltr_client::RoutinePort) -> bool {
        port.media == waltr_client::RoutineMedia::File
    }
    fn write<'a>(
        &'a self,
        frame: Frame<'a>,
        guard: &'a Guard<'_>,
    ) -> LocalFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            if let Frame::Blob { grant, .. } = frame {
                self.state.borrow_mut().reader = Some(
                    self.transfers
                        .read_output(grant)
                        .await
                        .map_err(|_| Error::Interaction)?,
                );
                if self.block {
                    self.state.borrow_mut().blocked = true;
                    futures_util::future::pending::<()>().await;
                }
                loop {
                    let chunk = self
                        .state
                        .borrow_mut()
                        .reader
                        .as_mut()
                        .unwrap()
                        .next()
                        .await;
                    let Some(chunk) = chunk else {
                        break;
                    };
                    let bytes = chunk.map_err(|_| Error::Interaction)?;
                    assert!(bytes.len() <= media::CHUNK_BYTES);
                    guard.check().map_err(|_| Error::Cancelled)?;
                    self.state.borrow_mut().bytes.extend(bytes);
                }
                if !self.state.borrow().reader.as_ref().unwrap().complete() {
                    return Err(Error::Interaction);
                }
            }
            Ok(())
        })
    }
    fn finish<'a>(
        &'a self,
        success: bool,
        guard: &'a Guard<'_>,
    ) -> LocalFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let reader = self.state.borrow_mut().reader.take();
            if let Some(mut reader) = reader {
                reader.close().await.map_err(|_| Error::Interaction)?;
            }
            if !success || guard.check().is_err() {
                self.state.borrow_mut().bytes.clear();
            }
            self.state.borrow_mut().finished = Some(success);
            Ok(())
        })
    }
}
struct ConsumerSink(Option<Sink>);
impl Consumer for ConsumerSink {
    fn sink(&self) -> Option<&dyn OutputSink> {
        self.0.as_ref().map(|s| s as &dyn OutputSink)
    }
    fn progress(&self, _: Progress) {}
    fn output<'a>(
        &'a self,
        _: &'a waltr_client::interaction::InteractionOutput,
        _: &'a str,
        _: Option<&'a str>,
        _: &'a Guard<'_>,
    ) -> LocalFuture<'a, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}
async fn execute(
    mode: &'static str,
    bytes: Vec<u8>,
    declared: u64,
    image: bool,
    source_mode: &'static str,
    output: bool,
) -> (
    Result<media::Output, Error>,
    Rc<RefCell<SourceState>>,
    Arc<Mutex<State>>,
    Option<Sink>,
) {
    let server = server(mode).await;
    let (input, state) = input(bytes, declared, image, source_mode);
    let cancel = CancellationToken::new();
    let generation = CancellationToken::new();
    let guard = Guard {
        cancel: &cancel,
        generation: &generation,
        valid: &|| true,
    };
    let consumer = ConsumerSink(output.then(|| Sink {
        transfers: server.transport.media_transfers(),
        state: RefCell::default(),
        block: false,
    }));
    let result = media::run(
        &server.transport,
        &invocation(image, output),
        &mut [input],
        None,
        &guard,
        &Observer::default(),
        &consumer,
    )
    .await;
    (result, state, server.fake.state.clone(), consumer.0)
}

#[tokio::test]
async fn finite_file_waits_for_accepted_and_uses_negotiated_seven_byte_chunks() {
    let (result, source, state, _) =
        execute("gate", (0..20).collect(), 20, false, "normal", false).await;
    assert!(result.is_ok());
    assert_eq!(source.borrow().reads, [7, 7, 6, 1]);
    assert_eq!(source.borrow().finished, Some(true));
    assert!(source.borrow().joined);
    let state = state.lock().unwrap();
    assert_eq!(state.chunks, [7, 7, 6]);
    assert_eq!(state.uploads["file"], (0..20).collect::<Vec<u8>>());
    assert_eq!(
        state.log,
        ["open", "begin:file", "commit:file", "blob:file", "finish"]
    );
}
#[tokio::test]
async fn zero_length_file_commits_without_empty_append() {
    let (result, source, state, _) = execute("normal", vec![], 0, false, "normal", false).await;
    assert!(result.is_ok());
    assert_eq!(source.borrow().reads, [1]);
    assert!(state.lock().unwrap().chunks.is_empty());
    assert!(state.lock().unwrap().log.contains(&"blob:file".into()));
}
#[tokio::test]
async fn selected_image_is_declared_and_quarantine_commit_precedes_input() {
    let (result, source, state, _) =
        execute("normal", vec![137, 80, 78, 71], 4, true, "normal", false).await;
    assert!(result.is_ok());
    assert_eq!(source.borrow().finished, Some(true));
    assert_eq!(
        state.lock().unwrap().declarations["file"].content_type,
        "image/png"
    );
}
#[tokio::test]
async fn native_size_policy_rejects_oversized_file_and_image_before_rpc() {
    for (size, image) in [
        (media::FILE_BYTES + 1, false),
        (media::IMAGE_BYTES + 1, true),
        (0, true),
    ] {
        let (result, source, state, _) =
            execute("normal", vec![], size, image, "normal", false).await;
        assert!(result.is_err());
        assert!(source.borrow().reads.is_empty());
        assert_eq!(source.borrow().finished, Some(false));
        assert!(state.lock().unwrap().log.is_empty());
    }
}
#[tokio::test]
async fn finite_source_size_change_never_commits_or_retries() {
    for (bytes, size) in [(vec![1, 2], 3), (vec![1, 2, 3], 2)] {
        let (result, source, state, _) =
            execute("normal", bytes, size, false, "normal", false).await;
        assert!(result.is_err());
        assert_eq!(source.borrow().finished, Some(false));
        let state = state.lock().unwrap();
        assert!(!state.log.contains(&"commit:file".into()));
        assert!(state.log.contains(&"cancel-upload".into()));
        assert_eq!(state.log.iter().filter(|s| *s == "begin:file").count(), 1);
    }
}
#[tokio::test]
async fn uncertain_begin_append_and_commit_failures_are_not_retried() {
    for mode in ["begin-fail", "append-fail", "commit-fail"] {
        let (result, source, state, _) =
            execute(mode, vec![1, 2, 3], 3, false, "normal", false).await;
        assert!(result.is_err());
        assert_eq!(source.borrow().finished, Some(false));
        let state = state.lock().unwrap();
        assert_eq!(state.log.iter().filter(|s| *s == "begin:file").count(), 1);
        assert!(state.chunks.len() <= 1);
        assert!(!state.log.contains(&"blob:file".into()));
    }
}
#[tokio::test]
async fn source_read_failure_cancels_upload_and_closes_source() {
    let (result, source, state, _) = execute("normal", vec![1], 1, false, "read-fail", false).await;
    assert!(result.is_err());
    assert_eq!(source.borrow().finished, Some(false));
    assert!(source.borrow().joined);
    assert!(state.lock().unwrap().log.contains(&"cancel-upload".into()));
}
#[tokio::test]
async fn finite_early_output_eof_fails_and_joins_source() {
    let (result, source, _, _) = execute("early", vec![1], 1, false, "read-block", false).await;
    assert!(result.is_err());
    assert_eq!(source.borrow().finished, Some(false));
    assert!(source.borrow().joined);
}
#[tokio::test]
async fn output_grant_stream_is_read_in_typed_chunks_and_closed() {
    let (result, source, state, sink) = execute("output", vec![1], 1, false, "normal", true).await;
    assert!(result.is_ok());
    assert!(source.borrow().joined);
    let sink = sink.unwrap();
    assert_eq!(sink.state.borrow().bytes, [1, 2, 3]);
    assert_eq!(sink.state.borrow().finished, Some(true));
    assert!(state.lock().unwrap().log.contains(&"close-reader".into()));
}
#[tokio::test]
async fn failed_or_short_output_read_closes_reader_without_success() {
    for mode in ["output-fail", "output-short"] {
        let (result, source, state, sink) = execute(mode, vec![1], 1, false, "normal", true).await;
        assert!(result.is_err());
        assert!(source.borrow().joined);
        let sink = sink.unwrap();
        assert_eq!(sink.state.borrow().finished, Some(false));
        assert!(sink.state.borrow().bytes.is_empty());
        assert!(state.lock().unwrap().log.contains(&"close-reader".into()));
    }
}

async fn cancelled(
    mode: &'static str,
    source_mode: &'static str,
    block_sink: bool,
    invalidate: bool,
) {
    let server = server(mode).await;
    let (input, source) = input(vec![1, 2, 3], 3, false, source_mode);
    let cancel = CancellationToken::new();
    let generation = CancellationToken::new();
    let valid = std::cell::Cell::new(true);
    let check = || valid.get();
    let guard = Guard {
        cancel: &cancel,
        generation: &generation,
        valid: &check,
    };
    let consumer = ConsumerSink(block_sink.then(|| Sink {
        transfers: server.transport.media_transfers(),
        state: RefCell::default(),
        block: true,
    }));
    let invocation = invocation(false, block_sink);
    let mut inputs = [input];
    let observer = Observer::default();
    let operation = media::run(
        &server.transport,
        &invocation,
        &mut inputs,
        None,
        &guard,
        &observer,
        &consumer,
    );
    tokio::pin!(operation);
    let wait = async {
        loop {
            let state = server.fake.state.lock().unwrap();
            let ready = match mode {
                "begin-block" => state.log.contains(&"begin:file".into()),
                "append-block" => !state.chunks.is_empty(),
                "commit-block" => state.log.contains(&"commit:file".into()),
                "sink-block" => consumer.0.as_ref().unwrap().state.borrow().blocked,
                _ => !source.borrow().reads.is_empty(),
            };
            drop(state);
            if ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        if invalidate {
            valid.set(false);
        } else {
            generation.cancel();
        }
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(operation, wait)
    })
    .await
    .expect("cancel and join must wake blocked work");
    assert!(matches!(result, Err(Error::Cancelled)));
    assert!(source.borrow().joined);
    assert_eq!(source.borrow().finished, Some(false));
    if let Some(sink) = &consumer.0 {
        assert_eq!(sink.state.borrow().finished, Some(false));
        assert!(
            server
                .fake
                .state
                .lock()
                .unwrap()
                .log
                .contains(&"close-reader".into())
        );
    }
}
#[tokio::test]
async fn generation_change_wakes_blocked_begin_append_and_commit() {
    for mode in ["begin-block", "append-block", "commit-block"] {
        cancelled(mode, "normal", false, false).await;
    }
}
#[tokio::test]
async fn credential_guard_change_wakes_blocked_source_and_joins_producer() {
    cancelled("normal", "read-block", false, true).await;
}
#[tokio::test]
async fn sink_backpressure_cancellation_closes_retained_reader_and_discards() {
    cancelled("sink-block", "normal", true, false).await;
}

#[tokio::test]
async fn finite_acceptance_deadline_is_twenty_seconds_and_false_drain_is_not_proof() {
    use waltr_desktop::host::{Diagnostic, RpcFailure};
    let server = server("pending").await;
    let (input, source) = input(vec![1], 1, false, "normal");
    let cancel = CancellationToken::new();
    let guard = Guard {
        cancel: &cancel,
        generation: &cancel,
        valid: &|| true,
    };
    let diagnostics = Arc::new(Mutex::new(Vec::new()));
    let events = diagnostics.clone();
    let observer = Observer::new(move |event| events.lock().unwrap().push(event));
    let invocation = invocation(false, false);
    let mut inputs = [input];
    let consumer = ConsumerSink(None);
    let operation = media::run(
        &server.transport,
        &invocation,
        &mut inputs,
        None,
        &guard,
        &observer,
        &consumer,
    );
    let drive = async {
        while server.fake.state.lock().unwrap().log.is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(21)).await;
    };
    let (result, ()) = tokio::join!(operation, drive);
    tokio::time::resume();
    assert!(result.is_err());
    assert!(source.borrow().reads.is_empty());
    assert!(source.borrow().joined);
    let events = diagnostics.lock().unwrap();
    assert!(events.contains(&Diagnostic::RpcFailure(RpcFailure::AcceptanceTimeout)));
    assert!(events.contains(&Diagnostic::SourceCleanup(Ok(()))));
    assert!(events.contains(&Diagnostic::RemoteCleanup(false)));
}

#[tokio::test]
async fn image_quarantine_rejection_never_sends_blob_input() {
    let (result, source, state, _) =
        execute("image-rejected", vec![1, 2, 3], 3, true, "normal", false).await;
    assert!(result.is_err());
    assert_eq!(source.borrow().finished, Some(false));
    assert!(!state.lock().unwrap().log.contains(&"blob:file".into()));
}
#[tokio::test]
async fn absent_output_capability_rejects_before_admission_and_closes_input() {
    let server = server("output").await;
    let (input, source) = input(vec![1], 1, false, "normal");
    let cancel = CancellationToken::new();
    let guard = Guard {
        cancel: &cancel,
        generation: &cancel,
        valid: &|| true,
    };
    let result = media::run(
        &server.transport,
        &invocation(false, true),
        &mut [input],
        None,
        &guard,
        &Observer::default(),
        &ConsumerSink(None),
    )
    .await;
    assert!(matches!(result, Err(Error::Unsupported)));
    assert_eq!(source.borrow().finished, Some(false));
    assert!(server.fake.state.lock().unwrap().log.is_empty());
}
#[tokio::test]
async fn descriptor_with_no_input_uses_structured_selection_and_normal_finish() {
    let server = server("normal").await;
    let mut invocation = invocation(false, false);
    invocation.limits.definition.ports.clear();
    let cancel = CancellationToken::new();
    let guard = Guard {
        cancel: &cancel,
        generation: &cancel,
        valid: &|| true,
    };
    let result = media::run(
        &server.transport,
        &invocation,
        &mut [],
        None,
        &guard,
        &Observer::default(),
        &ConsumerSink(None),
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(server.fake.state.lock().unwrap().log, ["open", "finish"]);
}
#[tokio::test]
async fn upload_backpressure_never_reads_a_second_chunk_before_acknowledgement() {
    let server = server("append-block").await;
    let (input, source) = input(vec![1; 20], 20, false, "normal");
    let cancel = CancellationToken::new();
    let guard = Guard {
        cancel: &cancel,
        generation: &cancel,
        valid: &|| true,
    };
    let invocation = invocation(false, false);
    let mut inputs = [input];
    let observer = Observer::default();
    let consumer = ConsumerSink(None);
    let operation = media::run(
        &server.transport,
        &invocation,
        &mut inputs,
        None,
        &guard,
        &observer,
        &consumer,
    );
    let drive = async {
        while server.fake.state.lock().unwrap().chunks.is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(source.borrow().reads, [7]);
        cancel.cancel();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(operation, drive)
    })
    .await
    .unwrap();
    assert!(matches!(result, Err(Error::Cancelled)));
    assert!(source.borrow().joined);
}

struct Capture {
    joined: Arc<std::sync::atomic::AtomicBool>,
    stopped: std::sync::atomic::AtomicBool,
    sent: bool,
    empty: bool,
}
struct CaptureFactory {
    joined: Arc<std::sync::atomic::AtomicBool>,
    empty: bool,
}
impl waltr_desktop::capture::Factory for CaptureFactory {
    fn open(
        &self,
        _: waltr_audio_io::Config,
    ) -> Result<Box<dyn waltr_desktop::capture::Source>, waltr_desktop::capture::Error> {
        Ok(Box::new(Capture {
            joined: self.joined.clone(),
            stopped: false.into(),
            sent: false,
            empty: self.empty,
        }))
    }
}
impl waltr_desktop::capture::Source for Capture {
    fn frame(&mut self) -> Option<Vec<u8>> {
        if self.sent || self.empty {
            None
        } else {
            self.sent = true;
            Some(vec![1, 0])
        }
    }
    fn terminal(&self) -> waltr_audio_io::Terminal {
        if self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
            waltr_audio_io::Terminal::Completed
        } else {
            waltr_audio_io::Terminal::Running
        }
    }
    fn stop(&self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    fn shutdown(self: Box<Self>) {
        self.joined.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}
#[tokio::test]
async fn capture_plus_finite_input_joins_both_and_preserves_empty_capture_no_rpc() {
    for image in [false, true] {
        for empty in [false, true] {
            let server = server("capture").await;
            let binary_port = invocation(image, false).limits.definition.ports.remove(0);
            let mut invocation = support::invocation("capture");
            invocation.limits.definition.ports.push(binary_port);
            let (input, source) = input(vec![1, 2, 3], 3, image, "normal");
            let joined = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let release = CancellationToken::new();
            release.cancel();
            let cancel = CancellationToken::new();
            let guard = Guard {
                cancel: &cancel,
                generation: &cancel,
                valid: &|| true,
            };
            let audio = waltr_desktop::config::Audio {
                device: waltr_desktop::config::DeviceSelector::Default,
                sample_rate_hz: 24000,
                channels: 1,
            };
            let result = waltr_desktop::interaction::run_with_inputs(
                &server.transport,
                Arc::new(CaptureFactory {
                    joined: joined.clone(),
                    empty,
                }),
                &audio,
                waltr_desktop::interaction::Selection {
                    invocation: &invocation,
                    input: "audio",
                    output: "clean-text",
                    max_capture_seconds: None,
                },
                &mut [input],
                &release,
                &guard,
                &Observer::default(),
                &ConsumerSink(None),
            )
            .await
            .unwrap();
            assert!(joined.load(std::sync::atomic::Ordering::SeqCst));
            assert!(source.borrow().joined);
            if empty {
                assert!(result.is_none());
                assert_eq!(source.borrow().finished, Some(false));
                assert!(server.fake.state.lock().unwrap().log.is_empty());
            } else {
                assert_eq!(result.unwrap().text, "selected");
                assert_eq!(source.borrow().finished, Some(true));
                assert!(
                    server
                        .fake
                        .state
                        .lock()
                        .unwrap()
                        .log
                        .contains(&"blob:file".into())
                );
            }
        }
    }
}

struct CountedTransport<'a> {
    inner: &'a NativeGrpcTransport,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}
impl RoutineInteractionTransport for CountedTransport<'_> {
    fn interact(
        &self,
        open: w::InteractionOpen,
        limits: waltr_client::interaction::InteractionLimits,
    ) -> impl std::future::Future<
        Output = Result<waltr_client::interaction::InteractionSession, waltr_client::ClientError>,
    > + Send {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.interact(open, limits)
    }
    fn open_attachment(
        &self,
        open: w::InteractionOpen,
        limits: waltr_client::interaction::InteractionLimits,
    ) -> impl std::future::Future<
        Output = Result<waltr_client::interaction::AttachmentSession, waltr_client::ClientError>,
    > {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.open_attachment(open, limits)
    }
    fn media_transfers(&self) -> MediaTransfers {
        self.inner.media_transfers()
    }
}
struct CountedCapture {
    opens: Arc<std::sync::atomic::AtomicUsize>,
    inner: CaptureFactory,
}
impl waltr_desktop::capture::Factory for CountedCapture {
    fn open(
        &self,
        config: waltr_audio_io::Config,
    ) -> Result<Box<dyn waltr_desktop::capture::Source>, waltr_desktop::capture::Error> {
        self.opens.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        waltr_desktop::capture::Factory::open(&self.inner, config)
    }
}

async fn invalid_binary_preflight(capture: bool) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use waltr_desktop::host::Diagnostic;
    for case in ["jpeg", "name", "digest", "purpose", "duplicate"] {
        let server = server("capture").await;
        let calls = Arc::new(AtomicUsize::new(0));
        let transport = CountedTransport {
            inner: &server.transport,
            calls: calls.clone(),
        };
        let mut invocation = invocation(true, false);
        if capture {
            let binary = invocation.limits.definition.ports.remove(0);
            invocation = support::invocation("capture");
            invocation.limits.definition.ports.push(binary);
        }
        let (mut selected, first) = input(vec![1], 1, true, "prestarted");
        match case {
            "jpeg" => {
                selected.declaration.content_type = "image/jpeg".into();
                selected.declaration.purpose = Some(w::binary_input_declaration::Purpose::Image(
                    w::ImageFormat::Jpeg as i32,
                ));
            }
            "name" => selected.declaration.display_name = "not/a/local/path.png".into(),
            "digest" => selected.declaration.expected_sha256 = Some(vec![1; 31]),
            "purpose" => {
                selected.declaration.purpose = Some(w::binary_input_declaration::Purpose::Image(
                    w::ImageFormat::Unspecified as i32,
                ))
            }
            "duplicate" => (),
            _ => unreachable!(),
        }
        let (mut additional, second) = input(vec![2], 1, true, "prestarted");
        if case != "duplicate" {
            additional.port = "other".into();
            let mut port = invocation
                .limits
                .definition
                .ports
                .iter()
                .find(|p| p.name == "file")
                .unwrap()
                .clone();
            port.name = "other".into();
            invocation.limits.definition.ports.push(port);
        }
        let mut inputs = [selected, additional];
        let cancel = CancellationToken::new();
        let guard = Guard {
            cancel: &cancel,
            generation: &cancel,
            valid: &|| true,
        };
        let opens = Arc::new(AtomicUsize::new(0));
        let diagnostics = Arc::new(Mutex::new(Vec::new()));
        let events = diagnostics.clone();
        let observer = Observer::new(move |event| events.lock().unwrap().push(event));
        let result = if capture {
            let audio = waltr_desktop::config::Audio {
                device: waltr_desktop::config::DeviceSelector::Default,
                sample_rate_hz: 24000,
                channels: 1,
            };
            waltr_desktop::interaction::run_with_inputs(
                &transport,
                Arc::new(CountedCapture {
                    opens: opens.clone(),
                    inner: CaptureFactory {
                        joined: Arc::default(),
                        empty: false,
                    },
                }),
                &audio,
                waltr_desktop::interaction::Selection {
                    invocation: &invocation,
                    input: "audio",
                    output: "clean-text",
                    max_capture_seconds: None,
                },
                &mut inputs,
                &CancellationToken::new(),
                &guard,
                &observer,
                &ConsumerSink(None),
            )
            .await
            .map(|_| ())
        } else {
            media::run(
                &transport,
                &invocation,
                &mut inputs,
                None,
                &guard,
                &observer,
                &ConsumerSink(None),
            )
            .await
            .map(|_| ())
        };
        assert!(matches!(result, Err(Error::Unsupported)), "{case}");
        assert_eq!(opens.load(Ordering::SeqCst), 0, "{case}");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{case}");
        let state = server.fake.state.lock().unwrap();
        assert!(state.log.is_empty(), "{case}");
        assert!(state.uploads.is_empty(), "{case}");
        drop(state);
        for source in [first, second] {
            let state = source.borrow();
            assert!(state.reads.is_empty(), "{case}");
            assert_eq!(state.finished, Some(false), "{case}");
            assert!(
                state.joined && state.worker_exited.load(Ordering::SeqCst),
                "{case}"
            );
        }
        let diagnostics = diagnostics.lock().unwrap();
        assert!(
            diagnostics.contains(&Diagnostic::SourceCleanup(Ok(()))),
            "{case}"
        );
        assert!(
            !diagnostics.iter().any(|event| matches!(
                event,
                Diagnostic::RemoteCleanup(_) | Diagnostic::CaptureCleanup(_)
            )),
            "{case}"
        );
    }
}

#[tokio::test]
async fn mixed_binary_metadata_is_validated_before_device_or_transport_effects() {
    invalid_binary_preflight(true).await;
}
#[tokio::test]
async fn finite_binary_metadata_is_validated_before_transport_effects() {
    invalid_binary_preflight(false).await;
}

#[tokio::test]
async fn capture_request_invalidation_closes_finite_source_without_opening_device() {
    use waltr_desktop::host::{Diagnostic, Stage};
    let server = server("capture").await;
    let (input, source) = input(vec![1], 1, false, "normal");
    let binary_port = invocation(false, false).limits.definition.ports.remove(0);
    let mut invocation = support::invocation("capture");
    invocation.limits.definition.ports.push(binary_port);
    let cancel = CancellationToken::new();
    let invalidate = cancel.clone();
    let observer = Observer::new(move |event| {
        if event == Diagnostic::Stage(Stage::CaptureRequested) {
            invalidate.cancel();
        }
    });
    let guard = Guard {
        cancel: &cancel,
        generation: &cancel,
        valid: &|| true,
    };
    let joined = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let audio = waltr_desktop::config::Audio {
        device: waltr_desktop::config::DeviceSelector::Default,
        sample_rate_hz: 24000,
        channels: 1,
    };
    let result = waltr_desktop::interaction::run_with_inputs(
        &server.transport,
        Arc::new(CaptureFactory {
            joined: joined.clone(),
            empty: false,
        }),
        &audio,
        waltr_desktop::interaction::Selection {
            invocation: &invocation,
            input: "audio",
            output: "clean-text",
            max_capture_seconds: None,
        },
        &mut [input],
        &CancellationToken::new(),
        &guard,
        &observer,
        &ConsumerSink(None),
    )
    .await;
    assert!(matches!(result, Err(Error::Cancelled)));
    assert!(!joined.load(std::sync::atomic::Ordering::SeqCst));
    assert!(source.borrow().joined);
    assert_eq!(source.borrow().finished, Some(false));
    assert!(server.fake.state.lock().unwrap().log.is_empty());
}
