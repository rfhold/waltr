#![cfg(feature = "grpc")]

//! Encoding and generated-surface contracts only; these tests do not exercise
//! runtime authorization, quotas, redaction middleware or lifecycle validation.
use prost::Message;
use waltr::grpc::waltr::routine::v2::*;

const SCHEMA: &str = include_str!("../../../proto/waltr/routine/v2/routine.proto");
const SERVICES: &str = include_str!("../src/grpc/waltr/routine/v2/waltr.routine.v2.tonic.rs");

fn roundtrip<T: Message + Default + PartialEq + std::fmt::Debug>(value: T) -> T {
    let decoded = T::decode(value.encode_to_vec().as_slice()).unwrap();
    assert_eq!(value, decoded);
    decoded
}

#[test]
fn json_octets_and_opaque_ids_are_not_double_projections() {
    let json =
        br#"{"id":"18446744073709551615","safe":9007199254740991,"empty":null,"unicode":"\u2603"}"#;
    let value = roundtrip(StructuredValue {
        json: json.to_vec(),
    });
    assert_eq!(value.json, json);
    let contract = roundtrip(ValueContract {
        schema_version: "waltr.value/v1".into(),
        schema_json: br#"{"type":"nullable","schema":{"type":"string","maxLength":128}}"#.to_vec(),
    });
    assert_eq!(contract.schema_version, "waltr.value/v1");
    assert!(!SCHEMA.contains("google.protobuf.Struct"));
}

#[test]
fn cursors_offsets_and_revisions_roundtrip_losslessly() {
    roundtrip(ListInvocationsRequest {
        after_cursor: u64::MAX,
        limit: 100,
        source_session: None,
    });
    roundtrip(ReadTextResultRequest {
        invocation_id: "opaque-invocation".into(),
        result_id: "opaque-result".into(),
        offset: 9_007_199_254_740_993,
        max_bytes: 65536,
    });
    roundtrip(Invocation {
        cursor: i64::MAX as u64,
        revision: "0000-revision-preserve-leading-zeroes".into(),
        admitted_ms: i64::MAX,
        ..Default::default()
    });
    roundtrip(WatchInvocationsRequest {
        after_cursor: "opaque-change-cursor-not-a-number".into(),
        limit: 100,
        source_session: None,
    });
}

#[test]
fn defaults_and_unknown_values_are_not_semantically_valid_requests() {
    // Prost correctly decodes absent oneofs/default scalars. The future adapters
    // MUST reject them; protobuf decoding alone is not protocol validation.
    assert!(InteractRequest::decode(&[][..]).unwrap().frame.is_none());
    assert!(InputEvent::default().event.is_none());
    assert!(OutputEvent::default().event.is_none());
    assert!(StreamOpen::default().format.is_none());
    assert!(StreamChunk::default().payload.is_none());
    assert!(BeginBlobUploadRequest::default().purpose.is_none());
    assert!(OpenAttachmentResponse::default().admission.is_none());
    assert_eq!(
        AttachmentCloseMode::default(),
        AttachmentCloseMode::Unspecified
    );
    assert_eq!(TextMutation::default(), TextMutation::Unspecified);
    assert!(TextMutation::try_from(123456).is_err());
    assert!(SCHEMA.contains("Zero/absent required limits and unknown enum/oneof values fail"));
}

