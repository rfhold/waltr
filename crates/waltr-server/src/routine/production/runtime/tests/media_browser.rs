use super::*;

#[path = "media_blobs.rs"]
mod binary;
pub(super) async fn blobs(address: std::net::SocketAddr, auth: Auth, token: &str) {
    binary::run(address, auth, token).await;
}

pub(super) async fn cancel(
    address: std::net::SocketAddr,
    auth: Auth,
    synthesis: bool,
    metrics: &super::super::media_upstream::Metrics,
) {
    let name = if synthesis {
        "audio-synthesize"
    } else {
        "audio-transcribe"
    };
    let mut definitions = send(
        address,
        &auth,
        "WatchDefinitions",
        p::WatchDefinitionsRequest {},
    )
    .await;
    let definition = definitions
        .next::<p::WatchDefinitionsResponse>()
        .await
        .unwrap()
        .definitions
        .into_iter()
        .find(|v| v.name == name)
        .unwrap();
    drop(definitions);
    let port = if synthesis { "text" } else { "audio" };
    let opened: p::OpenAttachmentResponse = unary(
        address,
        &auth,
        "OpenAttachment",
        p::OpenAttachmentRequest {
            open: Some(p::InteractionOpen {
                selection: Some(p::InvocationSelection {
                    routine: name.into(),
                    revision: definition.revision,
                    entrypoint: if synthesis { "speak" } else { "dictate" }.into(),
                    parameters: Some(p::StructuredValue {
                        json: parameters(name),
                    }),
                    ..Default::default()
                }),
                inputs: vec![p::InputDeclaration {
                    port: port.into(),
                    format: Some(if synthesis {
                        p::input_declaration::Format::Text(p::TextFormat {})
                    } else {
                        p::input_declaration::Format::Pcm16le(p::Pcm16Le {
                            sample_rate: 24000,
                            channels: 1,
                        })
                    }),
                }],
            }),
        },
    )
    .await;
    let credentials = match opened.admission.unwrap() {
        p::open_attachment_response::Admission::Accepted(v) => v.credentials.unwrap(),
        _ => panic!("browser cancel admission rejected"),
    };
    let invocation = credentials.invocation_id.clone();
    let mut output = send(
        address,
        &auth,
        "WatchLiveOutput",
        p::WatchLiveOutputRequest {
            credentials: Some(credentials.clone()),
        },
    )
    .await;
    let stop = tokio_util::sync::CancellationToken::new();
    let producer_stop = stop.clone();
    let producer_auth = auth.clone();
    let producer = tokio::spawn(async move {
        let events = vec![
            p::InputEvent {
                event: Some(p::input_event::Event::Open(p::StreamOpen {
                    port: port.into(),
                    stream_id: "cancel-browser".into(),
                    format: Some(if synthesis {
                        p::stream_open::Format::Text(p::TextFormat {})
                    } else {
                        p::stream_open::Format::Pcm16le(p::Pcm16Le {
                            sample_rate: 24000,
                            channels: 1,
                        })
                    }),
                })),
            },
            p::InputEvent {
                event: Some(p::input_event::Event::Chunk(p::StreamChunk {
                    stream_id: "cancel-browser".into(),
                    sequence: 0,
                    payload: Some(if synthesis {
                        p::stream_chunk::Payload::Text(p::TextRevision {
                            segment_id: "committed".into(),
                            mutation: 1,
                            text: "Committed first sentence.".into(),
                            committed: true,
                            ..Default::default()
                        })
                    } else {
                        p::stream_chunk::Payload::Audio(vec![0; 32768])
                    }),
                })),
            },
        ];
        let ack: p::SendInputBatchResponse = unary(
            address,
            &producer_auth,
            "SendInputBatch",
            p::SendInputBatchRequest {
                credentials: Some(credentials),
                sequence: 0,
                events,
            },
        )
        .await;
        assert_eq!(ack.accepted_sequence, 0);
        producer_stop.cancelled().await;
    });
    loop {
        let event = output
            .next::<p::WatchLiveOutputResponse>()
            .await
            .unwrap()
            .output
            .unwrap()
            .event
            .unwrap();
        if matches!(event, p::output_event::Event::Chunk(_)) {
            break;
        }
        assert!(!matches!(event, p::output_event::Event::Terminal(_)));
    }
    assert!(!producer.is_finished());
    assert_eq!(metrics.live.load(Ordering::SeqCst), 1);
    let _: p::CancelInvocationResponse = unary(
        address,
        &auth,
        "CancelInvocation",
        p::CancelInvocationRequest {
            invocation_id: invocation,
        },
    )
    .await;
    stop.cancel();
    let mut terminal = None;
    while let Some(value) = output.next::<p::WatchLiveOutputResponse>().await {
        if let p::output_event::Event::Terminal(v) = value.output.unwrap().event.unwrap() {
            assert!(terminal.replace(v).is_none());
        }
    }
    producer.await.unwrap();
    let terminal = terminal.unwrap();
    assert_eq!(terminal.status, p::InvocationStatus::Uncertain as i32);
    assert!(terminal.cleanup_complete);
    assert!(terminal.text_results.is_empty());
    tokio::time::timeout(Duration::from_secs(5), async {
        while metrics.live.load(Ordering::SeqCst) != 0 {
            metrics.dropped.notified().await;
        }
    })
    .await
    .unwrap();
}

