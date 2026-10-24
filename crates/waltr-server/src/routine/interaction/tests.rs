use super::*;
mod capabilities;
mod completion;
mod diarization;
mod diarization_wire;
mod fixtures;
mod limits;
mod recording;
mod regressions;
mod safety;
use fixtures::{next, request};
use std::sync::atomic::Ordering;

const TRANSCRIBE: &str =
    include_str!("../../../../../docs/examples/routines/audio-transcribe.yaml");
const CLEANUP: &str =
    include_str!("../../../../../docs/examples/routines/audio-transcribe-cleanup.yaml");
const SYNTHESIZE: &str =
    include_str!("../../../../../docs/examples/routines/audio-synthesize.yaml");

#[test]
fn text_revisions_replace_hypotheses_and_preserve_commitment() {
    let mut state = State::new(RoutineStreamFormat::Text, 1024);
    for (text, committed) in [("hel", false), ("hello", false), ("hello", true)] {
        state
            .accept(&Chunk::Text(Text {
                segment: "one".into(),
                text: text.into(),
                replace: true,
                committed,
            }))
            .unwrap();
    }
    state.close().unwrap();
    assert_eq!(state.collect(" ", 5).unwrap(), "hello");
    assert!(
        state
            .accept(&Chunk::Text(Text {
                segment: "one".into(),
                text: "!".into(),
                replace: false,
                committed: true
            }))
            .is_err()
    );
}

#[test]
fn uncommitted_close_and_wrong_pcm_fail() {
    let mut text = State::new(RoutineStreamFormat::Text, 64);
    text.accept(&Chunk::Text(Text {
        segment: "a".into(),
        text: "maybe".into(),
        replace: true,
        committed: false,
    }))
    .unwrap();
    assert_eq!(text.close().unwrap_err().code, "invalid_stream");
    let mut audio = State::new(
        RoutineStreamFormat::Audio {
            encoding: crate::resource_catalog::routine::RoutineAudioEncoding::PcmS16Le,
            sample_rate_hz: 16000,
            channels: 2,
        },
        64,
    );
    assert!(audio.accept(&Chunk::Audio(vec![0; 2])).is_err());
    assert!(audio.accept(&Chunk::Audio(vec![0; 4])).is_ok());
}

#[tokio::test(start_paused = true)]
async fn saturated_byte_queue_times_out_and_cancellation_wins() {
    let limits = InteractionLimits {
        chunk_bytes: 4,
        edge_bytes: 4,
        queued_chunks: 8,
        ..Default::default()
    };
    let (sender, mut receiver) = stream::edge(&limits);
    let cancel = CancellationToken::new();
    sender
        .send(Frame::Chunk(Chunk::Audio(vec![0; 4])), &cancel)
        .await
        .unwrap();
    assert_eq!(
        sender
            .send(Frame::Chunk(Chunk::Audio(vec![0; 2])), &cancel)
            .await
            .unwrap_err()
            .code,
        "resource_exhausted"
    );
    cancel.cancel();
    assert_eq!(
        sender
            .send(Frame::Chunk(Chunk::Audio(vec![0; 2])), &cancel)
            .await
            .unwrap_err()
            .code,
        "cancelled"
    );
    assert!(receiver.recv(&CancellationToken::new()).await.is_ok());
}

#[tokio::test]
async fn group_reject_and_interrupt_are_explicit() {
    use crate::resource_catalog::routine::{RoutineWorkGroup, RoutineWorkGroupBusy};
    let parent = CancellationToken::new();
    let group = group::Group::new(&RoutineWorkGroup {
        name: "g".into(),
        on_busy: RoutineWorkGroupBusy::Reject,
        max_queued: None,
    });
    let first = group.enter(&parent).await.unwrap();
    assert!(matches!(
        group.enter(&parent).await,
        Err(Error {
            code: "work_group_busy",
            ..
        })
    ));
    drop(first);
    assert!(group.enter(&parent).await.is_ok());
    let group = group::Group::new(&RoutineWorkGroup {
        name: "g".into(),
        on_busy: RoutineWorkGroupBusy::Interrupt,
        max_queued: None,
    });
    let first = group.enter(&parent).await.unwrap();
    let second = group.enter(&parent).await.unwrap();
    assert!(first.token.is_cancelled());
    assert!(!second.token.is_cancelled());
    drop(first);
    assert!(!second.token.is_cancelled());
}

