//! Native duplex and browser-compatible split-lane conformance.
#[path = "support/routine.rs"]
mod fixture;

#[tokio::test]
async fn production_pcm_and_utf8_declarations_use_current_formats() {
    let server = fixture::server().await;
    let mut limits = fixture::client_limits(&server.transport).await;
    limits.definition.ports[0].formats = vec!["pcm16le-24000-mono".into()];
    let mut open = fixture::open("production-formats");
    open.inputs[0].format = Some(wire::input_declaration::Format::Pcm16le(wire::Pcm16Le {
        sample_rate: 24000,
        channels: 1,
    }));
    assert!(limits.validate_open(&open).is_ok());
    limits.definition.ports[0].media = waltr_client::RoutineMedia::Text;
    limits.definition.ports[0].formats = vec!["utf8".into()];
    open.inputs[0].format = Some(wire::input_declaration::Format::Text(wire::TextFormat {}));
    assert!(limits.validate_open(&open).is_ok());
}

#[tokio::test]
async fn production_pcm_declarations_reject_other_rates_channels_and_contracts() {
    use waltr_client::{RoutineDirection as D, RoutineMedia as M};
    let server = fixture::server().await;
    let limits = fixture::client_limits(&server.transport).await;
    for (rate, channels, direction, media, format) in [
        (16000, 1, D::Input, M::Audio, "pcm16le-24000-mono"),
        (48000, 1, D::Input, M::Audio, "pcm16le-24000-mono"),
        (24000, 2, D::Input, M::Audio, "pcm16le-24000-mono"),
        (24000, 1, D::Output, M::Audio, "pcm16le-24000-mono"),
        (24000, 1, D::Input, M::Text, "pcm16le-24000-mono"),
        (24000, 1, D::Input, M::Audio, "pcm16le"),
        (24000, 1, D::Input, M::Audio, "pcm16le-48000-mono"),
    ] {
        let mut limits = limits.clone();
        limits.definition.ports[0].direction = direction;
        limits.definition.ports[0].media = media;
        limits.definition.ports[0].formats = vec![format.into()];
        let mut open = fixture::open("invalid-formats");
        open.inputs[0].format = Some(wire::input_declaration::Format::Pcm16le(wire::Pcm16Le {
            sample_rate: rate,
            channels,
        }));
        assert!(
            limits.validate_open(&open).is_err(),
            "{rate} {channels} {direction:?} {media:?} {format}"
        );
    }
}

#[tokio::test]
async fn native_accepted_narrowing_rebuilds_chunk_queue_text_and_stream_ceilings() {
    let server = fixture::server().await;
    let limits = fixture::client_limits(&server.transport).await;
    let mut s = server
        .transport
        .interact(fixture::open("narrowed"), limits)
        .await
        .unwrap();
    assert!(s.input.accepted_limits().is_err());
    s.output.next().await.unwrap().unwrap();
    let negotiated = s.input.accepted_limits().unwrap();
    assert_eq!(negotiated.chunk_bytes, 4);
    assert_eq!(negotiated.input_items, 1);
    assert_eq!(negotiated.input_bytes, 128);
    assert_eq!(negotiated.text_bytes, 4);
    assert_eq!(negotiated.text_segments, 1);
    assert_eq!(negotiated.streams, 1);
    s.input.send(fixture::stream_open()).await.unwrap();
    assert!(
        s.input
            .send(wire::input_event::Event::Chunk(wire::StreamChunk {
                stream_id: "in".into(),
                sequence: 0,
                payload: Some(wire::stream_chunk::Payload::Audio(vec![0; 8]))
            }))
            .await
            .is_err()
    );
    let debug = format!("{:?}", s.input);
    assert!(debug.contains("input_items: 1"));
    assert!(debug.contains("input_bytes: 128"));
    assert!(debug.contains("text_bytes: 4"));
    assert!(debug.contains("text_segments: 1"));
    assert!(debug.contains("streams: 1"));
    let mut second = fixture::stream_open();
    if let wire::input_event::Event::Open(o) = &mut second {
        o.stream_id = "second".into();
    }
    assert!(s.input.send(second).await.is_err());
    s.input.cancel();
    assert!(s.input.accepted_limits().is_err());
}

