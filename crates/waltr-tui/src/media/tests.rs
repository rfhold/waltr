use super::*;
mod rpc;
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
use waltr_client::{
    ClientRoutineTriggerKind, RoutineBudgets, RoutineContract, RoutineDirection, RoutineEntrypoint,
    RoutineMedia, RoutinePort, RoutineRevision, RoutineStartContext,
};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "waltr-tui-media-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self, name: &str) -> String {
        self.0.join(name).to_str().unwrap().into()
    }
    fn clean(&self) -> bool {
        fs::read_dir(&self.0).unwrap().all(|p| {
            !p.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".waltr-tui-")
        })
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        for entry in fs::read_dir(&self.0).unwrap() {
            fs::remove_file(entry.unwrap().path()).unwrap();
        }
        fs::remove_dir(&self.0).unwrap();
    }
}
fn limits() -> wire::ServerLimits {
    wire::ServerLimits {
        chunk_bytes: 65536,
        input_batch_bytes: 131072,
        input_batch_events: 32,
        edge_bytes: 131072,
        edge_chunks: 32,
        input_bytes: 256 * 1024 * 1024,
        output_bytes: 256 * 1024 * 1024,
        input_duration_ms: 3_900_000,
        total_duration_ms: 5_400_000,
        queue_wait_ms: 30000,
        idle_ms: 30000,
        streams_per_direction: 32,
        text_segments: 64,
        retained_text_bytes: 1024 * 1024,
        attachment_lease_ms: 30000,
        attachment_attach_timeout_ms: 10000,
        attachment_absolute_lifetime_ms: 5_400_000,
        file_bytes: 64 * 1024 * 1024,
        image_bytes: 20 * 1024 * 1024,
        image_dimension: 8192,
        image_pixels: 32_000_000,
        helper_attachment_bytes: 8 * 1024 * 1024,
        helper_source_bytes: 1024 * 1024,
        helper_encoded_bytes: 8 * 1024 * 1024,
        helper_attachments: 8,
        binary_output_lease_ms: 900000,
        text_result_bytes: 1024 * 1024,
        text_results_encoded_bytes: 8 * 1024 * 1024,
        text_results: 32,
        page_size: 100,
        structured_value_bytes: 65536,
        definition_bytes: 262144,
        definitions_per_snapshot: 128,
        definition_snapshot_bytes: 8 * 1024 * 1024,
        operation_projection_bytes: 4096,
        blob_invocation_bytes: 256 * 1024 * 1024,
        blob_principal_bytes: 512 * 1024 * 1024,
        blob_objects: 64,
        blob_readers: 4,
        ordinary_deadline_ms: 30000,
    }
}
fn definition(media: RoutineMedia) -> ClientManualRoutineDefinition {
    let schema = RoutineContract::from_wire(&wire::ValueContract {
        schema_version: "waltr.value/v1".into(),
        schema_json: br#"{"type":"object","properties":{},"required":[]}"#.to_vec(),
    })
    .unwrap();
    ClientManualRoutineDefinition {
        routine_name: "finite".into(),
        description: "test".into(),
        routine_revision: "opaque-9007199254740993".into(),
        api_version: "waltr.dev/v1alpha1".into(),
        engine: "javascript".into(),
        entrypoints: vec![RoutineEntrypoint {
            name: "live".into(),
            trigger: ClientRoutineTriggerKind::Interactive,
            selector: None,
            start_contexts: vec![RoutineStartContext::Global],
        }],
        parameters: schema.clone(),
        result: schema,
        ports: vec![RoutinePort {
            name: "input".into(),
            direction: RoutineDirection::Input,
            media,
            required: true,
            max_bytes: RoutineRevision(64 * 1024 * 1024),
            formats: match media {
                RoutineMedia::File => vec!["application/octet-stream".into()],
                RoutineMedia::Image => {
                    vec!["image/png".into(), "image/jpeg".into(), "image/webp".into()]
                }
                RoutineMedia::Audio => vec!["pcm16le".into()],
                RoutineMedia::Text => vec!["text".into()],
            },
        }],
        capabilities: vec![],
        budgets: RoutineBudgets {
            value_bytes: 65536,
            result_bytes: 65536,
            host_calls: 32,
            in_flight: 4,
            deadline_ms: RoutineRevision(30000),
        },
    }
}
fn selection(path: String, mime: &str) -> LocalSelection {
    LocalSelection {
        port: "input".into(),
        path: Some(path),
        text: None,
        mime: Some(mime.into()),
        pcm24k_mono: false,
    }
}

