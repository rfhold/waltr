use super::*;
use crate::resource_catalog::routine::{RoutineDefinition, RoutineStreamRef};
use tokio::sync::{Notify, Semaphore, mpsc};

fn graph(two_components: bool) -> String {
    let mut definition: RoutineDefinition = serde_yaml_ng::from_str(CLEANUP).unwrap();
    let mut producer = definition.spec.tasks[2].clone();
    producer.name = "producer-a".into();
    producer.run_after.clear();
    producer.task_spec.results.clear();
    producer
        .task_spec
        .session_template
        .as_mut()
        .unwrap()
        .user_prompt
        .text = "source".into();
    let mut consumer = definition.spec.tasks[1].clone();
    consumer.name = "consumer-a".into();
    consumer.task_spec.results.clear();
    consumer.task_spec.collect.as_mut().unwrap().stream =
        RoutineStreamRef::TaskOutput(producer.name.clone());
    definition.spec.outputs.clear();
    definition.spec.results.clear();
    definition.spec.triggers[0]
        .interaction
        .as_mut()
        .unwrap()
        .output_ports
        .clear();
    if two_components {
        definition.spec.work_groups = serde_yaml_ng::from_str(
            "[{name: x, onBusy: queue, maxQueued: 1}, {name: y, onBusy: queue, maxQueued: 1}]",
        )
        .unwrap();
        producer.work_group = Some("x".into());
        consumer.work_group = Some("y".into());
        let mut other_producer = producer.clone();
        other_producer.name = "producer-b".into();
        other_producer.work_group = Some("y".into());
        let mut other_consumer = consumer.clone();
        other_consumer.name = "consumer-b".into();
        other_consumer.work_group = Some("x".into());
        other_consumer.task_spec.collect.as_mut().unwrap().stream =
            RoutineStreamRef::TaskOutput(other_producer.name.clone());
        // Both producers are polled before either collector: per-task leases deadlock here.
        definition.spec.tasks = vec![producer, other_producer, consumer, other_consumer];
    } else {
        definition.spec.work_groups.clear();
        producer.work_group = None;
        consumer.work_group = None;
        let mut skipped = consumer.clone();
        skipped.name = "skipped".into();
        definition.spec.params =
            serde_yaml_ng::from_str("[{name: enabled, type: boolean, default: false}]").unwrap();
        definition.spec.triggers[0]
            .interaction
            .as_mut()
            .unwrap()
            .inputs = serde_yaml_ng::from_str("[{name: enabled}]").unwrap();
        skipped.when =
            serde_yaml_ng::from_str("[{input: '$(params.enabled)', operator: In, values: [true]}]")
                .unwrap();
        definition.spec.tasks = vec![skipped, consumer, producer];
    }
    serde_yaml_ng::to_string(&definition).unwrap()
}

struct Gated {
    events: mpsc::UnboundedSender<String>,
    release: [Notify; 2],
    consumed: [Semaphore; 2],
    multiple_chunks: bool,
}
impl Adapters for Gated {
    fn execute<'a>(
        &'a self,
        task: &'a RoutineTask,
        context: &'a mut TaskContext,
    ) -> AdapterFuture<'a> {
        Box::pin(async move {
            let index = usize::from(task.name.ends_with('b'));
            if task.name.starts_with("producer") {
                self.events.send(task.name.clone()).unwrap();
                for ordinal in 0..if self.multiple_chunks { 3 } else { 1 } {
                    context
                        .output
                        .publish(Chunk::Text(Text {
                            segment: ordinal.to_string(),
                            text: "x".into(),
                            replace: false,
                            committed: true,
                        }))
                        .await?;
                    tokio::select! {
                        () = context.cancellation.cancelled() => return Err(Error::cancelled()),
                        permit = self.consumed[index].acquire() => permit.unwrap().forget(),
                    }
                }
                if !self.multiple_chunks {
                    tokio::select! {
                        () = context.cancellation.cancelled() => return Err(Error::cancelled()),
                        () = self.release[index].notified() => (),
                    }
                }
            } else {
                assert_ne!(
                    task.name, "skipped",
                    "false task must not execute its adapter"
                );
                loop {
                    match context
                        .input
                        .as_mut()
                        .unwrap()
                        .recv(&context.cancellation)
                        .await?
                    {
                        Frame::Chunk(_) => {
                            self.events.send(task.name.clone()).unwrap();
                            self.consumed[index].add_permits(1);
                        }
                        Frame::Close => break,
                        Frame::Failed(error) => return Err(error),
                    }
                }
            }
            Ok(TaskResult::default())
        })
    }
}
async fn event(events: &mut mpsc::UnboundedReceiver<String>) -> String {
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap()
}
fn adapter(multiple_chunks: bool) -> (Arc<Gated>, mpsc::UnboundedReceiver<String>) {
    let (events, rx) = mpsc::unbounded_channel();
    (
        Arc::new(Gated {
            events,
            release: [Notify::new(), Notify::new()],
            consumed: [Semaphore::new(0), Semaphore::new(0)],
            multiple_chunks,
        }),
        rx,
    )
}

