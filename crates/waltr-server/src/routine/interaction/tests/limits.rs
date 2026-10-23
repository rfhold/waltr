use super::*;

#[tokio::test]
async fn collector_and_context_overflow_fail_without_creating_cleanup_child() {
    for context_overflow in [false, true] {
        let server = fixtures::SpeechServer::start().await;
        let mut definition: crate::resource_catalog::routine::RoutineDefinition =
            serde_yaml_ng::from_str(CLEANUP).unwrap();
        if !context_overflow {
            definition.spec.tasks[1]
                .task_spec
                .collect
                .as_mut()
                .unwrap()
                .max_bytes = 1;
        }
        let mut request = request(&serde_yaml_ng::to_string(&definition).unwrap()).await;
        let config = Arc::make_mut(&mut request.configuration);
        config.providers.get_mut("speech").unwrap().url = server.endpoint.clone();
        if context_overflow {
            config
                .providers
                .get_mut("inference")
                .unwrap()
                .models
                .get_mut("cleanup")
                .unwrap()
                .limits
                .as_mut()
                .unwrap()
                .context = Some(4);
        }
        let (host, _sandbox) = fixtures::Host::new(Arc::new(tokio::sync::Notify::new())).await;
        let mut interaction = start_sessions(request, host.clone()).unwrap();
        drop(next(&mut interaction.events).await);
        interaction
            .inputs
            .get_mut("audio")
            .unwrap()
            .send(Chunk::Audio(vec![0; 4]))
            .await
            .unwrap();
        interaction
            .inputs
            .get_mut("audio")
            .unwrap()
            .close()
            .await
            .unwrap();
        loop {
            if let Event::Completed(result) = next(&mut interaction.events).await {
                assert!(
                    matches!(result, Err(Error { code, .. }) if code == if context_overflow { "context_exceeded" } else { "resource_exhausted" }),
                    "{result:?}"
                );
                break;
            }
        }
        interaction.join().await;
        assert_eq!(host.created.load(Ordering::SeqCst), 0);
        use crate::session::SessionHost;
        host.shutdown().await;
    }
}

#[tokio::test]
async fn targeted_cancel_interrupts_helper_without_canceling_invocation_scope() {
    let server = fixtures::SpeechServer::start().await;
    let mut request = request(CLEANUP).await;
    Arc::make_mut(&mut request.configuration)
        .providers
        .get_mut("speech")
        .unwrap()
        .url = server.endpoint.clone();
    let (host, _sandbox) = fixtures::Host::new(Arc::new(tokio::sync::Notify::new())).await;
    let mut interaction = start_sessions(request, host.clone()).unwrap();
    drop(next(&mut interaction.events).await);
    interaction
        .inputs
        .get_mut("audio")
        .unwrap()
        .send(Chunk::Audio(vec![0; 4]))
        .await
        .unwrap();
    interaction
        .inputs
        .get_mut("audio")
        .unwrap()
        .close()
        .await
        .unwrap();
    assert!(matches!(
        next(&mut interaction.events).await,
        Event::Chunk { .. }
    ));
    assert!(interaction.control.cancel_task("cleanup"));
    assert!(!interaction.control.cancel_task("absent"));
    assert!(!interaction.control.cancel.is_cancelled());
    assert!(matches!(
        next(&mut interaction.events).await,
        Event::Closed {
            error: Some(Error {
                code: "task_interrupted",
                ..
            }),
            ..
        }
    ));
    assert!(matches!(
        next(&mut interaction.events).await,
        Event::Completed(Err(Error {
            code: "task_interrupted",
            ..
        }))
    ));
    interaction.join().await;
    assert_eq!(host.interrupts.load(Ordering::SeqCst), 1);
    use crate::session::SessionHost;
    for session in host.list().await.unwrap() {
        assert!(host.active_state(&session.id).await.is_none());
    }
    host.shutdown().await;
}
