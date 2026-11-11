use super::*;
use crate::native_media_test_support as support;
use futures_util::stream;
use std::sync::{
    Mutex as SyncMutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tonic::{Request, Response, Status as RpcStatus};
use w as wire;
#[path = "raw_download_tests.rs"]
pub(crate) mod raw_download_tests;
type Responses = std::pin::Pin<
    Box<dyn futures_util::Stream<Item = Result<w::InteractResponse, RpcStatus>> + Send>,
>;
type Outputs = std::pin::Pin<
    Box<dyn futures_util::Stream<Item = Result<w::WatchLiveOutputResponse, RpcStatus>> + Send>,
>;
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        Self::create_with(|| {
            std::env::temp_dir().join(format!(
                "waltr-native-media-{}-{:032x}-{}",
                std::process::id(),
                rand::random::<u128>(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ))
        })
    }
    fn create_with(mut path: impl FnMut() -> PathBuf) -> Self {
        for _ in 0..32 {
            let path = path();
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("native media fixture creation failed: {error}"),
            }
        }
        panic!("native media fixture collision limit reached")
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
#[test]
fn fixture_collision_and_parallel_roots_preserve_unowned_directories() {
    let parent = Directory::new();
    let existing = parent.path().join("existing");
    std::fs::create_dir(&existing).unwrap();
    std::fs::write(existing.join("sentinel"), b"preserved").unwrap();
    let mut attempt = 0;
    let owned = Directory::create_with(|| {
        attempt += 1;
        parent
            .path()
            .join(if attempt == 1 { "existing" } else { "owned" })
    });
    assert_eq!(attempt, 2);
    drop(owned);
    assert_eq!(
        std::fs::read(existing.join("sentinel")).unwrap(),
        b"preserved"
    );
    let threads = (0..32)
        .map(|_| std::thread::spawn(Directory::new))
        .collect::<Vec<_>>();
    let roots = threads
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect::<Vec<_>>();
    let unique = roots
        .iter()
        .map(|d| d.path())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(unique.len(), roots.len());
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _removed = std::fs::remove_dir_all(&self.0);
    }
}
#[derive(Default)]
struct FixtureState {
    output:
        Option<tokio::sync::mpsc::UnboundedSender<Result<w::WatchLiveOutputResponse, RpcStatus>>>,
    receiver:
        Option<tokio::sync::mpsc::UnboundedReceiver<Result<w::WatchLiveOutputResponse, RpcStatus>>>,
    declaration: Option<w::BinaryInputDeclaration>,
    bytes: Vec<u8>,
    sequence: u64,
    committed: bool,
    bound: bool,
    absolute: i64,
    checkout_gate: Option<Arc<tokio::sync::Semaphore>>,
}
#[derive(Clone, Default)]
struct Service {
    state: Arc<SyncMutex<FixtureState>>,
    opens: Arc<AtomicUsize>,
    closes: Arc<AtomicUsize>,
    reader_closes: Arc<AtomicUsize>,
    reader_opens: Arc<AtomicUsize>,
    checkout_delay_ms: Arc<AtomicUsize>,
    read_delay_ms: Arc<AtomicUsize>,
    validity_ms: Arc<AtomicUsize>,
    open_delay_ms: Arc<AtomicUsize>,
    accepted_delay_ms: Arc<AtomicUsize>,
    renewals: Arc<AtomicUsize>,
    checkout_error: Arc<AtomicBool>,
    checkout_panic: Arc<AtomicBool>,
    close_error: Arc<AtomicBool>,
    require_bearer: Arc<AtomicBool>,
    authorized_closes: Arc<AtomicUsize>,
}
fn blob() -> w::BlobDescriptor {
    w::BlobDescriptor {
        blob_id: "output-blob".into(),
        metadata: Some(w::BlobMetadata {
            display_name: "result.bin".into(),
            content_type: "application/octet-stream".into(),
            bytes: 100_001,
            sha256: vec![1; 32],
            image: None,
        }),
        remaining_validity_ms: Some(900000),
    }
}
fn emit_blob(service: &Service) {
    let mut descriptor = blob();
    let validity = service.validity_ms.load(Ordering::SeqCst);
    if validity != 0 {
        descriptor.remaining_validity_ms = Some(validity as u64);
    }
    service
        .state
        .lock()
        .unwrap()
        .output
        .as_ref()
        .unwrap()
        .send(output(w::output_event::Event::Blob(w::BlobOutput {
            port: "saved".into(),
            blob: Some(descriptor),
            output_grant: Some(w::BinaryOutputCredentials {
                invocation_id: "invocation".into(),
                port: "saved".into(),
                blob_id: "output-blob".into(),
                generation: "lease-generation".into(),
                secret: b"SECRET-OUTPUT-CANARY-0123456789012".to_vec(),
            }),
        })))
        .unwrap();
}
fn accepted() -> w::InteractionAccepted {
    w::InteractionAccepted {
        invocation_id: "invocation".into(),
        revision: "revision".into(),
        limits: Some(support::limits()),
    }
}
fn output(event: w::output_event::Event) -> Result<w::WatchLiveOutputResponse, RpcStatus> {
    Ok(w::WatchLiveOutputResponse {
        output: Some(w::OutputEvent { event: Some(event) }),
    })
}
impl Service {
    async fn interact(
        &self,
        _: Request<tonic::Streaming<w::InteractRequest>>,
    ) -> Result<Response<Responses>, RpcStatus> {
        Err(RpcStatus::unimplemented("split only"))
    }
    async fn watch_live_output(
        &self,
        _: Request<w::WatchLiveOutputRequest>,
    ) -> Result<Response<Outputs>, RpcStatus> {
        let receiver = self.state.lock().unwrap().receiver.take().unwrap();
        Ok(Response::new(Box::pin(stream::unfold(
            receiver,
            |mut r| async { r.recv().await.map(|v| (v, r)) },
        ))))
    }
    async fn read_blob(
        &self,
        _: Request<w::ReadBlobRequest>,
    ) -> Result<
        Response<<Self as w::routine_service_server::RoutineService>::ReadBlobStream>,
        RpcStatus,
    > {
        let frames = vec![
            Ok(w::ReadBlobResponse {
                data: vec![7; 65536],
                sequence: 0,
                eof: false,
            }),
            Ok(w::ReadBlobResponse {
                data: vec![7; 34465],
                sequence: 1,
                eof: true,
            }),
        ];
        let delay = self.read_delay_ms.load(Ordering::SeqCst);
        Ok(Response::new(Box::pin(stream::unfold(
            (frames.into_iter(), false),
            move |(mut frames, seen)| async move {
                if seen {
                    tokio::time::sleep(std::time::Duration::from_millis(delay as u64)).await;
                }
                frames.next().map(|frame| (frame, (frames, true)))
            },
        ))))
    }
}
support::fake_service!(Service {
    async fn open_attachment(&self, r: Request<w::OpenAttachmentRequest>) -> Result<Response<w::OpenAttachmentResponse>, RpcStatus> {
        tokio::time::sleep(std::time::Duration::from_millis(self.open_delay_ms.load(Ordering::SeqCst) as u64)).await;
        self.opens.fetch_add(1, Ordering::SeqCst);
        let open = r.into_inner().open.unwrap(); assert_eq!(open.selection.as_ref().unwrap().revision, "revision");
        let mut state = self.state.lock().unwrap(); state.declaration = open.inputs.into_iter().find_map(|d| match d.format { Some(w::input_declaration::Format::Binary(b)) => Some(b), _ => None });
        let (send, receiver) = tokio::sync::mpsc::unbounded_channel();
        let accepted_send = send.clone(); let delay = self.accepted_delay_ms.load(Ordering::SeqCst);
        tokio::spawn(async move { tokio::time::sleep(std::time::Duration::from_millis(delay as u64)).await; let _sent = accepted_send.send(output(w::output_event::Event::Accepted(accepted()))); });
        state.output = Some(send); state.receiver = Some(receiver);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
        state.absolute = now + 5_400_000;
        Ok(Response::new(w::OpenAttachmentResponse { admission: Some(w::open_attachment_response::Admission::Accepted(w::AttachmentAdmission { invocation: Some(accepted()), credentials: Some(w::AttachmentCredentials { invocation_id: "invocation".into(), attachment_id: "attachment".into(), generation: "generation".into(), secret: b"SECRET-ATTACHMENT-CANARY-0123456789".to_vec() }), attach_by_ms: now + 10000, lease_expires_ms: now + 30000, absolute_expires_ms: now + 5_400_000 })) }))
    }
    async fn send_input_batch(&self, r: Request<w::SendInputBatchRequest>) -> Result<Response<w::SendInputBatchResponse>, RpcStatus> {
        let r = r.into_inner(); let mut state = self.state.lock().unwrap();
        for event in r.events { match event.event {
            Some(w::input_event::Event::Blob(_)) => { assert!(state.committed); state.bound = true; },
            Some(w::input_event::Event::Finish(_)) => { if state.declaration.is_some() { assert!(state.bound); }
            if let Some(send) = state.output.take() {
                send.send(output(w::output_event::Event::Terminal(w::InteractionTerminal { status: w::InvocationStatus::Succeeded as i32, result: Some(w::StructuredValue { json: b"{}".to_vec() }), cleanup_complete: true, text_results: vec![] }))).unwrap();
            } }, _ => (),
        } }
        Ok(Response::new(w::SendInputBatchResponse { accepted_sequence: r.sequence }))
    }
    async fn close_attachment(&self, _: Request<w::CloseAttachmentRequest>) -> Result<Response<w::CloseAttachmentResponse>, RpcStatus> {
        self.closes.fetch_add(1, Ordering::SeqCst); self.state.lock().unwrap().output.take(); Ok(Response::new(w::CloseAttachmentResponse { status: w::InvocationStatus::Interrupted as i32, cleanup_complete: true }))
    }
    async fn begin_blob_upload(&self, r: Request<w::BeginBlobUploadRequest>) -> Result<Response<w::BeginBlobUploadResponse>, RpcStatus> {
        let r = r.into_inner(); let state = self.state.lock().unwrap(); let d = state.declaration.as_ref().unwrap(); assert_eq!(r.declared_bytes, d.declared_bytes); assert_eq!(r.display_name, d.display_name); assert_eq!(r.content_type, d.content_type);
        Ok(Response::new(w::BeginBlobUploadResponse { upload_id: "upload".into(), chunk_bytes: 4096 }))
    }
    async fn append_blob_upload(&self, r: Request<w::AppendBlobUploadRequest>) -> Result<Response<w::AppendBlobUploadResponse>, RpcStatus> {
        let r = r.into_inner(); let mut state = self.state.lock().unwrap(); assert!(r.data.len() <= 4096); assert_eq!(r.sequence, state.sequence); assert_eq!(r.offset, state.bytes.len() as u64); state.sequence += 1; state.bytes.extend(r.data);
        Ok(Response::new(w::AppendBlobUploadResponse { accepted_sequence: r.sequence, next_offset: state.bytes.len() as u64 }))
    }
    async fn commit_blob_upload(&self, r: Request<w::CommitBlobUploadRequest>) -> Result<Response<w::CommitBlobUploadResponse>, RpcStatus> {
        let r = r.into_inner(); let mut state = self.state.lock().unwrap(); assert_eq!(r.bytes, state.bytes.len() as u64); assert_eq!(r.next_sequence, state.sequence); state.committed = true; let d = state.declaration.as_ref().unwrap();
        Ok(Response::new(w::CommitBlobUploadResponse { blob: Some(w::BlobDescriptor { blob_id: "blob".into(), metadata: Some(w::BlobMetadata { display_name: d.display_name.clone(), content_type: d.content_type.clone(), bytes: r.bytes, sha256: vec![1; 32], image: None }), remaining_validity_ms: None }) }))
    }
    async fn cancel_blob_upload(&self, _: Request<w::CancelBlobUploadRequest>) -> Result<Response<w::CancelBlobUploadResponse>, RpcStatus> { Ok(Response::new(w::CancelBlobUploadResponse {})) }
    async fn open_blob_read(&self, _: Request<w::OpenBlobReadRequest>) -> Result<Response<w::OpenBlobReadResponse>, RpcStatus> {
        self.reader_opens.fetch_add(1, Ordering::SeqCst);
        let gate = self.state.lock().unwrap().checkout_gate.clone();
        if let Some(gate) = gate { gate.acquire().await.unwrap().forget(); }
        assert!(!self.checkout_panic.load(Ordering::SeqCst), "synthetic checkout panic");
        if self.checkout_error.load(Ordering::SeqCst) { return Err(RpcStatus::internal("synthetic checkout error")); }
        tokio::time::sleep(std::time::Duration::from_millis(self.checkout_delay_ms.load(Ordering::SeqCst) as u64)).await;
        Ok(Response::new(w::OpenBlobReadResponse {credentials:Some(w::BlobReaderCredentials {invocation_id:"invocation".into(),reader_id:"reader".into(),generation:"reader-generation".into(),secret:b"SECRET-READER-CANARY-0123456789012".to_vec()}),blob:Some(blob())}))
    }
    async fn close_blob_read(&self, r: Request<w::CloseBlobReadRequest>) -> Result<Response<w::CloseBlobReadResponse>, RpcStatus> {
        self.reader_closes.fetch_add(1, Ordering::SeqCst);
        if self.require_bearer.load(Ordering::SeqCst) {
            if r.metadata().get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer logout-test-canary") { return Err(RpcStatus::unauthenticated("test credential missing")); }
            self.authorized_closes.fetch_add(1, Ordering::SeqCst);
        }
        if self.close_error.load(Ordering::SeqCst) { return Err(RpcStatus::unavailable("synthetic uncertain close")); }
        Ok(Response::new(w::CloseBlobReadResponse {}))
    }
    async fn renew_attachment(&self, _: Request<w::RenewAttachmentRequest>) -> Result<Response<w::RenewAttachmentResponse>, RpcStatus> {
        self.renewals.fetch_add(1, Ordering::SeqCst);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
        Ok(Response::new(w::RenewAttachmentResponse { lease_expires_ms: now + 30000, absolute_expires_ms: self.state.lock().unwrap().absolute }))
    }
};
    start_invocation: StartInvocationRequest => StartInvocationResponse,
    list_invocations: ListInvocationsRequest => ListInvocationsResponse,
    list_operations: ListOperationsRequest => ListOperationsResponse,
    cancel_invocation: CancelInvocationRequest => CancelInvocationResponse,
    get_invocation: GetInvocationRequest => GetInvocationResponse,
    read_text_result: ReadTextResultRequest => ReadTextResultResponse,
);
fn args(operation: &str, fields: Value) -> Args {
    let mut value = fields;
    value["operation"] = json!(operation);
    value["generation"] = json!(1);
    serde_json::from_value(value).unwrap()
}
fn snapshot() -> ClientSnapshot {
    let mut s = support::snapshot("test");
    s.routines.definitions.definitions[0].ports = vec![waltr_client::RoutinePort {
        name: "file".into(),
        direction: RoutineDirection::Input,
        media: RoutineMedia::File,
        required: true,
        max_bytes: waltr_client::RoutineRevision(64 * 1024 * 1024),
        formats: vec!["application/octet-stream".into()],
    }];
    s
}
fn metadata(bytes: u64) -> String {
    json!({"routine":"test","revision":"revision","entrypoint":"dictate","parameters":"{}","correlation":"native-test","sourceSession":null,"inputs":[{"format":"file","port":"file","displayName":"file.bin","contentType":"application/octet-stream","declaredBytes":bytes}],"selectedTextOutput":null}).to_string()
}
fn host(transport: NativeGrpcTransport) -> Host {
    let audio = Arc::new(crate::native_audio::Host::new(
        crate::activity::ActivityTracker::default(),
    ));
    {
        let mut s = audio.lock();
        s.bind(1);
        s.access(true);
    }
    Host::new(
        snapshot(),
        transport,
        NativeCredentialProvider::new(),
        audio,
        1,
    )
}

