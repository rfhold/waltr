use super::*;
use crate::{
    persistence::{DatabaseConfig, Persistence},
    routine::blob::filesystem::{FilesystemBlobStore, FilesystemConfig},
};

const IDLE: Duration = Duration::from_millis(80);

#[path = "../../../../waltr-script/tests/support/watchdog.rs"]
mod watchdog;

fn isolated(name: &str) -> bool {
    if std::env::var("WALTR_RPC_OUTPUT_CHILD").as_deref() == Ok(name) {
        return false;
    }
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            &format!("routine::rpc::transport::tests::{name}"),
            "--nocapture",
        ])
        .env("WALTR_RPC_OUTPUT_CHILD", name);
    let result = watchdog::run(command, Duration::from_secs(20), 65536);
    assert!(
        result.success && !result.killed && !result.overflow,
        "{}",
        String::from_utf8_lossy(&result.output)
    );
    true
}

struct Fixture {
    pending: Arc<PendingTransport>,
    input: Reader,
    output: Writer,
    frames: mpsc::Receiver<OutputFrame>,
}
impl Fixture {
    async fn new(total: Duration) -> Self {
        Self::with_repeated_input(total, false).await
    }
    async fn with_repeated_input(total: Duration, repeated: bool) -> Self {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join("opencode")
            .join(format!("rpc-output-wait-{}", uuid::Uuid::new_v4()));
        let private = root.join("private");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&private).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let blobs = Arc::new(
            FilesystemBlobStore::new(FilesystemConfig {
                private_root: private,
                workspace_root: crate::routine::blob::filesystem::WorkspaceRoot::Local(workspace),
                clock: Arc::new(blob::MonotonicClock::default()),
            })
            .unwrap(),
        );
        let persistence = Persistence::initialize(
            DatabaseConfig::from_urls("sqlite::memory:", "sqlite::memory:").unwrap(),
        )
        .await
        .unwrap();
        let engine = waltr_script::Engine::new(1).unwrap();
        let source = include_str!("../../../tests/fixtures/routines/dictate.yaml");
        let source = if repeated { source.replace("[media.execute]", "[media.execute, media.inputs]") } else { source.into() };
        let definition = Definition::parse(
            &source,
            &engine,
        )
        .unwrap();
        let mut pending = PendingTransport::new(
            "output-wait-test".into(),
            definition,
            vec![p::InputDeclaration {
                port: "audio".into(),
                format: Some(p::input_declaration::Format::Pcm16le(p::Pcm16Le {
                    sample_rate: 24000,
                    channels: 1,
                })),
            }],
            stream::Pool::principal(),
            stream::Pool::process(),
            Arc::new(Semaphore::new(4 * 1024 * 1024)),
            Arc::new(Semaphore::new(4 * 1024 * 1024)),
            persistence.script_invocation_store(),
            blobs.clone(),
        )
        .unwrap();
        blobs.shutdown().await.unwrap();
        // Scale only the test's cold edges, before the production driver starts.
        let streams = Streams::new(
            stream::Limits {
                idle: IDLE,
                input_duration: total,
                total_duration: total,
                ..Default::default()
            },
            stream::Pool::principal(),
            stream::Pool::process(),
            CancellationToken::new(),
        )
        .unwrap();
        let (input_writer, input) = streams
            .open(
                stream::Direction::Input,
                Format::Pcm16Le {
                    rate: 24000,
                    channels: 1,
                },
                false,
            )
            .unwrap();
        let (output, consumer) = streams
            .open(stream::Direction::Output, Format::Text, false)
            .unwrap();
        let unique = Arc::get_mut(&mut pending).unwrap();
        unique.streams = streams;
        *unique.cold.get_mut().unwrap() = Some(Cold {
            readers: BTreeMap::new(),
            outputs: BTreeMap::new(),
            uploads: BTreeMap::new(),
            writers: BTreeMap::from([("audio".into(), input_writer)]),
            consumers: BTreeMap::from([("transcript".into(), consumer)]),
        });
        let frames = unique.output_rx.get_mut().unwrap().take().unwrap();
        pending.require_native_eof();
        Driver(pending.clone()).start().await.unwrap();
        pending
            .send(
                0,
                vec![p::InputEvent {
                    event: Some(p::input_event::Event::Open(p::StreamOpen {
                        port: "audio".into(),
                        stream_id: "capture".into(),
                        format: Some(p::stream_open::Format::Pcm16le(p::Pcm16Le {
                            sample_rate: 24000,
                            channels: 1,
                        })),
                    })),
                }],
            )
            .await
            .unwrap();
        Self {
            pending,
            input,
            output,
            frames,
        }
    }
    async fn frame(&mut self) -> OutputFrame {
        tokio::time::timeout(Duration::from_secs(2), self.frames.recv())
            .await
            .unwrap()
            .unwrap()
    }
    async fn send(&self, event: p::input_event::Event) {
        self.pending
            .send(
                self.pending.next_sequence().await,
                vec![p::InputEvent { event: Some(event) }],
            )
            .await
            .unwrap();
    }
    async fn finish_input(&mut self, sequence: u64) {
        self.send(p::input_event::Event::Close(p::StreamClose {
            stream_id: "capture".into(),
            next_sequence: sequence,
        }))
        .await;
        self.send(p::input_event::Event::Finish(p::InputFinish {}))
            .await;
        assert!(self.input.read().await.unwrap().is_none());
    }
}
fn ack(mut frame: OutputFrame) {
    frame.consumed.take().unwrap().send(()).unwrap();
}

