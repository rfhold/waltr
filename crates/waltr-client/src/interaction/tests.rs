use super::*;
use futures_util::{FutureExt, stream};

fn update(id: &str, operation: i32, text: &str, committed: bool) -> wire::TextUpdate {
    wire::TextUpdate {
        segment_id: id.into(),
        operation,
        text: text.into(),
        committed,
    }
}
fn format() -> wire::InteractionStreamFormat {
    wire::InteractionStreamFormat {
        content: Some(interaction_stream_format::Content::Text(
            wire::InteractionTextFormat {},
        )),
    }
}
fn accepted() -> wire::InteractResponse {
    wire::InteractResponse {
        event: Some(interact_response::Event::Accepted(
            wire::InteractionAccepted {
                invocation_id: "id".into(),
            },
        )),
    }
}
fn output(events: Vec<wire::InteractResponse>) -> InteractionOutput {
    InteractionOutput::new(
        Box::pin(stream::iter(events.into_iter().map(Ok))),
        InteractionQueue::new(),
        InteractionLimits::default(),
    )
}
fn event(event: interact_response::Event) -> wire::InteractResponse {
    wire::InteractResponse { event: Some(event) }
}
fn text_open(id: &str) -> wire::InteractResponse {
    event(interact_response::Event::OutputOpened(
        wire::InteractionOutputOpened {
            port_name: "answer".into(),
            stream_id: id.into(),
            format: Some(format()),
        },
    ))
}
fn chunk(id: &str, update: wire::TextUpdate) -> wire::InteractResponse {
    event(interact_response::Event::OutputChunk(
        wire::InteractionOutputChunk {
            stream_id: id.into(),
            content: Some(interaction_output_chunk::Content::Text(update)),
        },
    ))
}
fn close(id: &str) -> wire::InteractResponse {
    event(interact_response::Event::OutputClosed(
        wire::InteractionOutputClosed {
            stream_id: id.into(),
            status: 1,
            failure: None,
        },
    ))
}
fn terminal() -> wire::InteractResponse {
    event(interact_response::Event::Completed(
        wire::InteractionCompleted {
            status: wire::RoutineInvocationStatus::Succeeded as i32,
            failure: None,
        },
    ))
}

#[test]
fn segment_revisions_freeze_and_first_appearance_order() {
    let limits = InteractionLimits::default();
    let mut text = InteractionText::default();
    text.update(&update("b", 1, "draft", false), &limits)
        .unwrap();
    text.update(&update("a", 2, " second", true), &limits)
        .unwrap();
    text.update(&update("b", 2, "first", false), &limits)
        .unwrap();
    text.update(&update("b", 1, "!", true), &limits).unwrap();
    assert_eq!(text.text(), "first! second");
    assert!(text.is_committed());
    assert!(
        text.update(&update("b", 2, "wrong", true), &limits)
            .is_err()
    );
    assert_eq!(text.text(), "first! second");
    assert!(text.update(&update("", 1, "", false), &limits).is_err());
    assert!(text.update(&update("c", 0, "", false), &limits).is_err());
}

#[test]
fn segment_byte_and_count_limits_are_atomic_and_include_ids() {
    let limits = InteractionLimits {
        text_bytes: 5,
        text_segments: 1,
        ..Default::default()
    };
    let mut text = InteractionText::default();
    text.update(&update("id", 2, "123", false), &limits)
        .unwrap();
    assert!(text.update(&update("id", 1, "4", false), &limits).is_err());
    assert!(text.update(&update("new", 2, "", true), &limits).is_err());
    assert_eq!(text.text(), "123");
    text.update(&update("id", 2, "", true), &limits).unwrap();
    assert_eq!(text.text(), "");
}

#[test]
fn large_replacements_release_capacity_across_distinct_segments() {
    let limits = InteractionLimits::default();
    let mut text = InteractionText::default();
    let large = "x".repeat(200 * 1024);
    for index in 0..limits.text_segments {
        let id = index.to_string();
        text.update(&update(&id, 2, &large, false), &limits)
            .unwrap();
        text.update(&update(&id, 2, "", true), &limits).unwrap();
        let retained: usize = text
            .segments
            .iter()
            .map(|(id, value, _)| id.capacity() + value.capacity())
            .sum();
        assert!(retained <= limits.text_bytes);
        assert_eq!(text.segments[index].1.capacity(), 0);
    }
    assert_eq!(text.bytes, 0);
    assert!(text.is_committed());
}