#[tokio::test]
async fn native_accepted_text_and_segment_limits_reject_locally_without_consuming_sequence() {
    let server = fixture::server().await;
    let mut limits = fixture::client_limits(&server.transport).await;
    let port = limits
        .definition
        .ports
        .iter_mut()
        .find(|p| p.name == "audio")
        .unwrap();
    port.media = waltr_client::RoutineMedia::Text;
    port.formats = vec!["utf8".into()];
    let mut open = fixture::open("narrowed");
    open.inputs[0].format = Some(wire::input_declaration::Format::Text(wire::TextFormat {}));
    let mut s = server.transport.interact(open, limits).await.unwrap();
    s.output.next().await.unwrap().unwrap();
    s.input
        .send(wire::input_event::Event::Open(wire::StreamOpen {
            port: "audio".into(),
            stream_id: "in".into(),
            format: Some(wire::stream_open::Format::Text(wire::TextFormat {})),
        }))
        .await
        .unwrap();
    let chunk = |seq, segment: &str, text: &str| {
        wire::input_event::Event::Chunk(wire::StreamChunk {
            stream_id: "in".into(),
            sequence: seq,
            payload: Some(wire::stream_chunk::Payload::Text(wire::TextRevision {
                segment_id: segment.into(),
                text: text.into(),
                mutation: 2,
                committed: true,
            })),
        })
    };
    assert!(s.input.send(chunk(0, "s", "1234")).await.is_err());
    s.input.send(chunk(0, "s", "1")).await.unwrap();
    assert!(s.input.send(chunk(1, "t", "1")).await.is_err());
}

async fn aggregate_input_case(split: bool, text: bool) {
    use waltr_client::{RoutineMedia, RoutineRevision};
    let server = fixture::server().await;
    let mut limits = fixture::client_limits(&server.transport).await;
    let port = limits
        .definition
        .ports
        .iter_mut()
        .find(|p| p.name == "audio")
        .unwrap();
    port.max_bytes = RoutineRevision(6);
    if text {
        port.media = RoutineMedia::Text;
        port.formats = vec!["utf8".into()];
    }
    let mut other = port.clone();
    other.name = "other".into();
    limits.definition.ports.push(other);
    let mut open = fixture::open("aggregate");
    if text {
        open.inputs[0].format = Some(wire::input_declaration::Format::Text(wire::TextFormat {}));
    }
    let mut other = open.inputs[0].clone();
    other.port = "other".into();
    open.inputs.push(other);
    let session = if split {
        None
    } else {
        Some(
            server
                .transport
                .interact(open.clone(), limits.clone())
                .await
                .unwrap(),
        )
    };
    let attachment = if split {
        Some(
            server
                .transport
                .open_attachment(open, limits)
                .await
                .unwrap(),
        )
    } else {
        None
    };
    let (mut native, mut browser) = (session, attachment);
    if let Some(s) = &mut native {
        s.output.next().await.unwrap().unwrap();
    }
    if let Some(s) = &mut browser {
        s.output.next().await.unwrap().unwrap();
    }
    let open = |id: &str, port: &str| {
        wire::input_event::Event::Open(wire::StreamOpen {
            port: port.into(),
            stream_id: id.into(),
            format: Some(if text {
                wire::stream_open::Format::Text(wire::TextFormat {})
            } else {
                wire::stream_open::Format::Pcm16le(wire::Pcm16Le {
                    sample_rate: 24000,
                    channels: 1,
                })
            }),
        })
    };
    let chunk = |id: &str| {
        wire::input_event::Event::Chunk(wire::StreamChunk {
            stream_id: id.into(),
            sequence: 0,
            payload: Some(if text {
                wire::stream_chunk::Payload::Text(wire::TextRevision {
                    segment_id: "s".into(),
                    text: "four".into(),
                    mutation: 2,
                    committed: true,
                })
            } else {
                wire::stream_chunk::Payload::Audio(vec![0; 4])
            }),
        })
    };
    for event in [
        open("first", "audio"),
        chunk("first"),
        wire::input_event::Event::Close(wire::StreamClose {
            stream_id: "first".into(),
            next_sequence: 1,
        }),
        open("second", "audio"),
        open("independent", "other"),
        chunk("independent"),
    ] {
        if let Some(s) = &mut native {
            s.input.send(event.clone()).await.unwrap();
        }
        if let Some(s) = &mut browser {
            s.input
                .send(vec![wire::InputEvent { event: Some(event) }])
                .await
                .unwrap();
        }
    }
    if let Some(s) = &mut native {
        assert!(s.input.send(chunk("second")).await.is_err());
    }
    if let Some(s) = &mut browser {
        assert!(
            s.input
                .send(vec![wire::InputEvent {
                    event: Some(chunk("second"))
                }])
                .await
                .is_err()
        );
    }
    let small = wire::input_event::Event::Chunk(wire::StreamChunk {
        stream_id: "second".into(),
        sequence: 0,
        payload: Some(if text {
            wire::stream_chunk::Payload::Text(wire::TextRevision {
                segment_id: "s".into(),
                text: "ok".into(),
                mutation: 2,
                committed: true,
            })
        } else {
            wire::stream_chunk::Payload::Audio(vec![0; 2])
        }),
    });
    if let Some(s) = &mut native {
        s.input.send(small.clone()).await.unwrap();
    }
    if let Some(s) = &mut browser {
        s.input
            .send(vec![wire::InputEvent { event: Some(small) }])
            .await
            .unwrap();
    }
}
#[tokio::test]
async fn native_pcm_ports_accumulate_across_closed_streams() {
    aggregate_input_case(false, false).await;
}
#[tokio::test]
async fn split_pcm_ports_accumulate_across_closed_streams() {
    aggregate_input_case(true, false).await;
}
#[tokio::test]
async fn native_text_ports_accumulate_across_closed_streams() {
    aggregate_input_case(false, true).await;
}
#[tokio::test]
async fn split_text_ports_accumulate_across_closed_streams() {
    aggregate_input_case(true, true).await;
}