#[derive(Clone)]
pub(super) enum Auth {
    Cookie(String),
    Bearer(String),
}

struct Web {
    response: reqwest::Response,
    buffer: Vec<u8>,
    status: bool,
    eof: bool,
}
impl Web {
    async fn next<M: prost::Message + Default>(&mut self) -> Option<M> {
        loop {
            if self.buffer.len() >= 5 {
                let size = u32::from_be_bytes(self.buffer[1..5].try_into().unwrap()) as usize;
                assert!(size <= 131072);
                if self.buffer.len() >= size + 5 {
                    let flag = self.buffer[0];
                    let payload = self.buffer[5..5 + size].to_vec();
                    self.buffer.drain(..5 + size);
                    if flag == 128 {
                        let status = std::str::from_utf8(&payload)
                            .unwrap()
                            .lines()
                            .filter_map(|line| line.split_once(':'))
                            .find_map(|(name, value)| {
                                (name.trim() == "grpc-status").then(|| value.trim())
                            });
                        assert_eq!(status, Some("0"));
                        self.status = true;
                    } else {
                        assert_eq!(flag, 0);
                        return Some(M::decode(payload.as_slice()).unwrap());
                    }
                }
            }
            if self.eof {
                assert!(self.status && self.buffer.is_empty());
                return None;
            }
            match tokio::time::timeout(Duration::from_secs(15), self.response.chunk())
                .await
                .unwrap()
                .unwrap()
            {
                Some(bytes) => {
                    self.buffer.extend_from_slice(&bytes);
                    assert!(self.buffer.len() <= 262144);
                }
                None => self.eof = true,
            }
        }
    }
}
async fn send<M: prost::Message>(
    address: std::net::SocketAddr,
    auth: &Auth,
    method: &str,
    value: M,
) -> Web {
    let payload = value.encode_to_vec();
    let mut frame = vec![0];
    frame.extend(u32::try_from(payload.len()).unwrap().to_be_bytes());
    frame.extend(payload);
    let mut request = reqwest::Client::new()
        .post(format!(
            "http://{address}/waltr.routine.v2.RoutineService/{method}"
        ))
        .header("content-type", "application/grpc-web+proto")
        .header("x-grpc-web", "1")
        .header("origin", format!("http://{address}"))
        .body(frame);
    request = match auth {
        Auth::Cookie(cookie) => request.header("cookie", format!("waltr-session-local={cookie}")),
        Auth::Bearer(token) => request.bearer_auth(token),
    };
    let response = request.send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    if let Some(status) = response.headers().get("grpc-status") {
        assert_eq!(status, "0", "browser method {method} status");
    }
    let status = response
        .headers()
        .get("grpc-status")
        .is_some_and(|value| value == "0");
    Web {
        response,
        buffer: Vec::new(),
        status,
        eof: false,
    }
}
async fn unary<A: prost::Message, B: prost::Message + Default>(
    address: std::net::SocketAddr,
    auth: &Auth,
    method: &str,
    value: A,
) -> B {
    let mut response = send(address, auth, method, value).await;
    let value = response.next::<B>().await.unwrap();
    assert!(response.next::<B>().await.is_none());
    value
}
pub(super) async fn run(
    address: std::net::SocketAddr,
    auth: Auth,
    name: &str,
    entry: &str,
    bytes: usize,
) -> p::InteractionTerminal {
    marked(address, auth, name, entry, bytes, 0).await
}
pub(super) async fn marked(
    address: std::net::SocketAddr,
    auth: Auth,
    name: &str,
    entry: &str,
    bytes: usize,
    marker: u8,
) -> p::InteractionTerminal {
    let mut definitions = send(
        address,
        &auth,
        "WatchDefinitions",
        p::WatchDefinitionsRequest {},
    )
    .await;
    let definition = definitions
        .next::<p::WatchDefinitionsResponse>()
        .await
        .unwrap()
        .definitions
        .into_iter()
        .find(|value| value.name == name)
        .unwrap();
    drop(definitions);
    let opened: p::OpenAttachmentResponse = unary(
        address,
        &auth,
        "OpenAttachment",
        p::OpenAttachmentRequest {
            open: Some(p::InteractionOpen {
                selection: Some(p::InvocationSelection {
                    routine: name.into(),
                    revision: definition.revision,
                    entrypoint: entry.into(),
                    parameters: Some(p::StructuredValue {
                        json: parameters(name),
                    }),
                    ..Default::default()
                }),
                inputs: vec![p::InputDeclaration {
                    port: if bytes == 0 { "text" } else { "audio" }.into(),
                    format: Some(if bytes == 0 {
                        p::input_declaration::Format::Text(p::TextFormat {})
                    } else {
                        p::input_declaration::Format::Pcm16le(p::Pcm16Le {
                            sample_rate: 24000,
                            channels: 1,
                        })
                    }),
                }],
            }),
        },
    )
    .await;
    let admission = match opened.admission.unwrap() {
        p::open_attachment_response::Admission::Accepted(value) => value,
        _ => panic!("browser media admission rejected"),
    };
    let credentials = admission.credentials.unwrap();
    let invocation = credentials.invocation_id.clone();
    let mut output = send(
        address,
        &auth,
        "WatchLiveOutput",
        p::WatchLiveOutputRequest {
            credentials: Some(credentials.clone()),
        },
    )
    .await;
    let live = matches!(name, "audio-transcribe" | "audio-synthesize");
    let release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let producer_release = release.clone();
    let input_closed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let producer_closed = input_closed.clone();
    let producer_auth = auth.clone();
    let producer = tokio::spawn(async move {
        let mut batch = 0;
        let open = p::InputEvent {
            event: Some(p::input_event::Event::Open(p::StreamOpen {
                port: if bytes == 0 { "text" } else { "audio" }.into(),
                stream_id: "browser-input".into(),
                format: Some(if bytes == 0 {
                    p::stream_open::Format::Text(p::TextFormat {})
                } else {
                    p::stream_open::Format::Pcm16le(p::Pcm16Le {
                        sample_rate: 24000,
                        channels: 1,
                    })
                }),
            })),
        };
        let ack: p::SendInputBatchResponse = unary(
            address,
            &producer_auth,
            "SendInputBatch",
            p::SendInputBatchRequest {
                credentials: Some(credentials.clone()),
                sequence: batch,
                events: vec![open],
            },
        )
        .await;
        assert_eq!(ack.accepted_sequence, batch);
        batch += 1;
        let mut sequence = 0;
        let mut left = bytes;
        while left > 0 {
            let n = left.min(48000);
            let event = p::InputEvent {
                event: Some(p::input_event::Event::Chunk(p::StreamChunk {
                    stream_id: "browser-input".into(),
                    sequence,
                    payload: Some(p::stream_chunk::Payload::Audio(vec![marker; n])),
                })),
            };
            let ack: p::SendInputBatchResponse = unary(
                address,
                &producer_auth,
                "SendInputBatch",
                p::SendInputBatchRequest {
                    credentials: Some(credentials.clone()),
                    sequence: batch,
                    events: vec![event],
                },
            )
            .await;
            assert_eq!(ack.accepted_sequence, batch);
            batch += 1;
            sequence += 1;
            left -= n;
        }
        if bytes == 0 {
            let events = [
                ("one", "draft", false),
                ("one", "Committed first sentence.", true),
                ("two", " Second sentence.", true),
            ]
            .into_iter()
            .map(|(segment, text, committed)| {
                let event = p::InputEvent {
                    event: Some(p::input_event::Event::Chunk(p::StreamChunk {
                        stream_id: "browser-input".into(),
                        sequence,
                        payload: Some(p::stream_chunk::Payload::Text(p::TextRevision {
                            segment_id: segment.into(),
                            mutation: if segment == "one" && committed {
                                p::TextMutation::Replace
                            } else {
                                p::TextMutation::Append
                            } as i32,
                            text: text.into(),
                            committed,
                        })),
                    })),
                };
                sequence += 1;
                event
            })
            .collect();
            let _: p::SendInputBatchResponse = unary(
                address,
                &producer_auth,
                "SendInputBatch",
                p::SendInputBatchRequest {
                    credentials: Some(credentials.clone()),
                    sequence: batch,
                    events,
                },
            )
            .await;
            batch += 1;
        }
        if live {
            producer_release.acquire().await.unwrap().forget();
        }
        producer_closed.store(true, std::sync::atomic::Ordering::SeqCst);
        let events = vec![
            p::InputEvent {
                event: Some(p::input_event::Event::Close(p::StreamClose {
                    stream_id: "browser-input".into(),
                    next_sequence: sequence,
                })),
            },
            p::InputEvent {
                event: Some(p::input_event::Event::Finish(p::InputFinish {})),
            },
        ];
        let _: p::SendInputBatchResponse = unary(
            address,
            &producer_auth,
            "SendInputBatch",
            p::SendInputBatchRequest {
                credentials: Some(credentials),
                sequence: batch,
                events,
            },
        )
        .await;
    });
    let mut terminal = None;
    let mut marked_seen = marker == 0;
    let mut opens = 0;
    let mut closes = 0;
    let mut chunks = 0;
    let mut pcm = 0;
    let mut previews: Vec<p::TextRevision> = Vec::new();
    let mut released = false;
    while let Some(response) = output.next::<p::WatchLiveOutputResponse>().await {
        match response.output.unwrap().event.unwrap() {
            p::output_event::Event::Open(_) => opens += 1,
            p::output_event::Event::Close(_) => closes += 1,
            p::output_event::Event::Chunk(value) => {
                chunks += 1;
                if let Some(p::stream_chunk::Payload::Audio(data)) = value.payload {
                    assert_eq!(data.len() % 2, 0);
                    pcm += data.len();
                    if live && !released {
                        assert!(!input_closed.load(std::sync::atomic::Ordering::SeqCst));
                        release.add_permits(1);
                        released = true;
                    }
                } else if let Some(p::stream_chunk::Payload::Text(text)) = value.payload {
                    if live && !text.committed && !released {
                        assert!(!input_closed.load(std::sync::atomic::Ordering::SeqCst));
                        previews.push(text);
                        assert!(previews.len() <= 2);
                        if previews.len() == 2 {
                            assert_eq!(previews[0].segment_id, previews[1].segment_id);
                            assert_ne!(previews[0].text, previews[1].text);
                            release.add_permits(1);
                            released = true;
                        }
                    } else if live && text.committed {
                        assert!(input_closed.load(std::sync::atomic::Ordering::SeqCst));
                        if marker != 0 {
                            assert!(text.text.contains(if marker == 1 {
                                "source-one"
                            } else {
                                "source-two"
                            }));
                            assert!(!text.text.contains(if marker == 1 {
                                "source-two"
                            } else {
                                "source-one"
                            }));
                            marked_seen = true;
                        }
                    }
                }
            }
            p::output_event::Event::Terminal(value) => assert!(terminal.replace(value).is_none()),
            _ => {}
        }
    }
    producer.await.unwrap();
    assert!(marked_seen);
    let terminal = terminal.unwrap();
    assert_eq!(
        terminal.status,
        p::InvocationStatus::Succeeded as i32,
        "{name}"
    );
    assert!(terminal.cleanup_complete);
    assert_eq!(opens, closes);
    assert!(chunks > 0);
    if bytes == 0 {
        assert_eq!(pcm, 16);
    } else if live {
        assert_eq!(previews.len(), 2);
    }
    for descriptor in &terminal.text_results {
        let response: p::ReadTextResultResponse = unary(
            address,
            &auth,
            "ReadTextResult",
            p::ReadTextResultRequest {
                invocation_id: invocation.clone(),
                result_id: descriptor.id.clone(),
                offset: 0,
                max_bytes: 65536,
            },
        )
        .await;
        assert!(response.eof);
        let text = std::str::from_utf8(&response.data).unwrap();
        match descriptor.name.as_str() {
            "notes" => assert_eq!(text, "Meeting notes: substantive decision."),
            "deletion-audit" => assert_eq!(text, "REMOVED_INCIDENTAL: irrelevant noise"),
            "clean-text" => assert_eq!(text, "Clean final transcript."),
            _ => {}
        }
    }
    terminal
}
