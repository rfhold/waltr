//! Local fake RPC and synthetic capture only. No device, key, clipboard, or remote service access.
use super::*;
use crate::audio_permissions::{PermissionBackend, PermissionReply};
use futures_util::{Stream, StreamExt, stream};
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
use waltr_desktop::capture::{self, Factory, Source};
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
#[path = "dictation_cases.rs"]
mod cases;
type Responses = Pin<Box<dyn Stream<Item = Result<wire::InteractResponse, RpcStatus>> + Send>>;
#[derive(Clone, Default)]
struct Fake {
    admissions: Arc<AtomicUsize>,
    uploads: Arc<AtomicUsize>,
    completed: Arc<AtomicBool>,
    preview_step: Arc<AtomicUsize>,
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
        if open.routine_name == "SECRET_CONFIG_SENTINEL_reject" {
            return Err(RpcStatus::unavailable("SECRET_ERROR_SENTINEL"));
        }
        if open.routine_name == "remote-rejected" {
            return Ok(Response::new(Box::pin(stream::iter([event(
                Event::ValidationFailure(Default::default()),
            )]))));
        }
        if matches!(
            open.routine_name.as_str(),
            "remote-failed" | "remote-interrupted"
        ) {
            return Ok(Response::new(Box::pin(stream::iter([
                event(Event::Accepted(wire::InteractionAccepted {
                    invocation_id: "SECRET_INVOCATION_SENTINEL".into(),
                })),
                event(Event::Completed(wire::InteractionCompleted {
                    status: if open.routine_name == "remote-failed" {
                        wire::RoutineInvocationStatus::Failed
                    } else {
                        wire::RoutineInvocationStatus::Interrupted
                    } as i32,
                    failure: Some(wire::RoutineFailure {
                        code: "SECRET_CODE_SENTINEL".into(),
                        message: "SECRET_MESSAGE_SENTINEL".into(),
                    }),
                })),
            ]))));
        }
        if open.routine_name == "pending" {
            while input.message().await?.is_some() {}
            return Err(RpcStatus::cancelled("closed before admission"));
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
        if open.routine_name == "early-success" {
            prefix.extend(final_events("test"));
            return Ok(Response::new(Box::pin(stream::iter(prefix))));
        }
        if matches!(open.routine_name.as_str(), "ambiguous" | "other-port") {
            prefix.push(event(Event::OutputOpened(wire::InteractionOutputOpened {
                stream_id: "extra".into(),
                port_name: if open.routine_name == "ambiguous" {
                    "clean-text"
                } else {
                    "other"
                }
                .into(),
                format: Some(wire::InteractionStreamFormat {
                    content: Some(wire::interaction_stream_format::Content::Text(
                        wire::InteractionTextFormat {},
                    )),
                }),
            })));
            prefix.push(event(Event::OutputClosed(wire::InteractionOutputClosed {
                stream_id: "extra".into(),
                status: 1,
                failure: None,
            })));
        }
        let live = open.routine_name == "live";
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
                    event(Event::OutputChunk(wire::InteractionOutputChunk {
                        stream_id: "out".into(),
                        content: Some(wire::interaction_output_chunk::Content::Text(
                            wire::TextUpdate {
                                segment_id: "one".into(),
                                text: if step == 0 { "hello" } else { "hello world" }.into(),
                                operation: 2,
                                committed: step == 2,
                            },
                        )),
                    })),
                    step + 1,
                ))
            }
        });
        let tail = stream::once(async move {
            while let Some(message) = input.message().await? {
                match message.event {
                    Some(wire::interact_request::Event::InputChunk(chunk)) => {
                        if let Some(wire::interaction_input_chunk::Content::Audio(bytes)) =
                            chunk.content
                        {
                            fake.uploads.fetch_add(bytes.len(), Ordering::SeqCst);
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
            if open.routine_name == "failed" {
                return Err(RpcStatus::data_loss("static failure"));
            }
            Ok(final_events(&open.routine_name))
        })
        .map(|result| stream::iter(result.unwrap_or_else(|e| vec![Err(e)])))
        .flatten();
        Ok(Response::new(Box::pin(
            stream::iter(prefix).chain(previews).chain(tail),
        )))
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
                100
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
fn final_events(name: &str) -> Vec<Result<wire::InteractResponse, RpcStatus>> {
    let mut events = vec![
        event(Event::OutputChunk(wire::InteractionOutputChunk {
            stream_id: "out".into(),
            content: Some(wire::interaction_output_chunk::Content::Text(
                wire::TextUpdate {
                    segment_id: "one".into(),
                    text: if name == "SECRET_CONFIG_SENTINEL" {
                        "SECRET_TRANSCRIPT_SENTINEL".into()
                    } else if name == "large" {
                        "x".repeat(256 * 1024 + 1)
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
    if name == "live" {
        let _chunk = events.remove(0);
    }
    if name == "incomplete" {
        events.pop();
    }
    events
}