#[test]
fn media_is_typed_and_chunks_are_separate_from_aggregate_limits() {
    roundtrip(StreamOpen {
        port: "microphone".into(),
        stream_id: "input-1".into(),
        format: Some(stream_open::Format::Pcm16le(Pcm16Le {
            sample_rate: 24000,
            channels: 1,
        })),
    });
    roundtrip(StreamChunk {
        stream_id: "input-1".into(),
        sequence: u64::MAX,
        payload: Some(stream_chunk::Payload::Audio(vec![0xff; 65536])),
    });
    roundtrip(StreamChunk {
        stream_id: "text-1".into(),
        sequence: 1,
        payload: Some(stream_chunk::Payload::Text(TextRevision {
            segment_id: "stable-segment".into(),
            mutation: TextMutation::Replace.into(),
            text: "committed replacement".into(),
            committed: true,
        })),
    });
    let limits = roundtrip(ServerLimits {
        chunk_bytes: 65536,
        input_bytes: 256 * 1024 * 1024,
        input_duration_ms: 65 * 60 * 1000,
        total_duration_ms: 90 * 60 * 1000,
        text_result_bytes: 1024 * 1024,
        text_results_encoded_bytes: 8 * 1024 * 1024,
        ..Default::default()
    });
    assert!(u64::from(limits.chunk_bytes) < limits.input_bytes);
    let port = roundtrip(Port {
        max_bytes: 256 * 1024 * 1024,
        ..Default::default()
    });
    assert_ne!(port.max_bytes, u64::from(limits.chunk_bytes));
    // Protobuf permits larger payloads; the documented 64 KiB ceiling belongs to
    // adapter validation, not an imaginary bound enforced by generated types.
    assert!(SCHEMA.contains("<=65536 bytes, complete PCM frames"));
}

#[test]
fn finish_cancel_and_owner_release_have_distinct_wire_identity() {
    let finish = roundtrip(InputEvent {
        event: Some(input_event::Event::Finish(InputFinish {})),
    });
    let cancel = roundtrip(InputEvent {
        event: Some(input_event::Event::Cancel(InteractionCancel {})),
    });
    assert_ne!(finish.encode_to_vec(), cancel.encode_to_vec());
    assert_ne!(
        AttachmentCloseMode::FinishInput as i32,
        AttachmentCloseMode::ReleaseOwner as i32
    );
    assert_ne!(
        AttachmentCloseMode::ReleaseOwner as i32,
        AttachmentCloseMode::Cancel as i32
    );
    assert!(SCHEMA.contains("Transport EOF without terminal is failure"));
    assert!(SCHEMA.contains("consumer additionally verifies clean EOF"));
    assert!(SCHEMA.contains("Control, renewal and cancellation MUST bypass saturated data queues"));
}

#[test]
fn blob_metadata_and_durable_text_do_not_embed_payload_or_authority() {
    roundtrip(BlobDescriptor {
        blob_id: "public-mapped-identity".into(),
        metadata: Some(BlobMetadata {
            display_name: "photo.png".into(),
            content_type: "image/png".into(),
            bytes: 123,
            sha256: vec![7; 32],
            image: Some(ImageInfo {
                format: ImageFormat::Png.into(),
                width: 8,
                height: 9,
            }),
        }),
        remaining_validity_ms: Some(15 * 60 * 1000),
    });
    roundtrip(TextResultDescriptor {
        version: 1,
        id: "text-id".into(),
        name: "notes".into(),
        sha256: "f".repeat(64),
        utf8_bytes: 1024 * 1024,
        encoded_charge: 2 * 1024 * 1024,
        helper_id: Some("helper-id".into()),
    });
    roundtrip(ReadBlobResponse {
        data: vec![0; 65536],
        sequence: 0,
        eof: false,
    });
    assert!(SCHEMA.contains("Durable text is independent of binary lease expiry"));
}

fn message_body(name: &str) -> &str {
    SCHEMA
        .split_once(&format!("message {name} {{"))
        .unwrap()
        .1
        .split_once("\n}")
        .unwrap()
        .0
}