#[tokio::test]
async fn opposite_queue_groups_reserve_whole_live_components_without_deadlock() {
    let mut request = request(&graph(true)).await;
    request.limits.queued_chunks = 1;
    request.limits.chunk_bytes = 4;
    request.limits.edge_bytes = 4;
    let (adapter, mut events) = adapter(false);
    let mut interaction = start(request, adapter.clone()).unwrap();
    interaction
        .inputs
        .get_mut("audio")
        .unwrap()
        .close()
        .await
        .unwrap();
    for _ in 0..2 {
        let producer = event(&mut events).await;
        let suffix = producer.strip_prefix("producer-").unwrap();
        assert_eq!(event(&mut events).await, format!("consumer-{suffix}"));
        assert!(
            events.try_recv().is_err(),
            "other component must not hold a partial reservation"
        );
        adapter.release[usize::from(suffix == "b")].notify_one();
    }
    assert!(matches!(
        next(&mut interaction.events).await,
        Event::Completed(Ok(_))
    ));
    interaction.join().await;
}

#[tokio::test]
async fn skipped_consumer_detaches_without_interrupting_producer_or_healthy_fanout() {
    let mut request = request(&graph(false)).await;
    request.params.insert("enabled".into(), Value::Bool(false));
    request.limits.queued_chunks = 1;
    let (adapter, mut events) = adapter(true);
    let mut interaction = start(request, adapter).unwrap();
    interaction
        .inputs
        .get_mut("audio")
        .unwrap()
        .close()
        .await
        .unwrap();
    assert_eq!(event(&mut events).await, "producer-a");
    for _ in 0..3 {
        assert_eq!(event(&mut events).await, "consumer-a");
    }
    assert!(matches!(
        next(&mut interaction.events).await,
        Event::Completed(Ok(_))
    ));
    interaction.join().await;
}

