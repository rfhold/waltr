use waltr_client::{interaction::wire, *};

pub fn limits() -> wire::ServerLimits {
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
pub fn invocation(name: &str) -> waltr_desktop::interaction::Invocation {
    let contract = RoutineContract::from_wire(&wire::ValueContract {
        schema_version: "waltr.value/v1".into(),
        schema_json: br#"{"type":"object","properties":{},"required":[]}"#.to_vec(),
    })
    .unwrap();
    let mut definition = ClientManualRoutineDefinition {
        routine_name: name.into(),
        description: String::new(),
        routine_revision: "revision".into(),
        api_version: "waltr.dev/v1alpha1".into(),
        engine: "javascript".into(),
        entrypoints: vec![RoutineEntrypoint {
            name: "dictate".into(),
            trigger: ClientRoutineTriggerKind::Interactive,
            selector: None,
            start_contexts: vec![RoutineStartContext::Global],
        }],
        parameters: contract.clone(),
        result: contract,
        ports: vec![
            RoutinePort {
                name: "audio".into(),
                direction: RoutineDirection::Input,
                media: RoutineMedia::Audio,
                required: true,
                max_bytes: RoutineRevision(256 * 1024 * 1024),
                formats: vec!["pcm16le-24000-mono".into()],
            },
            RoutinePort {
                name: "clean-text".into(),
                direction: RoutineDirection::Output,
                media: RoutineMedia::Text,
                required: true,
                max_bytes: RoutineRevision(1024 * 1024),
                formats: vec!["utf8".into()],
            },
        ],
        capabilities: vec![],
        budgets: RoutineBudgets {
            value_bytes: 65536,
            result_bytes: 65536,
            host_calls: 128,
            in_flight: 8,
            deadline_ms: RoutineRevision(30000),
        },
    };
    if name == "pcm" {
        definition.ports.push(RoutinePort {
            name: "speaker".into(),
            direction: RoutineDirection::Output,
            media: RoutineMedia::Audio,
            required: true,
            max_bytes: RoutineRevision(65536),
            formats: vec!["pcm16le-24000-mono".into()],
        });
    }
    waltr_desktop::interaction::Invocation::new(
        definition,
        limits(),
        "dictate".into(),
        RoutineJson::parse("{}").unwrap(),
        "hold".into(),
        None,
    )
    .unwrap()
}

macro_rules! fake_service {
    ($ty:ty; $($method:ident: $request:ident => $response:ident),* $(,)?) => {
        #[tonic::async_trait]
        impl wire::routine_service_server::RoutineService for $ty {
            type InteractStream = Responses;
            async fn interact(&self, request: Request<tonic::Streaming<wire::InteractRequest>>) -> Result<Response<Responses>, RpcStatus> { self.interact(request).await }
            type WatchDefinitionsStream = std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<wire::WatchDefinitionsResponse, RpcStatus>> + Send>>;
            async fn watch_definitions(&self, _: Request<wire::WatchDefinitionsRequest>) -> Result<Response<Self::WatchDefinitionsStream>, RpcStatus> { Err(RpcStatus::unimplemented("unused")) }
            type WatchInvocationsStream = std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<wire::WatchInvocationsResponse, RpcStatus>> + Send>>;
            async fn watch_invocations(&self, _: Request<wire::WatchInvocationsRequest>) -> Result<Response<Self::WatchInvocationsStream>, RpcStatus> { Err(RpcStatus::unimplemented("unused")) }
            type WatchLiveOutputStream = std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<wire::WatchLiveOutputResponse, RpcStatus>> + Send>>;
            async fn watch_live_output(&self, _: Request<wire::WatchLiveOutputRequest>) -> Result<Response<Self::WatchLiveOutputStream>, RpcStatus> { Err(RpcStatus::unimplemented("unused")) }
            type ReadBlobStream = std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<wire::ReadBlobResponse, RpcStatus>> + Send>>;
            async fn read_blob(&self, _: Request<wire::ReadBlobRequest>) -> Result<Response<Self::ReadBlobStream>, RpcStatus> { Err(RpcStatus::unimplemented("unused")) }
            $(async fn $method(&self, _: Request<wire::$request>) -> Result<Response<wire::$response>, RpcStatus> { Err(RpcStatus::unimplemented("unused")) })*
        }
    };
}
pub(crate) use fake_service;
