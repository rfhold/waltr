//! Host-facing interaction tests against a local fake contract service.
use futures_core::Stream;
use futures_util::{StreamExt, stream};
use waltr_client::{
    NativeGrpcTransport, RoutineInteractionTransport,
    interaction::{
        InteractionLimits,
        wire::{
            self,
            interact_response::Event,
            routine_interaction_service_server::{
                RoutineInteractionService, RoutineInteractionServiceServer,
            },
        },
    },
};
use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tonic::{Request, Response, Status};

type Responses = Pin<Box<dyn Stream<Item = Result<wire::InteractResponse, Status>> + Send>>;
#[derive(Clone, Default)]
struct Fake {
    auth: Arc<Mutex<Vec<Option<String>>>>,
    uploads: Arc<Mutex<Vec<usize>>>,
}
const fn response(event: Event) -> wire::InteractResponse {
    wire::InteractResponse { event: Some(event) }
}
fn accepted() -> wire::InteractResponse {
    response(Event::Accepted(wire::InteractionAccepted {
        invocation_id: "invocation".into(),
    }))
}
const fn completed() -> wire::InteractResponse {
    response(Event::Completed(wire::InteractionCompleted {
        status: wire::RoutineInvocationStatus::Succeeded as i32,
        failure: None,
    }))
}
fn text_output() -> Vec<Result<wire::InteractResponse, Status>> {
    vec![
        Ok(response(Event::OutputOpened(
            wire::InteractionOutputOpened {
                port_name: "answer".into(),
                stream_id: "out".into(),
                format: Some(wire::InteractionStreamFormat {
                    content: Some(wire::interaction_stream_format::Content::Text(
                        wire::InteractionTextFormat {},
                    )),
                }),
            },
        ))),
        Ok(response(Event::OutputChunk(wire::InteractionOutputChunk {
            stream_id: "out".into(),
            content: Some(wire::interaction_output_chunk::Content::Text(
                wire::TextUpdate {
                    segment_id: "segment".into(),
                    operation: 2,
                    text: "done".into(),
                    committed: true,
                },
            )),
        }))),
        Ok(response(Event::OutputClosed(
            wire::InteractionOutputClosed {
                stream_id: "out".into(),
                status: 1,
                failure: None,
            },
        ))),
        Ok(completed()),
    ]
}
impl Fake {
    fn record<T>(&self, request: &Request<T>) {
        self.auth.lock().unwrap().push(
            request
                .metadata()
                .get("authorization")
                .map(|v| v.to_str().unwrap().to_owned()),
        );
    }
}
#[tonic::async_trait]
impl RoutineInteractionService for Fake {
    type InteractStream = Responses;
    async fn interact(
        &self,
        request: Request<tonic::Streaming<wire::InteractRequest>>,
    ) -> Result<Response<Responses>, Status> {
        self.record(&request);
        let mut input = request.into_inner();
        let first = input.message().await?.unwrap();
        assert!(matches!(
            first.event,
            Some(wire::interact_request::Event::Open(_))
        ));
        // Admission is returned while the input stream is still open. Then wait for release.
        let tail = stream::once(async move {
            loop {
                let message = input
                    .message()
                    .await?
                    .ok_or_else(|| Status::cancelled("input vanished before completion"))?;
                match message.event {
                    Some(wire::interact_request::Event::InputCompleted(_)) => break,
                    Some(wire::interact_request::Event::Cancel(_)) => {
                        return Err(Status::cancelled("cancelled"));
                    }
                    _ => (),
                }
            }
            Ok::<_, Status>(text_output().into_iter().skip(1).collect::<Vec<_>>())
        })
        .map(|result| stream::iter(result.unwrap_or_else(|e| vec![Err(e)])))
        .flatten();
        Ok(Response::new(Box::pin(
            stream::iter(vec![Ok(accepted()), text_output().remove(0)]).chain(tail),
        )))
    }
    type RunStream = Responses;
    async fn run(
        &self,
        request: Request<wire::RunInteractionRequest>,
    ) -> Result<Response<Responses>, Status> {
        self.record(&request);
        let body = request.into_inner();
        let name = body.open.as_ref().unwrap().routine_name.as_str();
        if name == "transport-error" {
            return Err(Status::unavailable("fake unavailable"));
        }
        if name == "auth-error" {
            return Err(Status::unauthenticated("fake rejected"));
        }
        let mut events = vec![Ok(accepted())];
        match name {
            "validation" => {
                events = vec![Ok(response(Event::ValidationFailure(
                    wire::RoutineValidationFailure::default(),
                )))];
            }
            "eof" => (),
            "late-error" => events.push(Err(Status::data_loss("fake upstream stream failure"))),
            "after-terminal" => {
                events.push(Ok(completed()));
                events.push(Ok(accepted()));
            }
            _ => events.extend(text_output()),
        }
        self.uploads.lock().unwrap().push(
            body.inputs
                .iter()
                .map(|i| match &i.content {
                    Some(wire::interaction_input::Content::Audio(a)) => a.data.len(),
                    _ => 0,
                })
                .sum(),
        );
        Ok(Response::new(Box::pin(stream::iter(events))))
    }
}
struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn fixture() -> (NativeGrpcTransport, Fake, Server) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = stream::unfold(listener, |listener| async {
        Some((listener.accept().await.map(|v| v.0), listener))
    });
    let fake = Fake::default();
    let service = RoutineInteractionServiceServer::new(fake.clone())
        .max_decoding_message_size(40 * 1024 * 1024);
    let task = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    (
        NativeGrpcTransport::new(format!("http://{addr}")),
        fake,
        Server(task),
    )
}
fn open(name: &str) -> wire::OpenInteraction {
    wire::OpenInteraction {
        routine_name: name.into(),
        trigger_name: "trigger".into(),
        ..Default::default()
    }
}
fn run(name: &str) -> wire::RunInteractionRequest {
    wire::RunInteractionRequest {
        open: Some(open(name)),
        inputs: vec![],
    }
}

