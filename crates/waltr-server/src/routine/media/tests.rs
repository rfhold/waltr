use super::{
    Error, Producer,
    speech::{GoogleSpeech, SpeechBackend},
    stream::*,
};
use crate::resource_catalog::speech::{ResolvedSpeechModel, SpeechSettings};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use waltr::grpc::waltr::config::v1 as proto;

#[path = "../../../../waltr-audio/tests/support/compact_wire.rs"]
mod compact_wire;
#[allow(dead_code)]
#[path = "../../../../waltr-audio/tests/support/mod.rs"]
mod fixtures;

mod snapshot_wait_tests;

fn pinned_recognition(endpoint: &str, rate: u32, diarization: bool) -> ResolvedSpeechModel {
    ResolvedSpeechModel {
        provider_key: format!("stt-{rate}"),
        model_key: format!("model-{rate}"),
        endpoint: endpoint.into(),
        capabilities: proto::SpeechCapabilities {
            unary: false,
            streaming: true,
        },
        settings: SpeechSettings::Recognition(proto::SpeechRecognitionOptions {
            recognizer: "recognizer".into(),
            model: format!("upstream-{rate}"),
            language_codes: vec!["en-US".into()],
            sample_rate_hz: rate,
            channels: 1,
            diarization,
            ..Default::default()
        }),
    }
}