#[tokio::test]
async fn native_and_split_output_ports_accumulate_pcm_and_text_across_streams() {
    for split in [false, true] {
        for pcm in [false, true] {
            let server = fixture::server().await;
            let mut limits = fixture::client_limits(&server.transport).await;
            let port = limits
                .definition
                .ports
                .iter_mut()
                .find(|p| p.name == "text")
                .unwrap();
            port.max_bytes = waltr_client::RoutineRevision(6);
            if pcm {
                port.media = waltr_client::RoutineMedia::Audio;
                port.formats = vec!["pcm16le-24000-mono".into()];
            }
            let open = fixture::open(if pcm {
                "aggregate-output-pcm"
            } else {
                "aggregate-output-text"
            });
            let mut native = if !split {
                Some(
                    server
                        .transport
                        .interact(open.clone(), limits.clone())
                        .await
                        .unwrap(),
                )
            } else {
                None
            };
            let mut browser = if split {
                Some(
                    server
                        .transport
                        .open_attachment(open, limits)
                        .await
                        .unwrap(),
                )
            } else {
                None
            };
            let output = if let Some(s) = &mut native {
                &mut s.output
            } else {
                &mut browser.as_mut().unwrap().output
            };
            for _ in 0..5 {
                output.next().await.unwrap().unwrap();
            }
            let failed = output.next().await.unwrap().is_err();
            assert!(failed);
            assert!(!output.cleanup_verified());
            assert!(output.final_text("text").is_err());
        }
    }
}