#[tokio::test]
async fn repeated_capture_identity_order_and_completion_share_one_admission_gate() {
    let mut fixture = Fixture::with_repeated_input(Duration::from_secs(5), true).await;
    let driver = Driver(fixture.pending.clone());
    ack(fixture.frame().await);
    let (first_id, mut first) = driver.next_capture("audio".into()).await.unwrap().unwrap();
    assert_eq!(first_id, "capture");
    assert!(!driver.seal_captures().await.unwrap());
    fixture.send(p::input_event::Event::Chunk(p::StreamChunk { stream_id: first_id.clone(), sequence: 0, payload: Some(p::stream_chunk::Payload::Audio(vec![1, 0])) })).await;
    fixture.send(p::input_event::Event::Close(p::StreamClose { stream_id: first_id.clone(), next_sequence: 1 })).await;
    assert!(matches!(first.read().await.unwrap().unwrap().chunk(), Chunk::Audio(bytes) if bytes == &[1, 0]));
    assert!(first.read().await.unwrap().is_none());
    assert!(!driver.seal_captures().await.unwrap());
    driver.acknowledge_capture(first_id.clone()).await.unwrap();
    assert!(driver.acknowledge_capture(first_id).await.is_err());
    fixture.send(p::input_event::Event::Open(p::StreamOpen { port: "audio".into(), stream_id: "second".into(), format: Some(p::stream_open::Format::Pcm16le(p::Pcm16Le { sample_rate: 24000, channels: 1 })) })).await;
    let (second_id, mut second) = driver.next_capture("audio".into()).await.unwrap().unwrap();
    assert_eq!(second_id, "second");
    fixture.send(p::input_event::Event::Close(p::StreamClose { stream_id: second_id.clone(), next_sequence: 0 })).await;
    assert!(second.read().await.unwrap().is_none());
    driver.acknowledge_capture(second_id).await.unwrap();
    assert!(driver.seal_captures().await.unwrap());
    assert!(driver.next_capture("audio".into()).await.unwrap().is_none());
    let late = p::InputEvent { event: Some(p::input_event::Event::Open(p::StreamOpen { port: "audio".into(), stream_id: "late".into(), format: Some(p::stream_open::Format::Pcm16le(p::Pcm16Le { sample_rate: 24000, channels: 1 })) })) };
    assert!(fixture.pending.send(fixture.pending.next_sequence().await, vec![late]).await.is_err());
    fixture.send(p::input_event::Event::Finish(p::InputFinish {})).await;
    fixture.pending.native_closed().await.unwrap();
    fixture.output.close(fixture.output.key()).await.unwrap();
    ack(fixture.frame().await);
    driver.finish().await.unwrap();
}

#[tokio::test]
async fn rpc_deferred_output_survives_capture_and_finalization_idle_with_ack_backpressure() {
    if isolated("rpc_deferred_output_survives_capture_and_finalization_idle_with_ack_backpressure")
    {
        return;
    }
    let mut fixture = Fixture::new(Duration::from_secs(5)).await;
    let open = fixture.frame().await;
    assert!(matches!(
        open.event.event,
        Some(p::output_event::Event::Open(_))
    ));
    ack(open);
    // Healthy capture lasts longer than output idle without producing output.
    for sequence in 0..10 {
        fixture
            .send(p::input_event::Event::Chunk(p::StreamChunk {
                stream_id: "capture".into(),
                sequence,
                payload: Some(p::stream_chunk::Payload::Audio(vec![0, 0])),
            }))
            .await;
        assert!(fixture.input.read().await.unwrap().is_some());
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !fixture.pending.streams.cancellation().is_cancelled(),
            "deferred RPC output cancelled healthy capture"
        );
    }
    fixture.finish_input(10).await;
    fixture.pending.native_closed().await.unwrap();
    // Diarization/finalization/helper work may also take longer than media idle.
    tokio::time::sleep(IDLE * 2).await;
    assert!(!fixture.pending.streams.cancellation().is_cancelled());
    fixture
        .output
        .write(
            fixture.output.key(),
            Chunk::Text(Text {
                segment: "notes".into(),
                text: "committed notes".into(),
                replace: false,
                committed: true,
            }),
        )
        .await
        .unwrap();
    fixture.output.close(fixture.output.key()).await.unwrap();
    let chunk = fixture.frame().await;
    let Some(p::output_event::Event::Chunk(value)) = &chunk.event.event else {
        panic!("expected notes")
    };
    assert_eq!(value.sequence, 0);
    let Some(p::stream_chunk::Payload::Text(text)) = &value.payload else {
        panic!("expected text")
    };
    assert_eq!(text.text, "committed notes");
    assert!(text.committed);
    assert!(chunk._packet.is_some());
    assert!(
        tokio::time::timeout(IDLE * 2, fixture.frames.recv())
            .await
            .is_err(),
        "output close bypassed packet acknowledgment"
    );
    ack(chunk);
    let close = fixture.frame().await;
    let Some(p::output_event::Event::Close(value)) = &close.event.event else {
        panic!("expected close")
    };
    assert_eq!(value.next_sequence, 1);
    ack(close);
    Driver(fixture.pending.clone()).finish().await.unwrap();
    assert_eq!(*fixture.pending.joined.lock().unwrap(), Some(true));
    assert!(!*fixture.pending.cleanup_failed.lock().unwrap());
}