#[tokio::test]
async fn separate_snapshot_pcm_operations_are_strict_before_provider_effects() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let plain = pinned_recognition(&endpoint, 24000, false);
    let snapshot = pinned_recognition(&endpoint, 16000, true);
    let both = GoogleSpeech::with_recognition_selections(
        Some(plain.clone()),
        Some(snapshot.clone()),
        None,
    )
    .unwrap();
    let snapshot_only =
        GoogleSpeech::with_recognition_selections(None, Some(snapshot.clone()), None).unwrap();
    let plain_only = GoogleSpeech::new(Some(plain.clone()), None).unwrap();
    let pcm16 = Format::Pcm16Le {
        rate: 16000,
        channels: 1,
    };
    let pcm48 = Format::Pcm16Le {
        rate: 48000,
        channels: 1,
    };
    both.preflight(pcm(), false).unwrap();
    both.preflight(pcm16, true).unwrap();
    snapshot_only.preflight(pcm16, true).unwrap();
    for (backend, format, is_snapshot) in [
        (both.clone(), pcm16, false),
        (both.clone(), pcm(), true),
        (both.clone(), pcm48, false),
        (both.clone(), pcm48, true),
        (snapshot_only, pcm16, false),
        (plain_only, pcm(), true),
    ] {
        assert_eq!(backend.preflight(format, is_snapshot), Err(Error::Invalid));
        let s = streams(Limits::default());
        let (_writer, input) = s.open(Direction::Input, format, false).unwrap();
        if is_snapshot {
            let workspace = super::snapshot::Workspace::new(
                &input,
                super::snapshot::WorkspacePool::principal(),
                super::snapshot::WorkspacePool::process(),
            );
            assert!(matches!(
                backend.transcribe_snapshot(input, workspace),
                Err(Error::Invalid)
            ));
        } else {
            let (output, _reader) = s.open(Direction::Output, Format::Text, false).unwrap();
            assert!(matches!(
                backend.transcribe(input, output),
                Err(Error::Invalid)
            ));
        }
    }
    assert!(
        GoogleSpeech::with_recognition_selections(
            Some(plain),
            Some(pinned_recognition(&endpoint, 16000, false)),
            None,
        )
        .is_err()
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn separate_snapshot_pcm_16khz_loopback_keeps_plain_selection_pinned() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let plain_endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (_client, server) = compact_wire::start(
        vec![compact_wire::cumulative_word(
            2,
            "hello",
            "snapshot-speaker",
            true,
        )],
        true,
        false,
    )
    .await;
    let backend = GoogleSpeech::with_recognition_selections(
        Some(pinned_recognition(&plain_endpoint, 24000, false)),
        Some(pinned_recognition(&server.endpoint, 16000, true)),
        None,
    )
    .unwrap();
    let s = streams(Limits::default());
    let (mut writer, input) = s
        .open(
            Direction::Input,
            Format::Pcm16Le {
                rate: 16000,
                channels: 1,
            },
            false,
        )
        .unwrap();
    let workspace = super::snapshot::Workspace::new(
        &input,
        super::snapshot::WorkspacePool::principal(),
        super::snapshot::WorkspacePool::process(),
    );
    let (owner, reader) = backend.transcribe_snapshot(input, workspace).unwrap();
    writer
        .write(writer.key(), Chunk::Audio(vec![0, 0]))
        .await
        .unwrap();
    writer.close(writer.key()).await.unwrap();
    let (result, note) = tokio::join!(joined(owner), reader.collect(1024));
    result.unwrap();
    let note = note.unwrap();
    assert!(note.committed());
    assert_eq!(
        note.chunks().collect::<String>(),
        "snapshot-speaker: hello hello"
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn compact_snapshot_multibyte_chunks_speaker_corrections_and_shortening_collect_latest() {
    use super::snapshot::{Workspace, WorkspacePool};
    let (_client, server) = compact_wire::start(
        vec![
            compact_wire::cumulative_word(20000, "éé", "讲者", true),
            compact_wire::cumulative_word(24000, "éé", "修订", false),
            compact_wire::cumulative_word(14000, "éé", "新", true),
        ],
        true,
        false,
    )
    .await;
    let s = streams(Limits::default());
    let (mut writer, input) = s.open(Direction::Input, pcm(), false).unwrap();
    let workspace = Workspace::new(&input, WorkspacePool::principal(), WorkspacePool::process());
    let (owner, mut reader) = backend(&server.endpoint, true)
        .transcribe_snapshot(input, workspace.clone())
        .unwrap();
    writer.close(writer.key()).await.unwrap();
    for (generation, speaker, count) in [(1, "讲者", 20000), (2, "修订", 24000)] {
        let revision = reader.next_revision().await.unwrap().unwrap();
        assert!(!revision.committed());
        assert_eq!(revision.generation(), generation);
        assert_eq!(revision.chunks().next().unwrap().len(), 65535);
        assert!(revision.chunks().all(|chunk| chunk.len() <= 65536));
        assert_eq!(
            revision.chunks().collect::<String>(),
            format!("{speaker}:{}", " éé".repeat(count))
        );
    }
    let (result, note) = tokio::join!(joined(owner), reader.collect(256 * 1024));
    result.unwrap();
    let note = note.unwrap();
    assert!(note.committed());
    assert_eq!(note.generation(), 3);
    assert_eq!(note.chunks().count(), 2);
    assert_eq!(
        note.chunks().collect::<String>(),
        format!("新:{}", " éé".repeat(14000))
    );
    drop(note);
    assert_eq!(workspace.retained(), 0);
}

#[tokio::test]
async fn compact_snapshot_process_pool_caps_different_principals_and_recovers_after_cancel() {
    use super::snapshot::{Workspace, WorkspacePool};
    let (_client, server) = compact_wire::start_stalled(Vec::new(), true).await;
    let process = WorkspacePool::process();
    let mut active = Vec::new();
    for i in 0..5 {
        let s = streams(Limits::default());
        let (writer, input) = s.open(Direction::Input, pcm(), false).unwrap();
        let principal = WorkspacePool::principal();
        let workspace = Workspace::new(&input, principal.clone(), process.clone());
        match backend(&server.endpoint, true).transcribe_snapshot(input, workspace.clone()) {
            Ok((owner, reader)) => {
                assert!(i < 4);
                assert_eq!(principal.retained(), 16 * 1024 * 1024);
                active.push((owner, reader, writer, workspace, principal));
            }
            Err(error) => {
                assert_eq!(i, 4);
                assert_eq!(error, Error::Limit);
                assert_eq!(workspace.retained(), 0);
                assert_eq!(principal.retained(), 0);
            }
        }
    }
    assert_eq!(process.retained(), 64 * 1024 * 1024);
    for (owner, reader, writer, workspace, principal) in active {
        owner.cancel();
        assert!(joined(owner).await.is_err());
        drop(reader);
        drop(writer);
        assert_eq!(workspace.retained(), 0);
        assert_eq!(principal.retained(), 0);
    }
    assert_eq!(process.retained(), 0);
    let s = streams(Limits::default());
    let (writer, input) = s.open(Direction::Input, pcm(), false).unwrap();
    let workspace = Workspace::new(&input, WorkspacePool::principal(), process.clone());
    let (owner, reader) = backend(&server.endpoint, true)
        .transcribe_snapshot(input, workspace.clone())
        .unwrap();
    owner.cancel();
    assert!(joined(owner).await.is_err());
    drop(reader);
    drop(writer);
    assert_eq!(workspace.retained(), 0);
    assert_eq!(process.retained(), 0);
}

#[tokio::test]
async fn compact_snapshot_queue_and_absolute_deadline_wake_checked_out_and_output_waits() {
    use super::snapshot::{Workspace, WorkspacePool};
    let (_client, server) = compact_wire::start_stalled(
        (0..6)
            .map(|_| compact_wire::cumulative(1000, "S", true))
            .collect(),
        true,
    )
    .await;
    let s = streams(Limits {
        idle: Duration::from_millis(100),
        queue_wait: Duration::from_millis(100),
        ..Limits::default()
    });
    let (mut writer, input) = s.open(Direction::Input, pcm(), false).unwrap();
    let workspace = Workspace::new(&input, WorkspacePool::principal(), WorkspacePool::process());
    let (owner, mut reader) = backend(&server.endpoint, true)
        .transcribe_snapshot(input, workspace.clone())
        .unwrap();
    writer.close(writer.key()).await.unwrap();
    let mut pinned = Vec::new();
    for _ in 0..3 {
        pinned.push(reader.next_revision().await.unwrap().unwrap());
    }
    assert_eq!(reader.next_revision().await.unwrap_err(), Error::Cancelled);
    assert_eq!(joined(owner).await, Err(Error::Deadline));
    assert!(s.cancellation().is_cancelled());
    assert_eq!(workspace.retained(), 16 * 1024 * 1024);
    drop(pinned);
    drop(reader);
    assert_eq!(workspace.retained(), 0);

    let s = streams(Limits {
        input_duration: Duration::from_millis(100),
        total_duration: Duration::from_millis(250),
        idle: Duration::from_secs(5),
        queue_wait: Duration::from_secs(5),
        ..Limits::default()
    });
    let (mut writer, input) = s.open(Direction::Input, pcm(), false).unwrap();
    let workspace = Workspace::new(&input, WorkspacePool::principal(), WorkspacePool::process());
    let (owner, mut reader) = backend(&server.endpoint, true)
        .transcribe_snapshot(input, workspace.clone())
        .unwrap();
    writer.close(writer.key()).await.unwrap();
    // No checkout: the second publication is blocked behind the first preview.
    assert_eq!(joined(owner).await, Err(Error::Deadline));
    assert!(s.cancellation().is_cancelled());
    assert!(reader.next_revision().await.is_err());
    assert_eq!(workspace.retained(), 16 * 1024 * 1024);
    drop(reader);
    assert_eq!(workspace.retained(), 0);
}

#[tokio::test]
async fn compact_snapshot_actual_mib_note_uses_chunk_reader_and_keeps_credit_after_join() {
    use super::snapshot::{Workspace, WorkspacePool};
    let (_client, server) = compact_wire::start(
        vec![compact_wire::cumulative(524287, "S", true)],
        true,
        false,
    )
    .await;
    let s = streams(Limits::default());
    let (mut writer, input) = s.open(Direction::Input, pcm(), false).unwrap();
    let principal = WorkspacePool::principal();
    let process = WorkspacePool::process();
    let workspace = Workspace::new(&input, principal.clone(), process.clone());
    let (owner, reader) = backend(&server.endpoint, true)
        .transcribe_snapshot(input, workspace.clone())
        .unwrap();
    writer
        .write(writer.key(), Chunk::Audio(vec![0, 0]))
        .await
        .unwrap();
    writer.close(writer.key()).await.unwrap();
    let (result, note) = tokio::join!(joined(owner), reader.collect(1024 * 1024));
    result.unwrap();
    let note = note.unwrap();
    assert_eq!(note.bytes(), 1024 * 1024);
    assert!(note.committed());
    assert_eq!(note.chunks().count(), 16);
    assert!(note.chunks().all(|chunk| chunk.len() <= 65536));
    assert!(note.chunks().next().unwrap().starts_with("S: a a"));
    assert_eq!(workspace.retained(), 16 * 1024 * 1024);
    assert_eq!(principal.retained(), workspace.retained());
    assert_eq!(process.retained(), workspace.retained());
    assert_eq!(s.retained_bytes(), 0);
    drop(note);
    assert_eq!(workspace.retained(), 0);
    assert_eq!(principal.retained(), 0);
    assert_eq!(process.retained(), 0);
}

#[tokio::test]
async fn compact_snapshot_corrections_shortening_and_e6_cap_fail_closed() {
    use super::snapshot::{Workspace, WorkspacePool};
    let frames = vec![
        compact_wire::cumulative(10000, "S", true),
        compact_wire::cumulative(12000, "T", false),
        compact_wire::cumulative(3, "U", true),
    ];
    let (_client, server) = compact_wire::start(frames, true, false).await;
    let s = streams(Limits::default());
    let (mut writer, input) = s.open(Direction::Input, pcm(), false).unwrap();
    let workspace = Workspace::new(&input, WorkspacePool::principal(), WorkspacePool::process());
    let (owner, mut reader) = backend(&server.endpoint, true)
        .transcribe_snapshot(input, workspace.clone())
        .unwrap();
    writer.close(writer.key()).await.unwrap();
    let mut previews = 0;
    let mut last = None;
    while let Some(revision) = reader.next_revision().await.unwrap() {
        if !revision.committed() {
            previews += 1;
        }
        last = Some(revision);
    }
    joined(owner).await.unwrap();
    let last = last.unwrap();
    assert_eq!(previews, 3);
    assert!(last.committed());
    assert_eq!(last.generation(), 3);
    assert_eq!(last.chunks().collect::<String>(), "U: a a a");
    drop(last);
    assert_eq!(workspace.retained(), 0);

    let (_client, server) = compact_wire::start(
        vec![compact_wire::cumulative(524287, "S", true)],
        true,
        false,
    )
    .await;
    let s = streams(Limits::default());
    let (mut writer, input) = s.open(Direction::Input, pcm(), false).unwrap();
    let workspace = Workspace::new(&input, WorkspacePool::principal(), WorkspacePool::process());
    let (owner, reader) = backend(&server.endpoint, true)
        .transcribe_snapshot(input, workspace.clone())
        .unwrap();
    writer.close(writer.key()).await.unwrap();
    assert!(matches!(
        reader.collect(256 * 1024).await,
        Err(Error::Limit)
    ));
    assert!(joined(owner).await.is_err());
    assert!(s.cancellation().is_cancelled());
    assert_eq!(workspace.retained(), 0);
}

#[tokio::test]
async fn compact_snapshot_checkout_backpressure_cancel_and_shared_workspace_admission() {
    use super::snapshot::{Workspace, WorkspacePool};
    let (_client, server) = compact_wire::start(
        (0..6)
            .map(|_| compact_wire::cumulative(10000, "S", true))
            .collect(),
        true,
        false,
    )
    .await;
    let s = streams(Limits::default());
    let (mut writer, input) = s.open(Direction::Input, pcm(), false).unwrap();
    let workspace = Workspace::new(&input, WorkspacePool::principal(), WorkspacePool::process());
    let (owner, mut reader) = backend(&server.endpoint, true)
        .transcribe_snapshot(input, workspace.clone())
        .unwrap();
    writer.close(writer.key()).await.unwrap();
    let mut pinned = Vec::new();
    for _ in 0..3 {
        pinned.push(reader.next_revision().await.unwrap().unwrap());
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(20), reader.next_revision())
            .await
            .is_err()
    );
    s.cancellation().cancel();
    assert!(joined(owner).await.is_err());
    assert!(reader.next_revision().await.is_err());
    assert_eq!(workspace.retained(), 16 * 1024 * 1024);
    drop(pinned);
    drop(reader);
    assert_eq!(workspace.retained(), 0);

    let principal = WorkspacePool::principal();
    let process = WorkspacePool::process();
    let mut active = Vec::new();
    for i in 0..3 {
        let s = streams(Limits::default());
        let (writer, input) = s.open(Direction::Input, pcm(), false).unwrap();
        let workspace = Workspace::new(&input, principal.clone(), process.clone());
        match backend(&server.endpoint, true).transcribe_snapshot(input, workspace) {
            Ok((owner, reader)) => {
                assert!(i < 2);
                active.push((owner, reader, writer));
            }
            Err(error) => {
                assert_eq!(i, 2);
                assert_eq!(error, Error::Limit);
            }
        }
    }
    assert_eq!(principal.retained(), 32 * 1024 * 1024);
    assert_eq!(process.retained(), 32 * 1024 * 1024);
    for (owner, reader, writer) in active {
        drop(reader);
        drop(writer);
        let _result = joined(owner).await;
    }
    assert_eq!(principal.retained(), 0);
    assert_eq!(process.retained(), 0);
}

#[tokio::test]
async fn compact_snapshot_never_commits_provider_error_or_premature_eof() {
    use super::snapshot::{Workspace, WorkspacePool};
    for early in [false, true] {
        let (_client, server) = compact_wire::start(
            vec![compact_wire::cumulative(10000, "S", true)],
            !early,
            !early,
        )
        .await;
        let s = streams(Limits::default());
        let (mut writer, input) = s.open(Direction::Input, pcm(), false).unwrap();
        let workspace =
            Workspace::new(&input, WorkspacePool::principal(), WorkspacePool::process());
        let (owner, mut reader) = backend(&server.endpoint, true)
            .transcribe_snapshot(input, workspace.clone())
            .unwrap();
        writer
            .write(writer.key(), Chunk::Audio(vec![0, 0]))
            .await
            .unwrap();
        if !early {
            writer.close(writer.key()).await.unwrap();
        }
        while let Ok(Some(revision)) = reader.next_revision().await {
            assert!(!revision.committed());
        }
        assert!(joined(owner).await.is_err());
        drop(reader);
        drop(writer);
        assert_eq!(workspace.retained(), 0);
    }
}

fn streams(limits: Limits) -> Streams {
    Streams::new(
        limits,
        Pool::principal(),
        Pool::process(),
        CancellationToken::new(),
    )
    .unwrap()
}
fn pcm() -> Format {
    Format::Pcm16Le {
        rate: 24000,
        channels: 1,
    }
}
fn text(segment: &str, value: &str, replace: bool, committed: bool) -> Chunk {
    Chunk::Text(Text {
        segment: segment.into(),
        text: value.into(),
        replace,
        committed,
    })
}
async fn joined(owner: Producer) -> Result<(), Error> {
    tokio::time::timeout(Duration::from_secs(10), owner.join())
        .await
        .expect("bounded producer join")
}
fn backend(endpoint: &str, diarization: bool) -> GoogleSpeech {
    let capabilities = proto::SpeechCapabilities {
        unary: true,
        streaming: true,
    };
    GoogleSpeech::new(
        Some(ResolvedSpeechModel {
            provider_key: "speech".into(),
            model_key: "stt".into(),
            endpoint: endpoint.into(),
            capabilities,
            settings: SpeechSettings::Recognition(proto::SpeechRecognitionOptions {
                recognizer: "recognizer".into(),
                model: "chirp_3".into(),
                language_codes: vec!["en-US".into()],
                sample_rate_hz: 24000,
                channels: 1,
                interim_results: true,
                automatic_punctuation: true,
                diarization,
            }),
        }),
        Some(ResolvedSpeechModel {
            provider_key: "speech".into(),
            model_key: "tts".into(),
            endpoint: endpoint.into(),
            capabilities,
            settings: SpeechSettings::Synthesis(proto::SpeechSynthesisOptions {
                voice: "voice".into(),
                language_code: "en-US".into(),
                sample_rate_hz: 24000,
                channels: 1,
                ..Default::default()
            }),
        }),
    )
    .unwrap()
}

#[tokio::test]
async fn identity_direction_lifetime_and_close_are_checked() {
    let s = streams(Limits::default());
    let (mut writer, mut reader) = s.open(Direction::Input, pcm(), false).unwrap();
    let (other, _) = s.open(Direction::Output, pcm(), false).unwrap();
    assert_eq!(
        writer.write(other.key(), Chunk::Audio(vec![0, 0])).await,
        Err(Error::Identity)
    );
    writer
        .write(writer.key(), Chunk::Audio(vec![0, 0]))
        .await
        .unwrap();
    writer.close(writer.key()).await.unwrap();
    assert_eq!(writer.close(writer.key()).await, Err(Error::Invalid));
    assert!(reader.read().await.unwrap().is_some());
    assert!(reader.read().await.unwrap().is_none());
    for _ in 0..3 {
        s.open(Direction::Input, pcm(), false).unwrap();
    }
    assert!(matches!(
        s.open(Direction::Input, pcm(), false),
        Err(Error::Limit)
    ));
}

#[tokio::test]
async fn broken_eof_missing_commit_pcm_and_caps_fail_closed() {
    let s = streams(Limits::default());
    let (writer, mut reader) = s.open(Direction::Input, pcm(), false).unwrap();
    drop(writer);
    assert!(matches!(reader.read().await, Err(Error::BrokenProducer)));
    let s = streams(Limits::default());
    let (mut writer, _reader) = s.open(Direction::Input, Format::Text, false).unwrap();
    writer
        .write(writer.key(), text("a", "preview", true, false))
        .await
        .unwrap();
    assert_eq!(writer.close(writer.key()).await, Err(Error::Uncommitted));
    let s = streams(Limits::default());
    let (mut writer, _) = s.open(Direction::Input, pcm(), false).unwrap();
    assert_eq!(
        writer.write(writer.key(), Chunk::Audio(vec![0])).await,
        Err(Error::Invalid)
    );
    let s = streams(Limits {
        input_bytes: 2,
        ..Limits::default()
    });
    let (mut writer, _reader) = s.open(Direction::Input, pcm(), false).unwrap();
    writer
        .write(writer.key(), Chunk::Audio(vec![0, 0]))
        .await
        .unwrap();
    assert_eq!(
        writer.write(writer.key(), Chunk::Audio(vec![0, 0])).await,
        Err(Error::Limit)
    );
}

#[tokio::test]
async fn ordered_revisions_collect_and_freeze() {
    let s = streams(Limits::default());
    let (mut writer, reader) = s.open(Direction::Input, Format::Text, false).unwrap();
    let collector = tokio::spawn(collect(reader, 256 * 1024));
    for chunk in [
        text("a", "bad", true, false),
        text("b", "second", true, true),
        text("a", "first", true, false),
        text("a", "!", false, true),
    ] {
        writer.write(writer.key(), chunk).await.unwrap();
    }
    writer.close(writer.key()).await.unwrap();
    let collected = collector.await.unwrap().unwrap();
    assert_eq!(
        collected.segments().collect::<Vec<_>>(),
        ["first!", "second"]
    );
    drop(collected);
    drop(writer);
    assert_eq!(s.retained_bytes(), 0);

    let s = streams(Limits::default());
    let (mut writer, _reader) = s.open(Direction::Input, Format::Text, false).unwrap();
    writer
        .write(writer.key(), text("a", "done", true, true))
        .await
        .unwrap();
    assert_eq!(
        writer
            .write(writer.key(), text("a", "changed", true, true))
            .await,
        Err(Error::Uncommitted)
    );
}

#[tokio::test]
async fn finite_segments_byte_caps_shared_pools_and_aggregate_directions() {
    let s = streams(Limits {
        segments: 1,
        ..Limits::default()
    });
    let (mut writer, _reader) = s.open(Direction::Input, Format::Text, false).unwrap();
    writer
        .write(writer.key(), text("a", "one", true, true))
        .await
        .unwrap();
    assert_eq!(
        writer
            .write(writer.key(), text("b", "two", true, true))
            .await,
        Err(Error::Limit)
    );
    let s = streams(Limits {
        text_bytes: 8,
        ..Limits::default()
    });
    let (mut writer, _reader) = s.open(Direction::Input, Format::Text, false).unwrap();
    assert_eq!(
        writer
            .write(writer.key(), text("a", "1234567", true, true))
            .await,
        Err(Error::Limit)
    );
    let s = streams(Limits {
        output_bytes: 4,
        ..Limits::default()
    });
    let (mut a, _ar) = s.open(Direction::Output, pcm(), false).unwrap();
    let (mut b, _br) = s.open(Direction::Output, pcm(), false).unwrap();
    a.write(a.key(), Chunk::Audio(vec![0; 4])).await.unwrap();
    assert_eq!(
        b.write(b.key(), Chunk::Audio(vec![0; 2])).await,
        Err(Error::Limit)
    );
    let principal = Pool::new(4).unwrap();
    let a = Streams::new(
        Limits::default(),
        principal.clone(),
        Pool::process(),
        CancellationToken::new(),
    )
    .unwrap();
    let b = Streams::new(
        Limits::default(),
        principal.clone(),
        Pool::process(),
        CancellationToken::new(),
    )
    .unwrap();
    let (mut writer, _reader) = a.open(Direction::Input, pcm(), false).unwrap();
    writer
        .write(writer.key(), Chunk::Audio(vec![0; 4]))
        .await
        .unwrap();
    let (mut writer, _reader) = b.open(Direction::Input, pcm(), false).unwrap();
    assert_eq!(
        writer.write(writer.key(), Chunk::Audio(vec![0; 2])).await,
        Err(Error::Limit)
    );
    assert_eq!(principal.retained(), 4);
}

#[tokio::test]
async fn sample_duration_is_exact_across_tiny_chunks_and_explicit_failure_is_terminal() {
    let s = streams(Limits {
        input_duration: Duration::from_secs(1),
        ..Limits::default()
    });
    let (mut writer, mut reader) = s.open(Direction::Input, pcm(), false).unwrap();
    for _ in 0..24000 {
        writer
            .write(writer.key(), Chunk::Audio(vec![0; 2]))
            .await
            .unwrap();
        drop(reader.read().await.unwrap().unwrap());
    }
    assert_eq!(
        writer.write(writer.key(), Chunk::Audio(vec![0; 2])).await,
        Err(Error::Limit)
    );
    let s = streams(Limits::default());
    let (mut writer, mut reader) = s.open(Direction::Input, pcm(), false).unwrap();
    writer.fail(Error::Provider).await.unwrap();
    assert!(matches!(reader.read().await, Err(Error::Provider)));
    assert!(matches!(reader.read().await, Err(Error::Provider)));
}

#[tokio::test]
async fn blocked_required_writer_wakes_on_consumer_drop_and_joined_success_does_not_cancel() {
    let s = streams(Limits {
        edge_chunks: 1,
        ..Limits::default()
    });
    let (mut writer, reader) = s.open(Direction::Input, pcm(), false).unwrap();
    writer
        .write(writer.key(), Chunk::Audio(vec![0; 2]))
        .await
        .unwrap();
    let task =
        tokio::spawn(async move { writer.write(writer.key(), Chunk::Audio(vec![0; 2])).await });
    tokio::task::yield_now().await;
    drop(reader);
    assert_eq!(task.await.unwrap(), Err(Error::ConsumerDropped));
    let cancel = CancellationToken::new();
    let owner = Producer::spawn(cancel.clone(), async { Ok(()) });
    joined(owner).await.unwrap();
    assert!(!cancel.is_cancelled());
}

#[tokio::test]
async fn inflight_packets_hold_byte_and_chunk_credit_and_cancel_wakes_both_sides() {
    let s = streams(Limits {
        edge_bytes: 4,
        chunk_bytes: 4,
        edge_chunks: 1,
        ..Limits::default()
    });
    let (mut writer, mut reader) = s.open(Direction::Input, pcm(), false).unwrap();
    writer
        .write(writer.key(), Chunk::Audio(vec![0; 4]))
        .await
        .unwrap();
    let packet = reader.read().await.unwrap().unwrap();
    assert_eq!(s.retained_bytes(), 4);
    let mut pending = Box::pin(writer.write(writer.key(), Chunk::Audio(vec![0; 4])));
    assert!(futures_util::poll!(pending.as_mut()).is_pending());
    assert_eq!(s.retained_bytes(), 8); // queued/in-flight send copy is charged too.
    s.cancellation().cancel();
    assert_eq!(pending.await, Err(Error::Cancelled));
    assert!(matches!(reader.read().await, Err(Error::Cancelled)));
    drop(packet);
    drop(writer);
    drop(reader);
    assert_eq!(s.retained_bytes(), 0);
}

#[tokio::test]
async fn required_tee_backpressure_and_detachable_drop() {
    let s = streams(Limits {
        edge_chunks: 1,
        ..Limits::default()
    });
    let (mut input, source) = s.open(Direction::Input, pcm(), false).unwrap();
    let (a, mut required) = s.open(Direction::Output, pcm(), false).unwrap();
    let (b, detached) = s.open(Direction::Output, pcm(), true).unwrap();
    drop(detached);
    let owner = tee(source, vec![a, b]).unwrap();
    input
        .write(input.key(), Chunk::Audio(vec![0, 0]))
        .await
        .unwrap();
    let first = required.read().await.unwrap().unwrap();
    input
        .write(input.key(), Chunk::Audio(vec![2, 0]))
        .await
        .unwrap();
    drop(first);
    assert_eq!(
        required.read().await.unwrap().unwrap().chunk(),
        &Chunk::Audio(vec![2, 0])
    );
    input.close(input.key()).await.unwrap();
    assert!(required.read().await.unwrap().is_none());
    joined(owner).await.unwrap();
    let s = streams(Limits::default());
    let (mut input, source) = s.open(Direction::Input, pcm(), false).unwrap();
    let (out, required) = s.open(Direction::Output, pcm(), false).unwrap();
    drop(required);
    let owner = tee(source, vec![out]).unwrap();
    input
        .write(input.key(), Chunk::Audio(vec![0, 0]))
        .await
        .unwrap();
    assert_eq!(joined(owner).await, Err(Error::ConsumerDropped));
    let s = streams(Limits::default());
    let (mut writer, detached) = s.open(Direction::Output, Format::Text, true).unwrap();
    writer
        .write(writer.key(), text("a", "preview", true, false))
        .await
        .unwrap();
    drop(detached);
    writer.close(writer.key()).await.unwrap();
    assert!(!s.cancellation().is_cancelled());
}

#[tokio::test(start_paused = true)]
async fn idle_and_queue_deadlines_are_finite() {
    let s = streams(Limits::default());
    let (_writer, mut reader) = s.open(Direction::Input, pcm(), false).unwrap();
    assert!(matches!(reader.read().await, Err(Error::Deadline)));
    let s = streams(Limits {
        edge_chunks: 1,
        ..Limits::default()
    });
    let (mut writer, _reader) = s.open(Direction::Input, pcm(), false).unwrap();
    writer
        .write(writer.key(), Chunk::Audio(vec![0, 0]))
        .await
        .unwrap();
    assert_eq!(
        writer.write(writer.key(), Chunk::Audio(vec![0, 0])).await,
        Err(Error::Deadline)
    );
}

#[tokio::test]
async fn synthetic_hour_notes_retention_is_bounded_and_collection_caps_fail_closed() {
    let s = streams(Limits::default());
    let (mut input, source) = s.open(Direction::Input, pcm(), false).unwrap();
    let (output, mut reader) = s.open(Direction::Output, pcm(), false).unwrap();
    let owner = tee(source, vec![output]).unwrap();
    let target = 172_800_000;
    let observer = s.clone();
    let consumer = tokio::spawn(async move {
        let mut bytes = 0;
        let mut peak = 0;
        while let Some(packet) = reader.read().await.unwrap() {
            bytes += packet.chunk().bytes();
            peak = peak.max(observer.retained_bytes());
            tokio::task::yield_now().await;
        }
        (bytes, peak)
    });
    let mut sent = 0;
    while sent < target {
        let bytes = (target - sent).min(65536);
        input
            .write(input.key(), Chunk::Audio(vec![0; bytes]))
            .await
            .unwrap();
        sent += bytes;
    }
    input.close(input.key()).await.unwrap();
    joined(owner).await.unwrap();
    let (received, peak) = consumer.await.unwrap();
    assert_eq!(received, target);
    assert!(peak <= 3 * 1024 * 1024, "peak {peak}");
    drop(input);
    assert_eq!(s.retained_bytes(), 0);
    let s = streams(Limits::default());
    let (mut writer, reader) = s.open(Direction::Input, Format::Text, false).unwrap();
    let collector = tokio::spawn(collect(reader, 16));
    writer
        .write(
            writer.key(),
            text("a", "this exceeds the final note cap", true, true),
        )
        .await
        .unwrap();
    writer.close(writer.key()).await.unwrap();
    assert!(matches!(collector.await.unwrap(), Err(Error::Limit)));
    assert!(s.cancellation().is_cancelled());
}

#[tokio::test]
async fn genuine_audio_cancel_joins_while_output_credit_is_checked_out() {
    let (_, server) = fixtures::start(fixtures::Mode::Media, Duration::from_secs(10)).await;
    let s = streams(Limits {
        edge_chunks: 1,
        ..Limits::default()
    });
    let (mut input, source) = s.open(Direction::Input, Format::Text, false).unwrap();
    let (output, mut reader) = s.open(Direction::Output, pcm(), false).unwrap();
    let owner = backend(&server.endpoint, false)
        .synthesize(source, output)
        .unwrap();
    input
        .write(input.key(), text("a", "first", true, true))
        .await
        .unwrap();
    let packet = reader.read().await.unwrap().unwrap();
    let baseline = s.retained_bytes();
    input
        .write(input.key(), text("b", "second", true, true))
        .await
        .unwrap();
    // Two retained text states cost 16 bytes; the additional PCM copy is blocked
    // behind the checked-out packet's item permit, and must remain charged.
    tokio::time::timeout(Duration::from_secs(2), async {
        while s.retained_bytes() < baseline + 18 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), owner.cancel_and_join())
            .await
            .unwrap(),
        Err(Error::Cancelled)
    );
    assert!(matches!(reader.read().await, Err(Error::Cancelled)));
    drop(packet);
    drop(input);
    drop(reader);
    assert_eq!(s.retained_bytes(), 0);
}