#[tokio::test]
async fn terminal_success_waits_for_delayed_clean_eof_on_native_and_split_lanes() {
    use waltr_client::interaction::InteractionEvent;
    for split in [false, true] {
        let server = fixture::server().await;
        let limits = fixture::client_limits(&server.transport).await;
        let mut open = fixture::open("delayed-eof");
        open.inputs.clear();
        let mut native = if !split {
            Some(
                server
                    .transport
                    .interact(open.clone(), limits.clone())
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let mut browser = if split {
            Some(
                server
                    .transport
                    .open_attachment(open, limits)
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let output = if let Some(s) = &mut native {
            &mut s.output
        } else {
            &mut browser.as_mut().unwrap().output
        };
        for _ in 0..3 {
            output.next().await.unwrap().unwrap();
        }
        if let Some(s) = &mut native {
            s.input.finish_input().await.unwrap();
        }
        if let Some(s) = &mut browser {
            s.input.finish_input().await.unwrap();
        }
        let output = if let Some(s) = &mut native {
            &mut s.output
        } else {
            &mut browser.as_mut().unwrap().output
        };
        loop {
            if matches!(
                output.next().await.unwrap().unwrap(),
                InteractionEvent::Terminal { .. }
            ) {
                break;
            }
        }
        assert!(output.final_text("text").is_err());
        assert!(!output.cleanup_verified());
        assert!(output.next().await.is_none());
        assert_eq!(output.final_text("text").unwrap(), "final");
        assert!(output.cleanup_verified());
        assert!(output.next().await.is_none());
    }
}

#[tokio::test]
async fn trailing_output_or_missing_eof_after_terminal_never_commits_final_text() {
    use futures_util::FutureExt;
    use waltr_client::interaction::InteractionEvent;
    for split in [false, true] {
        for mode in ["terminal-trailing", "never-eof"] {
            let server = fixture::server().await;
            let limits = fixture::client_limits(&server.transport).await;
            let mut open = fixture::open(mode);
            open.inputs.clear();
            let mut native = if !split {
                Some(
                    server
                        .transport
                        .interact(open.clone(), limits.clone())
                        .await
                        .unwrap(),
                )
            } else {
                None
            };
            let mut browser = if split {
                Some(
                    server
                        .transport
                        .open_attachment(open, limits)
                        .await
                        .unwrap(),
                )
            } else {
                None
            };
            let output = if let Some(s) = &mut native {
                &mut s.output
            } else {
                &mut browser.as_mut().unwrap().output
            };
            for _ in 0..3 {
                output.next().await.unwrap().unwrap();
            }
            if let Some(s) = &mut native {
                s.input.finish_input().await.unwrap();
            }
            if let Some(s) = &mut browser {
                s.input.finish_input().await.unwrap();
            }
            let output = if let Some(s) = &mut native {
                &mut s.output
            } else {
                &mut browser.as_mut().unwrap().output
            };
            loop {
                if matches!(
                    output.next().await.unwrap().unwrap(),
                    InteractionEvent::Terminal { .. }
                ) {
                    break;
                }
            }
            if mode == "never-eof" {
                assert!(output.next().now_or_never().is_none());
            } else {
                assert!(output.next().await.unwrap().is_err());
            }
            assert!(!output.cleanup_verified());
            assert!(output.final_text("text").is_err());
        }
    }
}

#[tokio::test]
async fn native_finite_file_uses_the_same_declaration_quarantine_and_commit_boundary() {
    let server = fixture::server().await;
    let mut limits = fixture::client_limits(&server.transport).await;
    limits
        .definition
        .ports
        .iter_mut()
        .find(|p| p.name == "file")
        .unwrap()
        .required = true;
    let mut open = fixture::open("file");
    open.inputs = vec![wire::InputDeclaration {
        port: "file".into(),
        format: Some(wire::input_declaration::Format::Binary(
            wire::BinaryInputDeclaration {
                display_name: "file.bin".into(),
                content_type: "application/octet-stream".into(),
                declared_bytes: Some(3),
                expected_sha256: None,
                purpose: Some(wire::binary_input_declaration::Purpose::File(
                    wire::FilePurpose {},
                )),
            },
        )),
    }];
    let mut s = server.transport.interact(open, limits).await.unwrap();
    let transfers = server.transport.media_transfers();
    assert!(s.input.begin_upload(&transfers, "file").is_err());
    for _ in 0..3 {
        s.output.next().await.unwrap().unwrap();
    }
    let mut upload = s.input.begin_upload(&transfers, "file").unwrap();
    upload.begin().await.unwrap();
    upload.append(&[1, 2, 3]).await.unwrap();
    let (_, event) = upload.commit().await.unwrap();
    s.input.send(event.event.unwrap()).await.unwrap();
    s.input.finish_input().await.unwrap();
    while let Some(event) = s.output.next().await {
        event.unwrap();
    }
    assert_eq!(s.output.final_text("text").unwrap(), "final");
}

#[tokio::test]
async fn dropped_pending_batch_waiter_cancels_owner_without_fabricated_join() {
    use futures_util::{
        future::{Either, select},
        pin_mut,
    };
    let server = fixture::server().await;
    let limits = fixture::client_limits(&server.transport).await;
    let mut s = server
        .transport
        .open_attachment(fixture::open("blocked-batch"), limits)
        .await
        .unwrap();
    s.output.next().await.unwrap().unwrap();
    {
        let send = s.input.send(vec![wire::InputEvent {
            event: Some(fixture::stream_open()),
        }]);
        pin_mut!(send);
        let wait = tokio::time::sleep(std::time::Duration::from_millis(10));
        pin_mut!(wait);
        assert!(matches!(select(send, wait).await, Either::Right(_)));
    }
    assert!(s.control.is_cancelled());
    assert!(!s.output.cleanup_verified());
    assert!(s.output.next().await.unwrap().is_err());
    assert!(s.control.cancel_and_join().await.unwrap());
    assert_eq!(server.service.admissions().1, 1);
}

#[tokio::test]
async fn ordered_text_segments_replace_append_commit_and_limit_without_reordering() {
    let server = fixture::server().await;
    let mut limits = fixture::client_limits(&server.transport).await;
    limits.text_bytes = 64;
    limits.text_segments = 2;
    let mut text = waltr_client::interaction::InteractionText::default();
    let update = |id: &str, value: &str, mutation, committed| wire::TextRevision {
        segment_id: id.into(),
        text: value.into(),
        mutation,
        committed,
    };
    text.update(&update("a", "one", 1, false), &limits).unwrap();
    text.update(&update("b", "two", 1, true), &limits).unwrap();
    text.update(&update("a", "ONE", 2, false), &limits).unwrap();
    text.update(&update("a", "!", 1, true), &limits).unwrap();
    assert_eq!(text.text(), "ONE!two");
    assert_eq!(text.committed_prefix(), "ONE!two");
    assert!(
        text.update(&update("a", "changed", 2, true), &limits)
            .is_err()
    );
    assert!(
        text.update(&update("c", "overflow", 1, true), &limits)
            .is_err()
    );
    assert!(text.is_committed());
}

#[tokio::test]
async fn unknown_output_bad_sequences_missing_close_and_failed_terminal_never_commit_text() {
    for mode in ["unknown-output", "bad-sequence", "missing-close", "failed"] {
        let server = fixture::server().await;
        let limits = fixture::client_limits(&server.transport).await;
        let mut s = server
            .transport
            .interact(fixture::open(mode), limits)
            .await
            .unwrap();
        if mode == "unknown-output" {
            assert!(s.output.next().await.unwrap().is_err());
        } else {
            s.output.next().await.unwrap().unwrap();
            s.output.next().await.unwrap().unwrap();
            if mode == "bad-sequence" {
                assert!(s.output.next().await.unwrap().is_err());
            } else {
                s.output.next().await.unwrap().unwrap();
                s.input.send(fixture::stream_open()).await.unwrap();
                s.input.send(fixture::stream_close()).await.unwrap();
                s.input.finish_input().await.unwrap();
                while let Some(r) = s.output.next().await {
                    if r.is_err() {
                        break;
                    }
                }
            }
        }
        assert!(s.output.final_text("text").is_err(), "{mode}");
        assert!(!s.output.succeeded(), "{mode}");
    }
}

#[tokio::test]
async fn process_output_grant_is_single_consumer_and_history_is_not_read_authority() {
    for mode in ["blob", "expired-blob"] {
        let server = fixture::server().await;
        let mut limits = fixture::client_limits(&server.transport).await;
        let mut port = limits
            .definition
            .ports
            .iter()
            .find(|p| p.name == "file")
            .unwrap()
            .clone();
        port.name = "download".into();
        port.direction = waltr_client::RoutineDirection::Output;
        limits.definition.ports.push(port);
        let mut s = server
            .transport
            .interact(fixture::open(mode), limits)
            .await
            .unwrap();
        for _ in 0..4 {
            s.output.next().await.unwrap().unwrap();
        }
        let grant = s.output.take_blob_grant("download").unwrap();
        assert!(!format!("{grant:?}").contains("SECRET-OUTPUT-CANARY"));
        assert!(s.output.take_blob_grant("download").is_none());
        let transfers = server.transport.media_transfers();
        assert!(
            transfers
                .read_input("invocation".into(), "output-blob".into())
                .await
                .is_err()
        );
        if mode == "expired-blob" {
            assert!(transfers.read_output(grant).await.is_err());
        } else {
            let mut reader = transfers.read_output(grant).await.unwrap();
            let mut bytes = vec![];
            while let Some(chunk) = reader.next().await {
                bytes.extend(chunk.unwrap());
            }
            assert_eq!(bytes, vec![1, 2, 3]);
            assert!(reader.complete());
            reader.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn full_native_input_queue_cancellation_wakes_the_blocked_writer() {
    use futures_util::{
        future::{Either, select},
        pin_mut,
    };
    let server = fixture::server().await;
    let limits = fixture::client_limits(&server.transport).await;
    let mut session = server
        .transport
        .interact(fixture::open("blocked"), limits)
        .await
        .unwrap();
    for _ in 0..3 {
        session.output.next().await.unwrap().unwrap();
    }
    let control = session.input.control();
    let writer = async {
        session.input.send(fixture::stream_open()).await?;
        for sequence in 0..100 {
            session
                .input
                .send(wire::input_event::Event::Chunk(wire::StreamChunk {
                    stream_id: "in".into(),
                    sequence,
                    payload: Some(wire::stream_chunk::Payload::Audio(vec![0; 65536])),
                }))
                .await?;
        }
        Ok::<(), waltr_client::ClientError>(())
    };
    pin_mut!(writer);
    let wait = tokio::time::sleep(std::time::Duration::from_millis(20));
    pin_mut!(wait);
    match select(writer.as_mut(), wait).await {
        Either::Left(_) => panic!("fixture must backpressure the native queue"),
        Either::Right(((), _)) => {}
    }
    control.cancel();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(1), writer)
            .await
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn blocked_unary_input_does_not_queue_renew_or_cancel_control() {
    use futures_util::{
        future::{Either, select},
        pin_mut,
    };
    let server = fixture::server().await;
    let limits = fixture::client_limits(&server.transport).await;
    let mut s = server
        .transport
        .open_attachment(fixture::open("blocked-batch"), limits)
        .await
        .unwrap();
    s.output.next().await.unwrap().unwrap();
    let send = s.input.send(vec![wire::InputEvent {
        event: Some(fixture::stream_open()),
    }]);
    pin_mut!(send);
    let wait = tokio::time::sleep(std::time::Duration::from_millis(20));
    pin_mut!(wait);
    assert!(matches!(
        select(send.as_mut(), wait).await,
        Either::Right(_)
    ));
    s.control.renew().await.unwrap();
    assert_eq!(server.service.renews(), 1);
    assert!(s.control.cancel_and_join().await.unwrap());
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(1), send)
            .await
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn access_generation_change_wakes_idle_output_and_refresh_does_not_replace_owner() {
    let server = fixture::server().await;
    let limits = fixture::client_limits(&server.transport).await;
    let lease = server
        .transport
        .replace_bearer_token("principal-one")
        .unwrap();
    let mut session = server
        .transport
        .interact(fixture::open("native"), limits)
        .await
        .unwrap();
    for _ in 0..3 {
        session.output.next().await.unwrap().unwrap();
    }
    assert!(
        server
            .transport
            .replace_bearer_token_if_current(lease, "refreshed")
            .unwrap()
    );
    session.input.send(fixture::stream_open()).await.unwrap();
    let pending = session.output.next();
    let replace = async {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        server
            .transport
            .replace_bearer_token("principal-two")
            .unwrap();
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        futures_util::join!(pending, replace)
    })
    .await
    .unwrap();
    assert!(result.unwrap().is_err());
    assert!(session.input.finish_input().await.is_err());
    assert!(!session.output.cleanup_verified());
}

use futures_util::StreamExt;
use waltr_client::{
    RoutineInteractionTransport,
    interaction::{InteractionEvent, wire},
};

#[tokio::test]
async fn native_cancel_after_physical_eof_joins_exact_invocation_and_drains_cleanup() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for mode in 0..3 {
            let server = fixture::server().await;
            server
                .transport
                .replace_bearer_token("post-eof-owner")
                .unwrap();
            let limits = fixture::client_limits(&server.transport).await;
            let waltr_client::interaction::InteractionSession {
                mut input,
                mut output,
            } = server
                .transport
                .interact(fixture::open("native-cancel-eof"), limits)
                .await
                .unwrap();
            for _ in 0..3 {
                output.next().await.unwrap().unwrap();
            }
            input.send(fixture::stream_open()).await.unwrap();
            input.send(fixture::stream_close()).await.unwrap();
            input.finish_input().await.unwrap();
            server.service.wait_native_eof().await;
            assert!(output.terminal().is_none());
            assert!(server.service.cancellations().is_empty());
            match mode {
                0 => {
                    let control = input.control();
                    control.cancel();
                    control.cancel();
                }
                1 => {
                    assert!(output.cancel_and_join().await.unwrap());
                }
                _ => drop(input),
            }
            while let Some(event) = output.next().await {
                event.unwrap();
            }
            assert_eq!(server.service.cancellations(), vec!["invocation"]);
            assert_eq!(server.service.cancel_acks(), 1);
            assert_eq!(output.terminal().unwrap().status, 5);
            assert!(output.cleanup_verified());
            assert!(!output.succeeded());
        }
    })
    .await
    .expect("post-EOF cancellation must not wait for natural completion");
}

#[tokio::test]
async fn native_half_close_keeps_output_alive_until_normal_terminal_and_eof() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let server = fixture::server().await;
        let limits = fixture::client_limits(&server.transport).await;
        let mut session = server
            .transport
            .interact(fixture::open("native-eof"), limits)
            .await
            .unwrap();
        for _ in 0..3 {
            session.output.next().await.unwrap().unwrap();
        }
        assert_eq!(session.output.preview_text("text").as_deref(), Some("live"));
        session.input.send(fixture::stream_open()).await.unwrap();
        session
            .input
            .send(wire::input_event::Event::Chunk(wire::StreamChunk {
                stream_id: "in".into(),
                sequence: 0,
                payload: Some(wire::stream_chunk::Payload::Audio(vec![0; 4])),
            }))
            .await
            .unwrap();
        session
            .input
            .send(wire::input_event::Event::Close(wire::StreamClose {
                stream_id: "in".into(),
                next_sequence: 1,
            }))
            .await
            .unwrap();
        session.input.finish_input().await.unwrap();
        assert!(session.output.terminal().is_none());
        assert!(!session.output.cleanup_verified());
        let mut terminal = false;
        while let Some(event) = session.output.next().await {
            if let InteractionEvent::Terminal {
                status,
                cleanup_complete,
            } = event.unwrap()
            {
                assert_eq!(
                    status,
                    waltr_client::ClientRoutineInvocationStatus::Succeeded
                );
                assert!(cleanup_complete);
                terminal = true;
            }
        }
        assert!(terminal);
        assert!(session.output.succeeded());
        assert_eq!(session.output.final_text("text").unwrap(), "final");
        assert!(server.service.cancellations().is_empty());
    })
    .await
    .expect("native Finish must half-close input without waiting for terminal");
}