#[tokio::test]
async fn rpc_deferred_output_absolute_deadline_and_explicit_cancel_join() {
    if isolated("rpc_deferred_output_absolute_deadline_and_explicit_cancel_join") {
        return;
    }
    for cancel in [false, true] {
        let mut fixture = Fixture::new(Duration::from_millis(240)).await;
        ack(fixture.frame().await);
        fixture.finish_input(0).await;
        fixture.pending.native_closed().await.unwrap();
        tokio::time::sleep(IDLE * 2).await;
        assert!(!fixture.pending.streams.cancellation().is_cancelled());
        if cancel {
            Driver(fixture.pending.clone()).cancel();
        }
        tokio::time::timeout(
            Duration::from_secs(2),
            Driver(fixture.pending.clone()).join(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(*fixture.pending.joined.lock().unwrap(), Some(false));
        assert!(Driver(fixture.pending.clone()).finish().await.is_err());
        assert!(fixture.pending.streams.cancellation().is_cancelled());
        assert!(
            fixture.frames.try_recv().is_err(),
            "pending output fabricated EOF"
        );
        assert!(
            fixture
                .output
                .write(
                    fixture.output.key(),
                    Chunk::Text(Text {
                        segment: "late".into(),
                        text: "late".into(),
                        replace: false,
                        committed: true,
                    })
                )
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn rpc_output_policy_does_not_change_ordinary_input_idle() {
    let streams = Streams::new(
        stream::Limits {
            idle: IDLE,
            ..Default::default()
        },
        stream::Pool::principal(),
        stream::Pool::process(),
        CancellationToken::new(),
    )
    .unwrap();
    let (_writer, mut reader) = streams
        .open(stream::Direction::Input, Format::Text, false)
        .unwrap();
    assert!(matches!(
        reader.read_rpc_output().await,
        Err(crate::routine::media::Error::Invalid)
    ));
    assert!(!streams.cancellation().is_cancelled());
    assert!(matches!(
        reader.read().await,
        Err(crate::routine::media::Error::Deadline)
    ));
    assert!(streams.cancellation().is_cancelled());
}

#[tokio::test]
async fn rpc_deferred_output_broken_producer_is_not_clean_eof() {
    if isolated("rpc_deferred_output_broken_producer_is_not_clean_eof") {
        return;
    }
    let mut fixture = Fixture::new(Duration::from_secs(2)).await;
    ack(fixture.frame().await);
    fixture.finish_input(0).await;
    fixture.pending.native_closed().await.unwrap();
    drop(fixture.output);
    tokio::time::timeout(
        Duration::from_secs(1),
        Driver(fixture.pending.clone()).join(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(*fixture.pending.joined.lock().unwrap(), Some(false));
    assert!(fixture.pending.streams.cancellation().is_cancelled());
    assert!(fixture.frames.try_recv().is_err());
    assert!(Driver(fixture.pending.clone()).finish().await.is_err());
}

#[tokio::test]
async fn rpc_deferred_output_subscriber_drop_releases_packet_and_credit() {
    if isolated("rpc_deferred_output_subscriber_drop_releases_packet_and_credit") {
        return;
    }
    let mut fixture = Fixture::new(Duration::from_secs(2)).await;
    ack(fixture.frame().await);
    fixture.finish_input(0).await;
    fixture.pending.native_closed().await.unwrap();
    let before = fixture.pending.principal_credit.available_permits();
    fixture
        .output
        .write(
            fixture.output.key(),
            Chunk::Text(Text {
                segment: "notes".into(),
                text: "notes".into(),
                replace: false,
                committed: true,
            }),
        )
        .await
        .unwrap();
    let frame = fixture.frame().await;
    assert!(frame._packet.is_some());
    assert!(fixture.pending.principal_credit.available_permits() < before);
    drop(frame);
    drop(fixture.frames);
    tokio::time::timeout(
        Duration::from_secs(1),
        Driver(fixture.pending.clone()).join(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(*fixture.pending.joined.lock().unwrap(), Some(false));
    assert_eq!(fixture.pending.principal_credit.available_permits(), before);
    assert!(fixture.output.close(fixture.output.key()).await.is_err());
}
