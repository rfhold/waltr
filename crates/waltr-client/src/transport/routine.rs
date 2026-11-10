use super::{RemoteTransport, status_error};
use crate::*;
use futures_core::Stream;
use futures_util::StreamExt;
use prost::Message;
use std::{
    collections::{BTreeMap, BTreeSet},
    pin::Pin,
};
use tonic::codegen::{Body, Bytes, StdError};
use waltr::grpc::waltr::routine::v2 as w;

/// Complete authenticated catalog snapshots.
pub type RoutineDefinitionWatchStream =
    Pin<Box<dyn Stream<Item = Result<ClientRoutineDefinitionSnapshot, ClientError>>>>;
/// Opaque-cursor observation pages and explicit reset signals.
pub type RoutineInvocationWatchStream =
    Pin<Box<dyn Stream<Item = Result<ClientRoutineInvocationWatchUpdate, ClientError>>>>;

pub(super) async fn request_routine<T>(
    transport: &T,
    command: RoutineRequest,
) -> Result<RoutineRequestResult, ClientError>
where
    T: RemoteTransport,
    T::Inner: tonic::client::GrpcService<tonic::body::Body>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::Error: Into<StdError>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody:
        Body<Data = Bytes> + Send + 'static,
    <<T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody as Body>::Error:
        Into<StdError> + Send,
{
    match command {
        RoutineRequest::Invocations {
            after_cursor,
            limit,
            source_session,
        } => {
            if !(1..=100).contains(&limit) {
                return Err(invalid());
            }
            let filters = source_session
                .map(|s| BTreeMap::from([(ROUTINE_SOURCE_SESSION_ID_METADATA_KEY.into(), s)]))
                .unwrap_or_default();
            list_routine_invocations(
                transport,
                String::new(),
                limit,
                Some(after_cursor.0),
                filters,
            )
            .await
            .map(RoutineRequestResult::Invocations)
        }
        RoutineRequest::Operations {
            invocation_id,
            after_cursor,
            limit,
        } => {
            identity(&invocation_id, 256)?;
            if !(1..=100).contains(&limit) {
                return Err(invalid());
            }
            let credentials = transport.request_credentials();
            let mut client = transport.routine_client(&credentials).await?;
            let r = client
                .list_operations(w::ListOperationsRequest {
                    invocation_id: invocation_id.clone(),
                    after_cursor: after_cursor.0,
                    limit,
                })
                .await
                .map_err(|e| super::interaction::media_error(&e, &credentials))?
                .into_inner();
            if r.operations.len() > limit as usize
                || (r.next_cursor != 0 && r.next_cursor <= after_cursor.0)
            {
                return Err(invalid());
            }
            let operations = r
                .operations
                .into_iter()
                .map(operation)
                .collect::<Result<Vec<_>, _>>()?;
            let mut cursor = after_cursor.0;
            for op in &operations {
                if op.invocation_id != invocation_id || op.cursor.0 <= cursor {
                    return Err(invalid());
                }
                cursor = op.cursor.0;
            }
            Ok(RoutineRequestResult::Operations {
                invocation_id,
                operations,
                next_cursor: (r.next_cursor != 0).then_some(RoutineRevision(r.next_cursor)),
            })
        }
        RoutineRequest::Cancel { invocation_id } => {
            identity(&invocation_id, 256)?;
            let credentials = transport.request_credentials();
            let mut client = transport.routine_client(&credentials).await?;
            let r = client
                .cancel_invocation(w::CancelInvocationRequest { invocation_id })
                .await
                .map_err(|e| super::interaction::media_error(&e, &credentials))?
                .into_inner();
            Ok(RoutineRequestResult::Cancelled(invocation_status(
                r.status,
            )?))
        }
    }
}