#[test]
fn append_replace_capacity_stays_bounded_and_rejection_is_atomic() {
    let limits = InteractionLimits {
        text_bytes: 64 * 1024,
        text_segments: 32,
        ..Default::default()
    };
    let mut text = InteractionText::default();
    for index in 0..limits.text_segments {
        let id = index.to_string();
        for _ in 0..513 {
            text.update(&update(&id, 1, "x", false), &limits).unwrap();
        }
        text.update(&update(&id, 2, "short", false), &limits)
            .unwrap();
        assert_eq!(text.segments[index].1.capacity(), "short".len());
        for _ in 0..257 {
            text.update(&update(&id, 1, "y", false), &limits).unwrap();
        }
        let retained: usize = text
            .segments
            .iter()
            .map(|(id, value, _)| {
                assert!(value.capacity() <= (2 * value.len()).max(8));
                id.capacity() + value.capacity()
            })
            .sum();
        assert!(retained <= 2 * limits.text_bytes + 8 * limits.text_segments);
    }
    let before = text.text();
    let before_bytes = text.bytes;
    let capacities: Vec<_> = text.segments.iter().map(|s| s.1.capacity()).collect();
    assert!(
        text.update(
            &update("0", 2, &"z".repeat(limits.text_bytes), true),
            &limits
        )
        .is_err()
    );
    assert!(text.text() == before);
    assert_eq!(text.bytes, before_bytes);
    assert!(text.segments.iter().all(|s| !s.2));
    assert!(text.segments.iter().map(|s| s.1.capacity()).eq(capacities));
}

#[tokio::test]
async fn output_strict_order_close_and_final_success_gating() {
    let valid = vec![
        accepted(),
        text_open("out"),
        chunk("out", update("s", 2, "final", true)),
        close("out"),
        terminal(),
    ];
    let mut result = output(valid.clone());
    for _ in 0..valid.len() {
        result.next().await.unwrap().unwrap();
        assert!(result.final_text("answer").is_err());
    }
    assert!(result.next().await.is_none());
    assert_eq!(result.final_text("answer").unwrap(), "final");
    let invalid_sequences = vec![
        vec![text_open("out")],
        vec![accepted(), accepted()],
        vec![accepted(), chunk("unknown", update("s", 1, "x", true))],
        vec![accepted(), close("unknown")],
        vec![accepted(), text_open("out"), text_open("out")],
        vec![
            accepted(),
            text_open("out"),
            chunk("out", update("s", 2, "x", false)),
            close("out"),
        ],
        vec![
            accepted(),
            text_open("out"),
            chunk("out", update("s", 2, "x", true)),
            chunk("out", update("s", 1, "y", true)),
        ],
        vec![
            accepted(),
            text_open("out"),
            close("out"),
            chunk("out", update("s", 1, "x", true)),
        ],
        vec![accepted(), text_open("out"), terminal()],
        vec![accepted(), terminal(), close("out")],
    ];
    for events in invalid_sequences {
        let mut result = output(events);
        let mut errors = 0;
        while let Some(event) = result.next().await {
            errors += usize::from(event.is_err());
        }
        assert_eq!(errors, 1);
        assert!(result.final_text("answer").is_err());
    }
}