#[test]
fn secret_exclusion_and_redaction_are_explicit_boundary_contracts() {
    for name in ["Invocation", "Definition", "Operation", "BlobDescriptor"] {
        let body = message_body(name);
        assert!(!body.contains("AttachmentCredentials"));
        assert!(!body.contains("secret"));
        assert!(!body.contains("principal ="));
    }
    assert!(SCHEMA.contains("Generated Debug is NOT"));
    assert!(SCHEMA.contains("excluded from URLs, snapshots, history and persistence"));
    // Do not format bearer-bearing generated messages: generated Debug is not a
    // redaction implementation. Runtime logging/privacy tests remain required.
    let credentials = AttachmentCredentials {
        attachment_id: "attachment".into(),
        secret: vec![0x3a; 32],
        invocation_id: "invocation".into(),
        generation: "generation".into(),
    };
    let decoded = AttachmentCredentials::decode(credentials.encode_to_vec().as_slice()).unwrap();
    assert!(decoded.secret == credentials.secret);
}

#[test]
fn generated_services_keep_native_duplex_and_browser_compatible_lanes() {
    assert!(SERVICES.contains("tonic::Streaming<super::InteractRequest>"));
    assert!(SERVICES.contains("tonic::codec::Streaming<super::InteractResponse>"));
    assert!(SERVICES.contains("tonic::codec::Streaming<super::WatchLiveOutputResponse>"));
    assert!(SERVICES.contains("tonic::codec::Streaming<super::ReadBlobResponse>"));
    for method in [
        "OpenAttachment",
        "SendInputBatch",
        "RenewAttachment",
        "CloseAttachment",
        "BeginBlobUpload",
        "AppendBlobUpload",
        "CommitBlobUpload",
        "ReadBlob",
    ] {
        assert!(SERVICES.contains(&format!("/waltr.routine.v2.RoutineService/{method}")));
        assert!(!SERVICES.contains(&format!("tonic::Streaming<super::{method}Request>")));
    }
    assert_eq!(SCHEMA.matches("rpc Interact(stream ").count(), 1);
    assert_eq!(
        SCHEMA
            .lines()
            .filter(|line| line.trim_start().starts_with("rpc "))
            .filter(|line| line.split(" returns ").next().unwrap().contains("(stream "))
            .count(),
        1
    );
    assert!(SERVICES.contains("pub trait RoutineService"));
    assert!(!SERVICES.contains("pub trait DeferredActionService"));
    assert!(message_body("ServerLimits").contains("reserved 36, 37"));
}

#[test]
fn history_watch_has_consistent_highwater_and_separate_change_namespace() {
    roundtrip(WatchInvocationsResponse {
        invocations: vec![Invocation {
            cursor: 5,
            status: InvocationStatus::Running.into(),
            ..Default::default()
        }],
        cursor: "invocation-changes:300".into(),
        snapshot_complete: true,
        phase: ObservationPhase::Snapshot.into(),
        snapshot_high_water: "invocation-changes:300".into(),
        reset_reason: String::new(),
        operations: vec![],
    });
    assert!(SCHEMA.contains("including\n  // updates to already-emitted rows"));
    assert!(SCHEMA.contains("Never use admission cursor or updated_ms"));
    assert!(SCHEMA.contains("historical") || SCHEMA.contains("Historical detail retains"));
    assert!(!message_body("Definition").contains("string source ="));
    assert!(message_body("GetInvocationResponse").contains("pinned_definition"));
}

#[test]
fn binary_read_is_owned_sequential_and_not_text_offset_read() {
    for name in ["OpenBlobReadRequest", "ReadBlobRequest", "ReadBlobResponse"] {
        assert!(!message_body(name).contains("uint64 offset"));
        assert!(!message_body(name).contains("uint64 next_offset"));
    }
    assert!(message_body("ReadTextResultRequest").contains("uint64 offset"));
    assert!(message_body("BlobOutput").contains("BinaryOutputCredentials"));
    assert!(!message_body("Invocation").contains("BinaryOutputCredentials"));
    assert!(message_body("BlobDescriptor").contains("remaining_validity_ms"));
    assert!(!message_body("BlobDescriptor").contains("int64 expires_ms"));
    assert!(message_body("ReadBlobRequest").contains("BlobReaderCredentials"));
}

