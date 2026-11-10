use super::*;
use sha2::{Digest, Sha256};

pub(super) async fn run(address: std::net::SocketAddr, auth: Auth, token: &str) {
    let mut discovery = send(
        address,
        &auth,
        "WatchDefinitions",
        p::WatchDefinitionsRequest {},
    )
    .await;
    let definition = discovery
        .next::<p::WatchDefinitionsResponse>()
        .await
        .unwrap()
        .definitions
        .into_iter()
        .find(|v| v.name == "blob-mirror")
        .unwrap();
    drop(discovery);
    let cases: Vec<(&str, Vec<u8>, Option<i32>)> = vec![
        (
            "image/png",
            include_bytes!("../../../../../tests/fixtures/routines/images/rgba.png").to_vec(),
            Some(1),
        ),
        (
            "image/jpeg",
            include_bytes!(
                "../../../../../tests/fixtures/routines/images/valid-ordinary-initial-zrl.jpg"
            )
            .to_vec(),
            Some(2),
        ),
        (
            "image/webp",
            include_bytes!("../../../../../tests/fixtures/routines/images/lossy.webp").to_vec(),
            Some(3),
        ),
        (
            "application/pdf",
            b"%PDF-1.7\nopaque production PDF\n%%EOF".to_vec(),
            None,
        ),
        ("text/plain", b"Production UTF-8 attachment.".to_vec(), None),
    ];
    for (mime, data, image) in cases {
        let digest = Sha256::digest(&data).to_vec();
        let len = data.len() as u64;
        let purpose = image
            .map(p::binary_input_declaration::Purpose::Image)
            .unwrap_or(p::binary_input_declaration::Purpose::File(
                p::FilePurpose {},
            ));
        let opened: p::OpenAttachmentResponse = unary(
            address,
            &auth,
            "OpenAttachment",
            p::OpenAttachmentRequest {
                open: Some(p::InteractionOpen {
                    selection: Some(p::InvocationSelection {
                        routine: "blob-mirror".into(),
                        revision: definition.revision.clone(),
                        entrypoint: "run".into(),
                        parameters: Some(p::StructuredValue {
                            json: b"{}".to_vec(),
                        }),
                        ..Default::default()
                    }),
                    inputs: vec![p::InputDeclaration {
                        port: "source".into(),
                        format: Some(p::input_declaration::Format::Binary(
                            p::BinaryInputDeclaration {
                                display_name: "opaque-file".into(),
                                content_type: mime.into(),
                                declared_bytes: Some(len),
                                expected_sha256: Some(digest.clone()),
                                purpose: Some(purpose),
                            },
                        )),
                    }],
                }),
            },
        )
        .await;
        let credentials = match opened.admission.unwrap() {
            p::open_attachment_response::Admission::Accepted(v) => v.credentials.unwrap(),
            _ => panic!("production binary admission rejected"),
        };
        let invocation = credentials.invocation_id.clone();
        let mut output = send(
            address,
            &auth,
            "WatchLiveOutput",
            p::WatchLiveOutputRequest {
                credentials: Some(credentials.clone()),
            },
        )
        .await;
        let purpose = image
            .map(p::begin_blob_upload_request::Purpose::Image)
            .unwrap_or(p::begin_blob_upload_request::Purpose::File(
                p::FilePurpose {},
            ));
        let begun: p::BeginBlobUploadResponse = unary(
            address,
            &auth,
            "BeginBlobUpload",
            p::BeginBlobUploadRequest {
                invocation_id: invocation.clone(),
                port: "source".into(),
                display_name: "opaque-file".into(),
                content_type: mime.into(),
                declared_bytes: Some(len),
                expected_sha256: Some(digest.clone()),
                purpose: Some(purpose),
            },
        )
        .await;
        assert!(begun.chunk_bytes <= 65536);
        let mut offset = 0;
        let mut sequence = 0;
        for chunk in data.chunks(65536) {
            let ack: p::AppendBlobUploadResponse = unary(
                address,
                &auth,
                "AppendBlobUpload",
                p::AppendBlobUploadRequest {
                    invocation_id: invocation.clone(),
                    upload_id: begun.upload_id.clone(),
                    sequence,
                    offset,
                    data: chunk.to_vec(),
                },
            )
            .await;
            offset += chunk.len() as u64;
            assert_eq!(ack.accepted_sequence, sequence);
            assert_eq!(ack.next_offset, offset);
            sequence += 1;
        }
        let committed: p::CommitBlobUploadResponse = unary(
            address,
            &auth,
            "CommitBlobUpload",
            p::CommitBlobUploadRequest {
                invocation_id: invocation.clone(),
                upload_id: begun.upload_id,
                next_sequence: sequence,
                bytes: len,
            },
        )
        .await;
        let input_blob = committed.blob.unwrap();
        let metadata = input_blob.metadata.unwrap();
        assert_eq!(metadata.sha256, digest);
        assert_eq!(metadata.bytes, len);
        assert!(metadata.image.is_none());
        let _: p::SendInputBatchResponse = unary(
            address,
            &auth,
            "SendInputBatch",
            p::SendInputBatchRequest {
                credentials: Some(credentials),
                sequence: 0,
                events: vec![
                    p::InputEvent {
                        event: Some(p::input_event::Event::Blob(p::BlobInput {
                            port: "source".into(),
                            blob_id: input_blob.blob_id,
                        })),
                    },
                    p::InputEvent {
                        event: Some(p::input_event::Event::Finish(p::InputFinish {})),
                    },
                ],
            },
        )
        .await;
        let mut grant = None;
        let mut terminal = None;
        let mut original_ttl = 0;
        while let Some(event) = output.next::<p::WatchLiveOutputResponse>().await {
            match event.output.unwrap().event.unwrap() {
                p::output_event::Event::Blob(v) => {
                    let descriptor = v.blob.unwrap();
                    original_ttl = descriptor.remaining_validity_ms.unwrap();
                    assert!(original_ttl > 0 && original_ttl <= 900000);
                    assert!(grant.replace(v.output_grant.unwrap()).is_none());
                }
                p::output_event::Event::Terminal(v) => {
                    assert!(terminal.replace(v).is_none());
                }
                _ => {}
            }
        }
        let terminal = terminal.unwrap();
        assert_eq!(terminal.status, p::InvocationStatus::Succeeded as i32);
        assert!(terminal.cleanup_complete);
        let grant = grant.unwrap();
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut native = p::routine_service_client::RoutineServiceClient::new(channel);
        let mut forged = grant.clone();
        forged.secret.fill(0);
        let mut request = tonic::Request::new(p::OpenBlobReadRequest {
            invocation_id: invocation.clone(),
            access: Some(p::open_blob_read_request::Access::OutputGrant(forged)),
        });
        request
            .metadata_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        assert!(matches!(
            native.open_blob_read(request).await.unwrap_err().code(),
            tonic::Code::PermissionDenied | tonic::Code::NotFound
        ));
        let opened: p::OpenBlobReadResponse = unary(
            address,
            &auth,
            "OpenBlobRead",
            p::OpenBlobReadRequest {
                invocation_id: invocation.clone(),
                access: Some(p::open_blob_read_request::Access::OutputGrant(grant)),
            },
        )
        .await;
        let remaining = opened.blob.unwrap().remaining_validity_ms.unwrap();
        assert!(remaining > 0 && remaining <= original_ttl);
        let reader = opened.credentials.unwrap();
        let mut stream = send(
            address,
            &auth,
            "ReadBlob",
            p::ReadBlobRequest {
                credentials: Some(reader.clone()),
                max_bytes: 127,
            },
        )
        .await;
        let mut actual = Vec::new();
        let mut sequence = 0;
        let mut eof = 0;
        while let Some(chunk) = stream.next::<p::ReadBlobResponse>().await {
            assert_eq!(chunk.sequence, sequence);
            sequence += 1;
            assert!(chunk.data.len() <= 127);
            actual.extend(chunk.data);
            if chunk.eof {
                eof += 1;
            }
        }
        assert_eq!(eof, 1);
        assert_eq!(actual, data);
        let message = p::CloseBlobReadRequest {
            credentials: Some(reader),
        };
        let bytes = prost::Message::encode_to_vec(&message);
        let mut frame = vec![0];
        frame.extend((bytes.len() as u32).to_be_bytes());
        frame.extend(bytes);
        let mut request = reqwest::Client::new()
            .post(format!(
                "http://{address}/waltr.routine.v2.RoutineService/CloseBlobRead"
            ))
            .header("content-type", "application/grpc-web+proto")
            .header("origin", format!("http://{address}"))
            .body(frame);
        request = match &auth {
            Auth::Cookie(cookie) => {
                request.header("cookie", format!("waltr-session-local={cookie}"))
            }
            Auth::Bearer(token) => request.bearer_auth(token),
        };
        let response = request.send().await.unwrap();
        assert_eq!(response.headers().get("grpc-status").unwrap(), "7");
        let _body = response.bytes().await.unwrap();
    }
}
