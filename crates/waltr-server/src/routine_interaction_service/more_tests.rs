use super::*;

#[tokio::test]
async fn complete_text_synthetic_segments_do_not_consume_original_request_quota() {
    use prost::Message as _;
    let limits = InteractionLimits {
        complete_request_bytes: 1024,
        chunk_bytes: 5,
        ..InteractionLimits::default()
    };
    let (service, engine, _bus, persistence, root) = setup(TEXT, limits).await;
    for text in ["x".repeat(900), "🦀".repeat(225)] {
        let request = wire::RunInteractionRequest {
            open: Some(open()),
            inputs: vec![text_input(&text)],
        };
        assert!(request.encoded_len() < 1024);
        let mut responses = service
            .run(Request::new(request))
            .await
            .unwrap()
            .into_inner();
        assert!(matches!(
            responses.next().await.unwrap().unwrap().event,
            Some(Out::Accepted(_))
        ));
        let mut actual = String::new();
        let completed = loop {
            match tokio::time::timeout(Duration::from_secs(5), responses.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .event
                .unwrap()
            {
                Out::OutputChunk(chunk) => {
                    let Some(wire::interaction_output_chunk::Content::Text(segment)) =
                        chunk.content
                    else {
                        panic!("text expected")
                    };
                    assert!(segment.text.len() + segment.segment_id.len() <= 5);
                    actual.push_str(&segment.text);
                }
                Out::Completed(completed) => break completed,
                _ => (),
            }
        };
        assert_eq!(
            completed.status,
            wire::RoutineInvocationStatus::Succeeded as i32,
            "{completed:?}"
        );
        assert_eq!(actual, text);
    }
    assert!(
        persistence
            .routine_invocation_store()
            .list(None, 100, &BTreeMap::new())
            .await
            .unwrap()
            .iter()
            .all(|row| row.status == RoutineInvocationStatus::Succeeded)
    );
    engine.shutdown().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn per_port_dispatch_allows_completion_dependency_input_to_close_behind_blocked_port() {
    let source = TEXT.replace(
        "        outputPorts:",
        "          - {name: prerequisite, required: true, format: {type: text}}\n        outputPorts:",
    ).replace(
        "    - name: collect\n      taskSpec:",
        "    - name: prerequisite\n      taskSpec:\n        collect: {stream: '$(interaction.inputs.prerequisite)', maxBytes: 1024, separator: ''}\n    - name: collect\n      runAfter: [prerequisite]\n      taskSpec:",
    );
    let limits = InteractionLimits {
        chunk_bytes: 5,
        queue_wait_seconds: 1,
        ..InteractionLimits::default()
    };
    let source = source.replace("maxBytes: 1024", "maxBytes: 65536");
    let (service, engine, _bus, persistence, root) = setup(&source, limits).await;
    for (size, reverse) in [(400, false), (400, true), (16000, false), (16000, true)] {
        let mut inputs = vec![
            text_input(&"a".repeat(size)),
            wire::InteractionInput {
                port_name: "prerequisite".into(),
                content: Some(wire::interaction_input::Content::Text("p".repeat(size))),
            },
        ];
        if reverse {
            inputs.reverse();
        }
        let mut responses = service
            .run(Request::new(wire::RunInteractionRequest {
                open: Some(open()),
                inputs,
            }))
            .await
            .unwrap()
            .into_inner();
        let Some(Out::Accepted(accepted)) = responses.next().await.unwrap().unwrap().event else {
            panic!("accepted first")
        };
        let mut received = String::new();
        let completed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match responses.next().await.unwrap().unwrap().event.unwrap() {
                    Out::OutputChunk(chunk) => {
                        let Some(wire::interaction_output_chunk::Content::Text(text)) =
                            chunk.content
                        else {
                            panic!("text expected")
                        };
                        received.push_str(&text.text);
                    }
                    Out::Completed(completed) => break completed,
                    _ => (),
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            completed.status,
            wire::RoutineInvocationStatus::Succeeded as i32,
            "{completed:?}"
        );
        assert_eq!(received, "a".repeat(size));
        assert_eq!(
            persistence
                .routine_invocation_store()
                .detail(&accepted.invocation_id)
                .await
                .unwrap()
                .summary
                .status,
            RoutineInvocationStatus::Succeeded
        );
    }
    engine.shutdown().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn complete_dependency_feeders_join_on_shutdown_and_output_drop() {
    use crate::persistence::routine_invocation::RoutineEventKind;
    let source = TEXT.replace(
        "        outputPorts:",
        "          - {name: prerequisite, required: true, format: {type: text}}\n        outputPorts:",
    ).replace(
        "    - name: collect\n      taskSpec:",
        "    - name: prerequisite\n      taskSpec:\n        collect: {stream: '$(interaction.inputs.prerequisite)', maxBytes: 65536, separator: ''}\n    - name: collect\n      runAfter: [prerequisite]\n      taskSpec:",
    ).replace("maxBytes: 1024", "maxBytes: 65536");
    for shutdown in [false, true] {
        let limits = InteractionLimits {
            chunk_bytes: 5,
            edge_bytes: 5,
            queued_chunks: 1,
            ..InteractionLimits::default()
        };
        let (service, engine, _bus, persistence, root, _) =
            setup_full(&source, limits, Arc::new(ownership_tests::Hold)).await;
        let mut wakeups = engine.lifecycle_wakeups();
        let mut engine = Some(engine);
        let mut responses = service
            .run(Request::new(wire::RunInteractionRequest {
                open: Some(open()),
                inputs: vec![
                    text_input(&"a".repeat(16000)),
                    wire::InteractionInput {
                        port_name: "prerequisite".into(),
                        content: Some(wire::interaction_input::Content::Text("p".repeat(16000))),
                    },
                ],
            }))
            .await
            .unwrap()
            .into_inner();
        let Some(Out::Accepted(accepted)) = responses.next().await.unwrap().unwrap().event else {
            panic!("accepted first");
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    responses.next().await.unwrap().unwrap().event,
                    Some(Out::OutputChunk(_))
                ) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        // A has reached the live graph but cannot drain: its consumer waits on held prerequisite.
        let detail = persistence
            .routine_invocation_store()
            .detail(&accepted.invocation_id)
            .await
            .unwrap();
        assert!(
            !detail
                .events
                .iter()
                .any(|event| event.kind == RoutineEventKind::TaskStarted
                    && event.task_name.as_deref() == Some("collect"))
        );
        if shutdown {
            tokio::time::timeout(Duration::from_secs(5), engine.take().unwrap().shutdown())
                .await
                .unwrap();
            drop(responses);
        } else {
            drop(responses);
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let detail = persistence
                    .routine_invocation_store()
                    .detail(&accepted.invocation_id)
                    .await
                    .unwrap();
                if detail.summary.status == RoutineInvocationStatus::Interrupted {
                    break;
                }
                wakeups.recv().await.unwrap();
            }
        })
        .await
        .unwrap();
        if let Some(engine) = engine {
            engine.shutdown().await;
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}

fn audio_source() -> String {
    TEXT.replace(
        "{type: text}",
        "{type: audio, encoding: pcmS16le, sampleRateHz: 24000, channels: 2}",
    )
    .replace(
        "        outputPorts:",
        "          - {name: text, required: true, format: {type: text}}\n        outputPorts:",
    )
    .replace(
        "collect: {stream: '$(interaction.inputs.input)'",
        "collect: {stream: '$(interaction.inputs.text)'",
    )
}
fn audio_format() -> wire::InteractionAudioFormat {
    wire::InteractionAudioFormat {
        encoding: 1,
        sample_rate_hz: 24000,
        channels: 2,
    }
}
fn audio_input(data: Vec<u8>) -> wire::InteractionInput {
    wire::InteractionInput {
        port_name: "input".into(),
        content: Some(wire::interaction_input::Content::Audio(
            wire::InteractionAudioInput {
                format: Some(audio_format()),
                data,
            },
        )),
    }
}

#[tokio::test]
async fn complete_audio_is_frame_split_and_alignment_is_validated_before_admission() {
    let limits = InteractionLimits {
        chunk_bytes: 12,
        ..InteractionLimits::default()
    };
    let (service, engine, _bus, persistence, root) = setup(&audio_source(), limits).await;
    let mut invalid = service
        .run(Request::new(wire::RunInteractionRequest {
            open: Some(open()),
            inputs: vec![audio_input(vec![0; 3])],
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(matches!(
        invalid.next().await.unwrap().unwrap().event,
        Some(Out::ValidationFailure(_))
    ));
    assert!(
        persistence
            .routine_invocation_store()
            .list(None, 100, &BTreeMap::new())
            .await
            .unwrap()
            .is_empty()
    );
    let data = (0..1200).map(|v| (v % 256) as u8).collect::<Vec<_>>();
    let mut output = service
        .run(Request::new(wire::RunInteractionRequest {
            open: Some(open()),
            inputs: vec![
                audio_input(data.clone()),
                wire::InteractionInput {
                    port_name: "text".into(),
                    content: Some(wire::interaction_input::Content::Text(String::new())),
                },
            ],
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(matches!(
        output.next().await.unwrap().unwrap().event,
        Some(Out::Accepted(_))
    ));
    let mut received = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), output.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .event
            .unwrap()
        {
            Out::OutputChunk(v) => {
                let Some(wire::interaction_output_chunk::Content::Audio(bytes)) = v.content else {
                    panic!()
                };
                assert!(bytes.len() <= 12 && bytes.len() % 4 == 0);
                received.extend(bytes);
                // Deliberately yield to the eager producer between bounded output reads.
                for _ in 0..32 {
                    tokio::task::yield_now().await;
                }
            }
            Out::Completed(v) => {
                assert_eq!(
                    v.status,
                    wire::RoutineInvocationStatus::Succeeded as i32,
                    "{v:?}"
                );
                break;
            }
            _ => (),
        }
    }
    assert_eq!(received, data);
    engine.shutdown().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn native_audio_is_output_before_close() {
    let (service, engine, _bus, _persistence, root) =
        setup(&audio_source(), InteractionLimits::default()).await;
    let mut server = Loopback::start(service).await;
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    tx.send(request(In::Open(open()))).await.unwrap();
    let mut output = server
        .client
        .interact(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    assert!(matches!(
        output.message().await.unwrap().unwrap().event,
        Some(Out::Accepted(_))
    ));
    tx.send(request(In::InputOpened(wire::InteractionInputOpened {
        port_name: "input".into(),
        stream_id: "s".into(),
        format: Some(wire::InteractionStreamFormat {
            content: Some(wire::interaction_stream_format::Content::Audio(
                audio_format(),
            )),
        }),
    })))
    .await
    .unwrap();
    tx.send(request(In::InputChunk(wire::InteractionInputChunk {
        stream_id: "s".into(),
        content: Some(wire::interaction_input_chunk::Content::Audio(vec![1; 8])),
    })))
    .await
    .unwrap();
    loop {
        if let Some(Out::OutputChunk(v)) =
            tokio::time::timeout(Duration::from_secs(5), output.message())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .event
        {
            assert!(
                matches!(v.content, Some(wire::interaction_output_chunk::Content::Audio(v)) if v == vec![1; 8])
            );
            break;
        }
    }
    tx.send(request(closed())).await.unwrap();
    tx.send(request(In::InputOpened(wire::InteractionInputOpened {
        port_name: "text".into(),
        stream_id: "t".into(),
        format: Some(wire::InteractionStreamFormat {
            content: Some(wire::interaction_stream_format::Content::Text(
                wire::InteractionTextFormat {},
            )),
        }),
    })))
    .await
    .unwrap();
    tx.send(request(In::InputClosed(wire::InteractionInputClosed {
        stream_id: "t".into(),
        status: 1,
        failure: None,
    })))
    .await
    .unwrap();
    drop(tx);
    loop {
        if let Some(Out::Completed(v)) = output.message().await.unwrap().unwrap().event {
            assert_eq!(v.status, wire::RoutineInvocationStatus::Succeeded as i32);
            break;
        }
    }
    drop(output);
    engine.shutdown().await;
    server.shutdown().await;
    std::fs::remove_dir_all(root).unwrap();
}
