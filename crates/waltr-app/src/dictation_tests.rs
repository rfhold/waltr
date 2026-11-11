//! Local v2 RPC and synthetic capture only; no device or external effects.
use super::*;
use crate::audio_permissions::{PermissionBackend, PermissionReply};
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
use waltr_desktop::capture::{self, Factory, Source};
#[path = "dictation_cases.rs"]
mod cases;
use crate::native_media_test_support as support;
type Responses = Pin<Box<dyn Stream<Item = Result<wire::InteractResponse, RpcStatus>> + Send>>;
#[derive(Clone, Default)]
struct Fake {
    admissions: Arc<AtomicUsize>,
    uploads: Arc<AtomicUsize>,
    completed: Arc<AtomicBool>,
    preview_step: Arc<AtomicUsize>,
}
fn event(event: Event) -> Result<wire::InteractResponse, RpcStatus> {
    Ok(wire::InteractResponse {
        output: Some(wire::OutputEvent { event: Some(event) }),
    })
}
impl Fake {
    async fn watch_live_output(
        &self,
        _: Request<wire::WatchLiveOutputRequest>,
    ) -> Result<
        Response<<Self as wire::routine_service_server::RoutineService>::WatchLiveOutputStream>,
        RpcStatus,
    > {
        Err(RpcStatus::unimplemented("unused"))
    }
    async fn read_blob(
        &self,
        _: Request<wire::ReadBlobRequest>,
    ) -> Result<
        Response<<Self as wire::routine_service_server::RoutineService>::ReadBlobStream>,
        RpcStatus,
    > {
        Err(RpcStatus::unimplemented("unused"))
    }
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
        let selection = open.selection.unwrap();
        assert_eq!(selection.revision, "revision");
        assert_eq!(selection.entrypoint, "dictate");
        let name = selection.routine;
        if name == "SECRET_CONFIG_SENTINEL_reject" {
            return Err(RpcStatus::unavailable("SECRET_ERROR_SENTINEL"));
        }
        if name == "remote-rejected" {
            return Ok(Response::new(Box::pin(stream::iter([event(
                Event::Rejected(wire::InteractionRejected {
                    code: wire::RejectionCode::Invalid as i32,
                    reason: "rejected".into(),
                }),
            )]))));
        }
        let accepted = || {
            event(Event::Accepted(wire::InteractionAccepted {
                invocation_id: if name == "malicious-id" {
                    "SECRET_INVOCATION_SENTINEL"
                } else {
                    "01234567-89ab-4cde-8fab-0123456789ab"
                }
                .into(),
                revision: "revision".into(),
                limits: Some(support::limits()),
            }))
        };
        if matches!(name.as_str(), "remote-failed" | "remote-interrupted") {
            let admission = accepted();
            let terminal = event(Event::Terminal(wire::InteractionTerminal {
                status: if name == "remote-failed" {
                    wire::InvocationStatus::Failed as i32
                } else {
                    wire::InvocationStatus::Interrupted as i32
                },
                cleanup_complete: true,
                text_results: vec![],
                result: None,
            }));
            return Ok(Response::new(Box::pin(stream::iter([admission]).chain(
                stream::once(async move {
                    let _result = input.message().await;
                    terminal
                }),
            ))));
        }
        if name == "pending" {
            while input.message().await?.is_some() {}
            return Err(RpcStatus::cancelled("closed before admission"));
        }
        let fake = self.clone();
        let mut prefix = vec![
            accepted(),
            event(Event::Open(wire::StreamOpen {
                stream_id: "out".into(),
                port: "clean-text".into(),
                format: Some(wire::stream_open::Format::Text(wire::TextFormat {})),
            })),
        ];
        if name == "early-success" {
            prefix.extend(final_events("test"));
            return Ok(Response::new(Box::pin(stream::iter(prefix))));
        }
        if matches!(name.as_str(), "ambiguous" | "other-port") {
            prefix.push(event(Event::Open(wire::StreamOpen {
                stream_id: "extra".into(),
                port: if name == "ambiguous" {
                    "clean-text"
                } else {
                    "other"
                }
                .into(),
                format: Some(wire::stream_open::Format::Text(wire::TextFormat {})),
            })));
            prefix.push(event(Event::Close(wire::StreamClose {
                stream_id: "extra".into(),
                next_sequence: 0,
            })));
        }
        let live = name == "live";
        let progress = self.preview_step.clone();
        let previews = stream::unfold(0, move |step| {
            let progress = progress.clone();
            async move {
                if !live || step == 3 {
                    return None;
                }
                while progress.load(Ordering::SeqCst) < step {
                    tokio::task::yield_now().await;
                }
                Some((
                    text(
                        step as u64,
                        if step == 0 {
                            "hello".into()
                        } else {
                            "hello world".into()
                        },
                        step == 2,
                    ),
                    step + 1,
                ))
            }
        });
        let tail = stream::once(async move {
            while let Some(message) = input.message().await? {
                match message.frame {
                    Some(wire::interact_request::Frame::Input(wire::InputEvent {
                        event: Some(wire::input_event::Event::Chunk(chunk)),
                    })) => {
                        if let Some(wire::stream_chunk::Payload::Audio(bytes)) = chunk.payload {
                            fake.uploads.fetch_add(bytes.len(), Ordering::SeqCst);
                        }
                    }
                    Some(wire::interact_request::Frame::Input(wire::InputEvent {
                        event: Some(wire::input_event::Event::Finish(_)),
                    })) => break,
                    _ => (),
                }
            }
            fake.completed.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            if name == "failed" {
                return Err(RpcStatus::data_loss("static failure"));
            }
            Ok(final_events(&name))
        })
        .map(|r| stream::iter(r.unwrap_or_else(|e| vec![Err(e)])))
        .flatten();
        Ok(Response::new(Box::pin(
            stream::iter(prefix).chain(previews).chain(tail),
        )))
    }
}
support::fake_service!(Fake;
    start_invocation: StartInvocationRequest => StartInvocationResponse,
    list_invocations: ListInvocationsRequest => ListInvocationsResponse,
    list_operations: ListOperationsRequest => ListOperationsResponse,
    cancel_invocation: CancelInvocationRequest => CancelInvocationResponse,
    get_invocation: GetInvocationRequest => GetInvocationResponse,
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
    read_text_result: ReadTextResultRequest => ReadTextResultResponse,
);
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
type Shutdown = Mutex<Option<Box<dyn FnOnce() + Send>>>;
#[derive(Default)]
struct Mock {
    started: AtomicBool,
    stopped: AtomicBool,
    joined: AtomicBool,
    burst: AtomicBool,
    emit: AtomicBool,
    empty: AtomicBool,
    zero_bytes: AtomicBool,
    tail_only: AtomicBool,
    tail: Mutex<Option<Vec<u8>>>,
    failure: Mutex<Option<waltr_audio_io::Error>>,
    startup: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    shutdown: Shutdown,
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
                100_000
            } else {
                1
            },
        }))
    }
}
impl Source for Samples {
    fn frame(&mut self) -> Option<Vec<u8>> {
        if self.remaining > 0
            && !self.state.empty.load(Ordering::SeqCst)
            && (!self.state.tail_only.load(Ordering::SeqCst)
                || self.state.stopped.load(Ordering::SeqCst)
                || self.state.burst.load(Ordering::SeqCst)
                || self.state.emit.load(Ordering::SeqCst))
        {
            self.remaining -= 1;
            Some(if let Some(tail) = self.state.tail.lock().unwrap().take() {
                tail
            } else if self.state.zero_bytes.load(Ordering::SeqCst) {
                Vec::new()
            } else {
                vec![0; 100]
            })
        } else {
            None
        }
    }
    fn terminal(&self) -> Terminal {
        if self.state.stopped.load(Ordering::SeqCst)
            && let Some(error) = self.state.failure.lock().unwrap().clone()
        {
            return Terminal::Failed(error);
        }
        if self.state.stopped.load(Ordering::SeqCst) {
            Terminal::Completed
        } else {
            Terminal::Running
        }
    }
    fn stop(&self) {
        self.state.stopped.store(true, Ordering::SeqCst);
    }
    fn shutdown(self: Box<Self>) {
        let shutdown = self.state.shutdown.lock().unwrap().take();
        if let Some(shutdown) = shutdown {
            shutdown();
        }
        self.state.joined.store(true, Ordering::SeqCst);
    }
}
fn text(sequence: u64, text: String, committed: bool) -> Result<wire::InteractResponse, RpcStatus> {
    event(Event::Chunk(wire::StreamChunk {
        stream_id: "out".into(),
        sequence,
        payload: Some(wire::stream_chunk::Payload::Text(wire::TextRevision {
            segment_id: "one".into(),
            text,
            mutation: wire::TextMutation::Replace as i32,
            committed,
        })),
    }))
}
fn final_events(name: &str) -> Vec<Result<wire::InteractResponse, RpcStatus>> {
    let mut events = vec![
        text(
            0,
            if name == "SECRET_CONFIG_SENTINEL" {
                "SECRET_TRANSCRIPT_SENTINEL".into()
            } else if name == "large" {
                "x".repeat(256 * 1024 + 1)
            } else {
                "literal $(touch nope); '{{ invocationId }}'".into()
            },
            name != "uncommitted",
        ),
        event(Event::Close(wire::StreamClose {
            stream_id: "out".into(),
            next_sequence: if name == "live" { 3 } else { 1 },
        })),
        event(Event::Terminal(wire::InteractionTerminal {
            status: wire::InvocationStatus::Succeeded as i32,
            cleanup_complete: true,
            text_results: vec![],
            result: Some(wire::StructuredValue {
                json: b"{}".to_vec(),
            }),
        })),
    ];
    if name == "live" {
        let _chunk = events.remove(0);
    }
    if name == "incomplete" {
        events.pop();
    }
    events
}