#[tokio::test]
async fn all_output_closes_and_terminal_failure_payloads_are_checked() {
    for status in [0, 1, 2, 3, 99] {
        let mut result = output(vec![
            accepted(),
            text_open("out"),
            event(interact_response::Event::OutputClosed(
                wire::InteractionOutputClosed {
                    stream_id: "out".into(),
                    status,
                    failure: Some(wire::RoutineFailure::default()),
                },
            )),
        ]);
        result.next().await.unwrap().unwrap();
        result.next().await.unwrap().unwrap();
        assert_eq!(result.next().await.unwrap().is_ok(), status == 3);
    }
    for status in [
        wire::RoutineInvocationStatus::Succeeded,
        wire::RoutineInvocationStatus::Failed,
        wire::RoutineInvocationStatus::Interrupted,
    ] {
        for failure in [false, true] {
            let mut result = output(vec![
                accepted(),
                event(interact_response::Event::Completed(
                    wire::InteractionCompleted {
                        status: status as i32,
                        failure: failure.then(Default::default),
                    },
                )),
            ]);
            result.next().await.unwrap().unwrap();
            assert_eq!(
                result.next().await.unwrap().is_ok(),
                failure != (status == wire::RoutineInvocationStatus::Succeeded)
            );
        }
    }
}

#[tokio::test]
async fn output_total_text_limits_and_ambiguous_selected_output() {
    let mut result = output(vec![
        accepted(),
        text_open("a"),
        chunk("a", update("s", 2, "123", true)),
        text_open("b"),
        chunk("b", update("s", 2, "456", true)),
    ]);
    result.limits.text_bytes = 7;
    for _ in 0..4 {
        result.next().await.unwrap().unwrap();
    }
    assert!(result.next().await.unwrap().is_err());
    let mut result = output(vec![
        accepted(),
        text_open("a"),
        close("a"),
        text_open("b"),
        close("b"),
        terminal(),
    ]);
    while let Some(event) = result.next().await {
        event.unwrap();
    }
    assert!(result.final_text("answer").is_err());
}

#[tokio::test]
async fn bounded_queue_cancel_wakes_waiter_and_bypasses_full_queue() {
    let queue = InteractionQueue::new();
    queue.state.lock().unwrap().accepted = true;
    let limits = InteractionLimits {
        input_items: 1,
        ..Default::default()
    };
    let mut input = InteractionInputHandle::new(queue.clone(), limits);
    let control = input.control();
    input
        .send(interact_request::Event::InputOpened(
            wire::InteractionInputOpened {
                port_name: "text".into(),
                stream_id: "in".into(),
                format: Some(format()),
            },
        ))
        .await
        .unwrap();
    let mut blocked = Box::pin(input.send(interact_request::Event::InputChunk(
        wire::InteractionInputChunk {
            stream_id: "in".into(),
            content: Some(interaction_input_chunk::Content::Text(update(
                "s", 1, "text", true,
            ))),
        },
    )));
    assert!(blocked.as_mut().now_or_never().is_none());
    control.cancel();
    assert!(blocked.await.is_err());
    assert_eq!(input.streams.entries["in"].text.text(), "");
    let mut requests = InteractionRequests(queue);
    assert!(matches!(
        requests.next().await.unwrap().event,
        Some(interact_request::Event::Cancel(_))
    ));
    assert!(requests.next().await.is_none());
}