#[tokio::test]
async fn genuine_audio_stt_revisions_final_and_diarization() {
    for diarized in [false, true] {
        let (_, server) = fixtures::start(
            if diarized {
                fixtures::Mode::Diarized
            } else {
                fixtures::Mode::Media
            },
            Duration::from_secs(10),
        )
        .await;
        let s = streams(Limits::default());
        let (mut input, source) = s.open(Direction::Input, pcm(), false).unwrap();
        let (output, mut reader) = s.open(Direction::Output, Format::Text, false).unwrap();
        let owner = backend(&server.endpoint, diarized)
            .transcribe(source, output)
            .unwrap();
        input
            .write(input.key(), Chunk::Audio(vec![0; 4]))
            .await
            .unwrap();
        let first = reader.read().await.unwrap().unwrap();
        assert!(matches!(
            first.chunk(),
            Chunk::Text(Text {
                committed: false,
                ..
            })
        ));
        drop(first);
        input.close(input.key()).await.unwrap();
        if diarized {
            let snapshot = reader.read().await.unwrap().unwrap();
            assert!(matches!(
                snapshot.chunk(),
                Chunk::Text(Text {
                    committed: false,
                    ..
                })
            ));
        }
        let final_packet = reader.read().await.unwrap().unwrap();
        let Chunk::Text(final_text) = final_packet.chunk() else {
            panic!("text");
        };
        assert!(final_text.committed);
        assert_eq!(
            final_text.text,
            if diarized {
                "S1: hello\nS2: world"
            } else {
                "hello world"
            }
        );
        drop(final_packet);
        assert!(reader.read().await.unwrap().is_none());
        joined(owner).await.unwrap();
        drop(input);
        drop(reader);
        assert_eq!(s.retained_bytes(), 0);
    }
}