#[allow(clippy::future_not_send)]
pub(super) async fn watch_manual_routines<T>(
    transport: &T,
    _request_id: String,
) -> Result<RoutineDefinitionWatchStream, ClientError>
where
    T: RemoteTransport,
    T::Inner: tonic::client::GrpcService<tonic::body::Body>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::Error: Into<StdError>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody:
        Body<Data = Bytes> + Send + 'static,
    <<T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody as Body>::Error:
        Into<StdError> + Send,
{
    let credentials = transport.request_credentials();
    let stream = transport
        .routine_client(&credentials)
        .await?
        .max_decoding_message_size(8 * 1024 * 1024)
        .watch_definitions(w::WatchDefinitionsRequest {})
        .await
        .map_err(|e| status_error(&e, &credentials))?
        .into_inner()
        .map(move |r| {
            r.map_err(|e| status_error(&e, &credentials))
                .and_then(definition_snapshot)
        });
    Ok(Box::pin(stream))
}
#[allow(clippy::future_not_send)]
pub(super) async fn start_manual_routine<T>(
    transport: &T,
    args: StartManualRoutineArgs,
) -> Result<ClientRoutineStartOutcome, ClientError>
where
    T: RemoteTransport,
    T::Inner: tonic::client::GrpcService<tonic::body::Body>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::Error: Into<StdError>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody:
        Body<Data = Bytes> + Send + 'static,
    <<T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody as Body>::Error:
        Into<StdError> + Send,
{
    let credentials = transport.request_credentials();
    let r = transport
        .routine_client(&credentials)
        .await?
        .max_encoding_message_size(128 * 1024)
        .max_decoding_message_size(2 * 1024 * 1024)
        .start_invocation(w::StartInvocationRequest {
            selection: Some(w::InvocationSelection {
                routine: args.routine_name.clone(),
                revision: args.routine_revision.clone(),
                entrypoint: args.trigger_name.clone(),
                parameters: Some(w::StructuredValue {
                    json: args.parameters.as_str().as_bytes().to_vec(),
                }),
                correlation: args.request_id.clone(),
                source_session: args.source_session_id.clone(),
            }),
        })
        .await
        .map_err(|e| status_error(&e, &credentials))?
        .into_inner();
    let row = summary(r.invocation.ok_or_else(invalid)?)?;
    if row.routine_name != args.routine_name
        || row.routine_revision != args.routine_revision
        || row.trigger_name != args.trigger_name
        || row.correlation_id != args.request_id
        || row.source_session != args.source_session_id
    {
        return Err(invalid());
    }
    Ok(ClientRoutineStartOutcome::Accepted(row.invocation_id))
}
#[allow(clippy::future_not_send)]
pub(super) async fn list_routine_invocations<T>(
    transport: &T,
    _request_id: String,
    page_size: u32,
    after: Option<u64>,
    filters: BTreeMap<String, String>,
) -> Result<ClientRoutineInvocationPage, ClientError>
where
    T: RemoteTransport,
    T::Inner: tonic::client::GrpcService<tonic::body::Body>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::Error: Into<StdError>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody:
        Body<Data = Bytes> + Send + 'static,
    <<T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody as Body>::Error:
        Into<StdError> + Send,
{
    if !(1..=100).contains(&page_size) {
        return Err(invalid());
    }
    let source_session = source_filter(filters)?;
    let credentials = transport.request_credentials();
    let r = transport
        .routine_client(&credentials)
        .await?
        .max_decoding_message_size(8 * 1024 * 1024)
        .list_invocations(w::ListInvocationsRequest {
            after_cursor: after.unwrap_or(0),
            limit: page_size,
            source_session: source_session.clone(),
        })
        .await
        .map_err(|e| status_error(&e, &credentials))?
        .into_inner();
    if r.invocations.len() > page_size as usize {
        return Err(invalid());
    }
    let invocations = summaries(r.invocations, source_session.as_deref())?;
    Ok(ClientRoutineInvocationPage {
        invocations,
        older_page_token: (r.next_cursor != 0).then_some(r.next_cursor),
    })
}
#[allow(clippy::future_not_send)]
pub(super) async fn get_routine_invocation<T>(
    transport: &T,
    _request_id: String,
    id: String,
) -> Result<ClientRoutineInvocationDetail, ClientError>
where
    T: RemoteTransport,
    T::Inner: tonic::client::GrpcService<tonic::body::Body>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::Error: Into<StdError>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody:
        Body<Data = Bytes> + Send + 'static,
    <<T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody as Body>::Error:
        Into<StdError> + Send,
{
    identity(&id, 256)?;
    let credentials = transport.request_credentials();
    let mut client = transport
        .routine_client(&credentials)
        .await?
        .max_decoding_message_size(2 * 1024 * 1024);
    let r = client
        .get_invocation(w::GetInvocationRequest {
            invocation_id: id.clone(),
        })
        .await
        .map_err(|e| status_error(&e, &credentials))?
        .into_inner();
    let summary = summary(r.invocation.ok_or_else(invalid)?)?;
    let pinned_definition = definition(r.pinned_definition.ok_or_else(invalid)?)?;
    if summary.invocation_id != id
        || pinned_definition.routine_name != summary.routine_name
        || pinned_definition.routine_revision != summary.routine_revision
    {
        return Err(invalid());
    }
    let r = client
        .list_operations(w::ListOperationsRequest {
            invocation_id: id.clone(),
            after_cursor: 0,
            limit: 100,
        })
        .await
        .map_err(|e| status_error(&e, &credentials))?
        .into_inner();
    if r.operations.len() > 100 {
        return Err(invalid());
    }
    let operations = r
        .operations
        .into_iter()
        .map(operation)
        .collect::<Result<Vec<_>, _>>()?;
    if operations.iter().any(|op| op.invocation_id != id) {
        return Err(invalid());
    }
    Ok(ClientRoutineInvocationDetail {
        summary,
        pinned_definition,
        operations,
        next_operation_cursor: (r.next_cursor != 0).then_some(RoutineRevision(r.next_cursor)),
    })
}
#[allow(clippy::future_not_send)]
pub(super) async fn watch_routine_invocations<T>(
    transport: &T,
    _request_id: String,
    after: Option<String>,
    limit: u32,
    filters: BTreeMap<String, String>,
) -> Result<RoutineInvocationWatchStream, ClientError>
where
    T: RemoteTransport,
    T::Inner: tonic::client::GrpcService<tonic::body::Body>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::Error: Into<StdError>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody:
        Body<Data = Bytes> + Send + 'static,
    <<T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody as Body>::Error:
        Into<StdError> + Send,
{
    if !(1..=100).contains(&limit)
        || after
            .as_ref()
            .is_some_and(|c| c.is_empty() || c.len() > 1024)
    {
        return Err(invalid());
    }
    let source = source_filter(filters)?;
    let credentials = transport.request_credentials();
    let response = transport
        .routine_client(&credentials)
        .await?
        .max_decoding_message_size(8 * 1024 * 1024)
        .watch_invocations(w::WatchInvocationsRequest {
            after_cursor: after.unwrap_or_default(),
            limit,
            source_session: source.clone(),
        })
        .await;
    let stream = match response {
        Ok(r) => r.into_inner(),
        Err(e) if e.code() == tonic::Code::OutOfRange => {
            return Ok(Box::pin(futures_util::stream::once(async { Ok(reset()) })));
        }
        Err(e) => return Err(status_error(&e, &credentials)),
    };
    Ok(Box::pin(stream.map(move |r| match r {
        Ok(r) => observation(r, limit, source.as_deref()),
        Err(e) if e.code() == tonic::Code::OutOfRange => Ok(reset()),
        Err(e) => Err(status_error(&e, &credentials)),
    })))
}
fn reset() -> ClientRoutineInvocationWatchUpdate {
    ClientRoutineInvocationWatchUpdate {
        invocations: vec![],
        operations: vec![],
        cursor: String::new(),
        snapshot_complete: false,
        phase: RoutineObservationPhase::ResetRequired,
        snapshot_high_water: String::new(),
    }
}
fn source_filter(mut filters: BTreeMap<String, String>) -> Result<Option<String>, ClientError> {
    let source = filters.remove(ROUTINE_SOURCE_SESSION_ID_METADATA_KEY);
    if !filters.is_empty() {
        return Err(invalid());
    }
    if let Some(source) = &source {
        identity(source, 256)?;
    }
    Ok(source)
}
pub(crate) fn identity(v: &str, max: usize) -> Result<(), ClientError> {
    if v.is_empty() || v.len() > max {
        Err(invalid())
    } else {
        Ok(())
    }
}
pub(crate) fn invalid() -> ClientError {
    ClientError::new("invalid Routine v2 protocol projection")
}
pub(crate) fn json(v: w::StructuredValue, max: usize) -> Result<RoutineJson, ClientError> {
    if v.json.len() > max {
        return Err(invalid());
    }
    RoutineJson::parse(String::from_utf8(v.json).map_err(|_| invalid())?)
}
pub(crate) fn validate_limits(l: &w::ServerLimits) -> Result<(), ClientError> {
    if [
        l.chunk_bytes as u64,
        l.input_batch_bytes as u64,
        l.input_batch_events as u64,
        l.edge_bytes as u64,
        l.edge_chunks as u64,
        l.input_bytes,
        l.output_bytes,
        l.input_duration_ms,
        l.total_duration_ms,
        l.queue_wait_ms,
        l.idle_ms,
        l.streams_per_direction as u64,
        l.text_segments as u64,
        l.retained_text_bytes as u64,
        l.attachment_lease_ms,
        l.attachment_attach_timeout_ms,
        l.attachment_absolute_lifetime_ms,
        l.file_bytes,
        l.image_bytes,
        l.image_dimension as u64,
        l.image_pixels,
        l.helper_attachment_bytes,
        l.helper_source_bytes,
        l.helper_encoded_bytes,
        l.helper_attachments as u64,
        l.binary_output_lease_ms,
        l.text_result_bytes,
        l.text_results_encoded_bytes,
        l.text_results as u64,
        l.page_size as u64,
        l.structured_value_bytes as u64,
        l.definition_bytes as u64,
        l.definitions_per_snapshot as u64,
        l.definition_snapshot_bytes as u64,
        l.operation_projection_bytes as u64,
        l.blob_invocation_bytes,
        l.blob_principal_bytes,
        l.blob_objects as u64,
        l.blob_readers as u64,
        l.ordinary_deadline_ms,
    ]
    .contains(&0)
        || l.chunk_bytes > 65536
        || l.page_size > 100
        || l.streams_per_direction > 32
        || l.total_duration_ms > 5_400_000
        || l.input_duration_ms > 3_900_000
        || l.file_bytes > 64 * 1024 * 1024
        || l.image_bytes > 20 * 1024 * 1024
    {
        return Err(invalid());
    }
    Ok(())
}
fn definition_snapshot(
    r: w::WatchDefinitionsResponse,
) -> Result<ClientRoutineDefinitionSnapshot, ClientError> {
    identity(&r.catalog_revision, 256)?;
    let limits = r.limits.ok_or_else(invalid)?;
    validate_limits(&limits)?;
    if r.definitions.len() > limits.definitions_per_snapshot as usize {
        return Err(invalid());
    }
    let mut names = BTreeSet::new();
    let definitions = r
        .definitions
        .into_iter()
        .map(|d| {
            if d.encoded_len() > limits.definition_bytes as usize || !names.insert(d.name.clone()) {
                return Err(invalid());
            }
            definition(d)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ClientRoutineDefinitionSnapshot {
        digest: r.catalog_revision,
        definitions,
        supported_capabilities: r.supported_capabilities,
        limits,
    })
}
pub(crate) fn definition(d: w::Definition) -> Result<ClientManualRoutineDefinition, ClientError> {
    identity(&d.name, 128)?;
    identity(&d.revision, 256)?;
    identity(&d.api_version, 128)?;
    identity(&d.engine, 128)?;
    if d.entrypoints.is_empty()
        || d.entrypoints.len() > 32
        || d.ports.len() > 32
        || d.capabilities.len() > 64
    {
        return Err(invalid());
    }
    let mut names = BTreeSet::new();
    let entrypoints = d
        .entrypoints
        .into_iter()
        .map(|e| {
            identity(&e.name, 128)?;
            if !names.insert(e.name.clone()) || e.start_contexts.len() > 2 {
                return Err(invalid());
            }
            let mut contexts = BTreeSet::new();
            let start_contexts = e
                .start_contexts
                .into_iter()
                .map(|c| {
                    if !contexts.insert(c) {
                        return Err(invalid());
                    }
                    match w::StartContext::try_from(c) {
                        Ok(w::StartContext::Global) => Ok(RoutineStartContext::Global),
                        Ok(w::StartContext::Session) => Ok(RoutineStartContext::Session),
                        _ => Err(invalid()),
                    }
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(RoutineEntrypoint {
                name: e.name,
                trigger: trigger(e.trigger)?,
                selector: e.selector,
                start_contexts,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    names.clear();
    let ports = d
        .ports
        .into_iter()
        .map(|p| {
            identity(&p.name, 128)?;
            if !names.insert(p.name.clone()) || p.formats.len() > 16 {
                return Err(invalid());
            }
            let direction = match w::Direction::try_from(p.direction) {
                Ok(w::Direction::Input) => RoutineDirection::Input,
                Ok(w::Direction::Output) => RoutineDirection::Output,
                _ => return Err(invalid()),
            };
            let media = match w::Media::try_from(p.media) {
                Ok(w::Media::Audio) => RoutineMedia::Audio,
                Ok(w::Media::Text) => RoutineMedia::Text,
                Ok(w::Media::Image) => RoutineMedia::Image,
                Ok(w::Media::File) => RoutineMedia::File,
                _ => return Err(invalid()),
            };
            for f in &p.formats {
                identity(f, 128)?;
            }
            Ok(RoutinePort {
                name: p.name,
                direction,
                media,
                required: p.required,
                max_bytes: RoutineRevision(p.max_bytes),
                formats: p.formats,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let b = d.budgets.ok_or_else(invalid)?;
    if [b.value_bytes, b.result_bytes, b.host_calls, b.in_flight].contains(&0)
        || b.value_bytes > 65536
        || b.result_bytes > 65536
        || b.deadline_ms == 0
        || b.deadline_ms > 5_400_000
    {
        return Err(invalid());
    }
    Ok(ClientManualRoutineDefinition {
        routine_name: d.name,
        description: d.description,
        routine_revision: d.revision,
        api_version: d.api_version,
        engine: d.engine,
        entrypoints,
        parameters: RoutineContract::from_wire(&d.parameters.ok_or_else(invalid)?)?,
        result: RoutineContract::from_wire(&d.result.ok_or_else(invalid)?)?,
        ports,
        capabilities: d.capabilities,
        budgets: RoutineBudgets {
            value_bytes: b.value_bytes,
            result_bytes: b.result_bytes,
            host_calls: b.host_calls,
            in_flight: b.in_flight,
            deadline_ms: RoutineRevision(b.deadline_ms),
        },
    })
}
fn trigger(t: i32) -> Result<ClientRoutineTriggerKind, ClientError> {
    Ok(match w::Trigger::try_from(t) {
        Ok(w::Trigger::Manual) => ClientRoutineTriggerKind::Manual,
        Ok(w::Trigger::Event) => ClientRoutineTriggerKind::Event,
        Ok(w::Trigger::Interactive) => ClientRoutineTriggerKind::Interactive,
        Ok(w::Trigger::Tool) => ClientRoutineTriggerKind::Tool,
        Ok(w::Trigger::Inline) => ClientRoutineTriggerKind::Inline,
        Ok(w::Trigger::Mcp) => ClientRoutineTriggerKind::Mcp,
        Ok(w::Trigger::Schedule) => ClientRoutineTriggerKind::Schedule,
        Ok(w::Trigger::Progress) => ClientRoutineTriggerKind::Progress,
        Ok(w::Trigger::Lifecycle) => ClientRoutineTriggerKind::Lifecycle,
        _ => return Err(invalid()),
    })
}
fn summaries(
    rows: Vec<w::Invocation>,
    source: Option<&str>,
) -> Result<Vec<ClientRoutineInvocationSummary>, ClientError> {
    let mut ids = BTreeSet::new();
    rows.into_iter()
        .map(|r| {
            let r = summary(r)?;
            if !ids.insert(r.invocation_id.clone())
                || source.is_some_and(|s| r.source_session.as_deref() != Some(s))
            {
                return Err(invalid());
            }
            Ok(r)
        })
        .collect()
}
pub(crate) fn summary(r: w::Invocation) -> Result<ClientRoutineInvocationSummary, ClientError> {
    if r.version != 1 || r.cursor == 0 || r.observation_revision == 0 {
        return Err(invalid());
    }
    identity(&r.id, 256)?;
    identity(&r.routine, 128)?;
    identity(&r.revision, 256)?;
    identity(&r.entrypoint, 128)?;
    identity(&r.correlation, 128)?;
    if let Some(source) = &r.source_session {
        identity(source, 256)?;
    }
    let status = invocation_status(r.status)?;
    if r.result.is_some() && status != ClientRoutineInvocationStatus::Succeeded {
        return Err(invalid());
    }
    let ownership = match w::Ownership::try_from(r.ownership) {
        Ok(w::Ownership::Detached) => RoutineOwnership::Detached,
        Ok(w::Ownership::Leased) => RoutineOwnership::Leased,
        Ok(w::Ownership::Tool) => RoutineOwnership::Tool,
        _ => return Err(invalid()),
    };
    Ok(ClientRoutineInvocationSummary {
        version: r.version,
        admission_sequence: RoutineRevision(r.cursor),
        invocation_id: r.id,
        routine_name: r.routine,
        routine_revision: r.revision,
        trigger_name: r.entrypoint,
        trigger_kind: trigger(r.origin)?,
        ownership,
        correlation_id: r.correlation,
        snapshot: r.snapshot,
        source_session: r.source_session,
        status,
        admitted_at_ms: r.admitted_ms,
        updated_at_ms: r.updated_ms,
        result: r.result.map(|v| json(v, 65536)).transpose()?,
        text_results: r
            .text_results
            .into_iter()
            .map(text_result)
            .collect::<Result<_, _>>()?,
        binary_results: r
            .binary_results
            .into_iter()
            .map(blob)
            .collect::<Result<_, _>>()?,
        helpers: r
            .helpers
            .into_iter()
            .map(|h| RoutineHelperReference {
                session_id: h.session_id,
                definition_identity: h.definition_identity,
                operation_id: h.operation_id,
            })
            .collect(),
        observation_revision: RoutineRevision(r.observation_revision),
    })
}
pub(crate) fn invocation_status(s: i32) -> Result<ClientRoutineInvocationStatus, ClientError> {
    Ok(match w::InvocationStatus::try_from(s) {
        Ok(w::InvocationStatus::Admitted) => ClientRoutineInvocationStatus::Admitted,
        Ok(w::InvocationStatus::Running) => ClientRoutineInvocationStatus::Running,
        Ok(w::InvocationStatus::Succeeded) => ClientRoutineInvocationStatus::Succeeded,
        Ok(w::InvocationStatus::Failed) => ClientRoutineInvocationStatus::Failed,
        Ok(w::InvocationStatus::Interrupted) => ClientRoutineInvocationStatus::Interrupted,
        Ok(w::InvocationStatus::Uncertain) => ClientRoutineInvocationStatus::Uncertain,
        _ => return Err(invalid()),
    })
}
pub(crate) fn text_result(t: w::TextResultDescriptor) -> Result<RoutineTextResult, ClientError> {
    if t.version != 1
        || t.utf8_bytes > 1024 * 1024
        || t.encoded_charge > 8 * 1024 * 1024
        || t.sha256.len() != 64
        || !t.sha256.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(invalid());
    }
    identity(&t.id, 256)?;
    identity(&t.name, 128)?;
    Ok(RoutineTextResult {
        id: t.id,
        name: t.name,
        sha256: t.sha256,
        utf8_bytes: RoutineRevision(t.utf8_bytes),
        encoded_charge: RoutineRevision(t.encoded_charge),
        helper_id: t.helper_id,
    })
}
pub(crate) fn blob(b: w::BlobDescriptor) -> Result<RoutineBlobDescriptor, ClientError> {
    identity(&b.blob_id, 256)?;
    let m = b.metadata.ok_or_else(invalid)?;
    identity(&m.display_name, 255)?;
    identity(&m.content_type, 128)?;
    if m.sha256.len() != 32
        || m.display_name
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
        || b.remaining_validity_ms.is_some_and(|v| v > 900_000)
    {
        return Err(invalid());
    }
    let image = m
        .image
        .map(|i| {
            if i.width == 0
                || i.height == 0
                || i.width > 8192
                || i.height > 8192
                || u64::from(i.width) * u64::from(i.height) > 32_000_000
            {
                return Err(invalid());
            }
            let format = match w::ImageFormat::try_from(i.format) {
                Ok(w::ImageFormat::Png) => "png",
                Ok(w::ImageFormat::Jpeg) => "jpeg",
                Ok(w::ImageFormat::Webp) => "webp",
                _ => return Err(invalid()),
            };
            Ok(RoutineImageInfo {
                format: format.into(),
                width: i.width,
                height: i.height,
            })
        })
        .transpose()?;
    Ok(RoutineBlobDescriptor {
        blob_id: b.blob_id,
        display_name: m.display_name,
        content_type: m.content_type,
        bytes: RoutineRevision(m.bytes),
        sha256: m.sha256,
        image,
        remaining_validity_ms: b.remaining_validity_ms.map(RoutineRevision),
    })
}
pub(crate) fn operation(o: w::Operation) -> Result<RoutineOperation, ClientError> {
    if o.version != 1 || o.cursor == 0 || o.observation_revision == 0 {
        return Err(invalid());
    }
    identity(&o.id, 256)?;
    identity(&o.invocation_id, 256)?;
    identity(&o.name, 128)?;
    let status = match w::OperationStatus::try_from(o.status) {
        Ok(w::OperationStatus::Dispatched) => RoutineOperationStatus::Dispatched,
        Ok(w::OperationStatus::Completed) => RoutineOperationStatus::Completed,
        Ok(w::OperationStatus::Failed) => RoutineOperationStatus::Failed,
        Ok(w::OperationStatus::Uncertain) => RoutineOperationStatus::Uncertain,
        _ => return Err(invalid()),
    };
    Ok(RoutineOperation {
        version: o.version,
        cursor: RoutineRevision(o.cursor),
        id: o.id,
        invocation_id: o.invocation_id,
        name: o.name,
        status,
        arguments: o.arguments.map(|v| json(v, 4096)).transpose()?,
        result: o.result.map(|v| json(v, 4096)).transpose()?,
        updated_ms: o.updated_ms,
        observation_revision: RoutineRevision(o.observation_revision),
    })
}
fn observation(
    r: w::WatchInvocationsResponse,
    limit: u32,
    source: Option<&str>,
) -> Result<ClientRoutineInvocationWatchUpdate, ClientError> {
    if r.invocations.len() + r.operations.len() > limit as usize
        || r.cursor.len() > 1024
        || r.snapshot_high_water.len() > 1024
    {
        return Err(invalid());
    }
    let phase = match w::ObservationPhase::try_from(r.phase) {
        Ok(w::ObservationPhase::Snapshot) => RoutineObservationPhase::Snapshot,
        Ok(w::ObservationPhase::Changes) => RoutineObservationPhase::Changes,
        Ok(w::ObservationPhase::ResetRequired) => RoutineObservationPhase::ResetRequired,
        _ => return Err(invalid()),
    };
    if phase == RoutineObservationPhase::ResetRequired
        && (!r.invocations.is_empty() || !r.operations.is_empty())
    {
        return Err(invalid());
    }
    Ok(ClientRoutineInvocationWatchUpdate {
        invocations: summaries(r.invocations, source)?,
        operations: r
            .operations
            .into_iter()
            .map(operation)
            .collect::<Result<_, _>>()?,
        cursor: r.cursor,
        snapshot_complete: r.snapshot_complete,
        phase,
        snapshot_high_water: r.snapshot_high_water,
    })
}
