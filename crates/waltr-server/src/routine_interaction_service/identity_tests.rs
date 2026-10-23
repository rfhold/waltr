use super::*;

async fn completed_id(
    mut output: impl futures_util::Stream<
        Item = std::result::Result<wire::InteractResponse, tonic::Status>,
    > + Unpin,
) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        let Some(Out::Accepted(accepted)) = output.next().await.unwrap().unwrap().event else {
            panic!("acceptance first")
        };
        let mut completed = false;
        while let Some(message) = output.next().await {
            if let Some(Out::Completed(terminal)) = message.unwrap().event {
                assert_eq!(
                    terminal.status,
                    wire::RoutineInvocationStatus::Succeeded as i32,
                    "{terminal:?}"
                );
                completed = true;
            }
        }
        assert!(completed);
        accepted.invocation_id
    })
    .await
    .unwrap()
}

async fn assert_history(
    persistence: &Persistence,
    engine: &RoutineEngine,
    id: &str,
    correlation: &str,
) {
    let detail = persistence
        .routine_invocation_store()
        .detail(id)
        .await
        .unwrap();
    assert_eq!(detail.summary.status, RoutineInvocationStatus::Succeeded);
    assert_eq!(detail.summary.correlation_id, correlation);
    assert!(matches!(&*engine.health().borrow(), EngineHealth::Running));
}

#[tokio::test]
async fn run_optional_request_id_falls_back_without_poisoning_admission() {
    let (service, engine, _bus, persistence, root) =
        setup(TEXT, InteractionLimits::default()).await;
    for correlation in [
        None,
        None,
        Some("explicit".to_owned()),
        Some("a".repeat(512)),
        Some("é".repeat(512)),
        Some(format!("{}a", "界".repeat(341))),
    ] {
        let mut selection = open();
        selection.request_id = correlation.clone();
        let output = service
            .run(Request::new(wire::RunInteractionRequest {
                open: Some(selection),
                inputs: vec![text_input("hello")],
            }))
            .await
            .unwrap()
            .into_inner();
        let id = completed_id(output).await;
        assert_history(
            &persistence,
            &engine,
            &id,
            correlation.as_deref().unwrap_or(&id),
        )
        .await;
    }
    assert_eq!(
        persistence
            .routine_invocation_store()
            .list(None, 100, &BTreeMap::new())
            .await
            .unwrap()
            .len(),
        6
    );
    engine.shutdown().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn interact_optional_request_id_falls_back_without_poisoning_admission() {
    let (service, engine, _bus, persistence, root) =
        setup(TEXT, InteractionLimits::default()).await;
    let mut server = Loopback::start(service).await;
    for _ in 0..2 {
        let mut selection = open();
        selection.request_id = None;
        let output = server
            .client
            .interact(tokio_stream::iter([
                request(In::Open(selection)),
                request(opened()),
                request(chunk(true)),
                request(closed()),
                request(In::InputCompleted(wire::InteractionInputCompleted {})),
            ]))
            .await
            .unwrap()
            .into_inner();
        let id = completed_id(Box::pin(output)).await;
        assert_history(&persistence, &engine, &id, &id).await;
    }
    server.shutdown().await;
    engine.shutdown().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn invalid_request_ids_are_rejected_before_admission() {
    let (service, engine, _bus, persistence, root) =
        setup(TEXT, InteractionLimits::default()).await;
    let mut server = Loopback::start(service.clone()).await;
    for correlation in [
        String::new(),
        "a".repeat(513),
        "é".repeat(513),
        "界".repeat(342),
    ] {
        let mut selection = open();
        selection.request_id = Some(correlation);
        let mut output = service
            .run(Request::new(wire::RunInteractionRequest {
                open: Some(selection.clone()),
                inputs: vec![text_input("hello")],
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(matches!(
            output.next().await.unwrap().unwrap().event,
            Some(Out::ValidationFailure(_))
        ));
        assert!(output.next().await.is_none());
        let mut output = server
            .client
            .interact(tokio_stream::iter([request(In::Open(selection))]))
            .await
            .unwrap()
            .into_inner();
        assert!(matches!(
            output.message().await.unwrap().unwrap().event,
            Some(Out::ValidationFailure(_))
        ));
        assert!(output.message().await.unwrap().is_none());
        assert!(matches!(&*engine.health().borrow(), EngineHealth::Running));
    }
    assert!(
        persistence
            .routine_invocation_store()
            .list(None, 100, &BTreeMap::new())
            .await
            .unwrap()
            .is_empty()
    );
    let output = service
        .run(Request::new(wire::RunInteractionRequest {
            open: Some(open()),
            inputs: vec![text_input("hello")],
        }))
        .await
        .unwrap()
        .into_inner();
    let id = completed_id(output).await;
    assert_history(&persistence, &engine, &id, "correlation").await;
    server.shutdown().await;
    engine.shutdown().await;
    std::fs::remove_dir_all(root).unwrap();
}