#[tokio::test]
async fn genuine_diarization_portion_finals_and_speaker_revisions_commit_only_at_end() {
    let (_, server) =
        fixtures::start(fixtures::Mode::DiarizedPortions, Duration::from_secs(10)).await;
    let s = streams(Limits::default());
    let (mut input, source) = s.open(Direction::Input, pcm(), false).unwrap();
    let (output, snapshots) = s.open(Direction::Output, Format::Text, false).unwrap();
    let (notes, note_reader) = s.open(Direction::Output, Format::Text, false).unwrap();
    let (observed, mut observer) = s.open(Direction::Output, Format::Text, false).unwrap();
    let owner = backend(&server.endpoint, true)
        .transcribe(source, output)
        .unwrap();
    let fanout = tee(snapshots, vec![notes, observed]).unwrap();
    let note = tokio::spawn(collect(note_reader, 256 * 1024));
    for expected in ["S1: hello", "S2: hello world", "S2: hello\nS3: world"] {
        let packet = observer.read().await.unwrap().unwrap();
        let Chunk::Text(text) = packet.chunk() else {
            panic!("text");
        };
        assert!(!text.committed);
        assert_eq!(text.text, expected);
    }
    assert!(!s.cancellation().is_cancelled());
    input.close(input.key()).await.unwrap();
    let mut committed = 0;
    while let Some(packet) = observer.read().await.unwrap() {
        let Chunk::Text(text) = packet.chunk() else {
            panic!("text");
        };
        assert_eq!(text.text, "S2: hello\nS3: world again");
        committed += usize::from(text.committed);
    }
    assert_eq!(committed, 1);
    joined(owner).await.unwrap();
    joined(fanout).await.unwrap();
    let note = note.await.unwrap().unwrap();
    assert_eq!(
        note.segments().collect::<Vec<_>>(),
        ["S2: hello\nS3: world again"]
    );
}

