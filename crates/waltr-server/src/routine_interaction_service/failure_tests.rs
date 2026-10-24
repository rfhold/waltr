use super::*;
use prost::Message as _;

#[test]
fn one_hour_pcm_and_wire_overhead_fit_default_request_budget() {
    let limits = InteractionLimits::default();
    let raw_bytes = 3600 * 24000 * 2;
    assert_eq!(raw_bytes, 172_800_000);
    let frame = request(In::InputChunk(wire::InteractionInputChunk {
        stream_id: "audio".into(),
        content: Some(wire::interaction_input_chunk::Content::Audio(vec![
            0;
            48_000
        ])),
    }));
    // One-second frames plus ample open/close/parameter envelope allowance.
    let streamed_bytes = 3600 * frame.encoded_len() + 1024 * 1024;
    assert!(streamed_bytes > raw_bytes);
    assert!(streamed_bytes < limits.complete_request_bytes);
    assert!(raw_bytes + 1024 * 1024 < limits.complete_request_bytes);
    assert!(48_000 < limits.chunk_bytes);
}

#[tokio::test]
async fn native_stream_exceeds_old_request_budget_with_bounded_revision_chunks() {
    let source = TEXT.replace("maxBytes: 1024", "maxBytes: 1048576");
    let (service, engine, _bus, _persistence, root) =
        setup(&source, InteractionLimits::default()).await;
    let mut server = Loopback::start(service).await;
    let (tx, rx) = tokio::sync::mpsc::channel(2);
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
    let producer = tokio::spawn(async move {
        tx.send(request(opened())).await.unwrap();
        for index in 0..70 {
            tx.send(request(In::InputChunk(wire::InteractionInputChunk {
                stream_id: "s".into(),
                content: Some(wire::interaction_input_chunk::Content::Text(
                    wire::TextUpdate {
                        segment_id: "a".into(),
                        operation: 2,
                        text: "x".repeat(512 * 1024),
                        committed: index == 69,
                    },
                )),
            })))
            .await
            .unwrap();
        }
        tx.send(request(closed())).await.unwrap();
        tx.send(request(In::InputCompleted(
            wire::InteractionInputCompleted {},
        )))
        .await
        .unwrap();
    });
    let mut bytes = 0;
    let terminal = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match output.message().await.unwrap().unwrap().event.unwrap() {
                Out::OutputChunk(chunk) => {
                    let Some(wire::interaction_output_chunk::Content::Text(text)) = chunk.content
                    else {
                        panic!("text expected")
                    };
                    bytes += text.text.len();
                }
                Out::Completed(completed) => break completed,
                _ => (),
            }
        }
    })
    .await
    .unwrap();
    producer.await.unwrap();
    assert_eq!(bytes, 70 * 512 * 1024);
    assert!(bytes > 32 * 1024 * 1024);
    assert_eq!(
        terminal.status,
        wire::RoutineInvocationStatus::Succeeded as i32,
        "{terminal:?}"
    );
    engine.shutdown().await;
    server.shutdown().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn complete_request_budget_is_checked_before_admission_and_loopback_decodes_above_four_mib() {
    let source = TEXT.replace("maxBytes: 1024", "maxBytes: 8388608");
    let (service, engine, _bus, persistence, root) = setup(
        &source,
        InteractionLimits {
            complete_request_bytes: 32 * 1024 * 1024,
            chunk_bytes: 256 * 1024,
            collector_bytes: 8 * 1024 * 1024,
            ..InteractionLimits::default()
        },
    )
    .await;
    let oversized = wire::RunInteractionRequest {
        open: Some(open()),
        inputs: vec![text_input(&"x".repeat(32 * 1024 * 1024))],
    };
    assert!(oversized.encoded_len() > 32 * 1024 * 1024);
    let error = service.run(Request::new(oversized)).await.err().unwrap();
    assert_eq!(error.code(), tonic::Code::ResourceExhausted);
    assert!(
        persistence
            .routine_invocation_store()
            .list(None, 100, &BTreeMap::new())
            .await
            .unwrap()
            .is_empty()
    );

    let mut server = Loopback::start(service).await;
    let mut output = server
        .client
        .run(wire::RunInteractionRequest {
            open: Some(open()),
            inputs: vec![text_input(&"x".repeat(5 * 1024 * 1024))],
        })
        .await
        .unwrap()
        .into_inner();
    let Some(Out::Accepted(accepted)) = output.message().await.unwrap().unwrap().event else {
        panic!("large bounded request must reach shared admission")
    };
    assert_eq!(
        persistence
            .routine_invocation_store()
            .detail(&accepted.invocation_id)
            .await
            .unwrap()
            .summary
            .trigger_kind,
        RoutineTriggerKind::Interaction
    );
    let mut received = 0;
    let mut chunks = 0;
    loop {
        match tokio::time::timeout(Duration::from_secs(10), output.message())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .event
            .unwrap()
        {
            Out::OutputChunk(chunk) => {
                let Some(wire::interaction_output_chunk::Content::Text(text)) = chunk.content
                else {
                    panic!("expected text")
                };
                assert!(text.text.bytes().all(|v| v == b'x'));
                received += text.text.len();
                chunks += 1;
            }
            Out::Completed(completed) => {
                assert_eq!(
                    completed.status,
                    wire::RoutineInvocationStatus::Succeeded as i32,
                    "{completed:?}"
                );
                break;
            }
            _ => (),
        }
    }
    assert_eq!(received, 5 * 1024 * 1024);
    assert!(chunks > InteractionLimits::default().queued_chunks);
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
    tokio::time::timeout(Duration::from_secs(5), engine.shutdown())
        .await
        .unwrap();
    server.shutdown().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn admission_persistence_failure_is_unavailable_and_closes_global_admission() {
    let (service, engine, _bus, persistence, root) =
        setup(TEXT, InteractionLimits::default()).await;
    persistence.sqlite_pool().unwrap().close().await;
    let mut health = engine.health();
    let mut output = service
        .run(Request::new(wire::RunInteractionRequest {
            open: Some(open()),
            inputs: vec![text_input("hello")],
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), output.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .code(),
        tonic::Code::Unavailable
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        health.wait_for(|v| matches!(v, EngineHealth::Failed { .. })),
    )
    .await
    .unwrap()
    .unwrap();
    let error = service
        .run(Request::new(wire::RunInteractionRequest {
            open: Some(open()),
            inputs: vec![text_input("hello")],
        }))
        .await
        .err()
        .unwrap();
    assert_eq!(error.code(), tonic::Code::Unavailable);
    tokio::time::timeout(Duration::from_secs(5), engine.shutdown())
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn saturated_response_queue_fails_structurally_without_blocking_cleanup() {
    let limits = InteractionLimits {
        queued_chunks: 1,
        queue_wait_seconds: 1,
        ..InteractionLimits::default()
    };
    let (service, engine, _bus, persistence, root) = setup(TEXT, limits).await;
    let mut wakeups = engine.lifecycle_wakeups();
    let mut output = service
        .run(Request::new(wire::RunInteractionRequest {
            open: Some(open()),
            inputs: vec![text_input("hello")],
        }))
        .await
        .unwrap()
        .into_inner();
    // Leave acceptance occupying the only data slot until the durable terminal wakeup.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let rows = persistence
                .routine_invocation_store()
                .list(None, 100, &BTreeMap::new())
                .await
                .unwrap();
            if rows
                .first()
                .is_some_and(|v| v.status == RoutineInvocationStatus::Failed)
            {
                break;
            }
            wakeups.recv().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        output.next().await.unwrap().unwrap().event,
        Some(Out::Accepted(_))
    ));
    let terminal = loop {
        if let Some(Out::Completed(v)) = output.next().await.unwrap().unwrap().event {
            break v;
        }
    };
    assert_eq!(
        terminal.status,
        wire::RoutineInvocationStatus::Failed as i32
    );
    assert_eq!(terminal.failure.unwrap().code, "resource_exhausted");
    tokio::time::timeout(Duration::from_secs(5), engine.shutdown())
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
