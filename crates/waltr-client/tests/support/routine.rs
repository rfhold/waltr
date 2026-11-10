#![allow(dead_code)]
use futures_channel::mpsc;
use futures_core::Stream;
use futures_util::StreamExt;
use std::{
    collections::VecDeque,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tonic::{Request, Response, Status};
use w::routine_service_server::{RoutineService, RoutineServiceServer};
use waltr::grpc::waltr::routine::v2 as w;
use waltr_client::{
    NativeGrpcTransport, WaltrTransport,
    interaction::{InteractionLimits, wire},
};

pub type Responses<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;
pub fn limits() -> w::ServerLimits {
    w::ServerLimits {
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
pub fn contract() -> w::ValueContract {
    w::ValueContract {
        schema_version: "waltr.value/v1".into(),
        schema_json: br#"{"type":"object","properties":{},"required":[]}"#.to_vec(),
    }
}
pub fn definition() -> w::Definition {
    w::Definition {
        name: "routine".into(),
        description: "descriptor".into(),
        revision: "revision".into(),
        api_version: "waltr.dev/v1alpha1".into(),
        engine: "javascript".into(),
        entrypoints: vec![
            w::Entrypoint {
                name: "manual".into(),
                trigger: w::Trigger::Manual as i32,
                selector: None,
                start_contexts: vec![1, 2],
            },
            w::Entrypoint {
                name: "live".into(),
                trigger: w::Trigger::Interactive as i32,
                selector: None,
                start_contexts: vec![1, 2],
            },
        ],
        parameters: Some(contract()),
        result: Some(contract()),
        ports: vec![
            w::Port {
                name: "audio".into(),
                direction: 1,
                media: 1,
                required: false,
                max_bytes: 256 * 1024 * 1024,
                formats: vec!["pcm16le-24000-mono".into()],
            },
            w::Port {
                name: "file".into(),
                direction: 1,
                media: 4,
                required: false,
                max_bytes: 64 * 1024 * 1024,
                formats: vec!["application/octet-stream".into()],
            },
            w::Port {
                name: "text".into(),
                direction: 2,
                media: 2,
                required: true,
                max_bytes: 1024 * 1024,
                formats: vec!["utf8".into()],
            },
        ],
        capabilities: vec![],
        budgets: Some(w::Budgets {
            value_bytes: 65536,
            result_bytes: 65536,
            host_calls: 128,
            in_flight: 8,
            deadline_ms: 30000,
        }),
    }
}
pub fn invocation(revision: u64) -> w::Invocation {
    w::Invocation {
        version: 1,
        cursor: u64::MAX - 1,
        id: "invocation".into(),
        routine: "routine".into(),
        revision: "revision".into(),
        entrypoint: "manual".into(),
        origin: 1,
        ownership: 1,
        correlation: "correlation".into(),
        snapshot: None,
        status: if revision > 1 { 3 } else { 1 },
        admitted_ms: 1,
        updated_ms: 2,
        result: None,
        text_results: vec![],
        binary_results: vec![],
        helpers: vec![],
        observation_revision: revision,
        source_session: Some("session".into()),
    }
}
pub fn operation(revision: u64) -> w::Operation {
    w::Operation {
        version: 1,
        cursor: 7,
        id: "operation".into(),
        invocation_id: "invocation".into(),
        name: "host.call".into(),
        status: if revision > 2 { 2 } else { 1 },
        arguments: None,
        result: None,
        updated_ms: 3,
        observation_revision: revision,
    }
}
pub fn open(mode: &str) -> w::InteractionOpen {
    w::InteractionOpen {
        selection: Some(w::InvocationSelection {
            routine: "routine".into(),
            revision: "revision".into(),
            entrypoint: "live".into(),
            parameters: Some(w::StructuredValue {
                json: b"{}".to_vec(),
            }),
            correlation: mode.into(),
            source_session: None,
        }),
        inputs: vec![w::InputDeclaration {
            port: "audio".into(),
            format: Some(w::input_declaration::Format::Pcm16le(w::Pcm16Le {
                sample_rate: 24000,
                channels: 1,
            })),
        }],
    }
}
pub fn accepted() -> w::InteractionAccepted {
    w::InteractionAccepted {
        invocation_id: "invocation".into(),
        revision: "revision".into(),
        limits: Some(limits()),
    }
}
fn event(e: w::output_event::Event) -> w::OutputEvent {
    w::OutputEvent { event: Some(e) }
}
pub fn initial(mode: &str) -> Vec<w::OutputEvent> {
    if mode.starts_with("aggregate-output") {
        let pcm = mode.ends_with("pcm");
        let mut events = vec![event(w::output_event::Event::Accepted(accepted()))];
        for id in ["one", "two"] {
            events.push(event(w::output_event::Event::Open(w::StreamOpen {
                port: "text".into(),
                stream_id: id.into(),
                format: Some(if pcm {
                    w::stream_open::Format::Pcm16le(w::Pcm16Le {
                        sample_rate: 24000,
                        channels: 1,
                    })
                } else {
                    w::stream_open::Format::Text(w::TextFormat {})
                }),
            })));
            events.push(event(w::output_event::Event::Chunk(w::StreamChunk {
                stream_id: id.into(),
                sequence: 0,
                payload: Some(if pcm {
                    w::stream_chunk::Payload::Audio(vec![0; 4])
                } else {
                    w::stream_chunk::Payload::Text(w::TextRevision {
                        segment_id: "segment".into(),
                        mutation: 2,
                        text: "four".into(),
                        committed: true,
                    })
                }),
            })));
            events.push(event(w::output_event::Event::Close(w::StreamClose {
                stream_id: id.into(),
                next_sequence: 1,
            })));
        }
        return events;
    }
    if mode == "unknown-output" {
        return vec![w::OutputEvent { event: None }];
    }
    let mut events = vec![
        event(w::output_event::Event::Accepted(accepted())),
        event(w::output_event::Event::Open(w::StreamOpen {
            port: "text".into(),
            stream_id: "out".into(),
            format: Some(w::stream_open::Format::Text(w::TextFormat {})),
        })),
        event(w::output_event::Event::Chunk(w::StreamChunk {
            stream_id: "out".into(),
            sequence: 0,
            payload: Some(w::stream_chunk::Payload::Text(w::TextRevision {
                segment_id: "segment".into(),
                mutation: 1,
                text: "live".into(),
                committed: false,
            })),
        })),
    ];
    if mode == "bad-sequence"
        && let Some(w::output_event::Event::Chunk(c)) = &mut events[2].event
    {
        c.sequence = 99;
    }
    if mode == "blob" || mode == "expired-blob" {
        events.push(event(w::output_event::Event::Blob(w::BlobOutput {
            port: "download".into(),
            blob: Some(output_blob()),
            output_grant: Some(w::BinaryOutputCredentials {
                invocation_id: "invocation".into(),
                port: "download".into(),
                blob_id: "output-blob".into(),
                generation: "generation".into(),
                secret: b"SECRET-OUTPUT-CANARY-012345678901234".to_vec(),
            }),
        })));
    }
    events
}
fn output_blob() -> w::BlobDescriptor {
    w::BlobDescriptor {
        blob_id: "output-blob".into(),
        metadata: Some(w::BlobMetadata {
            display_name: "output.bin".into(),
            content_type: "application/octet-stream".into(),
            bytes: 3,
            sha256: vec![1; 32],
            image: None,
        }),
        remaining_validity_ms: Some(900000),
    }
}
pub fn finish(mode: &str) -> Vec<w::OutputEvent> {
    let mut events = vec![
        event(w::output_event::Event::Chunk(w::StreamChunk {
            stream_id: "out".into(),
            sequence: 1,
            payload: Some(w::stream_chunk::Payload::Text(w::TextRevision {
                segment_id: "segment".into(),
                mutation: 2,
                text: "final".into(),
                committed: true,
            })),
        })),
        event(w::output_event::Event::Close(w::StreamClose {
            stream_id: "out".into(),
            next_sequence: 2,
        })),
        event(w::output_event::Event::Terminal(w::InteractionTerminal {
            status: 3,
            result: None,
            cleanup_complete: true,
            text_results: vec![],
        })),
    ];
    if mode == "missing-close" {
        events.remove(1);
    }
    if mode == "failed"
        && let Some(w::output_event::Event::Terminal(t)) = &mut events.last_mut().unwrap().event
    {
        t.status = 4;
    }
    if mode == "terminal-trailing" {
        events.push(w::OutputEvent { event: None });
    }
    events
}
pub fn credentials() -> w::AttachmentCredentials {
    w::AttachmentCredentials {
        attachment_id: "attachment".into(),
        secret: b"SECRET-CANARY-012345678901234567890".to_vec(),
        invocation_id: "invocation".into(),
        generation: "generation".into(),
    }
}
fn authenticate(c: Option<w::AttachmentCredentials>) -> Result<(), Status> {
    if c == Some(credentials()) {
        Ok(())
    } else {
        Err(Status::permission_denied("credential rejected"))
    }
}
#[derive(Default)]
struct State {
    grant_claimed: bool,
    catalog_mode: String,
    mode: String,
    batch: u64,
    output: Option<mpsc::UnboundedSender<Result<w::WatchLiveOutputResponse, Status>>>,
    receiver: Option<mpsc::UnboundedReceiver<Result<w::WatchLiveOutputResponse, Status>>>,
    declaration: Option<w::BinaryInputDeclaration>,
    upload: Vec<u8>,
    upload_sequence: u64,
    lease: i64,
    watch_requests: Vec<w::WatchInvocationsRequest>,
    starts: usize,
    opens: usize,
    renews: usize,
    closes: usize,
    native_eof: Arc<tokio::sync::Notify>,
    native_cancel: Arc<tokio::sync::Notify>,
    native_cancel_ack: Arc<tokio::sync::Notify>,
    cancellations: Vec<String>,
    cancel_acks: usize,
}
#[derive(Clone, Default)]
pub struct Service(Arc<Mutex<State>>);
impl Service {
    pub async fn wait_native_eof(&self) {
        let notify = self.0.lock().unwrap().native_eof.clone();
        notify.notified().await;
    }
    pub fn cancellations(&self) -> Vec<String> {
        self.0.lock().unwrap().cancellations.clone()
    }
    pub fn cancel_acks(&self) -> usize {
        self.0.lock().unwrap().cancel_acks
    }
    pub fn catalog_mode(&self, mode: &str) {
        self.0.lock().unwrap().catalog_mode = mode.into();
    }
    pub fn requests(&self) -> Vec<w::WatchInvocationsRequest> {
        self.0.lock().unwrap().watch_requests.clone()
    }
    pub fn admissions(&self) -> (usize, usize) {
        let s = self.0.lock().unwrap();
        (s.starts, s.opens)
    }
    pub fn renews(&self) -> usize {
        self.0.lock().unwrap().renews
    }
    pub fn closes(&self) -> usize {
        self.0.lock().unwrap().closes
    }
}
#[tonic::async_trait]
impl RoutineService for Service {
    type WatchDefinitionsStream = Responses<w::WatchDefinitionsResponse>;
    type WatchInvocationsStream = Responses<w::WatchInvocationsResponse>;
    type InteractStream = Responses<w::InteractResponse>;
    type WatchLiveOutputStream = Responses<w::WatchLiveOutputResponse>;
    type ReadBlobStream = Responses<w::ReadBlobResponse>;
    async fn watch_definitions(
        &self,
        _: Request<w::WatchDefinitionsRequest>,
    ) -> Result<Response<Self::WatchDefinitionsStream>, Status> {
        let mut def = definition();
        let mut effective = Some(limits());
        match self.0.lock().unwrap().catalog_mode.as_str() {
            "unknown-trigger" => def.entrypoints[0].trigger = 999,
            "duplicate-context" => def.entrypoints[0].start_contexts = vec![1, 1],
            "missing-limits" => effective = None,
            "missing-schema" => def.parameters = None,
            "duplicate-schema-key" => {
                def.parameters.as_mut().unwrap().schema_json =
                    br#"{"type":"object","type":"null"}"#.to_vec()
            }
            "unknown-schema" => {
                def.parameters.as_mut().unwrap().schema_version = "future/v99".into()
            }
            _ => {}
        }
        Ok(Response::new(Box::pin(
            futures_util::stream::iter([Ok(w::WatchDefinitionsResponse {
                catalog_revision: "catalog".into(),
                definitions: vec![def],
                supported_capabilities: vec!["media.execute".into()],
                limits: effective,
            })])
            .chain(futures_util::stream::pending()),
        )))
    }
    async fn start_invocation(
        &self,
        r: Request<w::StartInvocationRequest>,
    ) -> Result<Response<w::StartInvocationResponse>, Status> {
        let r = r
            .into_inner()
            .selection
            .ok_or_else(|| Status::invalid_argument("selection"))?;
        self.0.lock().unwrap().starts += 1;
        if r.correlation == "lost-start" {
            return Err(Status::unavailable("ambiguous admission"));
        }
        let mut i = invocation(1);
        i.correlation = r.correlation;
        i.entrypoint = r.entrypoint;
        i.source_session = r.source_session;
        Ok(Response::new(w::StartInvocationResponse {
            invocation: Some(i),
        }))
    }
    async fn list_invocations(
        &self,
        r: Request<w::ListInvocationsRequest>,
    ) -> Result<Response<w::ListInvocationsResponse>, Status> {
        let r = r.into_inner();
        assert!((1..=100).contains(&r.limit));
        Ok(Response::new(w::ListInvocationsResponse {
            invocations: vec![invocation(1)],
            next_cursor: 0,
        }))
    }
    async fn get_invocation(
        &self,
        _: Request<w::GetInvocationRequest>,
    ) -> Result<Response<w::GetInvocationResponse>, Status> {
        Ok(Response::new(w::GetInvocationResponse {
            invocation: Some(invocation(9)),
            pinned_definition: Some(definition()),
        }))
    }
    async fn watch_invocations(
        &self,
        r: Request<w::WatchInvocationsRequest>,
    ) -> Result<Response<Self::WatchInvocationsStream>, Status> {
        let r = r.into_inner();
        self.0.lock().unwrap().watch_requests.push(r.clone());
        if r.after_cursor == "expired" {
            return Err(Status::out_of_range("expired"));
        }
        let rows = if r.after_cursor.is_empty() {
            vec![
                w::WatchInvocationsResponse {
                    invocations: vec![invocation(1)],
                    cursor: "partial/snapshot".into(),
                    snapshot_complete: false,
                    phase: 1,
                    snapshot_high_water: "opaque/high-water".into(),
                    reset_reason: String::new(),
                    operations: vec![],
                },
                w::WatchInvocationsResponse {
                    invocations: vec![],
                    cursor: "opaque/high-water".into(),
                    snapshot_complete: true,
                    phase: 1,
                    snapshot_high_water: "opaque/high-water".into(),
                    reset_reason: String::new(),
                    operations: vec![operation(2)],
                },
                w::WatchInvocationsResponse {
                    invocations: vec![invocation(3)],
                    cursor: "not-a-number/change".into(),
                    snapshot_complete: false,
                    phase: 2,
                    snapshot_high_water: String::new(),
                    reset_reason: String::new(),
                    operations: vec![operation(4)],
                },
            ]
        } else {
            vec![w::WatchInvocationsResponse {
                invocations: vec![invocation(3)],
                cursor: "resume/nonadjacent".into(),
                snapshot_complete: false,
                phase: 2,
                snapshot_high_water: String::new(),
                reset_reason: String::new(),
                operations: vec![],
            }]
        };
        Ok(Response::new(Box::pin(
            futures_util::stream::iter(rows.into_iter().map(Ok))
                .chain(futures_util::stream::pending()),
        )))
    }
    async fn list_operations(
        &self,
        _: Request<w::ListOperationsRequest>,
    ) -> Result<Response<w::ListOperationsResponse>, Status> {
        Ok(Response::new(w::ListOperationsResponse {
            operations: vec![operation(4)],
            next_cursor: 0,
        }))
    }
    async fn cancel_invocation(
        &self,
        r: Request<w::CancelInvocationRequest>,
    ) -> Result<Response<w::CancelInvocationResponse>, Status> {
        let ack = {
            let mut state = self.0.lock().unwrap();
            if state.mode == "native-cancel-eof" {
                assert_eq!(
                    r.metadata().get("authorization").unwrap(),
                    "Bearer post-eof-owner"
                );
            }
            state.cancellations.push(r.into_inner().invocation_id);
            state.native_cancel.notify_one();
            (state.mode == "native-cancel-eof").then(|| state.native_cancel_ack.clone())
        };
        if let Some(ack) = ack {
            ack.notified().await;
        }
        self.0.lock().unwrap().cancel_acks += 1;
        Ok(Response::new(w::CancelInvocationResponse { status: 5 }))
    }
    async fn read_text_result(
        &self,
        r: Request<w::ReadTextResultRequest>,
    ) -> Result<Response<w::ReadTextResultResponse>, Status> {
        let offset = r.into_inner().offset as usize;
        let bytes = "😀x".as_bytes();
        let end = if offset == 0 { 2 } else { bytes.len() };
        if offset > end {
            return Err(Status::invalid_argument("offset"));
        }
        Ok(Response::new(w::ReadTextResultResponse {
            data: bytes[offset..end].to_vec(),
            next_offset: end as u64,
            eof: end == bytes.len(),
        }))
    }
    async fn interact(
        &self,
        r: Request<tonic::Streaming<w::InteractRequest>>,
    ) -> Result<Response<Self::InteractStream>, Status> {
        let mut input = r.into_inner();
        let open = match input.next().await.transpose()?.and_then(|r| r.frame) {
            Some(w::interact_request::Frame::Open(o)) => o,
            _ => return Err(Status::invalid_argument("first open")),
        };
        let mode = open.selection.unwrap().correlation;
        {
            let mut s = self.0.lock().unwrap();
            s.opens += 1;
            s.mode = mode.clone();
            s.declaration = open.inputs.into_iter().find_map(|d| match d.format {
                Some(w::input_declaration::Format::Binary(b)) => Some(b),
                _ => None,
            });
        }
        if mode == "lost-open" {
            return Err(Status::unavailable("ambiguous"));
        }
        let mut events = initial(&mode);
        if mode == "narrowed"
            && let Some(w::OutputEvent {
                event: Some(w::output_event::Event::Accepted(accepted)),
            }) = events.first_mut()
        {
            let limits = accepted.limits.as_mut().unwrap();
            limits.chunk_bytes = 4;
            limits.edge_chunks = 1;
            limits.input_batch_events = 1;
            limits.edge_bytes = 128;
            limits.input_batch_bytes = 128;
            limits.retained_text_bytes = 4;
            limits.text_segments = 1;
            limits.streams_per_direction = 1;
        }
        let queue = VecDeque::from(events);
        let service = self.clone();
        let source = futures_util::stream::unfold(
            (input, queue, false, mode, service),
            |(mut input, mut queue, mut ended, mode, service)| async move {
                loop {
                    if let Some(output) = queue.pop_front() {
                        return Some((
                            Ok(w::InteractResponse {
                                output: Some(output),
                            }),
                            (input, queue, ended, mode, service),
                        ));
                    }
                    if ended {
                        if mode == "native-cancel-eof" {
                            service.0.lock().unwrap().native_cancel_ack.notify_one();
                        }
                        if mode == "never-eof" {
                            futures_util::future::pending::<()>().await;
                        }
                        if mode == "delayed-eof" {
                            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                        }
                        return None;
                    }
                    if mode == "blocked" {
                        std::future::pending::<()>().await;
                    }
                    let next = input.next().await;
                    match next {
                        Some(Ok(w::InteractRequest {
                            frame:
                                Some(w::interact_request::Frame::Input(w::InputEvent {
                                    event: Some(w::input_event::Event::Finish(_)),
                                })),
                        })) => {
                            if mode == "native-eof" || mode == "native-cancel-eof" {
                                assert!(
                                    input.next().await.is_none(),
                                    "Finish must precede physical EOF"
                                );
                            }
                            if mode == "native-cancel-eof" {
                                let cancel = {
                                    let state = service.0.lock().unwrap();
                                    state.native_eof.notify_one();
                                    state.native_cancel.clone()
                                };
                                cancel.notified().await;
                            }
                            ended = true;
                            if mode != "bad-eof" {
                                queue.extend(finish(&mode));
                                if mode == "native-cancel-eof"
                                    && let Some(w::OutputEvent {
                                        event: Some(w::output_event::Event::Terminal(t)),
                                    }) = queue.back_mut()
                                {
                                    t.status = 5;
                                }
                            }
                        }
                        Some(Ok(w::InteractRequest {
                            frame:
                                Some(w::interact_request::Frame::Input(w::InputEvent {
                                    event: Some(w::input_event::Event::Cancel(_)),
                                })),
                        }))
                        | None => {
                            ended = true;
                            queue.push_back(event(w::output_event::Event::Terminal(
                                w::InteractionTerminal {
                                    status: 5,
                                    cleanup_complete: true,
                                    ..Default::default()
                                },
                            )));
                        }
                        Some(Err(e)) => return Some((Err(e), (input, queue, true, mode, service))),
                        _ => {}
                    }
                }
            },
        );
        Ok(Response::new(Box::pin(source)))
    }
    async fn open_attachment(
        &self,
        r: Request<w::OpenAttachmentRequest>,
    ) -> Result<Response<w::OpenAttachmentResponse>, Status> {
        let open = r.into_inner().open.unwrap();
        let mut s = self.0.lock().unwrap();
        s.opens += 1;
        s.mode = open.selection.unwrap().correlation;
        if s.mode == "lost-open" {
            return Err(Status::unavailable("ambiguous"));
        }
        s.declaration = open.inputs.into_iter().find_map(|d| match d.format {
            Some(w::input_declaration::Format::Binary(b)) => Some(b),
            _ => None,
        });
        let (tx, rx) = mpsc::unbounded();
        for o in initial(&s.mode) {
            tx.unbounded_send(Ok(w::WatchLiveOutputResponse { output: Some(o) }))
                .unwrap();
        }
        s.output = Some(tx);
        s.receiver = Some(rx);
        s.batch = 0;
        s.lease = 30000;
        Ok(Response::new(w::OpenAttachmentResponse {
            admission: Some(w::open_attachment_response::Admission::Accepted(
                w::AttachmentAdmission {
                    invocation: Some(accepted()),
                    credentials: Some(credentials()),
                    lease_expires_ms: 30000,
                    absolute_expires_ms: 5_400_000,
                    attach_by_ms: 10000,
                },
            )),
        }))
    }
    async fn send_input_batch(
        &self,
        r: Request<w::SendInputBatchRequest>,
    ) -> Result<Response<w::SendInputBatchResponse>, Status> {
        let r = r.into_inner();
        authenticate(r.credentials)?;
        if self.0.lock().unwrap().mode == "blocked-batch" {
            std::future::pending::<()>().await;
        }
        let mut s = self.0.lock().unwrap();
        if r.sequence != s.batch {
            return Err(Status::invalid_argument("batch sequence"));
        }
        s.batch += 1;
        if s.mode == "lost-batch" {
            return Err(Status::unavailable("ambiguous"));
        }
        if r.events
            .iter()
            .any(|e| matches!(e.event, Some(w::input_event::Event::Finish(_))))
        {
            if let Some(tx) = s.output.take() {
                if s.mode != "bad-eof" {
                    for output in finish(&s.mode) {
                        tx.unbounded_send(Ok(w::WatchLiveOutputResponse {
                            output: Some(output),
                        }))
                        .unwrap();
                    }
                }
            }
        }
        Ok(Response::new(w::SendInputBatchResponse {
            accepted_sequence: if s.mode == "future-ack" {
                r.sequence + 1
            } else {
                r.sequence
            },
        }))
    }
    async fn watch_live_output(
        &self,
        r: Request<w::WatchLiveOutputRequest>,
    ) -> Result<Response<Self::WatchLiveOutputStream>, Status> {
        authenticate(r.into_inner().credentials)?;
        let rx = self
            .0
            .lock()
            .unwrap()
            .receiver
            .take()
            .ok_or_else(|| Status::failed_precondition("one output owner"))?;
        let mode = self.0.lock().unwrap().mode.clone();
        Ok(Response::new(Box::pin(futures_util::stream::unfold(
            (rx, mode),
            move |(mut rx, mode)| async move {
                let value = rx.next().await;
                if value.is_none() && mode == "never-eof" {
                    futures_util::future::pending::<()>().await;
                }
                if value.is_none() && mode == "delayed-eof" {
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                }
                value.map(|value| (value, (rx, mode)))
            },
        ))))
    }
    async fn renew_attachment(
        &self,
        r: Request<w::RenewAttachmentRequest>,
    ) -> Result<Response<w::RenewAttachmentResponse>, Status> {
        authenticate(r.into_inner().credentials)?;
        let mut s = self.0.lock().unwrap();
        s.renews += 1;
        if s.mode == "renew-denied" {
            return Err(Status::failed_precondition("lease expired"));
        }
        s.lease += 10000;
        Ok(Response::new(w::RenewAttachmentResponse {
            lease_expires_ms: s.lease,
            absolute_expires_ms: 5_400_000,
        }))
    }
    async fn close_attachment(
        &self,
        r: Request<w::CloseAttachmentRequest>,
    ) -> Result<Response<w::CloseAttachmentResponse>, Status> {
        authenticate(r.into_inner().credentials)?;
        let mut s = self.0.lock().unwrap();
        s.closes += 1;
        if let Some(tx) = s.output.take() {
            drop(tx.unbounded_send(Ok(w::WatchLiveOutputResponse {
                output: Some(event(w::output_event::Event::Terminal(
                    w::InteractionTerminal {
                        status: 5,
                        cleanup_complete: true,
                        ..Default::default()
                    },
                ))),
            })));
        }
        Ok(Response::new(w::CloseAttachmentResponse {
            status: 5,
            cleanup_complete: true,
        }))
    }
    async fn begin_blob_upload(
        &self,
        r: Request<w::BeginBlobUploadRequest>,
    ) -> Result<Response<w::BeginBlobUploadResponse>, Status> {
        let r = r.into_inner();
        let mut s = self.0.lock().unwrap();
        let b = s
            .declaration
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("declaration"))?;
        if r.port != "file"
            || r.declared_bytes != b.declared_bytes
            || r.display_name != b.display_name
            || r.content_type != b.content_type
        {
            return Err(Status::invalid_argument("pinned metadata"));
        }
        s.upload.clear();
        s.upload_sequence = 0;
        Ok(Response::new(w::BeginBlobUploadResponse {
            upload_id: "upload".into(),
            chunk_bytes: 65536,
        }))
    }
    async fn append_blob_upload(
        &self,
        r: Request<w::AppendBlobUploadRequest>,
    ) -> Result<Response<w::AppendBlobUploadResponse>, Status> {
        let r = r.into_inner();
        let mut s = self.0.lock().unwrap();
        if r.offset != s.upload.len() as u64 || r.sequence != s.upload_sequence {
            return Err(Status::invalid_argument("upload order"));
        }
        s.upload.extend(r.data);
        s.upload_sequence += 1;
        Ok(Response::new(w::AppendBlobUploadResponse {
            accepted_sequence: r.sequence,
            next_offset: s.upload.len() as u64,
        }))
    }
    async fn commit_blob_upload(
        &self,
        r: Request<w::CommitBlobUploadRequest>,
    ) -> Result<Response<w::CommitBlobUploadResponse>, Status> {
        let r = r.into_inner();
        let s = self.0.lock().unwrap();
        let d = s.declaration.as_ref().unwrap();
        if r.bytes != s.upload.len() as u64
            || Some(r.bytes) != d.declared_bytes
            || r.next_sequence != s.upload_sequence
        {
            return Err(Status::invalid_argument("commit size"));
        }
        Ok(Response::new(w::CommitBlobUploadResponse {
            blob: Some(w::BlobDescriptor {
                blob_id: "blob".into(),
                metadata: Some(w::BlobMetadata {
                    display_name: d.display_name.clone(),
                    content_type: d.content_type.clone(),
                    bytes: r.bytes,
                    sha256: vec![1; 32],
                    image: None,
                }),
                remaining_validity_ms: None,
            }),
        }))
    }
    async fn cancel_blob_upload(
        &self,
        _: Request<w::CancelBlobUploadRequest>,
    ) -> Result<Response<w::CancelBlobUploadResponse>, Status> {
        Ok(Response::new(w::CancelBlobUploadResponse {}))
    }
    async fn open_blob_read(
        &self,
        request: Request<w::OpenBlobReadRequest>,
    ) -> Result<Response<w::OpenBlobReadResponse>, Status> {
        let mut state = self.0.lock().unwrap();
        let request = request.into_inner();
        if request.invocation_id != "invocation" {
            return Err(Status::permission_denied("invocation"));
        }
        let output = match request.access {
            Some(w::open_blob_read_request::Access::InputBlobId(id)) if id == "blob" => false,
            Some(w::open_blob_read_request::Access::OutputGrant(g))
                if !state.grant_claimed
                    && state.mode != "expired-blob"
                    && g.blob_id == "output-blob"
                    && g.port == "download"
                    && g.generation == "generation"
                    && g.secret == b"SECRET-OUTPUT-CANARY-012345678901234" =>
            {
                state.grant_claimed = true;
                true
            }
            _ => return Err(Status::permission_denied("output grant unavailable")),
        };
        Ok(Response::new(w::OpenBlobReadResponse {
            credentials: Some(w::BlobReaderCredentials {
                invocation_id: "invocation".into(),
                reader_id: "reader".into(),
                generation: "generation".into(),
                secret: vec![5; 32],
            }),
            blob: Some(if output {
                output_blob()
            } else {
                w::BlobDescriptor {
                    blob_id: "blob".into(),
                    metadata: Some(w::BlobMetadata {
                        display_name: "file.bin".into(),
                        content_type: "application/octet-stream".into(),
                        bytes: 3,
                        sha256: vec![1; 32],
                        image: None,
                    }),
                    remaining_validity_ms: None,
                }
            }),
        }))
    }
    async fn read_blob(
        &self,
        _: Request<w::ReadBlobRequest>,
    ) -> Result<Response<Self::ReadBlobStream>, Status> {
        Ok(Response::new(Box::pin(futures_util::stream::iter([
            Ok(w::ReadBlobResponse {
                data: vec![1, 2],
                sequence: 0,
                eof: false,
            }),
            Ok(w::ReadBlobResponse {
                data: vec![3],
                sequence: 1,
                eof: true,
            }),
        ]))))
    }
    async fn close_blob_read(
        &self,
        _: Request<w::CloseBlobReadRequest>,
    ) -> Result<Response<w::CloseBlobReadResponse>, Status> {
        Ok(Response::new(w::CloseBlobReadResponse {}))
    }
}

pub struct Server {
    pub transport: NativeGrpcTransport,
    pub service: Service,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub async fn server() -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let incoming = futures_util::stream::unfold(listener, |listener| async {
        let accepted = listener.accept().await.map(|(socket, _)| socket);
        Some((accepted, listener))
    });
    let service = Service::default();
    let served = service.clone();
    let task = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(RoutineServiceServer::new(served))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    Server {
        transport: NativeGrpcTransport::new(format!("http://{address}")),
        service,
        task,
    }
}
pub async fn client_limits(t: &NativeGrpcTransport) -> InteractionLimits {
    let mut stream = t.watch_manual_routines("catalog".into()).await.unwrap();
    let snapshot = stream.next().await.unwrap().unwrap();
    InteractionLimits::for_definition(snapshot.definitions[0].clone(), snapshot.limits).unwrap()
}
pub fn stream_open() -> wire::input_event::Event {
    wire::input_event::Event::Open(wire::StreamOpen {
        port: "audio".into(),
        stream_id: "in".into(),
        format: Some(wire::stream_open::Format::Pcm16le(wire::Pcm16Le {
            sample_rate: 24000,
            channels: 1,
        })),
    })
}
pub fn stream_close() -> wire::input_event::Event {
    wire::input_event::Event::Close(wire::StreamClose {
        stream_id: "in".into(),
        next_sequence: 0,
    })
}