#[tokio::test]
async fn genuine_diarization_portions_never_commit_on_early_eof_error_or_drop() {
    for mode in [
        fixtures::Mode::DiarizedPortionsEarlyEof,
        fixtures::Mode::DiarizedPortionsFail,
    ] {
        let (_, server) = fixtures::start(mode, Duration::from_secs(10)).await;
        let s = streams(Limits::default());
        let (mut input, source) = s.open(Direction::Input, pcm(), false).unwrap();
        let (output, mut reader) = s.open(Direction::Output, Format::Text, false).unwrap();
        let owner = backend(&server.endpoint, true)
            .transcribe(source, output)
            .unwrap();
        if matches!(mode, fixtures::Mode::DiarizedPortionsFail) {
            input.close(input.key()).await.unwrap();
        }
        while let Ok(Some(packet)) = reader.read().await {
            assert!(matches!(
                packet.chunk(),
                Chunk::Text(Text {
                    committed: false,
                    ..
                })
            ));
        }
        assert_eq!(
            joined(owner).await,
            Err(
                if matches!(mode, fixtures::Mode::DiarizedPortionsEarlyEof) {
                    Error::BrokenProducer
                } else {
                    Error::Provider
                }
            )
        );
        assert!(reader.read().await.is_err());
    }
    let (_, server) =
        fixtures::start(fixtures::Mode::DiarizedPortions, Duration::from_secs(10)).await;
    let s = streams(Limits::default());
    let (input, source) = s.open(Direction::Input, pcm(), false).unwrap();
    let (output, mut reader) = s.open(Direction::Output, Format::Text, false).unwrap();
    let owner = backend(&server.endpoint, true)
        .transcribe(source, output)
        .unwrap();
    for _ in 0..3 {
        assert!(matches!(
            reader.read().await.unwrap().unwrap().chunk(),
            Chunk::Text(Text {
                committed: false,
                ..
            })
        ));
    }
    drop(input);
    assert_eq!(joined(owner).await, Err(Error::BrokenProducer));
    assert!(reader.read().await.is_err());
}