#[test]
fn playback_support_requires_current_output_pcm_contract() {
    let host = host(NativeGrpcTransport::new("http://127.0.0.1:1"));
    let sink = Sink {
        files: Mutex::new(BTreeMap::new()),
        file_ports: Default::default(),
        playback_ports: vec!["speaker".into()],
        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
        playback_stream: RefCell::new(None),
        transfers: host.transport.media_transfers(),
        readers: Mutex::new(Vec::new()),
        checkout: Mutex::new(None),
        events: Default::default(),
        fence: Fence {
            generation: 1,
            epoch: host.audio.lock().media_epoch,
            current: None,
            audio: host.audio.clone(),
            credentials: host.credentials.clone(),
            revision: host.credentials.credential_revision(),
            cancel: CancellationToken::new(),
        },
        raw: None,
        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
        playback: Mutex::new(None),
    };
    let mut port = waltr_client::RoutinePort {
        name: "speaker".into(),
        direction: RoutineDirection::Output,
        media: RoutineMedia::Audio,
        required: true,
        max_bytes: waltr_client::RoutineRevision(65536),
        formats: vec!["pcm16le-24000-mono".into()],
    };
    assert!(sink.supports(&port));
    for formats in [
        vec![],
        vec!["pcm16le"],
        vec!["pcm16le-48000-mono"],
        vec!["utf8"],
        vec!["pcm16le-24000-mono", "pcm16le"],
    ] {
        port.formats = formats.into_iter().map(String::from).collect();
        assert!(!sink.supports(&port));
    }
    port.formats = vec!["pcm16le-24000-mono".into()];
    port.direction = RoutineDirection::Input;
    assert!(!sink.supports(&port));
    port.direction = RoutineDirection::Output;
    port.media = RoutineMedia::Image;
    assert!(!sink.supports(&port));
    port.media = RoutineMedia::Audio;
    port.name = "unselected".into();
    assert!(!sink.supports(&port));
}

