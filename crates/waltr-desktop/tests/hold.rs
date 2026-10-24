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
    interaction::wire::{
        self,
        interact_response::Event,
        routine_interaction_service_server::{
            RoutineInteractionService, RoutineInteractionServiceServer,
        },
    },
};
use waltr_desktop::{
    capture::{self, Factory, Source},
    config,
    host::{AudioAdmission, Error, Hold, Status},
};
type Responses = Pin<Box<dyn Stream<Item = Result<wire::InteractResponse, RpcStatus>> + Send>>;
#[derive(Clone, Default)]
struct Fake {
    admissions: Arc<AtomicUsize>,
    uploads: Arc<AtomicUsize>,
    completed: Arc<AtomicBool>,
    pcm: Arc<Mutex<Vec<u8>>>,
}
#[allow(
    clippy::unnecessary_wraps,
    reason = "fixture events share the fallible RPC stream item type"
)]
const fn event(event: Event) -> Result<wire::InteractResponse, RpcStatus> {
    Ok(wire::InteractResponse { event: Some(event) })
}
#[tonic::async_trait]
impl RoutineInteractionService for Fake {
    type InteractStream = Responses;
    async fn interact(
        &self,
        request: Request<tonic::Streaming<wire::InteractRequest>>,
    ) -> Result<Response<Responses>, RpcStatus> {
        self.admissions.fetch_add(1, Ordering::SeqCst);
        let mut input = request.into_inner();
        let first = input.message().await?.unwrap();
        let Some(wire::interact_request::Event::Open(open)) = first.event else {
            panic!("expected open")
        };
        assert!(
            open.source_session_id.is_none(),
            "production admission forbids source sessions"
        );
        if open.routine_name == "acceptance-gate" {
            let fake = self.clone();
            let output = stream::once(async move {
                assert!(
                    tokio::time::timeout(Duration::from_millis(20), input.message())
                        .await
                        .is_err(),
                    "audio must wait for Accepted"
                );
                let accepted = event(Event::Accepted(wire::InteractionAccepted {
                    invocation_id: "invocation".into(),
                }));
                let tail = stream::once(async move {
                    let mut kinds = Vec::new();
                    while let Some(message) = input.message().await.unwrap() {
                        match message.event.unwrap() {
                            wire::interact_request::Event::InputOpened(_) => kinds.push("open"),
                            wire::interact_request::Event::InputChunk(chunk) => {
                                kinds.push("chunk");
                                if let Some(wire::interaction_input_chunk::Content::Audio(bytes)) =
                                    chunk.content
                                {
                                    fake.uploads.fetch_add(bytes.len(), Ordering::SeqCst);
                                }
                            }
                            wire::interact_request::Event::InputClosed(_) => kinds.push("close"),
                            wire::interact_request::Event::InputCompleted(_) => {
                                kinds.push("finish");
                                break;
                            }
                            _ => panic!("unexpected input"),
                        }
                    }
                    assert_eq!(kinds, ["open", "chunk", "close", "finish"]);
                    fake.completed.store(true, Ordering::SeqCst);
                    let mut events =
                        vec![event(Event::OutputOpened(wire::InteractionOutputOpened {
                            stream_id: "out".into(),
                            port_name: "clean-text".into(),
                            format: Some(wire::InteractionStreamFormat {
                                content: Some(wire::interaction_stream_format::Content::Text(
                                    wire::InteractionTextFormat {},
                                )),
                            }),
                        }))];
                    events.extend(final_events("test"));
                    stream::iter(events)
                })
                .flatten();
                stream::iter([accepted]).chain(tail)
            })
            .flatten();
            return Ok(Response::new(Box::pin(output)));
        }
        if open.routine_name == "pending" {
            while input.message().await?.is_some() {}
            return Err(RpcStatus::cancelled("closed before admission"));
        }
        if open.routine_name == "early-success" {
            let mut events = vec![
                event(Event::Accepted(wire::InteractionAccepted {
                    invocation_id: "invocation".into(),
                })),
                event(Event::OutputOpened(wire::InteractionOutputOpened {
                    stream_id: "out".into(),
                    port_name: "clean-text".into(),
                    format: Some(wire::InteractionStreamFormat {
                        content: Some(wire::interaction_stream_format::Content::Text(
                            wire::InteractionTextFormat {},
                        )),
                    }),
                })),
            ];
            events.extend(final_events("test"));
            return Ok(Response::new(Box::pin(stream::iter(events))));
        }
        if open.routine_name == "early-rejected" {
            return Ok(Response::new(Box::pin(stream::iter([event(
                Event::ValidationFailure(Default::default()),
            )]))));
        }
        if open.routine_name.starts_with("early-") {
            return Ok(Response::new(Box::pin(stream::iter([
                event(Event::Accepted(wire::InteractionAccepted {
                    invocation_id: "SECRET_INVOCATION_SENTINEL".into(),
                })),
                event(Event::Completed(wire::InteractionCompleted {
                    status: match open.routine_name.as_str() {
                        "early-failed" => wire::RoutineInvocationStatus::Failed,
                        "early-interrupted" => wire::RoutineInvocationStatus::Interrupted,
                        _ => wire::RoutineInvocationStatus::Succeeded,
                    } as i32,
                    failure: (open.routine_name != "early-protocol").then(|| {
                        wire::RoutineFailure {
                            code: "SECRET_CODE_SENTINEL".into(),
                            message: "SECRET_MESSAGE_SENTINEL".into(),
                        }
                    }),
                })),
            ]))));
        }
        if open.routine_name.starts_with("delay-open-") {
            let ms = open
                .routine_name
                .rsplit('-')
                .next()
                .unwrap()
                .parse::<u64>()
                .unwrap();
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
        let fake = self.clone();
        let mut prefix = vec![
            event(Event::Accepted(wire::InteractionAccepted {
                invocation_id: "invocation".into(),
            })),
            event(Event::OutputOpened(wire::InteractionOutputOpened {
                stream_id: "out".into(),
                port_name: "clean-text".into(),
                format: Some(wire::InteractionStreamFormat {
                    content: Some(wire::interaction_stream_format::Content::Text(
                        wire::InteractionTextFormat {},
                    )),
                }),
            })),
        ];
        if open.routine_name.starts_with("typing") {
            prefix.extend([
                text_event("one", "draft", false),
                text_event("one", "first\r", true),
                text_event("two", "draft", false),
            ]);
        }
        let delay = if open.routine_name.starts_with("delay-accepted-") {
            open.routine_name
                .rsplit('-')
                .next()
                .unwrap()
                .parse::<u64>()
                .unwrap()
        } else {
            0
        };
        let tail = stream::once(async move {
            while let Some(message) = input.message().await? {
                match message.event {
                    Some(wire::interact_request::Event::InputChunk(chunk)) => {
                        if let Some(wire::interaction_input_chunk::Content::Audio(bytes)) =
                            chunk.content
                        {
                            fake.uploads.fetch_add(bytes.len(), Ordering::SeqCst);
                            fake.pcm.lock().unwrap().extend(bytes);
                        }
                    }
                    Some(wire::interact_request::Event::InputCompleted(_)) => break,
                    Some(wire::interact_request::Event::Cancel(_)) => {
                        return Err(RpcStatus::cancelled("cancelled"));
                    }
                    _ => (),
                }
            }
            fake.completed.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            if open.routine_name == "failed" || open.routine_name == "typing-failed" {
                return Err(RpcStatus::data_loss("static failure"));
            }
            Ok(final_events(&open.routine_name))
        })
        .map(|result| stream::iter(result.unwrap_or_else(|e| vec![Err(e)])))
        .flatten();
        let prefix = stream::once(async move {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            stream::iter(prefix)
        })
        .flatten();
        Ok(Response::new(Box::pin(prefix.chain(tail))))
    }
    type RunStream = Responses;
    async fn run(
        &self,
        _request: Request<wire::RunInteractionRequest>,
    ) -> Result<Response<Responses>, RpcStatus> {
        Err(RpcStatus::unimplemented("unused"))
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
    let service = RoutineInteractionServiceServer::new(fake.clone());
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
    fn open(&self, _config: waltr_audio_io::Config) -> Result<Box<dyn Source>, capture::Error> {
        let state = &self.0;
        state.started.store(true, Ordering::SeqCst);
        let startup = state.startup.lock().unwrap().take();
        if let Some(rx) = startup {
            rx.recv().unwrap();
        }
        Ok(Box::new(Samples {
            state: state.clone(),
            remaining: if state.burst.load(Ordering::SeqCst) {
                1000
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
        if self.remaining < 1000
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
    b.command.as_mut().unwrap().argv[4] = path.to_str().unwrap().into();
    b
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
            assert_eq!(
                result,
                Err(Error::Command(
                    waltr_desktop::command::CommandError::InputLimit
                ))
            );
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
            < position(Diagnostic::RpcFailure(RpcFailure::Protocol))
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
async fn shared_pipeline_waits_for_acceptance_then_closes_and_finishes_in_order() {
    let (transport, fake, stop, server) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("once");
    let binding = binding(&path, "acceptance-gate");
    let mock = Arc::new(Mock::default());
    let (hold, control) = Hold::press(&AudioAdmission::default()).unwrap();
    control.release();
    assert_eq!(
        hold.run(
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
    assert!(path.exists());
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn small_frames_survive_delayed_open_and_accepted_with_exact_pcm_after_release() {
    for routine in [
        "delay-open-100",
        "delay-open-250",
        "delay-accepted-100",
        "delay-accepted-250",
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
            while fake.admissions.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            control.release();
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
                        routine: "delay-accepted-250",
                        trigger: "dictate",
                        input: "audio",
                        output: "clean-text"
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
    if name.starts_with("typing") {
        return vec![
            text_event("two", "\nsecond\t界", true),
            event(Event::OutputClosed(wire::InteractionOutputClosed {
                stream_id: "out".into(),
                status: 1,
                failure: None,
            })),
            event(Event::Completed(wire::InteractionCompleted {
                status: wire::RoutineInvocationStatus::Succeeded as i32,
                failure: None,
            })),
        ];
    }
    let mut events = vec![
        event(Event::OutputChunk(wire::InteractionOutputChunk {
            stream_id: "out".into(),
            content: Some(wire::interaction_output_chunk::Content::Text(
                wire::TextUpdate {
                    segment_id: "one".into(),
                    text: if name == "large" {
                        "x".repeat(128 * 1024 + 1)
                    } else {
                        "literal $(touch nope); '{{ invocationId }}'".into()
                    },
                    operation: 2,
                    committed: name != "uncommitted",
                },
            )),
        })),
        event(Event::OutputClosed(wire::InteractionOutputClosed {
            stream_id: "out".into(),
            status: 1,
            failure: None,
        })),
        event(Event::Completed(wire::InteractionCompleted {
            status: wire::RoutineInvocationStatus::Succeeded as i32,
            failure: None,
        })),
    ];
    if name == "incomplete" {
        events.pop();
    }
    events
}

fn text_event(id: &str, text: &str, committed: bool) -> Result<wire::InteractResponse, RpcStatus> {
    event(Event::OutputChunk(wire::InteractionOutputChunk {
        stream_id: "out".into(),
        content: Some(wire::interaction_output_chunk::Content::Text(
            wire::TextUpdate {
                segment_id: id.into(),
                text: text.into(),
                operation: 2,
                committed,
            },
        )),
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