#[tokio::test]
async fn native_live_output_precedes_input_completion_and_clean_eof_commits_text() {
    let server = fixture::server().await;
    let limits = fixture::client_limits(&server.transport).await;
    let mut session = server
        .transport
        .interact(fixture::open("native"), limits)
        .await
        .unwrap();
    assert!(
        session.input.send(fixture::stream_open()).await.is_err(),
        "input must await Accepted"
    );
    assert!(matches!(
        session.output.next().await.unwrap().unwrap(),
        InteractionEvent::Accepted { .. }
    ));
    session.output.next().await.unwrap().unwrap();
    session.output.next().await.unwrap().unwrap();
    assert_eq!(session.output.preview_text("text").as_deref(), Some("live"));
    assert!(session.output.final_text("text").is_err());
    session.input.send(fixture::stream_open()).await.unwrap();
    session.input.send(fixture::stream_close()).await.unwrap();
    session.input.finish_input().await.unwrap();
    while let Some(r) = session.output.next().await {
        r.unwrap();
    }
    assert_eq!(session.output.final_text("text").unwrap(), "final");
    assert!(session.output.cleanup_verified());
}

#[tokio::test]
async fn split_live_input_and_independent_control_use_the_same_output_domain() {
    let server = fixture::server().await;
    let limits = fixture::client_limits(&server.transport).await;
    let mut session = server
        .transport
        .open_attachment(fixture::open("split"), limits)
        .await
        .unwrap();
    assert!(
        session
            .input
            .send(vec![wire::InputEvent {
                event: Some(fixture::stream_open())
            }])
            .await
            .is_err()
    );
    session.output.next().await.unwrap().unwrap();
    session.control.renew().await.unwrap();
    assert_eq!(server.service.renews(), 1);
    session.output.next().await.unwrap().unwrap();
    session.output.next().await.unwrap().unwrap();
    assert_eq!(session.output.preview_text("text").as_deref(), Some("live"));
    session
        .input
        .send(vec![
            wire::InputEvent {
                event: Some(fixture::stream_open()),
            },
            wire::InputEvent {
                event: Some(fixture::stream_close()),
            },
        ])
        .await
        .unwrap();
    session.input.finish_input().await.unwrap();
    while let Some(r) = session.output.next().await {
        r.unwrap();
    }
    assert_eq!(session.output.final_text("text").unwrap(), "final");
    assert_eq!(
        server.service.closes(),
        0,
        "normal input finish is not cancellation"
    );
    let debug = format!("{:?}", session);
    assert!(!debug.contains("SECRET-CANARY"));
}