#[tokio::test]
async fn synthetic_hour_pcm_through_genuine_audio_adapter_to_host_owned_note() {
    let (_, server) = fixtures::start(fixtures::Mode::Media, Duration::from_secs(60)).await;
    let s = streams(Limits::default());
    let (mut input, source) = s.open(Direction::Input, pcm(), false).unwrap();
    let (output, reader) = s.open(Direction::Output, Format::Text, false).unwrap();
    let owner = backend(&server.endpoint, false)
        .transcribe(source, output)
        .unwrap();
    let collector = tokio::spawn(collect(reader, 256 * 1024));
    let target = 172_800_000;
    let mut sent = 0;
    let mut peak = 0;
    while sent < target {
        let bytes = (target - sent).min(65536);
        input
            .write(input.key(), Chunk::Audio(vec![0; bytes]))
            .await
            .unwrap();
        sent += bytes;
        peak = peak.max(s.retained_bytes());
    }
    input.close(input.key()).await.unwrap();
    joined(owner).await.unwrap();
    let note = collector.await.unwrap().unwrap();
    assert_eq!(note.segments().collect::<Vec<_>>(), ["hello world"]);
    assert!(peak <= 8 * 1024 * 1024, "charged peak {peak}");
    assert_eq!(sent, target);
    drop(note);
    drop(input);
    assert_eq!(s.retained_bytes(), 0);
}

