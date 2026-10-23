use super::*;

struct Echo {
    active: Arc<std::sync::atomic::AtomicUsize>,
}
struct Active(Arc<std::sync::atomic::AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Adapters for Echo {
    fn execute<'a>(
        &'a self,
        _: &'a RoutineTask,
        context: &'a mut TaskContext,
    ) -> AdapterFuture<'a> {
        Box::pin(async move {
            self.active.fetch_add(1, Ordering::SeqCst);
            let _active = Active(self.active.clone());
            let mut ordinal = 0;
            loop {
                match context
                    .input
                    .as_mut()
                    .unwrap()
                    .recv(&context.cancellation)
                    .await?
                {
                    Frame::Chunk(_) => {
                        context
                            .output
                            .publish(Chunk::Text(Text {
                                segment: ordinal.to_string(),
                                text: "word".into(),
                                replace: false,
                                committed: true,
                            }))
                            .await?;
                        ordinal += 1;
                    }
                    Frame::Close => return Ok(TaskResult::default()),
                    Frame::Failed(error) => return Err(error),
                }
            }
        })
    }
}
fn echo() -> (Arc<Echo>, Arc<std::sync::atomic::AtomicUsize>) {
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    (
        Arc::new(Echo {
            active: active.clone(),
        }),
        active,
    )
}

#[tokio::test]
async fn full_output_queue_fails_and_drains_without_leaking_tasks() {
    let mut request = request(TRANSCRIBE).await;
    request.limits.queued_chunks = 1;
    request.limits.chunk_bytes = 8;
    request.limits.edge_bytes = 8;
    request.limits.queue_wait_seconds = 1;
    let store = recording::record(&mut request).await;
    let id = request.id.clone();
    let (adapter, active) = echo();
    let mut interaction = start(request, adapter).unwrap();
    let mut input = interaction.inputs.remove("audio").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while active.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // Opened fills the public lane; resource failure must terminate without a public drain.
    input.send(Chunk::Audio(vec![0; 4])).await.unwrap();
    let (inputs, mut events, _control, completion) = interaction.into_parts();
    tokio::time::timeout(Duration::from_secs(5), completion.join())
        .await
        .unwrap();
    drop(inputs);
    let mut closed = false;
    loop {
        match next(&mut events).await {
            Event::Closed {
                error:
                    Some(Error {
                        code: "resource_exhausted",
                        ..
                    }),
                ..
            } => closed = true,
            Event::Completed(Err(Error {
                code: "resource_exhausted",
                ..
            })) => {
                assert!(closed);
                break;
            }
            Event::Opened { .. } | Event::Chunk { .. } => (),
            other => panic!("unexpected event {other:?}"),
        }
    }
    assert_eq!(active.load(Ordering::SeqCst), 0);
    let detail = store.detail(&id).await.unwrap();
    assert_eq!(
        detail.summary.status,
        crate::persistence::routine_invocation::RoutineInvocationStatus::Failed
    );
    assert_eq!(
        detail
            .events
            .iter()
            .filter(|event| event.kind
                == crate::persistence::routine_invocation::RoutineEventKind::InvocationFailed)
            .count(),
        1
    );
}