#[tokio::test]
async fn lost_open_bad_eof_and_ambiguous_or_future_batch_ack_never_retry() {
    let server = fixture::server().await;
    let limits = fixture::client_limits(&server.transport).await;
    assert!(
        server
            .transport
            .open_attachment(fixture::open("lost-open"), limits.clone())
            .await
            .is_err()
    );
    assert_eq!(server.service.admissions(), (0, 1));
    for mode in ["bad-eof", "lost-batch", "future-ack"] {
        let mut s = server
            .transport
            .open_attachment(fixture::open(mode), limits.clone())
            .await
            .unwrap();
        for _ in 0..3 {
            s.output.next().await.unwrap().unwrap();
        }
        let r = s
            .input
            .send(vec![
                wire::InputEvent {
                    event: Some(fixture::stream_open()),
                },
                wire::InputEvent {
                    event: Some(fixture::stream_close()),
                },
            ])
            .await;
        if mode == "bad-eof" {
            r.unwrap();
            s.input.finish_input().await.unwrap();
            assert!(s.output.next().await.unwrap().is_err());
            assert!(s.output.final_text("text").is_err());
        } else {
            assert!(r.is_err());
            assert!(s.input.finish_input().await.is_err());
            assert!(s.control.cancel_and_join().await.unwrap());
        }
    }
    assert_eq!(server.service.admissions(), (0, 4));
}