async fn fixture() -> (
    Host,
    Service,
    CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    fixture_with_bearer(false).await
}
async fn fixture_with_bearer(
    bearer: bool,
) -> (
    Host,
    Service,
    CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = stream::unfold(listener, |listener| async {
        Some((listener.accept().await.map(|v| v.0), listener))
    });
    let service = Service::default();
    let server = w::routine_service_server::RoutineServiceServer::new(service.clone());
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    let join = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(server)
            .serve_with_incoming_shutdown(incoming, stopped.cancelled())
            .await
            .unwrap();
    });
    let credentials = NativeCredentialProvider::new();
    if bearer {
        credentials
            .replace_bearer_token("logout-test-canary")
            .unwrap();
        service.require_bearer.store(true, Ordering::SeqCst);
    }
    let transport = if bearer {
        NativeGrpcTransport::with_credential_provider(format!("http://{addr}"), credentials.clone())
    } else {
        NativeGrpcTransport::new(format!("http://{addr}"))
    };
    let mut host = host(transport);
    if bearer {
        host.credentials = credentials;
        host.state.borrow_mut().revision = host.credentials.credential_revision();
    }
    (host, service, stop, join)
}
async fn wait(host: &Host, phase: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while host.state.borrow().status["phase"] != phase {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn finite_native_handle_uses_negotiated_chunks_commits_then_binds_and_joins() {
    tokio::task::LocalSet::new().run_until(async {
        let (host, service, stop, join) = fixture().await; let dir = Directory::new(); let path = dir.path().join("SECRET-PRIVATE-PATH"); let bytes = vec![3; 100_001]; std::fs::write(&path, &bytes).unwrap();
        let input = host.call(args("pickInput", json!({})), Some(Selected::Input { file: File::open(path).unwrap(), public: json!({"port":"file","name":"file.bin","contentType":"application/octet-stream","bytes":bytes.len(),"purpose":"file"}) })).await.unwrap();
        assert!(!input.to_string().contains("SECRET"));
        let id = host.call(args("openNative", json!({"metadata":metadata(bytes.len() as u64),"inputs":[{"port":"file","handle":input["handle"]}],"outputs":[]})), None).await.unwrap().as_u64().unwrap() as u32;
        wait(&host, "live").await;
        host.call(args("finishInput", json!({"id":id})), None).await.unwrap();
        wait(&host, "succeeded").await;
        assert_eq!(service.state.lock().unwrap().bytes, bytes); assert!(host.state.borrow().status["cleanupVerified"].as_bool().unwrap());
        assert_eq!(service.opens.load(Ordering::SeqCst), 1); assert_eq!(service.closes.load(Ordering::SeqCst), 1); assert!(host.state.borrow().inputs.is_empty());
        host.drain().await; stop.cancel(); join.await.unwrap();
    }).await;
}
#[tokio::test]
async fn retirement_rejects_late_picker_results_until_runtime_reconciles() {
    let host = host(NativeGrpcTransport::new("http://127.0.0.1:1"));
    let directory = Directory::new();
    let path = directory.path().join("output");
    host.drain().await;
    assert!(
        host.call(
            args(
                "pickOutput",
                json!({"request":{"port":"saved","suggestedName":"output"}})
            ),
            Some(Selected::Output { path: path.clone() }),
        )
        .await
        .is_err()
    );
    assert!(host.state.borrow().outputs.is_empty());
    assert!(!path.exists());
    host.reconcile(&snapshot()).await;
    host.audio.lock().access(true);
    assert!(
        host.call(
            args(
                "pickOutput",
                json!({"request":{"port":"saved","suggestedName":"output"}})
            ),
            Some(Selected::Output { path: path.clone() }),
        )
        .await
        .is_ok()
    );
    host.drain().await;
    assert!(!path.exists());
}

#[tokio::test]
async fn native_metadata_mismatch_and_missing_handle_are_rejected_before_rpc() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (host, service, stop, join) = fixture().await;
            assert!(
                host.call(
                    args(
                        "openNative",
                        json!({"metadata":metadata(4),"inputs":[],"outputs":[]})
                    ),
                    None
                )
                .await
                .is_err()
            );
            assert_eq!(service.opens.load(Ordering::SeqCst), 0);
            host.drain().await;
            stop.cancel();
            join.await.unwrap();
        })
        .await;
}
#[tokio::test]
async fn native_sources_enforce_chunk_cap_and_owned_partial_deletion_never_removes_existing_file() {
    let dir = Directory::new();
    let path = dir.path().join("output");
    std::fs::write(&path, b"existing").unwrap();
    let mut destination = Destination {
        path: path.clone(),
        port: "file".into(),
        file: None,
        created: false,
        complete: false,
        staging: None,
    };
    destination.discard();
    assert_eq!(std::fs::read(&path).unwrap(), b"existing");
    let mut source = Input {
        file: Some(File::open(&path).unwrap()),
        public: Value::Null,
    };
    assert!(source.read(CHUNK + 1).await.is_err());
    assert_eq!(source.read(CHUNK).await.unwrap(), b"existing");
    source.finish(false).await.unwrap();
    assert!(source.file.is_none());
    let partial = dir.path().join("partial");
    let mut destination = Destination {
        path: partial.clone(),
        port: "file".into(),
        file: None,
        created: false,
        complete: false,
        staging: None,
    };
    destination.create().unwrap();
    let staging = destination.staging.clone().unwrap();
    std::fs::write(&partial, b"replacement").unwrap();
    drop(destination);
    assert_eq!(std::fs::read(&partial).unwrap(), b"replacement");
    assert!(!staging.exists());
}