// Keep the duplex lifecycle and subsequent credential reuse in one scenario.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn duplex_admission_while_input_open_release_continues_output_and_auth_reused() {
    let (transport, fake, _server) = fixture().await;
    transport.replace_bearer_token("first").unwrap();
    let mut session = tokio::time::timeout(
        Duration::from_secs(5),
        transport.interact(open("ok"), InteractionLimits::default()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(session.input.finish_input().await.is_err());
    assert!(matches!(
        session.output.next().await.unwrap().unwrap().event,
        Some(Event::Accepted(_))
    ));
    session
        .input
        .send(wire::interact_request::Event::InputOpened(
            wire::InteractionInputOpened {
                port_name: "microphone".into(),
                stream_id: "capture".into(),
                format: Some(wire::InteractionStreamFormat {
                    content: Some(wire::interaction_stream_format::Content::Audio(
                        wire::InteractionAudioFormat {
                            encoding: 1,
                            sample_rate_hz: 16000,
                            channels: 1,
                        },
                    )),
                }),
            },
        ))
        .await
        .unwrap();
    // Response progress does not depend on closing or completing the input stream.
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), session.output.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .event,
        Some(Event::OutputOpened(_))
    ));
    for _ in 0..3 {
        session
            .input
            .send(wire::interact_request::Event::InputChunk(
                wire::InteractionInputChunk {
                    stream_id: "capture".into(),
                    content: Some(wire::interaction_input_chunk::Content::Audio(
                        vec![0; 256 * 1024],
                    )),
                },
            ))
            .await
            .unwrap();
    }
    session
        .input
        .send(wire::interact_request::Event::InputClosed(
            wire::InteractionInputClosed {
                stream_id: "capture".into(),
                status: 1,
                failure: None,
            },
        ))
        .await
        .unwrap();
    session.input.finish_input().await.unwrap();
    while let Some(event) = session.output.next().await {
        event.unwrap();
    }
    assert_eq!(session.output.final_text("answer").unwrap(), "done");
    assert!(session.output.final_text("other").is_err());
    transport.replace_bearer_token("second").unwrap();
    let mut output = transport
        .run_interaction(run("ok"), InteractionLimits::default())
        .await
        .unwrap();
    while let Some(event) = output.next().await {
        event.unwrap();
    }
    assert_eq!(
        *fake.auth.lock().unwrap(),
        vec![Some("Bearer first".into()), Some("Bearer second".into())]
    );
}

#[tokio::test]
async fn run_large_complete_upload_and_preflight_limits() {
    let (transport, fake, _server) = fixture().await;
    let mut body = run("ok");
    body.inputs.push(wire::InteractionInput {
        port_name: "audio".into(),
        content: Some(wire::interaction_input::Content::Audio(
            wire::InteractionAudioInput {
                format: Some(wire::InteractionAudioFormat {
                    encoding: 1,
                    sample_rate_hz: 16000,
                    channels: 1,
                }),
                data: vec![0; 5 * 1024 * 1024],
            },
        )),
    });
    let mut output = transport
        .run_interaction(body.clone(), InteractionLimits::default())
        .await
        .unwrap();
    while let Some(event) = output.next().await {
        event.unwrap();
    }
    assert_eq!(output.final_text("answer").unwrap(), "done");
    assert_eq!(*fake.uploads.lock().unwrap(), vec![5 * 1024 * 1024]);
    let limits = InteractionLimits {
        complete_body_bytes: 1024,
        ..Default::default()
    };
    assert!(transport.run_interaction(body, limits).await.is_err());
    assert_eq!(fake.uploads.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn rejection_transport_stream_errors_eof_and_post_terminal_are_distinct_failures() {
    let (transport, _fake, _server) = fixture().await;
    assert!(
        transport
            .run_interaction(run("transport-error"), InteractionLimits::default())
            .await
            .is_err()
    );
    let mut rejected = transport
        .run_interaction(run("validation"), InteractionLimits::default())
        .await
        .unwrap();
    assert!(matches!(
        rejected.next().await.unwrap().unwrap().event,
        Some(Event::ValidationFailure(_))
    ));
    assert!(rejected.next().await.is_none());
    assert!(rejected.final_text("answer").is_err());
    for name in ["eof", "late-error", "after-terminal"] {
        let mut output = transport
            .run_interaction(run(name), InteractionLimits::default())
            .await
            .unwrap();
        let mut failed = false;
        while let Some(event) = output.next().await {
            if event.is_err() {
                failed = true;
            }
        }
        assert!(failed, "{name}");
        assert!(output.final_text("answer").is_err());
    }
}

#[tokio::test]
async fn explicit_control_and_input_drop_cancel_pending_output() {
    let (transport, _fake, _server) = fixture().await;
    for drop_input in [false, true] {
        let mut session = transport
            .interact(open("ok"), InteractionLimits::default())
            .await
            .unwrap();
        session.output.next().await.unwrap().unwrap();
        let control = session.input.control();
        if drop_input {
            drop(session.input);
        } else {
            control.cancel();
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(5), session.output.next())
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(session.output.next().await.is_none());
    }
}
