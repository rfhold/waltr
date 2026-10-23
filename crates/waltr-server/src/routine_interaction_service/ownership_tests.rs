use super::*;

pub(super) struct Hold;
impl interaction::Adapters for Hold {
    fn execute<'a>(
        &'a self,
        _: &'a crate::resource_catalog::routine::RoutineTask,
        context: &'a mut interaction::TaskContext,
    ) -> interaction::AdapterFuture<'a> {
        Box::pin(async move {
            context.cancellation.cancelled().await;
            Err(interaction::Error::new(
                "cancelled",
                "scripted adapter cancelled",
            ))
        })
    }
}

#[tokio::test]
async fn entirely_unpolled_run_is_owned_and_shutdown_interrupts_history() {
    let (service, engine, _bus, persistence, root, _) =
        setup_full(TEXT, InteractionLimits::default(), Arc::new(Hold)).await;
    let response = service
        .run(Request::new(wire::RunInteractionRequest {
            open: Some(open()),
            inputs: vec![text_input("hello")],
        }))
        .await
        .unwrap();
    // Never poll response: shutdown must own admission commit and runtime cleanup anyway.
    tokio::time::timeout(Duration::from_secs(5), engine.shutdown())
        .await
        .unwrap();
    let rows = persistence
        .routine_invocation_store()
        .list(None, 100, &BTreeMap::new())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, RoutineInvocationStatus::Interrupted);
    drop(response);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn shutdown_cancels_unpolled_backpressured_run_without_waiting_for_queue_expiry() {
    use crate::persistence::routine_invocation::RoutineEventKind;
    let limits = InteractionLimits {
        chunk_bytes: 5,
        edge_bytes: 5,
        queued_chunks: 1,
        ..InteractionLimits::default()
    };
    let source = TEXT.replace("maxBytes: 1024", "maxBytes: 8192");
    let (service, engine, _bus, persistence, root) = setup(&source, limits).await;
    let mut wakeups = engine.lifecycle_wakeups();
    let response = service
        .run(Request::new(wire::RunInteractionRequest {
            open: Some(open()),
            inputs: vec![text_input(&"x".repeat(4096))],
        }))
        .await
        .unwrap();
    // Acceptance occupies the sole response slot; output and eager ingress cannot drain.
    let id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let rows = persistence
                .routine_invocation_store()
                .list(None, 100, &BTreeMap::new())
                .await
                .unwrap();
            if let Some(row) = rows.first() {
                let detail = persistence
                    .routine_invocation_store()
                    .detail(&row.invocation_id)
                    .await
                    .unwrap();
                if detail
                    .events
                    .iter()
                    .any(|event| event.kind == RoutineEventKind::TaskStarted)
                {
                    break row.invocation_id.clone();
                }
            }
            wakeups.recv().await.unwrap();
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), engine.shutdown())
        .await
        .unwrap();
    assert_eq!(
        persistence
            .routine_invocation_store()
            .detail(&id)
            .await
            .unwrap()
            .summary
            .status,
        RoutineInvocationStatus::Interrupted
    );
    drop(response);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn admitted_snapshot_is_pinned_across_catalog_reload() {
    let (service, engine, _bus, persistence, root, state) = setup_full(
        TEXT,
        InteractionLimits::default(),
        session_fixture::adapters(),
    )
    .await;
    let mut server = Loopback::start(service).await;
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    tx.send(request(In::Open(open()))).await.unwrap();
    let mut output = server
        .client
        .interact(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    let Some(Out::Accepted(v)) = output.message().await.unwrap().unwrap().event else {
        panic!()
    };
    let revision = persistence
        .routine_invocation_store()
        .detail(&v.invocation_id)
        .await
        .unwrap()
        .summary
        .routine_revision;
    // Replace with a different valid definition; already admitted work keeps its original pin.
    std::fs::write(
        root.join(".waltr/resources/routines/echo.yaml"),
        TEXT.replace("Echo", "Changed"),
    )
    .unwrap();
    state.reload_from_disk().await;
    assert_ne!(
        state.snapshot().routine("echo").unwrap().source_etag(),
        revision
    );
    tx.send(request(opened())).await.unwrap();
    tx.send(request(chunk(true))).await.unwrap();
    tx.send(request(closed())).await.unwrap();
    drop(tx);
    loop {
        if let Some(Out::Completed(v)) =
            tokio::time::timeout(Duration::from_secs(5), output.message())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .event
        {
            assert_eq!(v.status, wire::RoutineInvocationStatus::Succeeded as i32);
            break;
        }
    }
    assert_eq!(
        persistence
            .routine_invocation_store()
            .detail(&v.invocation_id)
            .await
            .unwrap()
            .summary
            .routine_revision,
        revision
    );
    drop(output);
    engine.shutdown().await;
    server.shutdown().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn input_and_invocation_deadlines_are_independent() {
    for (input_seconds, invocation_seconds, complete) in [(1, 5, false), (1, 1, true)] {
        let limits = InteractionLimits {
            input_seconds,
            invocation_seconds,
            ..InteractionLimits::default()
        };
        let (service, engine, _bus, persistence, root, _) =
            setup_full(TEXT, limits, Arc::new(Hold)).await;
        let mut server = Loopback::start(service).await;
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        tx.send(request(In::Open(open()))).await.unwrap();
        let mut output = server
            .client
            .interact(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();
        let Some(Out::Accepted(v)) = output.message().await.unwrap().unwrap().event else {
            panic!()
        };
        if complete {
            tx.send(request(opened())).await.unwrap();
            tx.send(request(chunk(true))).await.unwrap();
            tx.send(request(closed())).await.unwrap();
            tx.send(request(In::InputCompleted(
                wire::InteractionInputCompleted {},
            )))
            .await
            .unwrap();
        }
        let terminal = loop {
            if let Some(Out::Completed(v)) =
                tokio::time::timeout(Duration::from_secs(4), output.message())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
                    .event
            {
                break v;
            }
        };
        assert_eq!(
            terminal.status,
            wire::RoutineInvocationStatus::Failed as i32,
            "{terminal:?}"
        );
        assert_eq!(terminal.failure.unwrap().code, "deadline_exceeded");
        assert_eq!(
            persistence
                .routine_invocation_store()
                .detail(&v.invocation_id)
                .await
                .unwrap()
                .summary
                .status,
            RoutineInvocationStatus::Failed
        );
        drop(output);
        drop(tx);
        engine.shutdown().await;
        server.shutdown().await;
        std::fs::remove_dir_all(root).unwrap();
    }
}
