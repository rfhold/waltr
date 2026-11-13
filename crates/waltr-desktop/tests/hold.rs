//! Local fake RPC and synthetic capture only. No device, key, clipboard, or remote service access.
use futures_util::{Stream, StreamExt, stream};
use std::{
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status as RpcStatus};
use waltr_audio_io::Terminal;
use waltr_client::{
    NativeGrpcTransport,
    interaction::wire::{self, output_event::Event, routine_service_server::RoutineServiceServer},
};
use waltr_desktop::{
    capture::{self, Factory, Source},
    config,
    host::{AudioAdmission, Error, Hold, Status},
};
mod support;
support::fake_service!(Fake;
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
    begin_blob_upload: BeginBlobUploadRequest => BeginBlobUploadResponse,
    append_blob_upload: AppendBlobUploadRequest => AppendBlobUploadResponse,
    commit_blob_upload: CommitBlobUploadRequest => CommitBlobUploadResponse,
    cancel_blob_upload: CancelBlobUploadRequest => CancelBlobUploadResponse,
    open_blob_read: OpenBlobReadRequest => OpenBlobReadResponse,
    close_blob_read: CloseBlobReadRequest => CloseBlobReadResponse,
);
type Responses = Pin<Box<dyn Stream<Item = Result<wire::InteractResponse, RpcStatus>> + Send>>;
#[derive(Clone, Default)]
struct Fake {
    cleanup_blocked: Arc<AtomicBool>,
    cleanup_release: Arc<tokio::sync::Notify>,
    admissions: Arc<AtomicUsize>,
    uploads: Arc<AtomicUsize>,
    completed: Arc<AtomicBool>,
    delay_waiting: Arc<AtomicBool>,
    pcm: Arc<Mutex<Vec<u8>>>,
    repeated_input: Arc<Mutex<Vec<wire::InputEvent>>>,
    repeated_closes: Arc<AtomicUsize>,
}
#[allow(
    clippy::unnecessary_wraps,
    reason = "fixture events share the fallible RPC stream item type"
)]
const fn event(event: Event) -> Result<wire::InteractResponse, RpcStatus> {
    Ok(wire::InteractResponse {
        output: Some(wire::OutputEvent { event: Some(event) }),
    })
}
fn accepted() -> Result<wire::InteractResponse, RpcStatus> {
    event(Event::Accepted(wire::InteractionAccepted {
        invocation_id: "invocation".into(),
        revision: "revision".into(),
        limits: Some(support::limits()),
    }))
}
fn output_open() -> Result<wire::InteractResponse, RpcStatus> {
    event(Event::Open(wire::StreamOpen {
        stream_id: "out".into(),
        port: "clean-text".into(),
        format: Some(wire::stream_open::Format::Text(wire::TextFormat {})),
    }))
}
impl Fake {
    async fn interact(
        &self,
        request: Request<tonic::Streaming<wire::InteractRequest>>,
    ) -> Result<Response<Responses>, RpcStatus> {
        self.admissions.fetch_add(1, Ordering::SeqCst);
        let mut input = request.into_inner();
        let first = input.message().await?.unwrap();
        let Some(wire::interact_request::Frame::Open(open)) = first.frame else {
            panic!("expected open")
        };
        let open = open.selection.unwrap();
        if open.routine.starts_with("repeated") {
            let fake = self.clone();
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            tokio::spawn(async move {
                tx.send(accepted()).await.unwrap();
                tx.send(event(Event::Open(wire::StreamOpen {
                    stream_id: "speaker".into(),
                    port: "speaker".into(),
                    format: Some(wire::stream_open::Format::Pcm16le(wire::Pcm16Le {
                        sample_rate: 24000,
                        channels: 1,
                    })),
                })))
                .await
                .unwrap();
                while let Ok(Some(message)) = input.message().await {
                    let Some(wire::interact_request::Frame::Input(message)) = message.frame else {
                        panic!("input")
                    };
                    fake.repeated_input.lock().unwrap().push(message.clone());
                    match message.event.unwrap() {
                        wire::input_event::Event::Open(_)
                            if fake.repeated_closes.load(Ordering::SeqCst) >= 2 =>
                        {
                            let _result = tx
                                .send(Err(RpcStatus::failed_precondition(
                                    "input admission sealed",
                                )))
                                .await;
                            return;
                        }
                        wire::input_event::Event::Close(_) => {
                            let count = fake.repeated_closes.fetch_add(1, Ordering::SeqCst) + 1;
                            if count == 1 {
                                tx.send(event(Event::Chunk(wire::StreamChunk {
                                    stream_id: "speaker".into(),
                                    sequence: 0,
                                    payload: Some(wire::stream_chunk::Payload::Audio(vec![
                                        1, 0, 2, 0,
                                    ])),
                                })))
                                .await
                                .unwrap();
                            }
                            if count == 2 {
                                tx.send(event(Event::Close(wire::StreamClose {
                                    stream_id: "speaker".into(),
                                    next_sequence: 1,
                                })))
                                .await
                                .unwrap();
                            }
                        }
                        wire::input_event::Event::Open(_)
                            if open.routine == "repeated-disconnect"
                                && fake.repeated_closes.load(Ordering::SeqCst) == 1 =>
                        {
                            let _result = tx
                                .send(Err(RpcStatus::unavailable("synthetic disconnect")))
                                .await;
                            return;
                        }
                        wire::input_event::Event::Finish(_) => {
                            assert_eq!(fake.repeated_closes.load(Ordering::SeqCst), 2);
                            assert!(
                                input.message().await.unwrap().is_none(),
                                "native EOF follows global finish"
                            );
                            tx.send(event(Event::Terminal(wire::InteractionTerminal {
                                status: wire::InvocationStatus::Succeeded as i32,
                                cleanup_complete: true,
                                ..Default::default()
                            })))
                            .await
                            .unwrap();
                            fake.completed.store(true, Ordering::SeqCst);
                            return;
                        }
                        _ => {}
                    }
                }
            });
            return Ok(Response::new(Box::pin(stream::unfold(
                rx,
                |mut rx| async move { rx.recv().await.map(|event| (event, rx)) },
            ))));
        }
        if open.routine == "playback-failure" {
            let fake = self.clone();
            let events = stream::iter([
                accepted(),
                event(Event::Open(wire::StreamOpen {
                    stream_id: "speaker".into(),
                    port: "speaker".into(),
                    format: Some(wire::stream_open::Format::Pcm16le(wire::Pcm16Le {
                        sample_rate: 24000,
                        channels: 1,
                    })),
                })),
                event(Event::Chunk(wire::StreamChunk {
                    stream_id: "speaker".into(),
                    sequence: 0,
                    payload: Some(wire::stream_chunk::Payload::Audio(vec![1, 0, 2, 0])),
                })),
                event(Event::Terminal(wire::InteractionTerminal {
                    status: wire::InvocationStatus::Failed as i32,
                    cleanup_complete: true,
                    ..Default::default()
                })),
            ]);
            let tail = stream::once(async move {
                // A terminal closes input admission, but remote EOF remains deliberately blocked.
                fake.cleanup_blocked.store(true, Ordering::SeqCst);
                fake.cleanup_release.notified().await;
                drop(input);
                stream::empty()
            })
            .flatten();
            return Ok(Response::new(Box::pin(events.chain(tail))));
        }
        assert!(
            open.source_session.is_none(),
            "production admission forbids source sessions"
        );
        if open.routine == "acceptance-gate" {
            let fake = self.clone();
            let output = stream::once(async move {
                assert!(
                    tokio::time::timeout(Duration::from_millis(20), input.message())
                        .await
                        .is_err(),
                    "audio must wait for Accepted"
                );
                let accepted = accepted();
                let tail = stream::once(async move {
                    let mut kinds = Vec::new();
                    while let Some(message) = input.message().await.unwrap() {
                        let Some(wire::interact_request::Frame::Input(message)) = message.frame
                        else {
                            panic!("input")
                        };
                        match message.event.unwrap() {
                            wire::input_event::Event::Open(_) => kinds.push("open"),
                            wire::input_event::Event::Chunk(chunk) => {
                                kinds.push("chunk");
                                if let Some(wire::stream_chunk::Payload::Audio(bytes)) =
                                    chunk.payload
                                {
                                    fake.uploads.fetch_add(bytes.len(), Ordering::SeqCst);
                                }
                            }
                            wire::input_event::Event::Close(_) => kinds.push("close"),
                            wire::input_event::Event::Finish(_) => {
                                kinds.push("finish");
                                break;
                            }
                            _ => panic!("unexpected input"),
                        }
                    }
                    assert_eq!(kinds, ["open", "chunk", "close", "finish"]);
                    fake.completed.store(true, Ordering::SeqCst);
                    let mut events = vec![output_open()];
                    events.extend(final_events("test"));
                    stream::iter(events)
                })
                .flatten();
                stream::iter([accepted]).chain(tail)
            })
            .flatten();
            return Ok(Response::new(Box::pin(output)));
        }
        if open.routine == "pending" {
            while input.message().await?.is_some() {}
            return Err(RpcStatus::cancelled("closed before admission"));
        }
        if open.routine == "early-success" {
            let mut events = vec![accepted(), output_open()];
            events.extend(final_events("test"));
            return Ok(Response::new(Box::pin(stream::iter(events))));
        }
        if open.routine == "disconnect-before-accepted" {
            return Ok(Response::new(Box::pin(stream::iter([Err(
                RpcStatus::unavailable("synthetic disconnect"),
            )]))));
        }
        if open.routine == "early-rejected" {
            return Ok(Response::new(Box::pin(stream::iter([event(
                Event::Rejected(wire::InteractionRejected {
                    code: 1,
                    reason: "fixture".into(),
                }),
            )]))));
        }
        if open.routine.starts_with("early-") {
            return Ok(Response::new(Box::pin(stream::iter([
                accepted(),
                event(Event::Terminal(wire::InteractionTerminal {
                    status: match open.routine.as_str() {
                        "early-failed" => wire::InvocationStatus::Failed,
                        "early-interrupted" => wire::InvocationStatus::Interrupted,
                        _ => wire::InvocationStatus::Succeeded,
                    } as i32,
                    cleanup_complete: true,
                    ..Default::default()
                })),
            ]))));
        }
        if open.routine.starts_with("delay-open-") {
            let ms = open
                .routine
                .rsplit('-')
                .next()
                .unwrap()
                .parse::<u64>()
                .unwrap();
            self.delay_waiting.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
        let fake = self.clone();
        let mut prefix = vec![accepted(), output_open()];
        if open.routine == "audio-answer" {
            prefix = vec![accepted()];
        }
        if open.routine == "pcm" {
            prefix.extend([
                event(Event::Open(wire::StreamOpen {
                    stream_id: "speaker".into(),
                    port: "speaker".into(),
                    format: Some(wire::stream_open::Format::Pcm16le(wire::Pcm16Le {
                        sample_rate: 24000,
                        channels: 1,
                    })),
                })),
                event(Event::Chunk(wire::StreamChunk {
                    stream_id: "speaker".into(),
                    sequence: 0,
                    payload: Some(wire::stream_chunk::Payload::Audio(vec![1, 0, 2, 0])),
                })),
                event(Event::Close(wire::StreamClose {
                    stream_id: "speaker".into(),
                    next_sequence: 1,
                })),
            ]);
        }
        if open.routine.starts_with("typing") {
            prefix.extend([
                text_event(0, "one", "draft", false),
                text_event(1, "one", "first\r", true),
                text_event(2, "two", "draft", false),
            ]);
        }
        let delay = if open.routine.starts_with("delay-accepted-") {
            open.routine
                .rsplit('-')
                .next()
                .unwrap()
                .parse::<u64>()
                .unwrap()
        } else {
            0
        };
        let tail = stream::once(async move {
            let mut closed = false;
            while let Some(message) = input.message().await? {
                let Some(wire::interact_request::Frame::Input(message)) = message.frame else {
                    panic!("input")
                };
                match message.event {
                    Some(wire::input_event::Event::Chunk(chunk)) => {
                        if let Some(wire::stream_chunk::Payload::Audio(bytes)) = chunk.payload {
                            fake.uploads.fetch_add(bytes.len(), Ordering::SeqCst);
                            fake.pcm.lock().unwrap().extend(bytes);
                        }
                    }
                    Some(wire::input_event::Event::Close(_value)) => {
                        assert!(!closed);
                        closed = true;
                    }
                    Some(wire::input_event::Event::Finish(_)) => {
                        assert!(closed, "normal input close must precede completion");
                        break;
                    }
                    Some(wire::input_event::Event::Cancel(_)) => {
                        return Ok(vec![event(Event::Terminal(wire::InteractionTerminal {
                            status: 5,
                            cleanup_complete: true,
                            ..Default::default()
                        }))]);
                    }
                    _ => (),
                }
            }
            fake.completed.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            if open.routine == "audio-answer" {
                return Ok(vec![
                    event(Event::Open(wire::StreamOpen {
                        stream_id: "answer".into(),
                        port: "speaker".into(),
                        format: Some(wire::stream_open::Format::Pcm16le(wire::Pcm16Le {
                            sample_rate: 24000,
                            channels: 1,
                        })),
                    })),
                    event(Event::Chunk(wire::StreamChunk {
                        stream_id: "answer".into(),
                        sequence: 0,
                        payload: Some(wire::stream_chunk::Payload::Audio(vec![1, 0, 2, 0])),
                    })),
                    event(Event::Close(wire::StreamClose {
                        stream_id: "answer".into(),
                        next_sequence: 1,
                    })),
                    event(Event::Terminal(wire::InteractionTerminal {
                        status: wire::InvocationStatus::Succeeded as i32,
                        cleanup_complete: true,
                        ..Default::default()
                    })),
                ]);
            }
            if open.routine == "failed" || open.routine == "typing-failed" {
                return Err(RpcStatus::data_loss("static failure"));
            }
            Ok(final_events(&open.routine))
        })
        .map(|result| stream::iter(result.unwrap_or_else(|e| vec![Err(e)])))
        .flatten();
        let waiting = self.delay_waiting.clone();
        let prefix = stream::once(async move {
            waiting.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(delay)).await;
            stream::iter(prefix)
        })
        .flatten();
        Ok(Response::new(Box::pin(prefix.chain(tail))))
    }
}
async fn fixture() -> (
    NativeGrpcTransport,
    Fake,
    CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = stream::unfold(listener, |listener| async {
        Some((listener.accept().await.map(|v| v.0), listener))
    });
    let fake = Fake::default();
    let service = RoutineServiceServer::new(fake.clone());
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming_shutdown(incoming, stopped.cancelled())
            .await
            .unwrap();
    });
    (
        NativeGrpcTransport::new(format!("http://{addr}")),
        fake,
        stop,
        server,
    )
}
#[derive(Default)]
struct Mock {
    started: AtomicBool,
    duration_ceiling: AtomicUsize,
    stopped: AtomicBool,
    joined: AtomicBool,
    burst: AtomicBool,
    small_chunks: AtomicBool,
    admit_burst: Mutex<Option<Arc<AtomicUsize>>>,
    fail_on_stop: AtomicBool,
    empty: AtomicBool,
    tail_only: AtomicBool,
    native_failure: Mutex<Option<waltr_audio_io::Error>>,
    startup: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}
