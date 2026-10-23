use super::*;
use prost::Message as _;

#[tokio::test]
async fn complete_request_budget_is_checked_before_admission_and_loopback_decodes_above_four_mib() {
    let source = TEXT.replace("maxBytes: 1024", "maxBytes: 8388608");
    let (service, engine, _bus, persistence, root) = setup(
        &source,
        InteractionLimits {
            collector_bytes: 8 * 1024 * 1024,
            ..InteractionLimits::default()
        },
    )
    .await;
    let oversized = wire::RunInteractionRequest {
        open: Some(open()),
        inputs: vec![text_input(&"x".repeat(32 * 1024 * 1024))],
    };
    assert!(oversized.encoded_len() > InteractionLimits::default().complete_request_bytes);
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