fn output_only(host: &Host, required: bool) -> String {
    host.state
        .borrow_mut()
        .snapshot
        .routines
        .definitions
        .definitions[0]
        .ports = vec![waltr_client::RoutinePort {
        name: "saved".into(),
        direction: RoutineDirection::Output,
        media: RoutineMedia::File,
        required,
        max_bytes: waltr_client::RoutineRevision(64 * 1024 * 1024),
        formats: vec!["application/octet-stream".into()],
    }];
    let mut m: Value = serde_json::from_str(&metadata(0)).unwrap();
    m["inputs"] = json!([]);
    m.to_string()
}

#[tokio::test]
async fn relevant_catalog_and_credential_changes_cancel_and_join_but_unrelated_snapshots_do_not() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (host, service, stop, join) = fixture().await;
            let metadata = output_only(&host, false);
            host.call(args("openNative", json!({"metadata":metadata})), None)
                .await
                .unwrap();
            wait(&host, "live").await;
            let mut snapshot = host.state.borrow().snapshot.clone();
            snapshot.config.current.as_mut().unwrap().etag = "unrelated".into();
            host.reconcile(&snapshot).await;
            assert!(host.state.borrow().active.is_some());
            assert_eq!(service.closes.load(Ordering::SeqCst), 0);
            snapshot.routines.definitions.definitions[0].routine_revision = "replacement".into();
            host.reconcile(&snapshot).await;
            assert!(host.state.borrow().active.is_none());
            assert_eq!(service.closes.load(Ordering::SeqCst), 1);
            snapshot.routines.definitions.definitions[0].routine_revision = "revision".into();
            host.reconcile(&snapshot).await;
            host.audio.lock().access(true);
            host.call(args("openNative", json!({"metadata":metadata})), None)
                .await
                .unwrap();
            wait(&host, "live").await;
            let _lease = host.credentials.clear_bearer_token();
            host.reconcile(&snapshot).await;
            assert!(host.state.borrow().active.is_none());
            assert_eq!(service.closes.load(Ordering::SeqCst), 2);
            assert_eq!(
                host.audio.activity.phase(std::time::Instant::now()),
                crate::activity::Phase::Idle
            );
            stop.cancel();
            join.await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn f1_retirement_revokes_effects_before_a_blocked_cleanup_join() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (host, service, stop, server) = fixture().await;
            service.checkout_delay_ms.store(100, Ordering::SeqCst);
            let metadata = output_only(&host, true);
            let dir = Directory::new(); let path = dir.path().join("destination");
            let selected = host.call(args("pickOutput", json!({"request":{"port":"saved","suggestedName":"result.bin"}})), Some(Selected::Output { path: path.clone() })).await.unwrap();
            host.call(args("openNative", json!({"metadata":metadata,"outputs":[{"kind":"file","port":"saved","handle":selected["handle"]}]})), None)
                .await
                .unwrap();
            wait(&host, "live").await;
            emit_blob(&service);
            while service.reader_opens.load(Ordering::SeqCst) == 0 { tokio::task::yield_now().await; }
            let current = Arc::new(std::sync::atomic::AtomicU64::new(1));
            let mut fence = host.state.borrow().active.as_ref().unwrap().fence.clone();
            fence.current = Some(current.clone());
            assert!(fence.valid());
            current.store(0, Ordering::Release);
            assert!(!fence.valid());
            current.store(1, Ordering::Release);
            let draining = host.drain();
            tokio::pin!(draining);
            assert!(futures_util::poll!(&mut draining).is_pending());
            assert!(!fence.valid());
            assert!(host.check_input(1).is_err());
            assert!(!path.exists());
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
            assert!(
                host.call(args("openNative", json!({"metadata":metadata})), None)
                    .await
                    .is_err()
            );
            draining.await;
            assert_eq!(service.reader_closes.load(Ordering::SeqCst), 1);
            assert!(!path.exists());
            { let mut audio = host.audio.lock(); audio.bind(2); audio.access(true); }
            host.revoke();
            let chat = crate::dictation::Host::new(host.audio.clone());
            assert!(!chat.access_for(1, false));
            assert!(host.audio.lock().allowed);
            assert_eq!(host.audio.lock().media_epoch, 0);
            stop.cancel();
            server.await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn f2_nonqueued_credits_bound_retained_raw_bytes_and_preserve_cancel_lane() {
    assert!(bounded_metadata(&json!({"metadata":"x".repeat(1024)})).is_ok());
    assert!(bounded_metadata(&json!({"metadata":"x".repeat(IPC_METADATA_BYTES)})).is_err());
    let raw = credit(true, false).unwrap();
    let controls: Vec<_> = (0..8).map(|_| credit(false, false).unwrap()).collect();
    for _ in 0..1000 {
        assert!(credit(true, false).is_err());
        assert!(credit(false, false).is_err());
    }
    let cancel = credit(false, true).unwrap();
    assert!(credit(false, true).is_err());
    // One raw credit retains at most one CHUNK, regardless of caller cancellation.
    let (send, receive) = oneshot::channel::<()>();
    let worker = tokio::spawn(async move {
        let _raw = raw;
        let _joined = receive.await;
    });
    assert!(credit(true, false).is_err());
    send.send(()).unwrap();
    worker.await.unwrap();
    assert!(credit(true, false).is_ok());
    drop((controls, cancel));
    tokio::task::LocalSet::new()
        .run_until(async {
            let (host, service, stop, server) = fixture().await;
            service.read_delay_ms.store(200, Ordering::SeqCst);
            let metadata = output_only(&host, true);
            let id = host
                .call(args("open", json!({"metadata":metadata})), None)
                .await
                .unwrap()
                .as_u64()
                .unwrap() as u32;
            wait(&host, "live").await;
            emit_blob(&service);
            let (raw, _) = host.raw_owner(id).unwrap();
            loop {
                if raw.lock().await.downloads.contains_key("saved") {
                    break;
                }
                tokio::task::yield_now().await;
            }
            host.call(args("beginDownload", json!({"id":id,"port":"saved"})), None)
                .await
                .unwrap();
            host.raw(
                args("download", json!({"id":id,"port":"saved"})),
                Vec::new(),
            )
            .await
            .unwrap();
            let (reply, result) = oneshot::channel();
            let request = RawRequest {
                args: args("download", json!({"id":id,"port":"saved"})),
                bytes: Vec::new(),
                reply,
                _permit: credit(true, false).unwrap(),
            };
            let owner = raw.lock().await.downloads["saved"].owner.clone().unwrap();
            let pending = host.dispatch_raw(request);
            tokio::pin!(pending);
            assert!(futures_util::poll!(&mut pending).is_pending());
            assert_eq!(owner.request_capacity(), 0);
            while owner.request_capacity() == 0 {
                tokio::task::yield_now().await;
            }
            assert!(raw.try_lock().is_ok());
            for _ in 0..1000 {
                assert!(credit(true, false).is_err());
            }
            let _cancel = credit(false, true).unwrap();
            host.call(args("cancel", json!({"id":id})), None)
                .await
                .unwrap();
            pending.await;
            assert!(result.await.unwrap().is_err());
            assert!(credit(true, false).is_ok());
            assert!(
                host.raw(
                    args("download", json!({"id":id,"port":"saved"})),
                    Vec::new()
                )
                .await
                .is_err()
            );
            assert_eq!(service.reader_closes.load(Ordering::SeqCst), 1);
            host.drain().await;
            stop.cancel();
            server.await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn f3_expiry_interrupts_stalled_checkout_and_partial_read_and_joins_close() {
    tokio::task::LocalSet::new().run_until(async {
        for checkout in [true, false] {
            let (host, service, stop, server) = fixture().await;
            service.validity_ms.store(30, Ordering::SeqCst);
            if checkout { service.checkout_delay_ms.store(90, Ordering::SeqCst); }
            else { service.read_delay_ms.store(90, Ordering::SeqCst); }
            let metadata = output_only(&host, true);
            let dir = Directory::new(); let path = dir.path().join("destination");
            let selected = host.call(args("pickOutput", json!({"request":{"port":"saved","suggestedName":"result.bin"}})), Some(Selected::Output { path: path.clone() })).await.unwrap();
            host.call(args("openNative", json!({"metadata":metadata,"outputs":[{"kind":"file","port":"saved","handle":selected["handle"]}]})), None).await.unwrap();
            wait(&host, "live").await;
            let start = std::time::Instant::now(); emit_blob(&service);
            wait(&host, "error").await;
            assert!(start.elapsed() < std::time::Duration::from_secs(2));
            assert_eq!(service.reader_closes.load(Ordering::SeqCst), 1);
            assert!(!path.exists());
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
            host.drain().await; stop.cancel(); server.await.unwrap();
        }
    }).await;
}

#[tokio::test]
async fn f5_same_generation_picker_ticket_cannot_survive_reconcile_and_late_callback() {
    let host = host(NativeGrpcTransport::new("http://127.0.0.1:1"));
    let epoch = host.audio.lock().picker_epoch;
    let mut snapshot = host.state.borrow().snapshot.clone();
    snapshot.routines.definitions.definitions[0].routine_revision = "replacement".into();
    host.revoke_if_changed(&snapshot);
    host.reconcile(&snapshot).await;
    host.audio.lock().access(true);
    let dir = Directory::new();
    let path = dir.path().join("never-created");
    let (reply, result) = oneshot::channel();
    host.dispatch(super::Request {
        args: args(
            "pickOutput",
            json!({"request":{"port":"saved","suggestedName":"result.bin"}}),
        ),
        selected: Some(Selected::Output { path: path.clone() }),
        reply,
        ticket: (Some(0), epoch),
        _permit: Arc::new(tokio::sync::Semaphore::new(1))
            .try_acquire_owned()
            .unwrap(),
    })
    .await;
    assert!(result.await.unwrap().is_err());
    assert!(host.state.borrow().outputs.is_empty());
    assert!(!path.exists());
    assert_ne!(host.audio.lock().picker_epoch, epoch);
}

#[tokio::test]
async fn f5_cancelled_dialog_caller_does_not_release_pending_callback_credit() {
    let pending = picker_credit().unwrap();
    let (invoke, callback) = oneshot::channel::<()>();
    let (publish, response) = oneshot::channel::<()>();
    let callback = tokio::spawn(async move {
        let _pending = pending;
        let _invoked = callback.await;
        let _discarded = publish.send(());
    });
    drop(response);
    for _ in 0..1000 {
        assert!(picker_credit().is_err());
    }
    invoke.send(()).unwrap();
    callback.await.unwrap();
    assert!(picker_credit().is_ok());
}

#[tokio::test]
async fn f7_open_and_first_accepted_share_the_original_twenty_second_deadline() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (host, service, stop, server) = fixture().await;
            service.open_delay_ms.store(12_000, Ordering::SeqCst);
            service.accepted_delay_ms.store(12_000, Ordering::SeqCst);
            let metadata = output_only(&host, false);
            let start = tokio::time::Instant::now();
            host.call(args("openNative", json!({"metadata":metadata})), None)
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(23), async {
                loop {
                    if host.state.borrow().status["phase"] == "error" {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert!(start.elapsed() >= std::time::Duration::from_secs(20));
            assert!(start.elapsed() < std::time::Duration::from_secs(22));
            assert_eq!(service.closes.load(Ordering::SeqCst), 1);
            host.drain().await;
            stop.cancel();
            server.await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn f7_combined_under_deadline_succeeds_and_cancel_before_accepted_joins() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for cancel in [false, true] {
                let (host, service, stop, server) = fixture().await;
                service.open_delay_ms.store(20, Ordering::SeqCst);
                service
                    .accepted_delay_ms
                    .store(if cancel { 2000 } else { 20 }, Ordering::SeqCst);
                let metadata = output_only(&host, false);
                let id = host
                    .call(args("openNative", json!({"metadata":metadata})), None)
                    .await
                    .unwrap()
                    .as_u64()
                    .unwrap();
                if cancel {
                    tokio::time::timeout(std::time::Duration::from_secs(2), async {
                        loop {
                            if host
                                .state
                                .borrow()
                                .active
                                .as_ref()
                                .is_some_and(|a| a.input.is_some())
                            {
                                break;
                            }
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                    host.call(args("cancel", json!({"id":id})), None)
                        .await
                        .unwrap();
                    assert_ne!(host.state.borrow().status["phase"], "succeeded");
                } else {
                    wait(&host, "live").await;
                    host.call(args("finishInput", json!({"id":id})), None)
                        .await
                        .unwrap();
                    wait(&host, "succeeded").await;
                }
                assert_eq!(service.closes.load(Ordering::SeqCst), 1);
                host.drain().await;
                stop.cancel();
                server.await.unwrap();
            }
        })
        .await;
}

#[tokio::test]
async fn f3_pending_checkout_does_not_deadlock_attachment_renewal() {
    tokio::task::LocalSet::new().run_until(async {
        let (host, service, stop, server) = fixture().await;
        service.checkout_delay_ms.store(5500, Ordering::SeqCst);
        service.validity_ms.store(7000, Ordering::SeqCst);
        let metadata = output_only(&host, true);
        let dir = Directory::new(); let path = dir.path().join("destination");
        let selected = host.call(args("pickOutput", json!({"request":{"port":"saved","suggestedName":"result.bin"}})), Some(Selected::Output { path: path.clone() })).await.unwrap();
        let id = host.call(args("openNative", json!({"metadata":metadata,"outputs":[{"kind":"file","port":"saved","handle":selected["handle"]}]})), None).await.unwrap().as_u64().unwrap();
        wait(&host, "live").await; emit_blob(&service);
        host.call(args("finishInput", json!({"id":id})), None).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(8), async { loop {
            if host.state.borrow().status["phase"] == "succeeded" { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }}).await.unwrap();
        assert!(service.renewals.load(Ordering::SeqCst) > 0);
        assert_eq!(std::fs::read(path).unwrap(), vec![7; 100_001]);
        host.drain().await; stop.cancel(); server.await.unwrap();
    }).await;
}

#[test]
fn f6_replaced_selected_path_is_never_unlinked_and_publication_never_overwrites() {
    let dir = Directory::new();
    let path = dir.path().join("selected");
    let mut destination = Destination {
        path: path.clone(),
        port: "file".into(),
        file: None,
        created: false,
        complete: false,
        staging: None,
    };
    destination.create().unwrap();
    destination
        .file
        .as_mut()
        .unwrap()
        .write_all(b"owned")
        .unwrap();
    destination.publish().unwrap();
    std::fs::rename(&path, dir.path().join("moved-owned")).unwrap();
    std::fs::write(&path, b"user-replacement").unwrap();
    assert!(destination.publish().is_err());
    destination.complete = false;
    destination.discard();
    assert_eq!(std::fs::read(path).unwrap(), b"user-replacement");
    assert_eq!(
        std::fs::read(dir.path().join("moved-owned")).unwrap(),
        b"owned"
    );
}

#[tokio::test]
async fn raw_file_ipc_upload_negotiates_chunks_and_cannot_be_reused_after_drain() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (host, service, stop, join) = fixture().await;
            let id = host
                .call(args("open", json!({"metadata":metadata(100_001)})), None)
                .await
                .unwrap()
                .as_u64()
                .unwrap() as u32;
            wait(&host, "live").await;
            host.call(args("beginUpload", json!({"id":id,"port":"file"})), None)
                .await
                .unwrap();
            host.raw(
                args("upload", json!({"id":id,"port":"file"})),
                vec![3; 65536],
            )
            .await
            .unwrap();
            host.raw(
                args("upload", json!({"id":id,"port":"file"})),
                vec![3; 34465],
            )
            .await
            .unwrap();
            let descriptor = host
                .call(args("commitUpload", json!({"id":id,"port":"file"})), None)
                .await
                .unwrap();
            assert!(!descriptor.to_string().contains("SECRET"));
            host.call(args("finishInput", json!({"id":id})), None)
                .await
                .unwrap();
            wait(&host, "succeeded").await;
            assert_eq!(service.state.lock().unwrap().bytes, vec![3; 100_001]);
            host.drain().await;
            assert!(
                host.raw(args("upload", json!({"id":id,"port":"file"})), vec![1])
                    .await
                    .is_err()
            );
            stop.cancel();
            join.await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn selected_native_destination_saves_bounded_file_and_closes_secret_reader() {
    tokio::task::LocalSet::new().run_until(async {
        let (host, service, stop, join) = fixture().await;
        let metadata = output_only(&host,true);
        let dir = Directory::new(); let path = dir.path().join("SECRET-PRIVATE-DESTINATION");
        let destination = host.call(args("pickOutput",json!({"request":{"port":"saved","suggestedName":"result.bin"}})),Some(Selected::Output{path:path.clone()})).await.unwrap();
        assert!(!destination.to_string().contains("SECRET"));
        let id = host.call(args("openNative",json!({"metadata":metadata,"outputs":[{"port":"saved","kind":"file","handle":destination["handle"]}]})),None).await.unwrap().as_u64().unwrap() as u32;
        wait(&host,"live").await; emit_blob(&service);
        host.call(args("finishInput",json!({"id":id})),None).await.unwrap();
        wait(&host,"succeeded").await;
        assert_eq!(std::fs::read(path).unwrap(),vec![7;100_001]);
        assert!(service.reader_closes.load(Ordering::SeqCst)>=1);
        assert!(!host.state.borrow().status.to_string().contains("SECRET"));
        assert!(!host.state.borrow().events.borrow().iter().any(|v|v.to_string().contains("SECRET")));
        host.drain().await; stop.cancel(); join.await.unwrap();
    }).await;
}

#[tokio::test]
async fn optional_unselected_output_has_no_filesystem_effect_and_required_missing_sink_preflights()
{
    tokio::task::LocalSet::new()
        .run_until(async {
            let (host, service, stop, join) = fixture().await;
            let metadata = output_only(&host, true);
            assert!(
                host.call(args("openNative", json!({"metadata":metadata})), None)
                    .await
                    .is_err()
            );
            assert_eq!(service.opens.load(Ordering::SeqCst), 0);
            let metadata = output_only(&host, false);
            let id = host
                .call(args("openNative", json!({"metadata":metadata})), None)
                .await
                .unwrap()
                .as_u64()
                .unwrap() as u32;
            wait(&host, "live").await;
            emit_blob(&service);
            host.call(args("finishInput", json!({"id":id})), None)
                .await
                .unwrap();
            wait(&host, "succeeded").await;
            assert_eq!(service.reader_closes.load(Ordering::SeqCst), 0);
            assert!(host.state.borrow().events.borrow().is_empty());
            host.drain().await;
            stop.cancel();
            join.await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn raw_download_lease_blocks_actual_io_after_expiry_and_drain_closes_reader() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (host, service, stop, join) = fixture().await;
            let metadata = output_only(&host, true);
            let id = host
                .call(args("open", json!({"metadata":metadata})), None)
                .await
                .unwrap()
                .as_u64()
                .unwrap() as u32;
            wait(&host, "live").await;
            emit_blob(&service);
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let (raw, _) = host.raw_owner(id).unwrap();
                    if raw.lock().await.downloads.contains_key("saved") {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let descriptor = host
                .call(args("beginDownload", json!({"id":id,"port":"saved"})), None)
                .await
                .unwrap();
            assert!(!descriptor.to_string().contains("SECRET"));
            assert_eq!(
                host.raw(args("download", json!({"id":id,"port":"saved"})), vec![])
                    .await
                    .unwrap(),
                vec![7; 65536]
            );
            let (raw, _) = host.raw_owner(id).unwrap();
            raw.lock().await.downloads.get_mut("saved").unwrap().expires =
                std::time::Instant::now();
            assert!(
                host.raw(args("download", json!({"id":id,"port":"saved"})), vec![])
                    .await
                    .is_err()
            );
            host.drain().await;
            assert!(service.reader_closes.load(Ordering::SeqCst) >= 1);
            stop.cancel();
            join.await.unwrap();
        })
        .await;
}