#[tokio::test]
async fn synthesis_model_name_reaches_wire_and_omission_preserves_default() {
    for model_name in [Some("kyutai-tts-1.6b-en-fr"), None] {
        let (_, server) = fixtures::start(fixtures::Mode::Media, Duration::from_secs(10)).await;
        let speech = GoogleSpeech::new(
            None,
            Some(ResolvedSpeechModel {
                provider_key: "speech".into(),
                model_key: "catalog-reader".into(),
                endpoint: server.endpoint.clone(),
                capabilities: proto::SpeechCapabilities {
                    unary: true,
                    streaming: true,
                },
                settings: SpeechSettings::Synthesis(proto::SpeechSynthesisOptions {
                    voice: "expresso-happy".into(),
                    language_code: "en-US".into(),
                    sample_rate_hz: 24000,
                    channels: 1,
                    speaking_rate: Some(1.0),
                    model_name: model_name.map(str::to_owned),
                    ..Default::default()
                }),
            }),
        )
        .unwrap();
        let s = streams(Limits::default());
        let (mut input, source) = s.open(Direction::Input, Format::Text, false).unwrap();
        let (output, mut reader) = s.open(Direction::Output, pcm(), false).unwrap();
        let owner = speech
            .synthesize_selected(
                source,
                output,
                super::speech::SynthesisSelection {
                    model: "catalog-reader".into(),
                    voice: "expresso-happy".into(),
                    language: "en-US".into(),
                    rate: 1.0,
                },
            )
            .unwrap();
        input
            .write(input.key(), text("a", "hello", true, true))
            .await
            .unwrap();
        input.close(input.key()).await.unwrap();
        while reader.read().await.unwrap().is_some() {}
        joined(owner).await.unwrap();
        let capture = server.synthesis_capture.lock().unwrap();
        assert_eq!(capture.len(), 1);
        let voice = capture[0].voice.as_ref().unwrap();
        assert_eq!(voice.model_name, model_name.unwrap_or_default());
        assert_eq!(voice.name, "expresso-happy");
        assert_eq!(voice.language_code, "en-US");
        let audio = capture[0].streaming_audio_config.as_ref().unwrap();
        assert_eq!(
            audio.audio_encoding,
            waltr_audio::tts::AudioEncoding::Pcm as i32
        );
        assert_eq!(audio.sample_rate_hertz, 24000);
        assert_eq!(audio.speaking_rate, 1.0);
    }
}