#[test]
fn transcribe_final_reuses_pending_identity_without_duplicate_hypotheses() {
    use waltr_audio::stt;
    let mut mapper = adapters::Transcripts::default();
    let response = |text: &str, is_final| stt::StreamingRecognizeResponse {
        results: vec![stt::StreamingRecognitionResult {
            alternatives: vec![stt::SpeechRecognitionAlternative {
                transcript: text.into(),
                ..Default::default()
            }],
            is_final,
            channel_tag: 1,
            ..Default::default()
        }],
        ..Default::default()
    };
    let interim = mapper.map(response("hi", false));
    let final_text = mapper.map(response("hello", true));
    assert_eq!(interim[0].segment, final_text[0].segment);
    assert!(final_text[0].replace && final_text[0].committed);
    assert_ne!(
        mapper.map(response("next", true))[0].segment,
        final_text[0].segment
    );
}

#[tokio::test]
async fn production_transcribe_publishes_before_input_close() {
    let server = fixtures::SpeechServer::start().await;
    let mut request = request(TRANSCRIBE).await;
    Arc::make_mut(&mut request.configuration)
        .providers
        .get_mut("speech")
        .unwrap()
        .url = server.endpoint.clone();
    let release = Arc::new(tokio::sync::Notify::new());
    let (host, _sandbox) = fixtures::Host::new(release).await;
    let mut interaction = start_sessions(request, host).unwrap();
    assert!(matches!(
        next(&mut interaction.events).await,
        Event::Opened { .. }
    ));
    let input = interaction.inputs.get_mut("audio").unwrap();
    input.send(Chunk::Audio(vec![0; 4])).await.unwrap();
    assert!(
        matches!(next(&mut interaction.events).await, Event::Chunk { chunk: Chunk::Text(Text { committed: true, text, .. }), .. } if text == "word0")
    );
    interaction
        .inputs
        .get_mut("audio")
        .unwrap()
        .close()
        .await
        .unwrap();
    assert!(matches!(
        next(&mut interaction.events).await,
        Event::Closed { error: None, .. }
    ));
    assert!(matches!(
        next(&mut interaction.events).await,
        Event::Completed(Ok(_))
    ));
    interaction.join().await;
}

#[tokio::test]
async fn production_synthesis_streams_only_newly_committed_segments() {
    let server = fixtures::SpeechServer::start().await;
    let mut request = request(SYNTHESIZE).await;
    Arc::make_mut(&mut request.configuration)
        .providers
        .get_mut("voice")
        .unwrap()
        .url = server.endpoint.clone();
    let (host, _sandbox) = fixtures::Host::new(Arc::new(tokio::sync::Notify::new())).await;
    let mut interaction = start_sessions(request, host).unwrap();
    assert!(matches!(
        next(&mut interaction.events).await,
        Event::Opened { .. }
    ));
    interaction
        .inputs
        .get_mut("text")
        .unwrap()
        .send(Chunk::Text(Text {
            segment: "one".into(),
            text: "draft".into(),
            replace: true,
            committed: false,
        }))
        .await
        .unwrap();
    interaction
        .inputs
        .get_mut("text")
        .unwrap()
        .send(Chunk::Text(Text {
            segment: "two".into(),
            text: "later".into(),
            replace: false,
            committed: true,
        }))
        .await
        .unwrap();
    // A later committed segment cannot overtake the first segment's pending revision.
    assert!(
        tokio::time::timeout(Duration::from_millis(20), interaction.events.recv())
            .await
            .is_err()
    );
    interaction
        .inputs
        .get_mut("text")
        .unwrap()
        .send(Chunk::Text(Text {
            segment: "one".into(),
            text: "final".into(),
            replace: true,
            committed: true,
        }))
        .await
        .unwrap();
    for _ in 0..2 {
        assert!(
            matches!(next(&mut interaction.events).await, Event::Chunk { chunk: Chunk::Audio(bytes), .. } if bytes == [1, 0])
        );
    }
    interaction
        .inputs
        .get_mut("text")
        .unwrap()
        .close()
        .await
        .unwrap();
    assert!(matches!(
        next(&mut interaction.events).await,
        Event::Closed { error: None, .. }
    ));
    assert!(matches!(
        next(&mut interaction.events).await,
        Event::Completed(Ok(_))
    ));
    interaction.join().await;
}

