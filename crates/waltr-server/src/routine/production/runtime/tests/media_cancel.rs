use super::*;

pub(super) async fn native(
    address: std::net::SocketAddr,
    auth: &str,
    synthesis: bool,
    metrics: &super::super::media_upstream::Metrics,
) {
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = p::routine_service_client::RoutineServiceClient::new(channel);
    let mut request = tonic::Request::new(p::WatchDefinitionsRequest {});
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {auth}").parse().unwrap());
    let definitions = client
        .watch_definitions(request)
        .await
        .unwrap()
        .into_inner()
        .message()
        .await
        .unwrap()
        .unwrap();
    let name = if synthesis {
        "audio-synthesize"
    } else {
        "audio-transcribe"
    };
    let definition = definitions
        .definitions
        .iter()
        .find(|value| value.name == name)
        .unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tx.send(p::InteractRequest {
        frame: Some(p::interact_request::Frame::Open(p::InteractionOpen {
            selection: Some(p::InvocationSelection {
                routine: name.into(),
                revision: definition.revision.clone(),
                entrypoint: if synthesis { "speak" } else { "dictate" }.into(),
                parameters: Some(p::StructuredValue {
                    json: parameters(name),
                }),
                ..Default::default()
            }),
            inputs: vec![p::InputDeclaration {
                port: if synthesis { "text" } else { "audio" }.into(),
                format: Some(if synthesis {
                    p::input_declaration::Format::Text(p::TextFormat {})
                } else {
                    p::input_declaration::Format::Pcm16le(p::Pcm16Le {
                        sample_rate: 24000,
                        channels: 1,
                    })
                }),
            }],
        })),
    })
    .await
    .unwrap();
    let mut request = tonic::Request::new(ReceiverStream::new(rx));
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {auth}").parse().unwrap());
    let mut output = client.interact(request).await.unwrap().into_inner();
    let stop = tokio_util::sync::CancellationToken::new();
    let ended = stop.clone();
    let producer = tokio::spawn(async move {
        tx.send(input(p::input_event::Event::Open(p::StreamOpen {
            port: if synthesis { "text" } else { "audio" }.into(),
            stream_id: "cancel-input".into(),
            format: Some(if synthesis {
                p::stream_open::Format::Text(p::TextFormat {})
            } else {
                p::stream_open::Format::Pcm16le(p::Pcm16Le {
                    sample_rate: 24000,
                    channels: 1,
                })
            }),
        })))
        .await
        .unwrap();
        tx.send(input(p::input_event::Event::Chunk(p::StreamChunk {
            stream_id: "cancel-input".into(),
            sequence: 0,
            payload: Some(if synthesis {
                p::stream_chunk::Payload::Text(p::TextRevision {
                    segment_id: "committed".into(),
                    mutation: p::TextMutation::Append as i32,
                    text: "Committed first sentence.".into(),
                    committed: true,
                })
            } else {
                p::stream_chunk::Payload::Audio(vec![0; 32768])
            }),
        })))
        .await
        .unwrap();
        ended.cancelled().await;
        drop(tx);
    });
    let mut invocation = None;
    let mut canceled = false;
    let mut terminal = None;
    while let Some(frame) = tokio::time::timeout(Duration::from_secs(15), output.message())
        .await
        .unwrap()
        .unwrap()
    {
        match frame.output.unwrap().event.unwrap() {
            p::output_event::Event::Accepted(value) => invocation = Some(value.invocation_id),
            p::output_event::Event::Chunk(_) if !canceled => {
                assert!(!producer.is_finished());
                assert_eq!(metrics.live.load(Ordering::SeqCst), 1);
                let mut request = tonic::Request::new(p::CancelInvocationRequest {
                    invocation_id: invocation.clone().unwrap(),
                });
                request
                    .metadata_mut()
                    .insert("authorization", format!("Bearer {auth}").parse().unwrap());
                client.cancel_invocation(request).await.unwrap();
                canceled = true;
                stop.cancel();
            }
            p::output_event::Event::Terminal(value) => {
                assert!(terminal.replace(value).is_none());
            }
            p::output_event::Event::Rejected(value) => {
                panic!("cancellation fixture admission: {value:?}")
            }
            _ => {}
        }
    }
    assert!(canceled);
    producer.await.unwrap();
    let terminal = terminal.unwrap();
    assert_eq!(terminal.status, p::InvocationStatus::Uncertain as i32);
    assert!(terminal.cleanup_complete);
    tokio::time::timeout(Duration::from_secs(5), async {
        while metrics.live.load(Ordering::SeqCst) != 0 {
            metrics.dropped.notified().await;
        }
    })
    .await
    .unwrap();
}