#[tokio::test]
async fn genuine_audio_tts_only_committed_and_audio_before_eof() {
    let (_, server) = fixtures::start(fixtures::Mode::Media, Duration::from_secs(10)).await;
    let s = streams(Limits::default());
    let (mut input, source) = s.open(Direction::Input, Format::Text, false).unwrap();
    let (output, mut reader) = s.open(Direction::Output, pcm(), false).unwrap();
    let owner = backend(&server.endpoint, false)
        .synthesize(source, output)
        .unwrap();
    input
        .write(input.key(), text("a", "preview", true, false))
        .await
        .unwrap();
    let mut pending = Box::pin(reader.read());
    assert!(futures_util::poll!(pending.as_mut()).is_pending());
    drop(pending);
    input
        .write(input.key(), text("a", "committed", true, true))
        .await
        .unwrap();
    assert_eq!(
        reader.read().await.unwrap().unwrap().chunk(),
        &Chunk::Audio(vec![1, 0])
    );
    input.close(input.key()).await.unwrap();
    assert!(reader.read().await.unwrap().is_none());
    joined(owner).await.unwrap();
}

#[tokio::test]
async fn genuine_audio_premature_eof_failure_odd_pcm_and_cancellation_join() {
    for mode in [fixtures::Mode::EarlyEof, fixtures::Mode::Fail] {
        let (_, server) = fixtures::start(mode, Duration::from_secs(10)).await;
        let s = streams(Limits::default());
        let (_input, source) = s.open(Direction::Input, pcm(), false).unwrap();
        let (output, _reader) = s.open(Direction::Output, Format::Text, false).unwrap();
        let owner = backend(&server.endpoint, false)
            .transcribe(source, output)
            .unwrap();
        assert_eq!(
            joined(owner).await,
            Err(if matches!(mode, fixtures::Mode::EarlyEof) {
                Error::BrokenProducer
            } else {
                Error::Provider
            })
        );
    }
    for incomplete in [false, true] {
        let (_, server) = fixtures::start(
            if incomplete {
                fixtures::Mode::IncompletePcm
            } else {
                fixtures::Mode::OddPcm
            },
            Duration::from_secs(10),
        )
        .await;
        let s = streams(Limits::default());
        let (mut input, source) = s.open(Direction::Input, Format::Text, false).unwrap();
        let (output, mut reader) = s.open(Direction::Output, pcm(), false).unwrap();
        let owner = backend(&server.endpoint, false)
            .synthesize(source, output)
            .unwrap();
        input
            .write(input.key(), text("a", "hello", true, true))
            .await
            .unwrap();
        input.close(input.key()).await.unwrap();
        if incomplete {
            assert!(reader.read().await.is_err());
            assert_eq!(joined(owner).await, Err(Error::Invalid));
        } else {
            assert_eq!(
                reader.read().await.unwrap().unwrap().chunk(),
                &Chunk::Audio(vec![1, 0])
            );
            assert!(reader.read().await.unwrap().is_none());
            joined(owner).await.unwrap();
        }
    }
    let (_, server) = fixtures::start(fixtures::Mode::Stall, Duration::from_secs(10)).await;
    let s = streams(Limits::default());
    let (_input, source) = s.open(Direction::Input, pcm(), false).unwrap();
    let (output, _reader) = s.open(Direction::Output, Format::Text, false).unwrap();
    let owner = backend(&server.endpoint, false)
        .transcribe(source, output)
        .unwrap();
    owner.cancel();
    assert_eq!(joined(owner).await, Err(Error::Cancelled));
    let s = streams(Limits {
        idle: Duration::from_millis(100),
        ..Limits::default()
    });
    let (mut input, source) = s.open(Direction::Input, pcm(), false).unwrap();
    input.close(input.key()).await.unwrap();
    let (output, _reader) = s.open(Direction::Output, Format::Text, false).unwrap();
    let owner = backend(&server.endpoint, false)
        .transcribe(source, output)
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), owner.join())
            .await
            .unwrap(),
        Err(Error::Deadline)
    );
}