#[tokio::test]
async fn full_output_queue_cancellation_does_not_wait_for_queue_timeout() {
    for drop_events in [false, true] {
        let mut request = request(TRANSCRIBE).await;
        request.limits.queued_chunks = 1;
        request.limits.chunk_bytes = 8;
        request.limits.edge_bytes = 8;
        let (adapter, active) = echo();
        let mut interaction = start(request, adapter).unwrap();
        let mut input = interaction.inputs.remove("audio").unwrap();
        // Opened occupies the public lane, so enough sends must back up the whole pipeline.
        let mut sends = Box::pin(async {
            for _ in 0..16 {
                input.send(Chunk::Audio(vec![0; 4])).await?;
            }
            Ok::<(), Error>(())
        });
        for _ in 0..32 {
            assert!(futures_util::poll!(&mut sends).is_pending());
            tokio::task::yield_now().await;
        }
        let (inputs, events, control, completion) = interaction.into_parts();
        if drop_events {
            drop(events);
        } else {
            control.cancel();
            drop(events);
        }
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), sends)
                .await
                .unwrap()
                .unwrap_err()
                .code,
            "cancelled"
        );
        drop(inputs);
        tokio::time::timeout(Duration::from_secs(1), completion.join())
            .await
            .unwrap();
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn deadline_closes_outputs_before_terminal_and_releases_tasks() {
    let mut request = request(TRANSCRIBE).await;
    request.limits.input_seconds = 1;
    request.limits.invocation_seconds = 1;
    let (adapter, active) = echo();
    let mut interaction = start(request, adapter).unwrap();
    assert!(matches!(
        next(&mut interaction.events).await,
        Event::Opened { .. }
    ));
    assert!(matches!(
        next(&mut interaction.events).await,
        Event::Closed {
            error: Some(Error {
                code: "deadline_exceeded",
                ..
            }),
            ..
        }
    ));
    assert!(matches!(
        next(&mut interaction.events).await,
        Event::Completed(Err(Error {
            code: "deadline_exceeded",
            ..
        }))
    ));
    interaction.join().await;
    assert_eq!(active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn request_budget_fails_before_enqueue_and_is_terminal_not_truncated() {
    let mut request = request(TRANSCRIBE).await;
    request.limits.complete_request_bytes = 4;
    request.limits.chunk_bytes = 4;
    let (adapter, active) = echo();
    let mut interaction = start(request, adapter).unwrap();
    drop(next(&mut interaction.events).await);
    let input = interaction.inputs.get_mut("audio").unwrap();
    input.send(Chunk::Audio(vec![0; 4])).await.unwrap();
    assert_eq!(
        input.send(Chunk::Audio(vec![0; 2])).await.unwrap_err().code,
        "resource_exhausted"
    );
    let mut closed = false;
    loop {
        match next(&mut interaction.events).await {
            Event::Closed {
                error:
                    Some(Error {
                        code: "resource_exhausted",
                        ..
                    }),
                ..
            } => closed = true,
            Event::Completed(Err(Error {
                code: "resource_exhausted",
                ..
            })) => {
                assert!(closed);
                break;
            }
            Event::Chunk { .. } => (),
            other => panic!("unexpected event {other:?}"),
        }
    }
    interaction.join().await;
    assert_eq!(active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn startup_rejects_active_task_limit_instead_of_deadlocking() {
    let mut request = request(CLEANUP).await;
    request.limits.active_tasks_per_interaction = 1;
    let (adapter, active) = echo();
    assert!(matches!(
        start(request, adapter),
        Err(Error {
            code: "resource_exhausted",
            ..
        })
    ));
    assert_eq!(active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn connected_live_component_unions_external_completion_prerequisites() {
    let request = request(CLEANUP).await;
    let mut tasks = request.routine.definition().spec.tasks.clone();
    let cleanup = tasks[2].name.clone();
    // Two consumers share one interaction input. The second's prerequisite must gate both.
    tasks[1].task_spec = tasks[0].task_spec.clone();
    tasks[1].run_after = vec![cleanup.clone()];
    tasks[2].run_after.clear();
    let prerequisites = component_prerequisites(&tasks);
    assert_eq!(
        prerequisites[&tasks[0].name],
        BTreeSet::from([cleanup.clone()])
    );
    assert_eq!(prerequisites[&tasks[1].name], BTreeSet::from([cleanup]));
}

#[tokio::test]
async fn bounded_fanout_delivers_to_every_consumer_without_loss() {
    let limits = InteractionLimits {
        queued_chunks: 1,
        edge_bytes: 4,
        chunk_bytes: 4,
        ..Default::default()
    };
    let (first, mut first_rx) = stream::edge(&limits);
    let (second, mut second_rx) = stream::edge(&limits);
    let cancel = CancellationToken::new();
    let mut publisher = Publisher {
        senders: vec![first, second],
        state: Some(State::new(
            RoutineStreamFormat::Audio {
                encoding: crate::resource_catalog::routine::RoutineAudioEncoding::PcmS16Le,
                sample_rate_hz: 24000,
                channels: 1,
            },
            64,
        )),
        cancel: cancel.clone(),
        fence: None,
    };
    publisher.publish(Chunk::Audio(vec![1, 0])).await.unwrap();
    let mut pending = Box::pin(publisher.publish(Chunk::Audio(vec![2, 0])));
    assert!(futures_util::poll!(&mut pending).is_pending());
    assert!(
        matches!(first_rx.recv(&cancel).await.unwrap(), Frame::Chunk(Chunk::Audio(bytes)) if bytes == [1, 0])
    );
    assert!(
        matches!(second_rx.recv(&cancel).await.unwrap(), Frame::Chunk(Chunk::Audio(bytes)) if bytes == [1, 0])
    );
    pending.await.unwrap();
    for receiver in [&mut first_rx, &mut second_rx] {
        assert!(
            matches!(receiver.recv(&cancel).await.unwrap(), Frame::Chunk(Chunk::Audio(bytes)) if bytes == [2, 0])
        );
    }
}