#[tokio::test]
async fn cleanup_barrier_runs_one_helper_then_agent_delta_reaches_tts_early() {
    let mut definition: crate::resource_catalog::routine::RoutineDefinition =
        serde_yaml_ng::from_str(CLEANUP).unwrap();
    let synthesis: crate::resource_catalog::routine::RoutineDefinition =
        serde_yaml_ng::from_str(SYNTHESIZE).unwrap();
    definition.spec.triggers[0]
        .interaction
        .as_mut()
        .unwrap()
        .output_ports = synthesis.spec.triggers[0]
        .interaction
        .as_ref()
        .unwrap()
        .output_ports
        .clone();
    let mut synthesize = synthesis.spec.tasks[0].clone();
    synthesize.task_spec.synthesize.as_mut().unwrap().input =
        RoutineStreamRef::TaskOutput("cleanup".into());
    definition.spec.tasks.push(synthesize);
    definition.spec.outputs = synthesis.spec.outputs;
    let server = fixtures::SpeechServer::start().await;
    let mut request = request(&serde_yaml_ng::to_string(&definition).unwrap()).await;
    for name in ["speech", "voice"] {
        Arc::make_mut(&mut request.configuration)
            .providers
            .get_mut(name)
            .unwrap()
            .url = server.endpoint.clone();
    }
    let release = Arc::new(tokio::sync::Notify::new());
    let (host, _sandbox) = fixtures::Host::new(release.clone()).await;
    let mut interaction = start_sessions(request, host.clone()).unwrap();
    assert!(matches!(
        next(&mut interaction.events).await,
        Event::Opened { .. }
    ));
    interaction
        .inputs
        .get_mut("audio")
        .unwrap()
        .send(Chunk::Audio(vec![0; 4]))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), interaction.events.recv())
            .await
            .is_err()
    );
    assert_eq!(host.created.load(Ordering::SeqCst), 0);
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
        Event::Chunk {
            chunk: Chunk::Audio(_),
            ..
        }
    ));
    assert_eq!(host.created.load(Ordering::SeqCst), 1);
    assert!(host.subscribed.load(Ordering::SeqCst));
    // The scripted provider cannot finish until this release; audio already arrived.
    release.notify_one();
    let mut completed = false;
    while !completed {
        match next(&mut interaction.events).await {
            Event::Closed { error, .. } => assert!(error.is_none(), "{error:?}"),
            Event::Completed(result) => {
                assert!(result.is_ok(), "{result:?}");
                completed = true;
            }
            _ => (),
        }
    }
    assert_eq!(host.created.load(Ordering::SeqCst), 1);
    use crate::session::SessionHost;
    let sessions = host.list().await.unwrap();
    let child = host.load(&sessions[0].id).await.unwrap();
    assert_eq!(child.record.snapshot().prompts, ["word0word1"]);
    interaction.join().await;
    host.shutdown().await;
}

#[tokio::test]
async fn output_drop_interrupts_real_helper_and_joins_owned_work() {
    let mut definition: crate::resource_catalog::routine::RoutineDefinition =
        serde_yaml_ng::from_str(CLEANUP).unwrap();
    // Use the normal cleanup graph so the durable helper starts after an explicit input close.
    definition.spec.tasks[2]
        .task_spec
        .session_template
        .as_mut()
        .unwrap()
        .system_prompt
        .text = "cleanup".into();
    let server = fixtures::SpeechServer::start().await;
    let mut request = request(&serde_yaml_ng::to_string(&definition).unwrap()).await;
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
    tokio::time::timeout(Duration::from_secs(5), interaction.shutdown())
        .await
        .unwrap();
    assert_eq!(host.interrupts.load(Ordering::SeqCst), 1);
    use crate::session::SessionHost;
    for session in host.list().await.unwrap() {
        assert!(host.active_state(&session.id).await.is_none());
    }
    host.shutdown().await;
}

#[tokio::test]
async fn generation_fence_rechecks_already_queued_publications() {
    use crate::resource_catalog::routine::{RoutineWorkGroup, RoutineWorkGroupBusy};
    let group = group::Group::new(&RoutineWorkGroup {
        name: "replace".into(),
        on_busy: RoutineWorkGroupBusy::Interrupt,
        max_queued: None,
    });
    let cancel = CancellationToken::new();
    let first = group.enter(&cancel).await.unwrap();
    let limits = InteractionLimits::default();
    let (sender, mut receiver) = stream::edge(&limits);
    sender
        .send_fenced(
            Frame::Chunk(Chunk::Audio(vec![0; 2])),
            &cancel,
            Some(&first.fence()),
        )
        .await
        .unwrap();
    let (tx, rx) = mpsc::channel(1);
    let output = OutputSender {
        tx,
        bytes: Arc::new(tokio::sync::Semaphore::new(16)),
        max: 16,
        queue_wait: Duration::from_secs(30),
    };
    output
        .send_fenced(
            Event::Chunk {
                port: "old".into(),
                chunk: Chunk::Audio(vec![0; 2]),
            },
            &cancel,
            Some(first.fence()),
        )
        .await
        .unwrap();
    let (terminal, terminal_rx) = mpsc::channel(1);
    let mut events = Events {
        rx,
        terminal: terminal_rx,
        cancel: cancel.clone(),
    };
    let _second = group.enter(&cancel).await.unwrap();
    assert_eq!(
        receiver.recv(&cancel).await.unwrap_err().code,
        "task_interrupted"
    );
    drop(output);
    terminal
        .send(Event::Completed(Ok(Vec::new())))
        .await
        .unwrap();
    assert!(matches!(events.recv().await, Some(Event::Completed(Ok(_)))));
}