#[tokio::test]
async fn atomic_reservations_bound_each_queue_and_release_abandoned_waiters() {
    let reservations = reservation::Reservations::new(BTreeMap::from([
        ("x".into(), 1),
        ("y".into(), 1),
        ("z".into(), 1),
    ]));
    let cancel = CancellationToken::new();
    let first = reservations
        .enter(BTreeSet::from(["x".into()]), &cancel)
        .await
        .unwrap();
    let mut waiting =
        Box::pin(reservations.enter(BTreeSet::from(["x".into(), "y".into()]), &cancel));
    assert!(futures_util::poll!(&mut waiting).is_pending());
    assert!(matches!(
        reservations
            .enter(BTreeSet::from(["y".into()]), &cancel)
            .await,
        Err(Error {
            code: "resource_exhausted",
            ..
        })
    ));
    let disjoint = reservations
        .enter(BTreeSet::from(["z".into()]), &cancel)
        .await
        .unwrap();
    drop(waiting);
    let free_y = reservations
        .enter(BTreeSet::from(["y".into()]), &cancel)
        .await
        .unwrap();
    drop((first, free_y, disjoint));
    assert!(
        reservations
            .enter(
                BTreeSet::from(["x".into(), "y".into(), "z".into()]),
                &cancel
            )
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn bounded_enqueue_allows_receiver_progress_and_cancels_while_waiting() {
    let limits = InteractionLimits {
        queued_chunks: 1,
        chunk_bytes: 2,
        edge_bytes: 2,
        ..Default::default()
    };
    let (sender, mut receiver) = stream::edge(&limits);
    let cancel = CancellationToken::new();
    sender
        .send(Frame::Chunk(Chunk::Audio(vec![1, 0])), &cancel)
        .await
        .unwrap();
    let read = tokio::spawn(async move {
        let first = receiver.recv(&CancellationToken::new()).await.unwrap();
        (first, receiver)
    });
    sender
        .send(Frame::Chunk(Chunk::Audio(vec![2, 0])), &cancel)
        .await
        .unwrap();
    let (first, mut receiver) = read.await.unwrap();
    assert!(matches!(first, Frame::Chunk(Chunk::Audio(bytes)) if bytes == [1, 0]));
    assert!(
        matches!(receiver.recv(&cancel).await.unwrap(), Frame::Chunk(Chunk::Audio(bytes)) if bytes == [2, 0])
    );
    sender
        .send(Frame::Chunk(Chunk::Audio(vec![3, 0])), &cancel)
        .await
        .unwrap();
    let mut pending = Box::pin(sender.send(Frame::Close, &cancel));
    assert!(futures_util::poll!(&mut pending).is_pending());
    cancel.cancel();
    assert_eq!(pending.await.unwrap_err().code, "cancelled");
}

#[tokio::test(start_paused = true)]
async fn queue_wait_uses_one_deadline_and_releases_byte_credit_on_slot_timeout() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    tx.send(()).await.unwrap();
    let bytes = Arc::new(tokio::sync::Semaphore::new(4));
    let held = bytes.clone().acquire_many_owned(4).await.unwrap();
    let cancel = CancellationToken::new();
    let mut pending = Box::pin(stream::capacity(
        &tx,
        &bytes,
        4,
        &cancel,
        None,
        Duration::from_secs(30),
    ));
    assert!(futures_util::poll!(&mut pending).is_pending());
    tokio::time::advance(Duration::from_secs(20)).await;
    drop(held);
    assert!(futures_util::poll!(&mut pending).is_pending());
    assert_eq!(bytes.available_permits(), 0);
    tokio::time::advance(Duration::from_secs(10)).await;
    assert_eq!(pending.await.unwrap_err().code, "resource_exhausted");
    assert_eq!(bytes.available_permits(), 4);
    assert_eq!(rx.recv().await, Some(()));
    assert!(tx.try_reserve().is_ok());
}

#[tokio::test(start_paused = true)]
async fn delayed_consumer_progress_succeeds_and_dropped_wait_releases_reservations() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    tx.send(()).await.unwrap();
    let bytes = Arc::new(tokio::sync::Semaphore::new(4));
    let cancel = CancellationToken::new();
    let mut pending = Box::pin(stream::capacity(
        &tx,
        &bytes,
        4,
        &cancel,
        None,
        Duration::from_secs(30),
    ));
    assert!(futures_util::poll!(&mut pending).is_pending());
    assert_eq!(bytes.available_permits(), 0);
    drop(pending);
    assert_eq!(bytes.available_permits(), 4);
    let mut pending = Box::pin(stream::capacity(
        &tx,
        &bytes,
        4,
        &cancel,
        None,
        Duration::from_secs(30),
    ));
    assert!(futures_util::poll!(&mut pending).is_pending());
    tokio::time::advance(Duration::from_secs(5)).await;
    assert_eq!(rx.recv().await, Some(()));
    let (slot, permit) = pending.await.unwrap().unwrap();
    slot.send(());
    drop(permit);
    assert_eq!(bytes.available_permits(), 4);
    assert_eq!(rx.recv().await, Some(()));

    tx.send(()).await.unwrap();
    let detached = CancellationToken::new();
    let mut pending = Box::pin(stream::capacity(
        &tx,
        &bytes,
        4,
        &cancel,
        Some(&detached),
        Duration::from_secs(30),
    ));
    assert!(futures_util::poll!(&mut pending).is_pending());
    detached.cancel();
    assert!(pending.await.unwrap().is_none());
    assert_eq!(bytes.available_permits(), 4);
}