#[test]
fn local_paths_are_not_wire_authority_and_metadata_precedes_admission() {
    let dir = Scratch::new();
    let path = dir.path("private.txt");
    fs::write(&path, b"secret").unwrap();
    let d = definition(RoutineMedia::File);
    let limits = InteractionLimits::for_definition(d.clone(), limits()).unwrap();
    let plan = LocalPlan {
        inputs: vec![selection(path.clone(), "application/octet-stream")],
        ..Default::default()
    };
    let (open, _) = prepare(&d, "live", None, "{}".into(), &plan, &limits).unwrap();
    let debug = format!("{open:?}");
    assert!(!debug.contains(&path));
    assert!(!debug.contains("secret"));
    let Some(wire::input_declaration::Format::Binary(binary)) = &open.inputs[0].format else {
        panic!()
    };
    assert_eq!(binary.display_name, "private.txt");
    assert_eq!(binary.declared_bytes, Some(6));
    assert!(matches!(
        binary.purpose,
        Some(wire::binary_input_declaration::Purpose::File(_))
    ));
    assert!(
        prepare(
            &d,
            "live",
            None,
            "{}".into(),
            &LocalPlan::default(),
            &limits
        )
        .is_err()
    );
}

#[test]
fn local_upload_reads_are_bounded_and_detect_changed_length() {
    let dir = Scratch::new();
    let path = dir.path("input");
    fs::write(&path, vec![42; CHUNK + 7]).unwrap();
    let d = definition(RoutineMedia::File);
    let limits = InteractionLimits::for_definition(d.clone(), limits()).unwrap();
    let mut source = LocalInput::open(
        &selection(path.clone(), "application/octet-stream"),
        &d,
        &limits,
    )
    .unwrap();
    let mut buffer = vec![0; CHUNK];
    assert_eq!(source.read(&mut buffer).unwrap(), CHUNK);
    assert_eq!(source.read(&mut buffer).unwrap(), 7);
    assert_eq!(source.read(&mut buffer).unwrap(), 0);
    let mut source = LocalInput::open(
        &selection(path.clone(), "application/octet-stream"),
        &d,
        &limits,
    )
    .unwrap();
    fs::write(&path, b"short").unwrap();
    assert!(source.read(&mut buffer).is_err());
    assert!(source.read(&mut vec![0; CHUNK + 1]).is_err());
}

#[test]
fn wrong_port_oversize_and_untrusted_image_mime_are_rejected() {
    let dir = Scratch::new();
    let path = dir.path("input");
    fs::write(&path, b"not an image").unwrap();
    let d = definition(RoutineMedia::Image);
    let limits = InteractionLimits::for_definition(d.clone(), limits()).unwrap();
    assert!(LocalInput::open(&selection(path.clone(), "image/gif"), &d, &limits).is_err());
    let mut s = selection(path.clone(), "image/png");
    s.port = "wrong".into();
    assert!(LocalInput::open(&s, &d, &limits).is_err());
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(20 * 1024 * 1024 + 1)
        .unwrap();
    assert!(LocalInput::open(&selection(path, "image/png"), &d, &limits).is_err());
}

#[test]
fn duplicate_and_aggregate_input_budgets_fail_before_admission() {
    let dir = Scratch::new();
    let path = dir.path("input");
    fs::write(&path, b"123456").unwrap();
    let mut d = definition(RoutineMedia::File);
    let mut second = d.ports[0].clone();
    second.name = "second".into();
    d.ports.push(second);
    let mut limits = InteractionLimits::for_definition(d.clone(), limits()).unwrap();
    let first = selection(path, "application/octet-stream");
    let mut second = first.clone();
    second.port = "second".into();
    let duplicate = LocalPlan {
        inputs: vec![first.clone(), first.clone()],
        ..Default::default()
    };
    assert!(
        prepare(&d, "live", None, "{}".into(), &duplicate, &limits)
            .err()
            .unwrap()
            .contains("Duplicate")
    );
    limits.server.input_bytes = 10;
    let plan = LocalPlan {
        inputs: vec![first, second],
        ..Default::default()
    };
    assert!(
        prepare(&d, "live", None, "{}".into(), &plan, &limits)
            .err()
            .unwrap()
            .contains("total input budget")
    );
}