#[tokio::test]
async fn finite_file_metadata_is_admitted_before_quarantined_upload_and_blob_input() {
    let server = fixture::server().await;
    let mut limits = fixture::client_limits(&server.transport).await;
    limits
        .definition
        .ports
        .iter_mut()
        .find(|p| p.name == "file")
        .unwrap()
        .required = true;
    let mut open = fixture::open("file");
    open.inputs = vec![wire::InputDeclaration {
        port: "file".into(),
        format: Some(wire::input_declaration::Format::Binary(
            wire::BinaryInputDeclaration {
                display_name: "file.bin".into(),
                content_type: "application/octet-stream".into(),
                declared_bytes: Some(3),
                expected_sha256: None,
                purpose: Some(wire::binary_input_declaration::Purpose::File(
                    wire::FilePurpose {},
                )),
            },
        )),
    }];
    let mut session = server
        .transport
        .open_attachment(open, limits)
        .await
        .unwrap();
    assert!(session.input.begin_upload("file").is_err());
    session.output.next().await.unwrap().unwrap();
    let mut upload = session.input.begin_upload("file").unwrap();
    upload.begin().await.unwrap();
    assert!(
        upload.commit().await.is_err(),
        "quarantine cannot commit a short file"
    );
    upload.append(&[1, 2, 3]).await.unwrap();
    let (descriptor, event) = upload.commit().await.unwrap();
    assert_eq!(descriptor.bytes.0, 3);
    session.input.send(vec![event]).await.unwrap();
    session.input.finish_input().await.unwrap();
    while let Some(r) = session.output.next().await {
        r.unwrap();
    }
    assert_eq!(session.output.final_text("text").unwrap(), "final");
}

#[tokio::test]
async fn sequential_download_and_durable_utf8_chunk_boundaries_are_distinct() {
    let server = fixture::server().await;
    let transfers = server.transport.media_transfers();
    let mut blob = transfers
        .read_input("invocation".into(), "blob".into())
        .await
        .unwrap();
    let mut data = vec![];
    while let Some(r) = blob.next().await {
        data.extend(r.unwrap());
    }
    assert_eq!(data, vec![1, 2, 3]);
    assert!(blob.complete());
    let d = waltr_client::RoutineTextResult {
        id: "result".into(),
        name: "text".into(),
        sha256: "a".repeat(64),
        utf8_bytes: waltr_client::RoutineRevision(5),
        encoded_charge: waltr_client::RoutineRevision(5),
        helper_id: None,
    };
    let mut text = transfers
        .text_reader("invocation".into(), &[d], "result")
        .unwrap();
    assert_eq!(text.next_chunk().await.unwrap().as_deref(), Some(""));
    assert_eq!(text.next_chunk().await.unwrap().as_deref(), Some("😀x"));
    assert_eq!(text.next_chunk().await.unwrap(), None);
}