struct Samples {
    state: Arc<Mock>,
    remaining: usize,
}
struct MockFactory(Arc<Mock>);
impl Factory for MockFactory {
    fn open(&self, config: waltr_audio_io::Config) -> Result<Box<dyn Source>, capture::Error> {
        let state = &self.0;
        state.duration_ceiling.store(
            config.limits.max_duration_seconds as usize,
            Ordering::SeqCst,
        );
        state.started.store(true, Ordering::SeqCst);
        let startup = state.startup.lock().unwrap().take();
        if let Some(rx) = startup {
            rx.recv().unwrap();
        }
        Ok(Box::new(Samples {
            state: state.clone(),
            remaining: if state.burst.load(Ordering::SeqCst) {
                20_000
            } else if state.small_chunks.load(Ordering::SeqCst) {
                120
            } else {
                1
            },
        }))
    }
}
impl Source for Samples {
    fn frame(&mut self) -> Option<Vec<u8>> {
        if self.remaining < 20_000
            && self
                .state
                .admit_burst
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|admissions| admissions.load(Ordering::SeqCst) == 0)
        {
            return None;
        }
        if self.remaining > 0
            && !self.state.empty.load(Ordering::SeqCst)
            && (!self.state.tail_only.load(Ordering::SeqCst)
                || self.state.stopped.load(Ordering::SeqCst)
                || self.state.burst.load(Ordering::SeqCst))
        {
            self.remaining -= 1;
            if self.state.small_chunks.load(Ordering::SeqCst) {
                Some(vec![self.remaining as u8; 96])
            } else {
                Some(vec![0; 100])
            }
        } else {
            None
        }
    }
    fn terminal(&self) -> Terminal {
        if self.state.stopped.load(Ordering::SeqCst)
            && let Some(error) = self.state.native_failure.lock().unwrap().clone()
        {
            return Terminal::Failed(error);
        }
        if self.state.stopped.load(Ordering::SeqCst)
            && self.state.fail_on_stop.load(Ordering::SeqCst)
        {
            Terminal::Failed(waltr_audio_io::Error::Backend(
                "SECRET_CAPTURE_SENTINEL".into(),
            ))
        } else if self.state.stopped.load(Ordering::SeqCst) {
            Terminal::Completed
        } else {
            Terminal::Running
        }
    }
    fn stop(&self) {
        self.state.stopped.store(true, Ordering::SeqCst);
    }
    fn shutdown(self: Box<Self>) {
        self.state.joined.store(true, Ordering::SeqCst);
    }
}
fn binding(path: &std::path::Path, name: &str) -> config::Keybind {
    // Static test fixture shell only; final text enters stdin, never shell source.
    let mut c = config::parse(br#"version: 1
keybinds:
 - id: test
   key: Ctrl+Alt+Space
   mode: hold
   routine: { name: test, trigger: dictate, input: audio, output: clean-text }
   audio: { device: default, sampleRateHz: 24000, channels: 1 }
   command: { argv: [/bin/sh, '-c', 'cat >> "$1"', fixture, unused], stdin: '{{ text }}', timeoutSeconds: 10 }
"#).unwrap();
    let mut b = c.keybinds.remove(0);
    b.routine.name = name.into();
    b.resolved = Some(support::invocation(name));
    b.command.as_mut().unwrap().argv[4] = path.to_str().unwrap().into();
    b
}
#[tokio::test]
async fn capture_auto_stop_drains_silence_tail_and_long_notes_once_with_release_and_cancel_races() {
    let (transport, fake, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    for case in [
        "no-release",
        "early-release",
        "racing-release",
        "cancel",
        "access-loss",
    ] {
        fake.completed.store(false, Ordering::SeqCst);
        fake.uploads.store(0, Ordering::SeqCst);
        let path = dir.path().join(case);
        let mut binding = binding(&path, "hour-notes");
        let seconds = if case == "early-release" { 3600 } else { 1 };
        binding.max_capture_seconds = Some(seconds);
        let mock = Arc::new(Mock {
            tail_only: AtomicBool::new(true),
            ..Default::default()
        });
        let (hold, control) = Hold::press(&AudioAdmission::default()).unwrap();
        let generation = CancellationToken::new();
        let drive = async {
            match case {
                "no-release" => (),
                "early-release" => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    control.release();
                }
                _ => {
                    while !mock.stopped.load(Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    match case {
                        "racing-release" => control.release(),
                        "cancel" => control.cancel(),
                        "access-loss" => generation.cancel(),
                        _ => unreachable!(),
                    }
                }
            }
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                hold.run(
                    &binding,
                    &transport,
                    Arc::new(MockFactory(mock.clone())),
                    &generation
                ),
                drive
            )
        })
        .await
        .unwrap();
        assert!(mock.joined.load(Ordering::SeqCst));
        assert_eq!(
            mock.duration_ceiling.load(Ordering::SeqCst),
            seconds as usize + 60
        );
        if matches!(case, "cancel" | "access-loss") {
            assert_eq!(result, Err(Error::Cancelled));
            assert!(!path.exists());
        } else {
            assert_eq!(result, Ok(()));
            assert!(fake.completed.load(Ordering::SeqCst));
            assert_eq!(fake.uploads.load(Ordering::SeqCst), 100);
            assert_eq!(std::fs::read(&path).unwrap(), vec![b'x'; 512 * 1024 - 64]);
            // Late key-up after successful automatic completion cannot repeat the command.
            control.release();
            control.release();
            assert_eq!(std::fs::metadata(&path).unwrap().len(), 512 * 1024 - 64);
        }
    }
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn auto_stop_during_startup_remains_pending_until_native_open_returns() {
    let (transport, fake, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty");
    let mut binding = binding(&path, "test");
    binding.max_capture_seconds = Some(1);
    let (tx, rx) = std::sync::mpsc::channel();
    let mock = Arc::new(Mock {
        startup: Mutex::new(Some(rx)),
        empty: AtomicBool::new(true),
        ..Default::default()
    });
    let (hold, _control) = Hold::press(&AudioAdmission::default()).unwrap();
    let startup = async {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(!mock.joined.load(Ordering::SeqCst));
        tx.send(()).unwrap();
    };
    let generation = CancellationToken::new();
    let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(
            hold.run(
                &binding,
                &transport,
                Arc::new(MockFactory(mock.clone())),
                &generation
            ),
            startup
        )
    })
    .await
    .unwrap();
    assert_eq!(result, Ok(()));
    assert!(mock.stopped.load(Ordering::SeqCst));
    assert!(mock.joined.load(Ordering::SeqCst));
    assert_eq!(fake.admissions.load(Ordering::SeqCst), 0);
    assert!(!path.exists());
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn empty_capture_never_opens_rpc_or_runs_command_and_next_hold_works() {
    use waltr_desktop::host::{Diagnostic, Observer};
    let (transport, fake, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("command");
    let binding = binding(&path, "test");
    let gate = AudioAdmission::default();
    let mock = Arc::new(Mock {
        empty: AtomicBool::new(true),
        ..Default::default()
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let seen = events.clone();
    let (hold, control) = Hold::press(&gate).unwrap();
    control.release();
    assert_eq!(
        hold.observed(Observer::new(move |event| seen.lock().unwrap().push(event)))
            .run(
                &binding,
                &transport,
                Arc::new(MockFactory(mock.clone())),
                &CancellationToken::new()
            )
            .await,
        Ok(())
    );
    assert!(mock.joined.load(Ordering::SeqCst));
    assert_eq!(fake.admissions.load(Ordering::SeqCst), 0);
    assert_eq!(fake.uploads.load(Ordering::SeqCst), 0);
    assert!(!path.exists());
    assert!(events.lock().unwrap().contains(&Diagnostic::NoInput));
    let mock = Arc::new(Mock {
        tail_only: AtomicBool::new(true),
        ..Default::default()
    });
    let (hold, control) = Hold::press(&gate).unwrap();
    control.release();
    assert_eq!(
        hold.run(
            &binding,
            &transport,
            Arc::new(MockFactory(mock)),
            &CancellationToken::new()
        )
        .await,
        Ok(())
    );
    assert_eq!(fake.admissions.load(Ordering::SeqCst), 1);
    assert_eq!(fake.uploads.load(Ordering::SeqCst), 100);
    assert!(path.exists());
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn callback_overflow_after_first_frame_preserves_primary_and_cleanup() {
    use waltr_desktop::host::{Diagnostic, Observer};
    let (transport, fake, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("never");
    let binding = binding(&path, "pending");
    let gate = AudioAdmission::default();
    let (hold, control) = Hold::press(&gate).unwrap();
    let mock = Arc::new(Mock {
        native_failure: Mutex::new(Some(waltr_audio_io::Error::CallbackOverflow)),
        ..Default::default()
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let seen = events.clone();
    let generation = CancellationToken::new();
    let run = hold
        .observed(Observer::new(move |event| seen.lock().unwrap().push(event)))
        .run(
            &binding,
            &transport,
            Arc::new(MockFactory(mock.clone())),
            &generation,
        );
    let release = async {
        tokio::time::timeout(Duration::from_secs(2), async {
            while fake.admissions.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        control.release();
    };
    let (result, ()) = tokio::join!(run, release);
    assert_eq!(
        result,
        Err(Error::Capture(capture::Error::CallbackOverflow))
    );
    assert!(mock.joined.load(Ordering::SeqCst));
    let events = events.lock().unwrap();
    let primary = events
        .iter()
        .position(|event| *event == Diagnostic::Primary(result))
        .unwrap();
    let cleanup = events
        .iter()
        .position(|event| {
            *event == Diagnostic::CaptureCleanup(Err(capture::Error::CallbackOverflow))
        })
        .unwrap();
    assert!(primary < cleanup);
    assert!(!events.contains(&Diagnostic::NoInput));
    assert!(!path.exists());
    assert!(gate.reserve().is_ok());
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn guarded_credential_revision_rejects_rpc_and_command_without_monitor_poll() {
    use waltr_desktop::host::{Diagnostic, Observer, Stage};
    for boundary in [Stage::CaptureRequested, Stage::RpcOpen, Stage::Action] {
        let (transport, fake, stop, server) = fixture().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("must-not-run");
        let binding = binding(&path, "test");
        let gate = AudioAdmission::default();
        let (hold, control) = Hold::press(&gate).unwrap();
        control.release();
        let credentials = waltr_client::NativeCredentialProvider::new();
        let revision = credentials.credential_revision();
        let changed = credentials.clone();
        let observer = Observer::new(move |event| {
            if event == Diagnostic::Stage(boundary) {
                let _lease = changed.clear_bearer_token();
            }
        });
        let guard = || credentials.credential_revision() == revision;
        let mock = Arc::new(Mock::default());
        let result = hold
            .observed(observer)
            .run_guarded(
                &binding,
                &transport,
                Arc::new(MockFactory(mock.clone())),
                &CancellationToken::new(),
                &guard,
            )
            .await;
        assert_eq!(result, Err(Error::Cancelled));
        assert_eq!(
            fake.admissions.load(Ordering::SeqCst),
            usize::from(boundary == Stage::Action)
        );
        assert!(!path.exists());
        assert_eq!(
            mock.started.load(Ordering::SeqCst),
            boundary != Stage::CaptureRequested
        );
        assert_eq!(
            mock.joined.load(Ordering::SeqCst),
            boundary != Stage::CaptureRequested
        );
        assert!(gate.reserve().is_ok());
        stop.cancel();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn release_before_start_flushes_late_output_once_and_literal_command() {
    let (transport, fake, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("result");
    let binding = binding(&path, "test");
    let gate = AudioAdmission::default();
    let (hold, control) = Hold::press(&gate).unwrap();
    assert_eq!(Hold::press(&gate).unwrap_err(), Error::Busy);
    control.release();
    control.release();
    let mock = Arc::new(Mock::default());
    let status = control.status();
    hold.run(
        &binding,
        &transport,
        Arc::new(MockFactory(mock.clone())),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(*status.borrow(), Status::Finished(Ok(())));
    assert!(mock.stopped.load(Ordering::SeqCst));
    assert!(mock.joined.load(Ordering::SeqCst));
    assert_eq!(fake.uploads.load(Ordering::SeqCst), 100);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "literal $(touch nope); '{{ invocationId }}'"
    );
    assert!(Hold::press(&gate).is_ok());
    stop.cancel();
    server.await.unwrap();
}
#[tokio::test]
async fn failed_and_stale_postrelease_output_never_execute() {
    let (transport, fake, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("result");
    for name in ["failed", "uncommitted", "incomplete", "large", "test"] {
        fake.completed.store(false, Ordering::SeqCst);
        let binding = binding(&path, name);
        let (hold, control) = Hold::press(&AudioAdmission::default()).unwrap();
        control.release();
        let generation = CancellationToken::new();
        let stale = generation.clone();
        let invalidate = async {
            if name == "test" {
                while !fake.completed.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
                stale.cancel();
            }
        };
        let mock = Arc::new(Mock::default());
        let (result, ()) = tokio::join!(
            hold.run(
                &binding,
                &transport,
                Arc::new(MockFactory(mock.clone())),
                &generation
            ),
            invalidate
        );
        assert!(result.is_err());
        if name == "large" {
            assert_eq!(result, Err(Error::Interaction));
        }
        assert!(!path.exists());
        assert!(mock.joined.load(Ordering::SeqCst));
    }
    stop.cancel();
    server.await.unwrap();
}
#[tokio::test]
async fn blocked_startup_cancel_and_release_retain_worker_until_join() {
    let (transport, _fake, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(&dir.path().join("never"), "pending");
    for cancel in [false, true] {
        let (tx, rx) = std::sync::mpsc::channel();
        let mock = Arc::new(Mock {
            startup: Mutex::new(Some(rx)),
            ..Default::default()
        });
        let (hold, control) = Hold::press(&AudioAdmission::default()).unwrap();
        let generation = CancellationToken::new();
        let events = async {
            while !mock.started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            if cancel {
                control.cancel();
            } else {
                control.release();
            }
            assert!(!mock.joined.load(Ordering::SeqCst));
            tx.send(()).unwrap();
            if !cancel {
                while !mock.stopped.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
                control.cancel();
            }
        };
        let (result, ()) = tokio::join!(
            hold.run(
                &binding,
                &transport,
                Arc::new(MockFactory(mock.clone())),
                &generation
            ),
            events
        );
        assert_eq!(result, Err(Error::Cancelled));
        assert!(mock.joined.load(Ordering::SeqCst));
    }
    stop.cancel();
    server.await.unwrap();
}
#[tokio::test]
async fn preaccept_full_queue_fails_and_joins_without_command() {
    let (transport, fake, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("never");
    let binding = binding(&path, "pending");
    let mock = Arc::new(Mock {
        burst: AtomicBool::new(true),
        admit_burst: Mutex::new(Some(fake.admissions.clone())),
        ..Default::default()
    });
    let (hold, _control) = Hold::press(&AudioAdmission::default()).unwrap();
    let diagnostics = Arc::new(Mutex::new(Vec::new()));
    let events = diagnostics.clone();
    let joined = mock.clone();
    let observer = waltr_desktop::host::Observer::new(move |event| {
        events
            .lock()
            .unwrap()
            .push((event, joined.joined.load(Ordering::SeqCst)));
    });
    let result = hold
        .observed(observer)
        .run(
            &binding,
            &transport,
            Arc::new(MockFactory(mock.clone())),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(result, Err(Error::Capture(capture::Error::Overflow)));
    assert_eq!(fake.admissions.load(Ordering::SeqCst), 1);
    assert_eq!(fake.uploads.load(Ordering::SeqCst), 0);
    {
        use waltr_desktop::host::{Diagnostic, Stage};
        let diagnostics = diagnostics.lock().unwrap();
        assert_eq!(
            diagnostics
                .iter()
                .filter(|(event, _)| *event == Diagnostic::Stage(Stage::CaptureReady))
                .count(),
            1
        );
        assert!(
            !diagnostics
                .iter()
                .any(|(event, _)| matches!(event, Diagnostic::Stage(Stage::Accepted)))
        );
        assert!(diagnostics.iter().any(|(event, _)| *event
            == Diagnostic::Primary(Err(Error::Capture(capture::Error::Overflow)))));
        assert!(
            diagnostics
                .iter()
                .any(|(event, joined)| matches!(event, Diagnostic::CaptureCleanup(_)) && *joined)
        );
        drop(diagnostics);
    }
    assert!(mock.joined.load(Ordering::SeqCst));
    assert!(!path.exists());
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn capture_failure_is_primary_before_matching_join_cleanup() {
    use waltr_desktop::host::{Diagnostic, Observer, Stage};
    let (transport, _fake, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("never");
    let binding = binding(&path, "test");
    let mock = Arc::new(Mock {
        fail_on_stop: AtomicBool::new(true),
        empty: AtomicBool::new(true),
        ..Default::default()
    });
    let diagnostics = Arc::new(Mutex::new(Vec::new()));
    let seen = diagnostics.clone();
    let (hold, control) = Hold::press(&AudioAdmission::default()).unwrap();
    control.release();
    let result = hold
        .observed(Observer::new(move |event| seen.lock().unwrap().push(event)))
        .run(
            &binding,
            &transport,
            Arc::new(MockFactory(mock.clone())),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(result, Err(Error::Capture(capture::Error::Backend)));
    assert!(mock.joined.load(Ordering::SeqCst));
    let diagnostics = diagnostics.lock().unwrap();
    let position = |event| {
        diagnostics
            .iter()
            .position(|candidate| *candidate == event)
            .unwrap()
    };
    let primary = position(Diagnostic::Primary(Err(Error::Capture(
        capture::Error::Backend,
    ))));
    assert!(primary < position(Diagnostic::CaptureCleanup(Err(capture::Error::Backend))));
    assert!(primary < position(Diagnostic::Stage(Stage::Cleanup)));
    assert!(!format!("{diagnostics:?}").contains("SECRET"));
    assert!(!path.exists());
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn validated_remote_terminals_are_not_protocol_failures() {
    use waltr_desktop::host::{Diagnostic, Observer, RpcFailure};
    let (transport, _fake, stop, server) = fixture().await;
    for (routine, expected) in [
        ("early-failed", RpcFailure::RemoteFailed),
        ("early-interrupted", RpcFailure::RemoteInterrupted),
        ("early-rejected", RpcFailure::Rejected),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never");
        let binding = binding(&path, routine);
        let mock = Arc::new(Mock::default());
        let diagnostics = Arc::new(Mutex::new(Vec::new()));
        let seen = diagnostics.clone();
        let (hold, _control) = Hold::press(&AudioAdmission::default()).unwrap();
        let result = hold
            .observed(Observer::new(move |event| seen.lock().unwrap().push(event)))
            .run(
                &binding,
                &transport,
                Arc::new(MockFactory(mock.clone())),
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(result, Err(Error::Interaction));
        assert!(mock.joined.load(Ordering::SeqCst));
        let diagnostics = diagnostics.lock().unwrap();
        assert!(
            diagnostics.contains(&Diagnostic::RpcFailure(expected)),
            "{diagnostics:?}"
        );
        assert!(
            !diagnostics.contains(&Diagnostic::RpcFailure(RpcFailure::Protocol)),
            "{diagnostics:?}"
        );
        assert!(!format!("{diagnostics:?}").contains("SECRET"));
        assert!(!path.exists());
    }
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn rpc_waits_for_first_audio_then_preserves_protocol_boundary() {
    use waltr_desktop::host::{Diagnostic, Observer, RpcFailure, Stage};
    let (transport, _fake, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("never");
    let binding = binding(&path, "early-protocol");
    let (unblock, blocked) = std::sync::mpsc::channel();
    let mock = Arc::new(Mock {
        startup: Mutex::new(Some(blocked)),
        ..Default::default()
    });
    let diagnostics = Arc::new(Mutex::new(Vec::new()));
    let seen = diagnostics.clone();
    let observer = Observer::new(move |event| {
        seen.lock().unwrap().push(event);
    });
    let (hold, _control) = Hold::press(&AudioAdmission::default()).unwrap();
    let generation = CancellationToken::new();
    let run = hold.observed(observer).run(
        &binding,
        &transport,
        Arc::new(MockFactory(mock.clone())),
        &generation,
    );
    let startup = async {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !mock.started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(_fake.admissions.load(Ordering::SeqCst), 0);
        unblock.send(()).unwrap();
    };
    let (result, ()) = tokio::join!(run, startup);
    assert_eq!(result, Err(Error::Interaction));
    assert!(mock.joined.load(Ordering::SeqCst));
    let diagnostics = diagnostics.lock().unwrap().clone();
    let position = |event| {
        diagnostics
            .iter()
            .position(|candidate| *candidate == event)
            .unwrap()
    };
    assert!(
        position(Diagnostic::Stage(Stage::Accepted))
            < position(Diagnostic::RpcFailure(RpcFailure::Output))
    );
    assert!(
        position(Diagnostic::Stage(Stage::FirstFrameObserved))
            < position(Diagnostic::Stage(Stage::RpcOpen))
    );
    assert!(diagnostics.contains(&Diagnostic::Stage(Stage::FirstFrameObserved)));
    assert!(!path.exists());
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn early_successful_eof_cancels_capture_without_release_or_terminal_command() {
    let (transport, _, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("never");
    let binding = binding(&path, "early-success");
    let gate = AudioAdmission::default();
    let (hold, _control) = Hold::press(&gate).unwrap();
    let mock = Arc::new(Mock::default());
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        hold.run(
            &binding,
            &transport,
            Arc::new(MockFactory(mock.clone())),
            &CancellationToken::new(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(result, Err(Error::Interaction));
    assert!(mock.joined.load(Ordering::SeqCst));
    assert!(gate.reserve().is_ok());
    assert!(!path.exists());
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn missing_or_mismatched_current_selection_never_captures_or_admits() {
    let (transport, fake, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    for missing in [true, false] {
        let mut binding = binding(&dir.path().join("never"), "test");
        if missing {
            binding.resolved = None;
        } else {
            binding.resolved.as_mut().unwrap().selection.entrypoint = "different".into();
        }
        let mock = Arc::new(Mock::default());
        let (hold, _) = Hold::press(&AudioAdmission::default()).unwrap();
        assert_eq!(
            hold.run(
                &binding,
                &transport,
                Arc::new(MockFactory(mock.clone())),
                &CancellationToken::new()
            )
            .await,
            Err(Error::Unsupported)
        );
        assert!(!mock.started.load(Ordering::SeqCst));
        assert_eq!(fake.admissions.load(Ordering::SeqCst), 0);
    }
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn shared_pipeline_waits_for_acceptance_then_closes_and_finishes_in_order() {
    let (transport, fake, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("once");
    let binding = binding(&path, "acceptance-gate");
    let mock = Arc::new(Mock::default());
    let diagnostics = Arc::new(Mutex::new(Vec::new()));
    let seen = diagnostics.clone();
    let (hold, control) = Hold::press(&AudioAdmission::default()).unwrap();
    control.release();
    assert_eq!(
        hold.observed(waltr_desktop::host::Observer::new(move |event| seen
            .lock()
            .unwrap()
            .push(event)))
            .run(
                &binding,
                &transport,
                Arc::new(MockFactory(mock.clone())),
                &CancellationToken::new()
            )
            .await,
        Ok(())
    );
    assert_eq!(fake.uploads.load(Ordering::SeqCst), 100);
    assert!(fake.completed.load(Ordering::SeqCst));
    assert!(mock.joined.load(Ordering::SeqCst));
    let events = diagnostics.lock().unwrap();
    assert!(events.contains(&waltr_desktop::host::Diagnostic::CaptureCleanup(Ok(()))));
    assert!(events.contains(&waltr_desktop::host::Diagnostic::RemoteCleanup(true)));
    drop(events);
    assert!(path.exists());
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn small_frames_survive_delayed_open_and_accepted_with_exact_pcm_after_release() {
    for routine in [
        "delay-open-500",
        "delay-open-5000",
        "delay-open-19900",
        "delay-accepted-500",
        "delay-accepted-5000",
        "delay-accepted-19900",
    ] {
        let (transport, fake, stop, server) = fixture().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("once");
        let binding = binding(&path, routine);
        let mock = Arc::new(Mock {
            small_chunks: AtomicBool::new(true),
            ..Default::default()
        });
        let (hold, control) = Hold::press(&AudioAdmission::default()).unwrap();
        let release = async {
            while !fake.delay_waiting.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            assert_eq!(fake.uploads.load(Ordering::SeqCst), 0);
            control.release();
            let millis = routine.rsplit('-').next().unwrap().parse::<u64>().unwrap();
            tokio::time::pause();
            tokio::time::advance(Duration::from_millis(millis)).await;
            tokio::time::resume();
        };
        let generation = CancellationToken::new();
        let (result, ()) = tokio::time::timeout(Duration::from_secs(30), async {
            tokio::join!(
                hold.run(
                    &binding,
                    &transport,
                    Arc::new(MockFactory(mock.clone())),
                    &generation
                ),
                release
            )
        })
        .await
        .unwrap();
        assert_eq!(result, Ok(()), "{routine}");
        let expected: Vec<u8> = (0..120u8).rev().flat_map(|value| vec![value; 96]).collect();
        assert_eq!(*fake.pcm.lock().unwrap(), expected, "{routine}");
        assert!(mock.joined.load(Ordering::SeqCst));
        assert!(path.exists());
        stop.cancel();
        server.await.unwrap();
    }
}

struct ChatConsumer(AtomicUsize);
#[tokio::test]
async fn production_capture_formats_reach_capture_and_transport() {
    use waltr_desktop::{interaction, typing::Guard};
    let (transport, fake, stop, server) = fixture().await;
    let mut invocation = support::invocation("test");
    invocation.limits.definition.ports[0].formats = vec!["pcm16le-24000-mono".into()];
    invocation.limits.definition.ports[1].formats = vec!["utf8".into()];
    let mock = Arc::new(Mock::default());
    let release = CancellationToken::new();
    release.cancel();
    let cancel = CancellationToken::new();
    let generation = CancellationToken::new();
    let guard = Guard {
        cancel: &cancel,
        generation: &generation,
        valid: &|| true,
    };
    let consumer = ChatConsumer(AtomicUsize::new(0));
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        interaction::run(
            &transport,
            Arc::new(MockFactory(mock.clone())),
            &config::Audio {
                device: config::DeviceSelector::Default,
                sample_rate_hz: 24000,
                channels: 1,
            },
            interaction::Selection {
                invocation: &invocation,
                input: "audio",
                output: "clean-text",
                max_capture_seconds: None,
            },
            &release,
            &guard,
            &Default::default(),
            &consumer,
        ),
    )
    .await
    .unwrap();
    stop.cancel();
    server.await.unwrap();
    assert!(matches!(result, Ok(Some(_))), "{result:?}");
    assert!(mock.started.load(Ordering::SeqCst));
    assert!(mock.joined.load(Ordering::SeqCst));
    assert_eq!(fake.admissions.load(Ordering::SeqCst), 1);
    assert_eq!(fake.uploads.load(Ordering::SeqCst), 100);
    assert_eq!(consumer.0.load(Ordering::SeqCst), 1);
}

#[derive(Default)]
struct NativeConsumer {
    revoked: std::cell::Cell<bool>,
    blocked: bool,
    fail_finish: bool,
    bytes: std::cell::Cell<usize>,
    closes: std::cell::Cell<usize>,
    finished: std::cell::Cell<Option<bool>>,
}
impl waltr_desktop::media::OutputSink for NativeConsumer {
    fn revoke(&self) {
        self.revoked.set(true);
    }
    fn supports(&self, port: &waltr_client::RoutinePort) -> bool {
        port.media == waltr_client::RoutineMedia::Audio && port.formats == ["pcm16le-24000-mono"]
    }
    fn write<'a>(
        &'a self,
        frame: waltr_desktop::media::Frame<'a>,
        guard: &'a waltr_desktop::typing::Guard<'_>,
    ) -> waltr_desktop::typing::LocalFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            guard.check().map_err(|_| Error::Cancelled)?;
            if let waltr_desktop::media::Frame::Stream(event) = frame {
                match event {
                    waltr_client::interaction::InteractionEvent::Chunk(chunk) => {
                        if let Some(wire::stream_chunk::Payload::Audio(bytes)) = &chunk.payload {
                            assert!(bytes.len() <= 65536);
                            if self.blocked {
                                std::future::pending::<()>().await;
                            }
                            self.bytes.set(self.bytes.get() + bytes.len());
                        }
                    }
                    waltr_client::interaction::InteractionEvent::Close(close)
                        if close.stream_id == "speaker" =>
                    {
                        self.closes.set(self.closes.get() + 1)
                    }
                    _ => {}
                }
            }
            Ok(())
        })
    }
    fn finish<'a>(
        &'a self,
        success: bool,
        _guard: &'a waltr_desktop::typing::Guard<'_>,
    ) -> waltr_desktop::typing::LocalFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            assert!(self.finished.replace(Some(success)).is_none());
            if self.fail_finish {
                Err(Error::Interaction)
            } else {
                Ok(())
            }
        })
    }
}
impl waltr_desktop::interaction::Consumer for NativeConsumer {
    fn sink(&self) -> Option<&dyn waltr_desktop::media::OutputSink> {
        Some(self)
    }
    fn progress(&self, _: waltr_desktop::interaction::Progress) {}
    fn output<'a>(
        &'a self,
        _: &'a waltr_client::interaction::InteractionOutput,
        _: &'a str,
        _: Option<&'a str>,
        _: &'a waltr_desktop::typing::Guard<'_>,
    ) -> waltr_desktop::typing::LocalFuture<'a, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}

struct SequentialFactory {
    sources: Vec<Arc<Mock>>,
    opens: AtomicUsize,
}
impl Factory for SequentialFactory {
    fn open(&self, config: waltr_audio_io::Config) -> Result<Box<dyn Source>, capture::Error> {
        let index = self.opens.fetch_add(1, Ordering::SeqCst);
        for source in &self.sources[..index] {
            assert!(
                source.joined.load(Ordering::SeqCst),
                "previous microphone must be joined before opening the next"
            );
        }
        MockFactory(self.sources[index].clone()).open(config)
    }
}

fn repeated_binding() -> config::Keybind {
    let mut binding = binding(std::path::Path::new("unused"), "repeated");
    binding.command = None;
    binding.playback = Some(config::PlaybackSpec {});
    binding.routine.output = "speaker".into();
    let mut invocation = support::invocation("pcm");
    invocation.selection.routine = "repeated".into();
    invocation.limits.definition.routine_name = "repeated".into();
    invocation
        .limits
        .definition
        .ports
        .retain(|p| p.name != "clean-text");
    invocation
        .limits
        .definition
        .capabilities
        .push("media.inputs".into());
    binding.resolved = Some(invocation);
    binding
}

#[tokio::test]
async fn repeated_holds_share_rpc_playback_and_join_sequential_microphones() {
    for case in [
        "playback",
        "startup-release",
        "cancel",
        "disconnect",
        "seal-race",
        "deadline",
        "byte-limit",
        "empty-initial",
    ] {
        let (transport, fake, stop, server) = fixture().await;
        let mut binding = repeated_binding();
        if case == "disconnect" {
            binding.routine.name = "repeated-disconnect".into();
            let invocation = binding.resolved.as_mut().unwrap();
            invocation.selection.routine = "repeated-disconnect".into();
            invocation.limits.definition.routine_name = "repeated-disconnect".into();
        }
        if case == "deadline" {
            binding.max_capture_seconds = Some(1);
        }
        if case == "byte-limit" {
            binding.resolved.as_mut().unwrap().limits.definition.ports[0].max_bytes =
                waltr_client::RoutineRevision(150);
        }
        let (startup_tx, startup_rx) = std::sync::mpsc::channel();
        let mut startup_rx = Some(startup_rx);
        let sources: Vec<_> = (0..3)
            .map(|index| {
                Arc::new(Mock {
                    empty: AtomicBool::new(case == "empty-initial" && index == 0),
                    startup: Mutex::new(
                        if index == 1 && matches!(case, "startup-release" | "cancel") {
                            startup_rx.take()
                        } else {
                            None
                        },
                    ),
                    ..Default::default()
                })
            })
            .collect();
        let factory = Arc::new(SequentialFactory {
            sources: sources.clone(),
            opens: AtomicUsize::new(0),
        });
        let sink = NativeConsumer::default();
        let gate = AudioAdmission::default();
        let (hold, control) = Hold::press_repeated(&gate).unwrap();
        control.release(); // Also proves pending release for initial capture startup.
        if case == "empty-initial" {
            control.press_again().unwrap().release();
        }
        let generation = CancellationToken::new();
        let drive = async {
            while sink.bytes.get() == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            assert_eq!(fake.repeated_closes.load(Ordering::SeqCst), 1);
            assert!(
                !fake
                    .repeated_input
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|e| matches!(e.event, Some(wire::input_event::Event::Finish(_))))
            );
            assert!(
                matches!(Hold::press(&gate), Err(Error::Busy)),
                "unrelated operations remain busy through playback"
            );
            let second = control.press_again().unwrap();
            assert!(
                matches!(second.press_again(), Err(Error::Busy)),
                "no duplicate press while still held"
            );
            if !matches!(case, "deadline" | "disconnect") {
                second.release();
            }
            if case == "seal-race" {
                let third = second.press_again().unwrap();
                third.release();
            }
            if matches!(case, "startup-release" | "cancel") {
                while !sources[1].started.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                if case == "cancel" {
                    generation.cancel();
                }
                startup_tx.send(()).unwrap();
            }
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                hold.run_guarded_with_outputs(
                    &binding,
                    &transport,
                    factory.clone(),
                    &generation,
                    &|| true,
                    None,
                    Some(&sink)
                ),
                drive
            )
        })
        .await
        .unwrap();
        if matches!(case, "cancel" | "disconnect" | "seal-race" | "byte-limit") {
            assert!(result.is_err(), "{case}: {result:?}");
            assert!(sink.revoked.get());
        } else {
            assert_eq!(result, Ok(()), "{case}");
            assert_eq!(
                factory.opens.load(Ordering::SeqCst),
                if case == "empty-initial" { 3 } else { 2 }
            );
            assert_eq!(sink.closes.get(), 1);
            assert_eq!(sink.finished.get(), Some(true));
            let events = fake.repeated_input.lock().unwrap();
            let kinds: Vec<_> = events
                .iter()
                .map(|e| match e.event.as_ref().unwrap() {
                    wire::input_event::Event::Open(o) => format!("open:{}", o.stream_id),
                    wire::input_event::Event::Chunk(c) => {
                        assert_eq!(c.sequence, 0);
                        format!("chunk:{}", c.stream_id)
                    }
                    wire::input_event::Event::Close(c) => {
                        assert_eq!(c.next_sequence, 1);
                        format!("close:{}", c.stream_id)
                    }
                    wire::input_event::Event::Finish(_) => "finish".into(),
                    _ => panic!("unexpected input"),
                })
                .collect();
            assert_eq!(
                kinds,
                [
                    "open:audio-0",
                    "chunk:audio-0",
                    "close:audio-0",
                    "open:audio-1",
                    "chunk:audio-1",
                    "close:audio-1",
                    "finish"
                ]
            );
        }
        assert_eq!(fake.admissions.load(Ordering::SeqCst), 1);
        for source in &sources[..factory.opens.load(Ordering::SeqCst)] {
            assert!(
                source.joined.load(Ordering::SeqCst),
                "{case}: capture joined"
            );
        }
        assert!(control.press_again().is_err());
        let (_next, next_control) = Hold::press(&gate).unwrap();
        assert_eq!(*next_control.status().borrow(), Status::Starting);
        stop.cancel();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn repeated_initial_empty_capture_without_followup_opens_no_rpc() {
    let (transport, fake, stop, server) = fixture().await;
    let binding = repeated_binding();
    let source = Arc::new(Mock {
        empty: AtomicBool::new(true),
        ..Default::default()
    });
    let sink = NativeConsumer::default();
    let (hold, control) = Hold::press_repeated(&AudioAdmission::default()).unwrap();
    control.release();
    let result = hold
        .run_guarded_with_outputs(
            &binding,
            &transport,
            Arc::new(MockFactory(source.clone())),
            &CancellationToken::new(),
            &|| true,
            None,
            Some(&sink),
        )
        .await;
    assert_eq!(result, Ok(()));
    assert!(source.joined.load(Ordering::SeqCst));
    assert_eq!(fake.admissions.load(Ordering::SeqCst), 0);
    assert!(control.press_again().is_err());
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn typed_pcm_sink_is_explicit_and_finishes_after_validated_streams() {
    use waltr_desktop::{interaction, typing::Guard};
    let (transport, fake, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(&dir.path().join("never"), "pcm");
    let mock = Arc::new(Mock::default());
    let release = CancellationToken::new();
    release.cancel();
    let cancel = CancellationToken::new();
    let generation = CancellationToken::new();
    let guard = Guard {
        cancel: &cancel,
        generation: &generation,
        valid: &|| true,
    };
    let selection = || interaction::Selection {
        invocation: binding.resolved.as_ref().unwrap(),
        input: "audio",
        output: "clean-text",
        max_capture_seconds: None,
    };
    assert!(matches!(
        interaction::run(
            &transport,
            Arc::new(MockFactory(mock.clone())),
            &binding.audio,
            selection(),
            &release,
            &guard,
            &Default::default(),
            &ChatConsumer(AtomicUsize::new(0))
        )
        .await,
        Err(Error::Unsupported)
    ));
    assert!(!mock.started.load(Ordering::SeqCst));
    assert_eq!(fake.admissions.load(Ordering::SeqCst), 0);
    let consumer = NativeConsumer::default();
    let result = interaction::run(
        &transport,
        Arc::new(MockFactory(mock.clone())),
        &binding.audio,
        selection(),
        &release,
        &guard,
        &Default::default(),
        &consumer,
    )
    .await;
    assert!(matches!(result, Ok(Some(_))));
    assert_eq!(consumer.bytes.get(), 4);
    assert_eq!(consumer.closes.get(), 1);
    assert_eq!(consumer.finished.get(), Some(true));
    assert!(mock.joined.load(Ordering::SeqCst));
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn rpc_input_failure_revokes_queued_pcm_before_blocked_remote_cleanup() {
    use waltr_desktop::{interaction, typing::Guard};
    let (transport, fake, stop, server) = fixture().await;
    let mut invocation = support::invocation("playback-failure");
    let pcm = support::invocation("pcm");
    invocation.limits.definition.ports.push(
        pcm.limits
            .definition
            .ports
            .iter()
            .find(|p| p.name == "speaker")
            .unwrap()
            .clone(),
    );
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(&dir.path().join("never"), "playback-failure");
    let mock = Arc::new(Mock::default());
    let release = CancellationToken::new();
    let cancel = CancellationToken::new();
    let generation = CancellationToken::new();
    let guard = Guard {
        cancel: &cancel,
        generation: &generation,
        valid: &|| true,
    };
    let sink = NativeConsumer::default();
    let observer = Default::default();
    let run = interaction::run(
        &transport,
        Arc::new(MockFactory(mock.clone())),
        &binding.audio,
        interaction::Selection {
            invocation: &invocation,
            input: "audio",
            output: "clean-text",
            max_capture_seconds: None,
        },
        &release,
        &guard,
        &observer,
        &sink,
    );
    let inspect = async {
        while !sink.revoked.get() {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sink.bytes.get(),
            4,
            "PCM reached the output owner before failure"
        );
        assert!(fake.cleanup_blocked.load(Ordering::SeqCst));
        assert_eq!(
            sink.finished.get(),
            None,
            "revocation precedes the retained owner's join"
        );
        assert!(
            !cancel.is_cancelled(),
            "the driver does not own the caller's token"
        );
        assert!(!generation.is_cancelled());
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            sink.finished.get(),
            None,
            "remote EOF must still block cleanup"
        );
        fake.cleanup_release.notify_one();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(3), async { tokio::join!(run, inspect) })
            .await
            .unwrap();
    assert!(result.is_err());
    assert!(mock.joined.load(Ordering::SeqCst));
    assert_eq!(sink.finished.get(), Some(false));
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn audio_only_hold_release_joins_sink_and_rejects_cancel_revocation_source_and_drain_failures()
 {
    let (transport, fake, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    for case in ["success", "cancel", "revoked", "source", "drain"] {
        let mut binding = binding(&dir.path().join("never"), "audio-answer");
        binding.command = None;
        binding.playback = Some(config::PlaybackSpec {});
        binding.routine.output = "speaker".into();
        let ports = &mut binding.resolved.as_mut().unwrap().limits.definition.ports;
        ports.retain(|p| p.direction != waltr_client::RoutineDirection::Output);
        ports.push(waltr_client::RoutinePort {
            name: "speaker".into(),
            direction: waltr_client::RoutineDirection::Output,
            media: waltr_client::RoutineMedia::Audio,
            required: true,
            max_bytes: waltr_client::RoutineRevision(65536),
            formats: vec!["pcm16le-24000-mono".into()],
        });
        let gate = AudioAdmission::default();
        let (hold, control) = Hold::press(&gate).unwrap();
        let mock = Arc::new(Mock::default());
        mock.fail_on_stop.store(case == "source", Ordering::SeqCst);
        let generation = CancellationToken::new();
        let valid = std::cell::Cell::new(true);
        let sink = NativeConsumer {
            blocked: matches!(case, "cancel" | "revoked"),
            fail_finish: case == "drain",
            ..Default::default()
        };
        let valid_fn = || valid.get();
        let run = hold.run_guarded_with_outputs(
            &binding,
            &transport,
            Arc::new(MockFactory(mock.clone())),
            &generation,
            &valid_fn,
            None,
            Some(&sink),
        );
        let steer = async {
            while !mock.started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            control.release();
            if matches!(case, "cancel" | "revoked") {
                tokio::time::sleep(Duration::from_millis(80)).await;
                if case == "cancel" {
                    control.cancel();
                } else {
                    valid.set(false);
                }
            }
        };
        let (result, ()) =
            tokio::time::timeout(Duration::from_secs(3), async { tokio::join!(run, steer) })
                .await
                .unwrap();
        assert_eq!(result.is_ok(), case == "success", "{case}: {result:?}");
        assert!(mock.joined.load(Ordering::SeqCst), "{case}");
        assert!(gate.reserve().is_ok(), "{case}");
        assert!(sink.finished.get().is_some(), "{case}");
        if case != "success" {
            assert!(sink.revoked.get(), "{case}");
        }
        if case == "success" {
            assert_eq!(sink.bytes.get(), 4);
            assert_eq!(sink.finished.get(), Some(true));
            assert!(fake.completed.load(Ordering::SeqCst));
        } else if case != "drain" {
            assert_eq!(sink.finished.get(), Some(false));
            assert!(sink.revoked.get(), "{case}");
        }
    }
    stop.cancel();
    server.await.unwrap();
}
impl waltr_desktop::interaction::Consumer for ChatConsumer {
    fn progress(&self, _: waltr_desktop::interaction::Progress) {}
    fn output<'a>(
        &'a self,
        _: &'a waltr_client::interaction::InteractionOutput,
        _: &'a str,
        final_text: Option<&'a str>,
        guard: &'a waltr_desktop::typing::Guard<'_>,
    ) -> waltr_desktop::typing::LocalFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            guard.check().map_err(|_| Error::Cancelled)?;
            if final_text.is_some() {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        })
    }
}

#[tokio::test]
async fn shared_chat_consumer_releases_or_cancels_with_small_frames_before_admission() {
    use waltr_desktop::{host::Observer, interaction, typing::Guard};
    for cancel_before_acceptance in [false, true] {
        let (transport, fake, stop, server) = fixture().await;
        let dir = tempfile::tempdir().unwrap();
        let binding = binding(&dir.path().join("unused"), "delay-accepted-250");
        let mock = Arc::new(Mock {
            small_chunks: AtomicBool::new(true),
            ..Default::default()
        });
        let cancel = CancellationToken::new();
        let generation = CancellationToken::new();
        let release = CancellationToken::new();
        let guard = Guard {
            cancel: &cancel,
            generation: &generation,
            valid: &|| true,
        };
        let consumer = ChatConsumer(AtomicUsize::new(0));
        let control = async {
            while fake.admissions.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            // Give the source time to queue all 120 small frames, without admission.
            tokio::time::sleep(Duration::from_millis(20)).await;
            if cancel_before_acceptance {
                cancel.cancel();
            } else {
                release.cancel();
            }
        };
        let observer = Observer::default();
        let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(
                interaction::run(
                    &transport,
                    Arc::new(MockFactory(mock.clone())),
                    &binding.audio,
                    interaction::Selection {
                        invocation: binding.resolved.as_ref().unwrap(),
                        input: "audio",
                        output: "clean-text",
                        max_capture_seconds: None,
                    },
                    &release,
                    &guard,
                    &observer,
                    &consumer,
                ),
                control
            )
        })
        .await
        .unwrap();
        assert!(mock.joined.load(Ordering::SeqCst));
        if cancel_before_acceptance {
            assert!(matches!(result, Err(Error::Cancelled)));
            assert_eq!(fake.uploads.load(Ordering::SeqCst), 0);
            assert_eq!(consumer.0.load(Ordering::SeqCst), 0);
        } else {
            assert!(result.unwrap().is_some());
            let expected: Vec<u8> = (0..120u8).rev().flat_map(|value| vec![value; 96]).collect();
            assert_eq!(*fake.pcm.lock().unwrap(), expected);
            assert_eq!(consumer.0.load(Ordering::SeqCst), 1);
        }
        stop.cancel();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn actor_routes_release_ignores_repeat_and_busy_other_binding() {
    use waltr_desktop::{
        actor::{Actor, ActorError},
        shortcut::{Effective, Router},
    };
    let (transport, _fake, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("once");
    let first = binding(&path, "test");
    let mut second = binding(&path, "test");
    second.id = "other".into();
    second.key = "Ctrl+B".into();
    let config = config::Config {
        version: 1,
        keybinds: vec![first, second],
    };
    let mock = Arc::new(Mock::default());
    let (actor, mut status) = Actor::new(
        config,
        transport,
        Arc::new(MockFactory(mock.clone())),
        AudioAdmission::default(),
    )
    .unwrap();
    let (router, mut events) = Router::channel();
    router
        .effective(vec![
            Effective {
                id: "test".into(),
                trigger: "Space".into(),
            },
            Effective {
                id: "other".into(),
                trigger: "B".into(),
            },
        ])
        .unwrap();
    let generation = CancellationToken::new();
    let drive = async {
        router.edge("s", "s", "test", true);
        while !mock.started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        router.edge("s", "s", "test", true); // repeat
        router.edge("s", "s", "other", true); // busy, never queued
        router.edge("s", "s", "other", false);
        router.edge("s", "s", "test", false);
        while !matches!(*status.borrow(), Status::Finished(_)) {
            status.changed().await.unwrap();
        }
        assert_eq!(*status.borrow(), Status::Finished(Ok(())));
        generation.cancel();
    };
    let (result, ()) = tokio::join!(actor.run(&mut events, &generation), drive);
    assert_eq!(result, Err(ActorError::Cancelled));
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "literal $(touch nope); '{{ invocationId }}'"
    );
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn actor_requires_explicit_typing_capability_and_delivers_with_current_fence() {
    use waltr_desktop::{
        actor::{Actor, ActorError},
        shortcut::{Effective, Router},
    };
    let (transport, fake, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let make_config = || config::Config {
        version: 1,
        keybinds: vec![typing_binding(&dir.path().join("never"), "typing")],
    };
    let mock = Arc::new(Mock::default());
    assert!(matches!(
        Actor::new(
            make_config(),
            transport.clone(),
            Arc::new(MockFactory(mock.clone())),
            AudioAdmission::default()
        ),
        Err(ActorError::Unsupported)
    ));
    assert!(!mock.started.load(Ordering::SeqCst));
    let state = std::rc::Rc::new(Typed::default());
    let (_dummy, control) = Hold::press(&AudioAdmission::default()).unwrap();
    let valid = std::rc::Rc::new(std::cell::Cell::new(true));
    let fence = valid.clone();
    let (actor, mut status) = Actor::with_effects(
        make_config(),
        transport,
        Arc::new(MockFactory(mock.clone())),
        AudioAdmission::default(),
        std::rc::Rc::new(TypingFactory {
            state: state.clone(),
            control,
        }),
        std::rc::Rc::new(move || fence.get()),
    )
    .unwrap();
    let (router, mut events) = Router::channel();
    router
        .effective(vec![Effective {
            id: "test".into(),
            trigger: "Space".into(),
        }])
        .unwrap();
    let generation = CancellationToken::new();
    let drive = async {
        router.edge("s", "s", "test", true);
        state.entered.notified().await;
        router.edge("s", "s", "test", false);
        while !matches!(*status.borrow(), Status::Finished(_)) {
            status.changed().await.unwrap();
        }
        assert_eq!(*status.borrow(), Status::Finished(Ok(())));
        assert_eq!(&*state.text.borrow(), "first second 界");
        assert!(state.closed.get());
        assert!(mock.joined.load(Ordering::SeqCst));
        valid.set(false);
        router.edge("s", "s", "test", true);
        // Wait for the next hold result rather than accepting the previous terminal snapshot.
        status.changed().await.unwrap();
        while !matches!(*status.borrow(), Status::Finished(Err(Error::Cancelled))) {
            status.changed().await.unwrap();
        }
        assert_eq!(fake.admissions.load(Ordering::SeqCst), 1);
        generation.cancel();
    };
    let (result, ()) = tokio::join!(actor.run(&mut events, &generation), drive);
    assert_eq!(result, Err(ActorError::Cancelled));
    stop.cancel();
    server.await.unwrap();
}

fn final_events(name: &str) -> Vec<Result<wire::InteractResponse, RpcStatus>> {
    if name == "hour-notes" {
        let text = "x".repeat(512 * 1024 - 64);
        let mut events: Vec<_> = text
            .as_bytes()
            .chunks(60000)
            .enumerate()
            .map(|(i, bytes)| {
                event(Event::Chunk(wire::StreamChunk {
                    stream_id: "out".into(),
                    sequence: i as u64,
                    payload: Some(wire::stream_chunk::Payload::Text(wire::TextRevision {
                        segment_id: "one".into(),
                        text: String::from_utf8(bytes.to_vec()).unwrap(),
                        mutation: 1,
                        committed: (i + 1) * 60000 >= text.len(),
                    })),
                }))
            })
            .collect();
        let count = events.len() as u64;
        events.extend([
            event(Event::Close(wire::StreamClose {
                stream_id: "out".into(),
                next_sequence: count,
            })),
            event(Event::Terminal(wire::InteractionTerminal {
                status: wire::InvocationStatus::Succeeded as i32,
                cleanup_complete: true,
                ..Default::default()
            })),
        ]);
        return events;
    }
    if name.starts_with("typing") {
        return vec![
            text_event(3, "two", "\nsecond\t界", true),
            event(Event::Close(wire::StreamClose {
                stream_id: "out".into(),
                next_sequence: 4,
            })),
            event(Event::Terminal(wire::InteractionTerminal {
                status: wire::InvocationStatus::Succeeded as i32,
                cleanup_complete: true,
                ..Default::default()
            })),
        ];
    }
    let mut events = vec![
        event(Event::Chunk(wire::StreamChunk {
            stream_id: "out".into(),
            sequence: 0,
            payload: Some(wire::stream_chunk::Payload::Text(wire::TextRevision {
                segment_id: "one".into(),
                text: if name == "large" {
                    "x".repeat(256 * 1024 + 1)
                } else {
                    "literal $(touch nope); '{{ invocationId }}'".into()
                },
                mutation: 2,
                committed: name != "uncommitted",
            })),
        })),
        event(Event::Close(wire::StreamClose {
            stream_id: "out".into(),
            next_sequence: 1,
        })),
        event(Event::Terminal(wire::InteractionTerminal {
            status: wire::InvocationStatus::Succeeded as i32,
            cleanup_complete: true,
            ..Default::default()
        })),
    ];
    if name == "incomplete" {
        events.pop();
    }
    events
}

fn text_event(
    sequence: u64,
    id: &str,
    text: &str,
    committed: bool,
) -> Result<wire::InteractResponse, RpcStatus> {
    event(Event::Chunk(wire::StreamChunk {
        stream_id: "out".into(),
        sequence,
        payload: Some(wire::stream_chunk::Payload::Text(wire::TextRevision {
            segment_id: id.into(),
            text: text.into(),
            mutation: 2,
            committed,
        })),
    }))
}

#[derive(Default)]
struct Typed {
    text: std::cell::RefCell<String>,
    first_text: std::cell::RefCell<Option<String>>,
    entered: tokio::sync::Notify,
    close_entered: tokio::sync::Notify,
    close_allowed: tokio::sync::Notify,
    closed: std::cell::Cell<bool>,
    block: bool,
    fail_after: Option<usize>,
    writes: std::cell::Cell<usize>,
}
struct TypingFactory {
    state: std::rc::Rc<Typed>,
    control: waltr_desktop::host::HoldControl,
}
struct TypingSink {
    state: std::rc::Rc<Typed>,
    control: waltr_desktop::host::HoldControl,
}
impl waltr_desktop::typing::Factory for TypingFactory {
    fn create(
        &self,
    ) -> Result<Box<dyn waltr_desktop::typing::Session>, waltr_desktop::typing::Error> {
        Ok(Box::new(TypingSink {
            state: self.state.clone(),
            control: self.control.clone(),
        }))
    }
}
impl waltr_desktop::typing::Session for TypingSink {
    fn open<'a>(
        &'a mut self,
        guard: &'a waltr_desktop::typing::Guard<'_>,
    ) -> waltr_desktop::typing::LocalFuture<'a, Result<(), waltr_desktop::typing::Error>> {
        Box::pin(async move { guard.check() })
    }
    fn write<'a>(
        &'a mut self,
        text: &'a str,
        guard: &'a waltr_desktop::typing::Guard<'_>,
    ) -> waltr_desktop::typing::LocalFuture<'a, Result<(), waltr_desktop::typing::Error>> {
        Box::pin(async move {
            self.state.entered.notify_one();
            if self.state.block {
                std::future::pending::<()>().await;
            }
            guard.check()?;
            if self.state.fail_after == Some(self.state.writes.get()) {
                return Err(waltr_desktop::typing::Error::Backend);
            }
            if self.state.writes.get() == 0 {
                *self.state.first_text.borrow_mut() = Some(text.into());
            }
            self.state.writes.set(self.state.writes.get() + 1);
            self.state.text.borrow_mut().push_str(text);
            self.control.release();
            Ok(())
        })
    }
    fn supports_backspace(&self) -> bool {
        true
    }
    fn backspace<'a>(
        &'a mut self,
        count: usize,
        guard: &'a waltr_desktop::typing::Guard<'_>,
    ) -> waltr_desktop::typing::LocalFuture<'a, Result<(), waltr_desktop::typing::Error>> {
        Box::pin(async move {
            use unicode_segmentation::UnicodeSegmentation;
            guard.check()?;
            let mut text = self.state.text.borrow_mut();
            let keep = text.graphemes(true).count() - count;
            let end: usize = text.graphemes(true).take(keep).map(str::len).sum();
            text.truncate(end);
            Ok(())
        })
    }
    fn finish<'a>(
        &'a mut self,
        guard: &'a waltr_desktop::typing::Guard<'_>,
    ) -> waltr_desktop::typing::LocalFuture<'a, Result<(), waltr_desktop::typing::Error>> {
        Box::pin(async move { guard.check() })
    }
    fn close(
        &mut self,
    ) -> waltr_desktop::typing::LocalFuture<'_, Result<(), waltr_desktop::typing::Error>> {
        Box::pin(async move {
            self.state.close_entered.notify_one();
            if self.state.block {
                self.state.close_allowed.notified().await;
            }
            self.state.closed.set(true);
            Ok(())
        })
    }
}
#[tokio::test]
async fn preaccepted_disconnect_or_rejection_never_uploads_or_runs_an_action() {
    for routine in ["disconnect-before-accepted", "early-rejected"] {
        let (transport, fake, stop, server) = fixture().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never");
        let binding = binding(&path, routine);
        let gate = AudioAdmission::default();
        let (hold, _control) = Hold::press(&gate).unwrap();
        let mock = Arc::new(Mock {
            small_chunks: AtomicBool::new(true),
            ..Default::default()
        });
        assert_eq!(
            hold.run(
                &binding,
                &transport,
                Arc::new(MockFactory(mock.clone())),
                &CancellationToken::new()
            )
            .await,
            Err(Error::Interaction)
        );
        assert_eq!(fake.uploads.load(Ordering::SeqCst), 0);
        assert!(mock.joined.load(Ordering::SeqCst));
        assert!(!path.exists());
        assert!(gate.reserve().is_ok());
        stop.cancel();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn delayed_acceptance_typing_drains_all_pcm_and_closes_its_backend() {
    let (transport, fake, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("never-command");
    let binding = typing_binding(&path, "delay-accepted-5000");
    let gate = AudioAdmission::default();
    let (hold, control) = Hold::press(&gate).unwrap();
    let state = std::rc::Rc::new(Typed::default());
    let factory = TypingFactory {
        state: state.clone(),
        control: control.clone(),
    };
    let mock = Arc::new(Mock {
        small_chunks: AtomicBool::new(true),
        ..Default::default()
    });
    let generation = CancellationToken::new();
    let run = hold.run_guarded_with_typing(
        &binding,
        &transport,
        Arc::new(MockFactory(mock.clone())),
        &generation,
        &|| true,
        Some(&factory),
    );
    let clock = async {
        while !fake.delay_waiting.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        assert!(state.text.borrow().is_empty());
        control.release();
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::time::resume();
    };
    let (result, ()) = tokio::join!(run, clock);
    assert_eq!(result, Ok(()));
    let expected: Vec<u8> = (0..120u8).rev().flat_map(|value| vec![value; 96]).collect();
    assert_eq!(*fake.pcm.lock().unwrap(), expected);
    assert!(!state.text.borrow().is_empty());
    assert!(state.closed.get());
    assert!(mock.joined.load(Ordering::SeqCst));
    assert!(gate.reserve().is_ok());
    assert!(!path.exists());
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn acceptance_deadline_covers_open_and_accepted_and_joins_without_upload() {
    use waltr_desktop::host::{Diagnostic, Observer, RpcFailure};
    for routine in ["delay-open-20100", "delay-accepted-20100"] {
        let (transport, fake, stop, server) = fixture().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never");
        let binding = binding(&path, routine);
        let gate = AudioAdmission::default();
        let (hold, control) = Hold::press(&gate).unwrap();
        control.release();
        let mock = Arc::new(Mock::default());
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = events.clone();
        let generation = CancellationToken::new();
        let run = hold
            .observed(Observer::new(move |event| seen.lock().unwrap().push(event)))
            .run(
                &binding,
                &transport,
                Arc::new(MockFactory(mock.clone())),
                &generation,
            );
        let clock = async {
            while !fake.delay_waiting.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            tokio::time::pause();
            tokio::time::advance(waltr_desktop::interaction::ACCEPTANCE_TIMEOUT).await;
            tokio::time::resume();
        };
        let (result, ()) = tokio::join!(run, clock);
        assert_eq!(result, Err(Error::Interaction), "{routine}");
        assert!(
            events
                .lock()
                .unwrap()
                .contains(&Diagnostic::RpcFailure(RpcFailure::AcceptanceTimeout))
        );
        assert!(mock.joined.load(Ordering::SeqCst));
        assert_eq!(fake.uploads.load(Ordering::SeqCst), 0);
        assert!(!path.exists());
        assert!(gate.reserve().is_ok());
        stop.cancel();
        server.await.unwrap();
    }
}

struct LongFactory {
    uploaded: Arc<AtomicUsize>,
    joined: Arc<AtomicBool>,
}
struct LongSamples {
    uploaded: Arc<AtomicUsize>,
    joined: Arc<AtomicBool>,
    sent: usize,
    stopped: AtomicBool,
}
const LONG_PCM_BYTES: usize = 90 * 24_000 * 2;
impl Factory for LongFactory {
    fn open(&self, _: waltr_audio_io::Config) -> Result<Box<dyn Source>, capture::Error> {
        Ok(Box::new(LongSamples {
            uploaded: self.uploaded.clone(),
            joined: self.joined.clone(),
            sent: 0,
            stopped: AtomicBool::new(false),
        }))
    }
}
impl Source for LongSamples {
    fn frame(&mut self) -> Option<Vec<u8>> {
        // Permit a finite two-second burst, then wait for downstream acknowledgement.
        if self.sent == LONG_PCM_BYTES
            || self
                .sent
                .saturating_sub(self.uploaded.load(Ordering::SeqCst))
                >= 96_000
        {
            return None;
        }
        let bytes = (0..750)
            .flat_map(|index| ((self.sent / 2 + index) as u16).to_le_bytes())
            .collect();
        self.sent += 1500;
        Some(bytes)
    }
    fn terminal(&self) -> Terminal {
        if self.stopped.load(Ordering::SeqCst) {
            Terminal::Completed
        } else {
            Terminal::Running
        }
    }
    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
    fn shutdown(self: Box<Self>) {
        self.joined.store(true, Ordering::SeqCst);
    }
}
#[tokio::test]
async fn ninety_seconds_of_pcm_streams_beyond_backlog_capacity_without_loss_or_duplicates() {
    let (transport, fake, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(&dir.path().join("once"), "test");
    let gate = AudioAdmission::default();
    let (hold, control) = Hold::press(&gate).unwrap();
    let joined = Arc::new(AtomicBool::new(false));
    let factory = Arc::new(LongFactory {
        uploaded: fake.uploads.clone(),
        joined: joined.clone(),
    });
    let generation = CancellationToken::new();
    let run = hold.run(&binding, &transport, factory, &generation);
    let release = async {
        while fake.uploads.load(Ordering::SeqCst) < LONG_PCM_BYTES {
            tokio::task::yield_now().await;
        }
        control.release();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(run, release)
    })
    .await
    .unwrap();
    assert_eq!(result, Ok(()));
    let pcm = fake.pcm.lock().unwrap();
    assert_eq!(pcm.len(), LONG_PCM_BYTES);
    for (index, sample) in pcm.chunks_exact(2).enumerate() {
        assert_eq!(u16::from_le_bytes([sample[0], sample[1]]), index as u16);
    }
    drop(pcm);
    assert!(joined.load(Ordering::SeqCst));
    assert!(gate.reserve().is_ok());
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn accepted_capture_can_outlive_startup_deadline_then_release_cleanly() {
    let (transport, fake, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("once");
    let binding = binding(&path, "test");
    let gate = AudioAdmission::default();
    let (hold, control) = Hold::press(&gate).unwrap();
    let status = control.status();
    let mock = Arc::new(Mock::default());
    let generation = CancellationToken::new();
    let run = hold.run(
        &binding,
        &transport,
        Arc::new(MockFactory(mock.clone())),
        &generation,
    );
    let clock = async {
        while *status.borrow() != Status::Streaming {
            tokio::task::yield_now().await;
        }
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(90)).await;
        tokio::time::resume();
        control.release();
    };
    let (result, ()) = tokio::join!(run, clock);
    assert_eq!(result, Ok(()));
    assert_eq!(fake.uploads.load(Ordering::SeqCst), 100);
    assert!(mock.joined.load(Ordering::SeqCst));
    assert!(path.exists());
    assert!(gate.reserve().is_ok());
    stop.cancel();
    server.await.unwrap();
}

fn typing_binding(path: &std::path::Path, name: &str) -> config::Keybind {
    let mut binding = binding(path, name);
    binding.command = None;
    binding.typing = Some(config::TypingSpec {});
    binding
}

#[tokio::test]
async fn interim_typing_precedes_release_and_keeps_input_alive_without_final_duplication() {
    for name in ["typing", "typing-failed"] {
        let (transport, rpc, stop, server) = fixture().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("must-not-run");
        let binding = typing_binding(&path, name);
        let gate = AudioAdmission::default();
        let (hold, control) = Hold::press(&gate).unwrap();
        let state = std::rc::Rc::new(Typed::default());
        let factory = TypingFactory {
            state: state.clone(),
            control,
        };
        let mock = Arc::new(Mock::default());
        let result = hold
            .run_guarded_with_typing(
                &binding,
                &transport,
                Arc::new(MockFactory(mock.clone())),
                &CancellationToken::new(),
                &|| true,
                Some(&factory),
            )
            .await;
        if name == "typing" {
            assert_eq!(result, Ok(()));
            assert_eq!(*state.text.borrow(), "first second 界");
        } else {
            assert_eq!(result, Err(Error::Interaction));
            assert_eq!(
                *state.text.borrow(),
                "first draft",
                "earlier effects survive failure"
            );
        }
        assert_eq!(state.first_text.borrow().as_deref(), Some("draft"));
        assert_eq!(rpc.uploads.load(Ordering::SeqCst), 100);
        assert!(state.closed.get());
        assert!(mock.joined.load(Ordering::SeqCst));
        assert!(!path.exists());
        assert!(gate.reserve().is_ok());
        stop.cancel();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn backend_failure_stops_without_retry_and_preserves_already_delivered_text() {
    let (transport, _, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let binding = typing_binding(&dir.path().join("never"), "typing");
    let gate = AudioAdmission::default();
    let (hold, control) = Hold::press(&gate).unwrap();
    let state = std::rc::Rc::new(Typed {
        fail_after: Some(1),
        ..Default::default()
    });
    let factory = TypingFactory {
        state: state.clone(),
        control,
    };
    let mock = Arc::new(Mock::default());
    let result = hold
        .run_guarded_with_typing(
            &binding,
            &transport,
            Arc::new(MockFactory(mock.clone())),
            &CancellationToken::new(),
            &|| true,
            Some(&factory),
        )
        .await;
    assert_eq!(
        result,
        Err(Error::Typing(waltr_desktop::typing::Error::Backend))
    );
    assert_eq!(
        *state.text.borrow(),
        "",
        "revision deletion is not retried or rolled back"
    );
    assert_eq!(state.writes.get(), 1);
    assert!(state.closed.get());
    assert!(mock.joined.load(Ordering::SeqCst));
    assert!(gate.reserve().is_ok());
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn typing_without_backend_is_unsupported_without_capture_rpc_or_command() {
    let (transport, rpc, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("never");
    let binding = typing_binding(&path, "typing");
    let gate = AudioAdmission::default();
    let (hold, control) = Hold::press(&gate).unwrap();
    let status = control.status();
    let mock = Arc::new(Mock::default());
    let result = hold
        .run(
            &binding,
            &transport,
            Arc::new(MockFactory(mock.clone())),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(result, Err(Error::Unsupported));
    assert_eq!(*status.borrow(), Status::Finished(result));
    assert!(!mock.started.load(Ordering::SeqCst));
    assert_eq!(rpc.admissions.load(Ordering::SeqCst), 0);
    assert!(!path.exists());
    assert!(gate.reserve().is_ok());
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn cancellation_drops_blocked_write_then_joins_cleanup_before_admission_release() {
    let (transport, _, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let binding = typing_binding(&dir.path().join("never"), "typing");
    let gate = AudioAdmission::default();
    let (hold, control) = Hold::press(&gate).unwrap();
    let state = std::rc::Rc::new(Typed {
        block: true,
        ..Default::default()
    });
    let factory = TypingFactory {
        state: state.clone(),
        control: control.clone(),
    };
    let mock = Arc::new(Mock::default());
    let drive = async {
        state.entered.notified().await;
        assert!(gate.reserve().is_err());
        control.cancel();
        state.close_entered.notified().await;
        assert!(mock.joined.load(Ordering::SeqCst));
        assert!(gate.reserve().is_err());
        assert!(!state.closed.get());
        state.close_allowed.notify_one();
    };
    let generation = CancellationToken::new();
    let (result, ()) = tokio::join!(
        hold.run_guarded_with_typing(
            &binding,
            &transport,
            Arc::new(MockFactory(mock.clone())),
            &generation,
            &|| true,
            Some(&factory)
        ),
        drive
    );
    assert_eq!(result, Err(Error::Cancelled));
    assert!(state.text.borrow().is_empty());
    assert!(state.closed.get());
    assert!(gate.reserve().is_ok());
    stop.cancel();
    server.await.unwrap();
}