#[test]
fn accepted_image_metadata_is_not_claimed_as_content_validation() {
    let dir = Scratch::new();
    let path = dir.path("input");
    fs::write(&path, b"invalid bytes validated by server").unwrap();
    let d = definition(RoutineMedia::Image);
    let limits = InteractionLimits::for_definition(d.clone(), limits()).unwrap();
    for (mime, format) in [
        ("image/png", wire::ImageFormat::Png),
        ("image/jpeg", wire::ImageFormat::Jpeg),
        ("image/webp", wire::ImageFormat::Webp),
    ] {
        let source = LocalInput::open(&selection(path.clone(), mime), &d, &limits).unwrap();
        let Some(wire::input_declaration::Format::Binary(binary)) = source.declaration.format
        else {
            panic!()
        };
        assert_eq!(
            binary.purpose,
            Some(wire::binary_input_declaration::Purpose::Image(
                format as i32
            ))
        );
    }
}

#[test]
fn raw_pcm_requires_explicit_24k_mono_selection() {
    let dir = Scratch::new();
    let path = dir.path("input");
    fs::write(&path, [0, 0, 1, 0]).unwrap();
    let d = definition(RoutineMedia::Audio);
    let limits = InteractionLimits::for_definition(d.clone(), limits()).unwrap();
    let mut s = selection(path, "audio/wav");
    assert!(LocalInput::open(&s, &d, &limits).is_err());
    s.mime = None;
    s.pcm24k_mono = true;
    let input = LocalInput::open(&s, &d, &limits).unwrap();
    assert!(matches!(
        input.declaration.format,
        Some(wire::input_declaration::Format::Pcm16le(wire::Pcm16Le {
            sample_rate: 24000,
            channels: 1
        }))
    ));
}

#[test]
fn save_is_no_overwrite_and_cleanup_never_removes_chosen_replacement() {
    let dir = Scratch::new();
    let path = dir.path("save");
    {
        let mut save = Destination::new(&path).unwrap();
        save.write(b"owned").unwrap();
        save.publish().unwrap();
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"replacement").unwrap();
    }
    assert_eq!(fs::read(&path).unwrap(), b"replacement");
    assert!(dir.clean());
    {
        let mut save = Destination::new(&path).unwrap();
        save.write(b"new").unwrap();
        assert!(save.publish().is_err());
    }
    assert_eq!(fs::read(&path).unwrap(), b"replacement");
    assert!(dir.clean());
}

#[tokio::test]
async fn verified_save_requires_exact_digest_length_and_clean_source_end() {
    let dir = Scratch::new();
    let path = dir.path("save");
    let bytes = vec![9; CHUNK + 1];
    let hash = Sha256::digest(&bytes);
    let mut source = stream_chunks(&bytes);
    let mut save = copy_verified(&mut source, &path, bytes.len() as u64, &hash, || Ok(()))
        .await
        .unwrap();
    assert!(!std::path::Path::new(&path).exists());
    save.publish().unwrap();
    drop(save);
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert!(dir.clean());
    for (count, digest) in [
        (bytes.len() as u64 + 1, hash.to_vec()),
        (bytes.len() as u64, vec![0; 32]),
        (bytes.len() as u64 - 1, hash.to_vec()),
    ] {
        let mut source = stream_chunks(&bytes);
        assert!(
            copy_verified(&mut source, &dir.path("bad"), count, &digest, || Ok(()))
                .await
                .is_err()
        );
        assert!(!std::path::Path::new(&dir.path("bad")).exists());
        assert!(dir.clean());
    }
}

#[tokio::test]
async fn blocked_download_absolute_expiry_cleans_only_owned_partial() {
    let dir = Scratch::new();
    let path = dir.path("save");
    let initial = futures_util::stream::iter([Ok(vec![1, 2, 3])]);
    let mut source = initial.chain(futures_util::stream::pending());
    assert!(
        timeout_at(
            Instant::now() + Duration::from_millis(5),
            copy_verified(&mut source, &path, 4, &[0; 32], || Ok(()))
        )
        .await
        .is_err()
    );
    assert!(dir.clean());
    assert!(!std::path::Path::new(&path).exists());
}