#[test]
fn native_and_browser_declare_required_png_and_pdf_before_bound_uploads() {
    // Wire fixtures only: headers/digests below do not claim full image validation.
    // Media metadata stays outside this closed, empty business-parameter contract.
    roundtrip(ValueContract {
        schema_version: "waltr.value/v1".into(),
        schema_json: br#"{"type":"object","properties":{},"required":[]}"#.to_vec(),
    });
    let inputs = vec![
        InputDeclaration {
            port: "photo".into(),
            format: Some(input_declaration::Format::Binary(BinaryInputDeclaration {
                display_name: "photo.png".into(),
                content_type: "image/png".into(),
                declared_bytes: Some(8),
                expected_sha256: Some(vec![1; 32]),
                purpose: Some(binary_input_declaration::Purpose::Image(
                    ImageFormat::Png.into(),
                )),
            })),
        },
        InputDeclaration {
            port: "document".into(),
            format: Some(input_declaration::Format::Binary(BinaryInputDeclaration {
                display_name: "document.pdf".into(),
                content_type: "application/pdf".into(),
                declared_bytes: Some(8),
                expected_sha256: Some(vec![2; 32]),
                purpose: Some(binary_input_declaration::Purpose::File(FilePurpose {})),
            })),
        },
    ];
    for browser in [false, true] {
        let open = InteractionOpen {
            selection: Some(InvocationSelection {
                routine: "inspect-inputs".into(),
                revision: "pinned-revision".into(),
                entrypoint: "interactive".into(),
                parameters: Some(StructuredValue {
                    json: b"{}".to_vec(),
                }),
                ..Default::default()
            }),
            inputs: inputs.clone(),
        };
        let open = if browser {
            roundtrip(OpenAttachmentRequest { open: Some(open) })
                .open
                .unwrap()
        } else {
            let request = roundtrip(InteractRequest {
                frame: Some(interact_request::Frame::Open(open)),
            });
            let Some(interact_request::Frame::Open(open)) = request.frame else {
                panic!("expected native first Open")
            };
            open
        };
        let accepted = InteractionAccepted {
            invocation_id: "service-generated-after-SQL-commit".into(),
            revision: "pinned-revision".into(),
            ..Default::default()
        };
        if browser {
            roundtrip(OpenAttachmentResponse {
                admission: Some(open_attachment_response::Admission::Accepted(
                    AttachmentAdmission {
                        invocation: Some(accepted.clone()),
                        credentials: Some(AttachmentCredentials {
                            invocation_id: accepted.invocation_id.clone(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )),
            });
        } else {
            roundtrip(InteractResponse {
                output: Some(OutputEvent {
                    event: Some(output_event::Event::Accepted(accepted.clone())),
                }),
            });
        }
        for (batch_sequence, input) in open.inputs.into_iter().enumerate() {
            let Some(input_declaration::Format::Binary(binary)) = input.format else {
                panic!("expected finite binary declaration")
            };
            let (purpose, image, data) = match binary.purpose.clone().unwrap() {
                binary_input_declaration::Purpose::Image(format) => (
                    begin_blob_upload_request::Purpose::Image(format),
                    Some(ImageInfo {
                        format,
                        width: 1,
                        height: 1,
                    }),
                    b"\x89PNG\r\n\x1a\n".to_vec(),
                ),
                binary_input_declaration::Purpose::File(file) => (
                    begin_blob_upload_request::Purpose::File(file),
                    None,
                    b"%PDF-1.7".to_vec(),
                ),
            };
            let begin = roundtrip(BeginBlobUploadRequest {
                invocation_id: accepted.invocation_id.clone(),
                port: input.port.clone(),
                display_name: binary.display_name.clone(),
                content_type: binary.content_type.clone(),
                declared_bytes: binary.declared_bytes,
                expected_sha256: binary.expected_sha256.clone(),
                purpose: Some(purpose),
            });
            assert_eq!(begin.port, input.port);
            assert_eq!(begin.declared_bytes, Some(data.len() as u64));
            assert_eq!(begin.expected_sha256, binary.expected_sha256);
            let upload = roundtrip(BeginBlobUploadResponse {
                upload_id: format!("upload-{}", input.port),
                chunk_bytes: 65536,
            });
            roundtrip(AppendBlobUploadRequest {
                invocation_id: accepted.invocation_id.clone(),
                upload_id: upload.upload_id.clone(),
                sequence: 0,
                offset: 0,
                data,
            });
            roundtrip(CommitBlobUploadRequest {
                invocation_id: accepted.invocation_id.clone(),
                upload_id: upload.upload_id,
                next_sequence: 1,
                bytes: binary.declared_bytes.unwrap(),
            });
            let committed = roundtrip(CommitBlobUploadResponse {
                blob: Some(BlobDescriptor {
                    blob_id: format!("committed-{}", input.port),
                    metadata: Some(BlobMetadata {
                        display_name: binary.display_name,
                        content_type: binary.content_type,
                        bytes: binary.declared_bytes.unwrap(),
                        sha256: binary.expected_sha256.unwrap(),
                        image,
                    }),
                    remaining_validity_ms: None,
                }),
            });
            let input = roundtrip(InputEvent {
                event: Some(input_event::Event::Blob(BlobInput {
                    port: input.port,
                    blob_id: committed.blob.unwrap().blob_id,
                })),
            });
            if browser {
                roundtrip(SendInputBatchRequest {
                    credentials: Some(AttachmentCredentials {
                        invocation_id: accepted.invocation_id.clone(),
                        ..Default::default()
                    }),
                    sequence: batch_sequence as u64,
                    events: vec![input],
                });
            } else {
                roundtrip(InteractRequest {
                    frame: Some(interact_request::Frame::Input(input)),
                });
            }
        }
        let finish = InputEvent {
            event: Some(input_event::Event::Finish(InputFinish {})),
        };
        if browser {
            roundtrip(SendInputBatchRequest {
                credentials: Some(AttachmentCredentials {
                    invocation_id: accepted.invocation_id,
                    ..Default::default()
                }),
                sequence: 2,
                events: vec![finish],
            });
        } else {
            roundtrip(InteractRequest {
                frame: Some(interact_request::Frame::Input(finish)),
            });
        }
    }
}

#[test]
fn pcm_and_text_declarations_preserve_native_and_browser_live_formats() {
    let formats = [
        (
            "microphone",
            input_declaration::Format::Pcm16le(Pcm16Le {
                sample_rate: 24000,
                channels: 1,
            }),
            stream_open::Format::Pcm16le(Pcm16Le {
                sample_rate: 24000,
                channels: 1,
            }),
        ),
        (
            "transcript",
            input_declaration::Format::Text(TextFormat {}),
            stream_open::Format::Text(TextFormat {}),
        ),
    ];
    for (port, declaration, stream_format) in formats {
        let open = roundtrip(InteractionOpen {
            selection: Some(InvocationSelection {
                routine: "live-control".into(),
                revision: "pinned-revision".into(),
                entrypoint: "interactive".into(),
                parameters: Some(StructuredValue {
                    json: b"{}".to_vec(),
                }),
                ..Default::default()
            }),
            inputs: vec![InputDeclaration {
                port: port.into(),
                format: Some(declaration),
            }],
        });
        roundtrip(InteractRequest {
            frame: Some(interact_request::Frame::Open(open.clone())),
        });
        roundtrip(OpenAttachmentRequest { open: Some(open) });
        roundtrip(StreamOpen {
            port: port.into(),
            stream_id: format!("{port}-stream"),
            format: Some(stream_format),
        });
    }
}

#[test]
fn declarations_express_presence_and_document_preflight_rejection_contract() {
    assert!(InputDeclaration::default().format.is_none());
    assert!(BinaryInputDeclaration::default().purpose.is_none());
    assert!(BinaryInputDeclaration::default().declared_bytes.is_none());
    let zero = roundtrip(BinaryInputDeclaration {
        declared_bytes: Some(0),
        expected_sha256: Some(vec![9; 32]),
        purpose: Some(binary_input_declaration::Purpose::File(FilePurpose {})),
        ..Default::default()
    });
    assert_eq!(zero.declared_bytes, Some(0));
    assert_ne!(
        zero.encode_to_vec(),
        BinaryInputDeclaration::default().encode_to_vec()
    );
    for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Webp] {
        roundtrip(BinaryInputDeclaration {
            declared_bytes: Some(1),
            purpose: Some(binary_input_declaration::Purpose::Image(format.into())),
            ..Default::default()
        });
    }
    // These are representation assertions, not a fake production validator.
    assert!(message_body("InteractionOpen").contains("At most 32 unique known input ports"));
    assert!(
        message_body("InteractionOpen")
            .contains("Missing required, duplicate/unknown/output ports")
    );
    assert!(
        message_body("InteractionOpen")
            .contains("absent finite size or metadata/format mismatch reject before admission")
    );
    assert!(message_body("BinaryInputDeclaration").contains("exactly 32 bytes"));
    assert!(message_body("BeginBlobUploadRequest").contains("string port = 8"));
    assert!(message_body("BeginBlobUploadRequest").contains("MUST EXACTLY match"));
    assert!(message_body("StreamOpen").contains("exactly match its pinned InteractionOpen input"));
    assert!(
        message_body("BlobInput").contains("authenticated invocation AND declared upload port")
    );
}

#[test]
fn observation_revisions_preserve_newer_get_and_older_watch_without_cursor_aliasing() {
    let previous = 9_007_199_254_740_993_u64;
    let newer = previous + 17;
    let old = Invocation {
        id: "invocation".into(),
        cursor: 7,
        revision: "unchanged-program-revision".into(),
        status: InvocationStatus::Running.into(),
        observation_revision: previous,
        source_session: Some("actual-source-session".into()),
        ..Default::default()
    };
    let current = Invocation {
        status: InvocationStatus::Succeeded.into(),
        observation_revision: newer,
        ..old.clone()
    };
    let get = roundtrip(GetInvocationResponse {
        invocation: Some(current.clone()),
        pinned_definition: None,
    });
    let watch = roundtrip(WatchInvocationsResponse {
        invocations: vec![old],
        phase: ObservationPhase::Changes.into(),
        cursor: "opaque-watch-token-not-record-revision".into(),
        ..Default::default()
    });
    let listed = roundtrip(ListInvocationsResponse {
        invocations: vec![current],
        next_cursor: 7,
    });
    let get = get.invocation.unwrap();
    assert_eq!(get.observation_revision, newer);
    assert_eq!(listed.invocations[0], get);
    assert!(watch.invocations[0].observation_revision < get.observation_revision);
    assert_eq!(watch.invocations[0].cursor, get.cursor);
    assert_eq!(watch.invocations[0].revision, get.revision);
    let operation = Operation {
        id: "operation".into(),
        invocation_id: get.id,
        cursor: 3,
        status: OperationStatus::Completed.into(),
        observation_revision: newer,
        ..Default::default()
    };
    let operations = roundtrip(ListOperationsResponse {
        operations: vec![operation.clone()],
        next_cursor: 3,
    });
    let older_operation = Operation {
        status: OperationStatus::Dispatched.into(),
        observation_revision: previous,
        ..operation
    };
    let watch = roundtrip(WatchInvocationsResponse {
        operations: vec![older_operation],
        ..Default::default()
    });
    assert_eq!(operations.operations[0].cursor, watch.operations[0].cursor);
    assert_eq!(operations.operations[0].observation_revision, newer);
    assert!(watch.operations[0].observation_revision < newer);
    // Representation and specification only, not a SQL transaction/reducer test.
    assert!(message_body("Invocation").contains("Positive uint64"));
    assert!(
        message_body("Invocation")
            .contains("Same record ID + observation_revision MUST have identical fields")
    );
    assert!(
        message_body("GetInvocationResponse")
            .contains("Atomic consistent record + descriptor view")
    );
    assert!(message_body("Operation").contains("SAME monotonic domain"));
}

#[test]
fn public_start_contexts_are_explicit_and_separate_from_trigger_selector_and_capabilities() {
    let offers = [
        vec![],
        vec![StartContext::Global as i32],
        vec![StartContext::Session as i32],
        vec![StartContext::Global as i32, StartContext::Session as i32],
    ];
    let mut encodings = Vec::new();
    for start_contexts in offers {
        let entrypoint = roundtrip(Entrypoint {
            name: "manual".into(),
            trigger: Trigger::Manual.into(),
            selector: Some("opaque-host-selector".into()),
            start_contexts,
        });
        encodings.push(entrypoint.encode_to_vec());
        let definition = roundtrip(Definition {
            entrypoints: vec![entrypoint],
            capabilities: vec!["same-capability-does-not-imply-eligibility".into()],
            ..Default::default()
        });
        assert_eq!(definition.capabilities.len(), 1);
    }
    for (i, left) in encodings.iter().enumerate() {
        for right in encodings.iter().skip(i + 1) {
            assert_ne!(left, right);
        }
    }
    roundtrip(Entrypoint {
        name: "event".into(),
        trigger: Trigger::Event.into(),
        selector: Some("event-selector-not-a-public-start-policy".into()),
        start_contexts: vec![StartContext::Global.into()],
    });
    assert!(Entrypoint::default().start_contexts.is_empty());
    assert_eq!(StartContext::default(), StartContext::Unspecified);
    assert!(StartContext::try_from(123456).is_err());
    let contract = message_body("Entrypoint");
    assert!(contract.contains("At most two unique offers"));
    assert!(contract.contains("UNSPECIFIED/unknown/duplicate values reject"));
    assert!(contract.contains("Empty means neither public Start nor Interact is offered"));
    assert!(contract.contains("event/lifecycle/etc are not manually runnable"));
    assert!(contract.contains("preflight MUST enforce"));
    assert!(contract.contains("never current start authority"));
}

#[test]
fn source_history_filters_and_trusted_provenance_preserve_presence() {
    for source_session in [None, Some("stable-actual-source".to_owned())] {
        let list = roundtrip(ListInvocationsRequest {
            after_cursor: 19,
            limit: 50,
            source_session: source_session.clone(),
        });
        let watch = roundtrip(WatchInvocationsRequest {
            after_cursor: "opaque-principal-and-filter-bound-token".into(),
            limit: 50,
            source_session: source_session.clone(),
        });
        let invocation = roundtrip(Invocation {
            source_session: source_session.clone(),
            snapshot: Some("distinct-host-snapshot".into()),
            observation_revision: u64::MAX,
            ..Default::default()
        });
        assert_eq!(list.source_session, source_session);
        assert_eq!(watch.source_session, source_session);
        assert_eq!(invocation.source_session, source_session);
        assert_ne!(invocation.snapshot, invocation.source_session);
    }
    // Empty presence is representable but invalid; absence means ALL, not GLOBAL.
    let empty = ListInvocationsRequest {
        source_session: Some(String::new()),
        ..Default::default()
    };
    assert_ne!(
        empty.encode_to_vec(),
        ListInvocationsRequest::default().encode_to_vec()
    );
    let list_contract = message_body("ListInvocationsRequest");
    assert!(list_contract.contains("Exact nonempty source ID"));
    assert!(list_contract.contains("Absent means ALL authorized records"));
    assert!(list_contract.contains("BEFORE LIMIT"));
    let watch_contract = message_body("WatchInvocationsRequest");
    assert!(watch_contract.contains("binds this filter AND authenticated principal"));
    assert!(watch_contract.contains("changing either cannot reuse"));
    assert!(watch_contract.contains("SAME filter"));
    assert!(
        message_body("Invocation").contains("persisted only after host authorization/preflight")
    );
}