#[tokio::test]
async fn dropped_send_is_transactional_and_output_drop_cancels_input() {
    let queue = InteractionQueue::new();
    queue.state.lock().unwrap().accepted = true;
    let limits = InteractionLimits {
        input_items: 1,
        ..Default::default()
    };
    let mut input = InteractionInputHandle::new(queue.clone(), limits.clone());
    input
        .send(interact_request::Event::InputOpened(
            wire::InteractionInputOpened {
                port_name: "text".into(),
                stream_id: "in".into(),
                format: Some(format()),
            },
        ))
        .await
        .unwrap();
    {
        let mut blocked = Box::pin(input.finish_input());
        // Fails validation, because the stream is still open.
        assert!(blocked.as_mut().now_or_never().unwrap().is_err());
    }
    {
        let mut blocked = Box::pin(input.send(interact_request::Event::InputClosed(
            wire::InteractionInputClosed {
                stream_id: "in".into(),
                status: 1,
                failure: None,
            },
        )));
        assert!(blocked.as_mut().now_or_never().is_none());
    }
    assert_eq!(input.streams.entries["in"].closed, None);
    let output = InteractionOutput::new(Box::pin(stream::pending()), queue, limits);
    drop(output);
    assert!(
        input
            .send(interact_request::Event::InputClosed(
                wire::InteractionInputClosed {
                    stream_id: "in".into(),
                    status: 1,
                    failure: None
                }
            ))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn release_keeps_request_side_open_and_cancellation_available() {
    let queue = InteractionQueue::new();
    queue.state.lock().unwrap().accepted = true;
    let mut input = InteractionInputHandle::new(queue.clone(), InteractionLimits::default());
    let mut requests = InteractionRequests(queue);
    input.finish_input().await.unwrap();
    assert!(matches!(
        requests.next().await.unwrap().event,
        Some(interact_request::Event::InputCompleted(_))
    ));
    assert!(requests.next().now_or_never().is_none());
    assert!(input.finish_input().await.is_err());
    input.control().cancel();
    assert!(matches!(
        requests.next().await.unwrap().event,
        Some(interact_request::Event::Cancel(_))
    ));
    assert!(requests.next().await.is_none());
}

#[tokio::test]
async fn message_chunk_and_queue_byte_limits_reject_before_enqueue() {
    let queue = InteractionQueue::new();
    queue.state.lock().unwrap().accepted = true;
    let limits = InteractionLimits {
        input_bytes: 128,
        chunk_bytes: 8,
        ..Default::default()
    };
    let mut input = InteractionInputHandle::new(queue.clone(), limits);
    input
        .send(interact_request::Event::InputOpened(
            wire::InteractionInputOpened {
                port_name: "text".into(),
                stream_id: "in".into(),
                format: Some(format()),
            },
        ))
        .await
        .unwrap();
    assert!(
        input
            .send(interact_request::Event::InputChunk(
                wire::InteractionInputChunk {
                    stream_id: "in".into(),
                    content: Some(interaction_input_chunk::Content::Text(update(
                        "s",
                        2,
                        "too long text",
                        true
                    )))
                }
            ))
            .await
            .is_err()
    );
    assert_eq!(queue.state.lock().unwrap().items.len(), 1);
    assert!(input.streams.entries["in"].text.segments.is_empty());
    let mut out = output(vec![accepted(), text_open("a")]);
    out.limits.output_message_bytes = 1;
    assert!(out.next().await.unwrap().is_err());
}

#[test]
fn input_formats_frame_alignment_duplicate_ids_and_complete_body_validation() {
    let limits = InteractionLimits::default();
    let mut streams = Streams::default();
    let audio = wire::InteractionAudioFormat {
        encoding: 1,
        sample_rate_hz: 48000,
        channels: 2,
    };
    let format = wire::InteractionStreamFormat {
        content: Some(interaction_stream_format::Content::Audio(audio)),
    };
    streams
        .open("audio".into(), "port".into(), Some(format), &limits)
        .unwrap();
    assert!(
        streams
            .chunk("audio", Some(&[0, 0]), None, &limits)
            .is_err()
    );
    assert!(streams.chunk("audio", Some(&[0; 4]), None, &limits).is_ok());
    assert!(
        streams
            .chunk("audio", None, Some(&update("s", 1, "text", true)), &limits)
            .is_err()
    );
    assert!(
        streams
            .open(
                "audio".into(),
                "port".into(),
                Some(super::tests::format()),
                &limits
            )
            .is_err()
    );
    streams.close("audio", 1, false).unwrap();
    assert!(
        streams
            .chunk("audio", Some(&[0; 4]), None, &limits)
            .is_err()
    );
    let mut request = wire::RunInteractionRequest {
        open: Some(wire::OpenInteraction::default()),
        inputs: vec![wire::InteractionInput {
            port_name: "p".into(),
            content: Some(wire::interaction_input::Content::Audio(
                wire::InteractionAudioInput {
                    format: Some(audio),
                    data: vec![0; 2],
                },
            )),
        }],
    };
    assert!(validate_complete_interaction(&request, &limits).is_err());
    request.inputs[0].content = Some(wire::interaction_input::Content::Text("text".into()));
    validate_complete_interaction(&request, &limits).unwrap();
    request.inputs.push(request.inputs[0].clone());
    assert!(validate_complete_interaction(&request, &limits).is_err());
}