#[tokio::test]
async fn foreign_generation_cannot_publish_or_continue_local_save() {
    let dir = Scratch::new();
    let path = dir.path("save");
    let credentials = NativeCredentialProvider::new();
    let generation = credentials.credential_lease().generation();
    let _lease = credentials.clear_bearer_token();
    let mut source = futures_util::stream::iter([Ok(vec![1])]);
    assert!(
        copy_verified(&mut source, &path, 1, &[0; 32], || current(
            &credentials,
            generation
        ))
        .await
        .is_err()
    );
    assert!(dir.clean());
    assert!(!std::path::Path::new(&path).exists());
}

#[test]
fn preview_and_input_chunks_preserve_utf8_and_exact_whitespace() {
    let text = format!("  {}  ", "é".repeat(PREVIEW));
    let (preview, truncated) = bounded_preview(&text);
    assert!(truncated);
    assert!(preview.len() <= PREVIEW);
    assert!(preview.starts_with("  "));
    let chunks = limits_text_chunk(&text, 1023);
    assert_eq!(chunks.concat(), text);
    assert!(chunks.iter().all(|c| c.len() <= 1023));
}

#[test]
fn debug_omits_local_selections_and_media_previews() {
    let request = MediaRequest::Text {
        id: 1,
        invocation: "invocation".into(),
        descriptors: vec![],
        selected: "result".into(),
        destination: Some("private-destination".into()),
    };
    let event = MediaEvent {
        id: 1,
        status: "status".into(),
        preview: "private-preview".into(),
        truncated: false,
        done: true,
    };
    let debug = format!("{request:?} {event:?}");
    assert!(!debug.contains("private-destination"));
    assert!(!debug.contains("private-preview"));
}

#[test]
fn input_failure_cannot_hide_unverified_reader_cleanup() {
    assert_eq!(
        prefer_cleanup::<()>(
            Err("input failed".into()),
            Err("Reader close is unverified".into())
        ),
        Err("Reader close is unverified".into())
    );
}

fn stream_chunks(
    bytes: &[u8],
) -> impl futures_util::Stream<Item = Result<Vec<u8>, waltr_client::ClientError>> + Unpin {
    let mut chunks = Vec::new();
    for chunk in bytes.chunks(CHUNK) {
        chunks.push(Ok(chunk.to_vec()));
    }
    futures_util::stream::iter(chunks)
}

#[test]
fn durable_text_reader_uses_separate_bounded_result_selection() {
    let transfers = NativeGrpcTransport::new("http://127.0.0.1:1").media_transfers();
    let mut result = RoutineTextResult {
        id: "result".into(),
        name: "business-text".into(),
        sha256: "0".repeat(64),
        utf8_bytes: RoutineRevision(1024 * 1024),
        encoded_charge: RoutineRevision(1024 * 1024),
        helper_id: None,
    };
    assert!(
        transfers
            .text_reader("invocation".into(), &[result.clone()], "result")
            .is_ok()
    );
    assert!(
        transfers
            .text_reader("invocation".into(), &[result.clone()], "foreign")
            .is_err()
    );
    result.utf8_bytes = RoutineRevision(1024 * 1024 + 1);
    assert!(
        transfers
            .text_reader("invocation".into(), &[result.clone()], "result")
            .is_err()
    );
    result.utf8_bytes = RoutineRevision(1);
    result.encoded_charge = RoutineRevision(8 * 1024 * 1024 + 1);
    assert!(
        transfers
            .text_reader("invocation".into(), &[result], "result")
            .is_err()
    );
}

#[tokio::test]
async fn canceled_checkout_never_starts_admission_or_retries() {
    let dir = Scratch::new();
    let path = dir.path("input");
    fs::write(&path, b"finite").unwrap();
    let credentials = NativeCredentialProvider::new();
    let generation = credentials.credential_lease().generation();
    let transport =
        NativeGrpcTransport::with_credential_provider("http://127.0.0.1:1", credentials.clone());
    let (_cancel, canceled) = watch::channel(true);
    let request = MediaRequest::Interaction {
        id: 1,
        definition: Box::new(definition(RoutineMedia::File)),
        limits: Box::new(limits().into()),
        entrypoint: "live".into(),
        source: None,
        parameters: "{}".into(),
        plan: Box::new(LocalPlan {
            inputs: vec![selection(path, "application/octet-stream")],
            ..Default::default()
        }),
    };
    let error = execute(request, transport, credentials, generation, canceled)
        .await
        .unwrap_err();
    assert!(error.contains("Canceled before admission"));
}
