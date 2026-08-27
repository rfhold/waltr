use std::collections::{BTreeMap, BTreeSet};
use std::pin::Pin;

use futures_core::Stream;
use futures_util::StreamExt;
use waltr::grpc::waltr::routine::v1::{
    GetRoutineInvocationRequest, ListRoutineInvocationsRequest, RoutineEventKind,
    RoutineInputValue, RoutineInvocationStatus, RoutineScope, RoutineTaskStatus,
    RoutineTriggerKind, RoutineValue, RoutineValueType, StartManualRoutineRequest,
    WatchManualRoutinesRequest, WatchRoutineInvocationsRequest, routine_value,
    start_manual_routine_response, watch_routine_invocations_response,
};
use tonic::codegen::{Body, Bytes, StdError};

use super::{RemoteTransport, status_error};
use crate::{
    ClientError, ClientManualRoutineDefinition, ClientRoutineAdmission,
    ClientRoutineDefinitionSnapshot, ClientRoutineEventKind, ClientRoutineFailure,
    ClientRoutineFieldViolation, ClientRoutineInputDefinition, ClientRoutineInvocationDetail,
    ClientRoutineInvocationEvent, ClientRoutineInvocationPage, ClientRoutineInvocationSnapshot,
    ClientRoutineInvocationStatus, ClientRoutineInvocationSummary,
    ClientRoutineInvocationWatchUpdate, ClientRoutineResult, ClientRoutineResultDefinition,
    ClientRoutineScope, ClientRoutineStartOutcome, ClientRoutineTask, ClientRoutineTaskDescriptor,
    ClientRoutineTaskStatus, ClientRoutineTriggerKind, ClientRoutineValidationFailure,
    ClientRoutineValue, ClientRoutineValueType, StartManualRoutineArgs,
};

/// Stream of validated complete global Routine definition snapshots.
pub type RoutineDefinitionWatchStream =
    Pin<Box<dyn Stream<Item = Result<ClientRoutineDefinitionSnapshot, ClientError>>>>;
/// Stream of validated Routine history snapshots or replay events.
pub type RoutineInvocationWatchStream =
    Pin<Box<dyn Stream<Item = Result<ClientRoutineInvocationWatchUpdate, ClientError>>>>;

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
pub(super) async fn watch_manual_routines<T>(
    transport: &T,
    request_id: String,
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
        .watch_manual_routines(WatchManualRoutinesRequest {
            request_id: request_id.clone(),
        })
        .await
        .map_err(|error| status_error(&error, &credentials))?
        .into_inner()
        .map(move |response| {
            response
                .map_err(|error| status_error(&error, &credentials))
                .and_then(|response| convert_definition_snapshot(response, &request_id))
        });
    Ok(Box::pin(stream))
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
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
    let request_id = args.request_id.clone();
    let inputs = args
        .inputs
        .into_iter()
        .map(|(name, value)| RoutineInputValue {
            name,
            value: Some(RoutineValue {
                value: Some(to_proto_value(value)),
            }),
        })
        .collect();
    let response = transport
        .routine_client(&credentials)
        .await?
        .start_manual_routine(StartManualRoutineRequest {
            request_id: args.request_id,
            routine_name: args.routine_name,
            trigger_name: args.trigger_name,
            source_session_id: args.source_session_id,
            inputs,
        })
        .await
        .map_err(|error| status_error(&error, &credentials))?
        .into_inner();
    require_request_id(&response.request_id, &request_id, "Routine start")?;
    match response.outcome {
        Some(start_manual_routine_response::Outcome::Accepted(accepted))
            if !accepted.invocation_id.is_empty() =>
        {
            Ok(ClientRoutineStartOutcome::Accepted(accepted.invocation_id))
        }
        Some(start_manual_routine_response::Outcome::Accepted(_)) => Err(data_loss(
            "Routine start response has an empty invocation ID",
        )),
        Some(start_manual_routine_response::Outcome::ValidationFailure(failure)) => Ok(
            ClientRoutineStartOutcome::ValidationFailed(ClientRoutineValidationFailure {
                message: failure.message,
                violations: failure
                    .violations
                    .into_iter()
                    .map(|violation| ClientRoutineFieldViolation {
                        field_path: violation.field_path,
                        message: violation.message,
                    })
                    .collect(),
            }),
        ),
        None => Err(data_loss("Routine start response is missing its outcome")),
    }
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
pub(super) async fn list_routine_invocations<T>(
    transport: &T,
    request_id: String,
    page_size: u32,
    before_admission_sequence: Option<u64>,
    metadata_filters: BTreeMap<String, String>,
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
    let credentials = transport.request_credentials();
    let response = transport
        .routine_client(&credentials)
        .await?
        .list_routine_invocations(ListRoutineInvocationsRequest {
            request_id: request_id.clone(),
            page_size,
            before_admission_sequence,
            metadata_filters: metadata_filters.into_iter().collect(),
        })
        .await
        .map_err(|error| status_error(&error, &credentials))?
        .into_inner();
    require_request_id(&response.request_id, &request_id, "Routine history page")?;
    let invocations = convert_summaries(response.invocations)?;
    validate_newest_first(&invocations)?;
    Ok(ClientRoutineInvocationPage {
        invocations,
        older_page_token: response.older_page_token,
    })
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
pub(super) async fn get_routine_invocation<T>(
    transport: &T,
    request_id: String,
    invocation_id: String,
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
    let credentials = transport.request_credentials();
    let response = transport
        .routine_client(&credentials)
        .await?
        .get_routine_invocation(GetRoutineInvocationRequest {
            request_id: request_id.clone(),
            invocation_id: invocation_id.clone(),
        })
        .await
        .map_err(|error| status_error(&error, &credentials))?
        .into_inner();
    require_request_id(&response.request_id, &request_id, "Routine detail")?;
    let detail = convert_detail(
        response
            .invocation
            .ok_or_else(|| data_loss("Routine detail response is missing its invocation"))?,
    )?;
    if detail.summary.invocation_id != invocation_id {
        return Err(data_loss("Routine detail response invocation ID mismatch"));
    }
    Ok(detail)
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
pub(super) async fn watch_routine_invocations<T>(
    transport: &T,
    request_id: String,
    after_sequence: Option<u64>,
    snapshot_size: u32,
    metadata_filters: BTreeMap<String, String>,
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
    let credentials = transport.request_credentials();
    let stream = transport
        .routine_client(&credentials)
        .await?
        .watch_routine_invocations(WatchRoutineInvocationsRequest {
            request_id: request_id.clone(),
            after_sequence,
            snapshot_size,
            metadata_filters: metadata_filters.into_iter().collect(),
        })
        .await
        .map_err(|error| status_error(&error, &credentials))?
        .into_inner()
        .map(move |response| {
            response
                .map_err(|error| status_error(&error, &credentials))
                .and_then(|response| convert_invocation_watch_update(response, &request_id))
        });
    Ok(Box::pin(stream))
}

fn convert_definition_snapshot(
    response: waltr::grpc::waltr::routine::v1::WatchManualRoutinesResponse,
    request_id: &str,
) -> Result<ClientRoutineDefinitionSnapshot, ClientError> {
    require_request_id(&response.request_id, request_id, "Routine definition watch")?;
    if response.digest.is_empty() {
        return Err(data_loss("Routine definition snapshot has an empty digest"));
    }
    let mut identities = BTreeSet::new();
    let definitions = response
        .routines
        .into_iter()
        .map(convert_definition)
        .collect::<Result<Vec<_>, _>>()?;
    if definitions.iter().any(|definition| {
        !identities.insert((
            definition.routine_name.clone(),
            definition.trigger_name.clone(),
        ))
    }) {
        return Err(data_loss(
            "Routine definition snapshot contains duplicate identities",
        ));
    }
    Ok(ClientRoutineDefinitionSnapshot {
        digest: response.digest,
        definitions,
    })
}

fn convert_definition(
    definition: waltr::grpc::waltr::routine::v1::ManualRoutineDefinition,
) -> Result<ClientManualRoutineDefinition, ClientError> {
    if definition.routine_name.is_empty()
        || definition.trigger_name.is_empty()
        || definition.routine_revision.is_empty()
    {
        return Err(data_loss(
            "Routine definition has an empty identity or revision",
        ));
    }
    let scope = match RoutineScope::try_from(definition.scope) {
        Ok(RoutineScope::Session) => ClientRoutineScope::Session,
        _ => return Err(data_loss("Routine definition has an invalid scope")),
    };
    let mut input_names = BTreeSet::new();
    let inputs = definition
        .inputs
        .into_iter()
        .map(|input| {
            if input.name.is_empty() || !input_names.insert(input.name.clone()) {
                return Err(data_loss(
                    "Routine definition has an empty or duplicate input",
                ));
            }
            let r#type = convert_value_type(input.r#type)?;
            let default_value = input
                .default_value
                .map(|value| convert_value(Some(value)))
                .transpose()?;
            if default_value.as_ref().is_some_and(|value| {
                !matches!(
                    (r#type, value),
                    (
                        ClientRoutineValueType::String,
                        ClientRoutineValue::String(_)
                    ) | (
                        ClientRoutineValueType::Boolean,
                        ClientRoutineValue::Boolean(_)
                    )
                )
            }) {
                return Err(data_loss("Routine input default has the wrong type"));
            }
            Ok(ClientRoutineInputDefinition {
                name: input.name,
                r#type,
                required: input.required,
                default_value,
                minimum_string_length: input.minimum_string_length,
                string_pattern: input.string_pattern,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut result_names = BTreeSet::new();
    let results = definition
        .results
        .into_iter()
        .map(|result| {
            if result.name.is_empty() || !result_names.insert(result.name.clone()) {
                return Err(data_loss(
                    "Routine definition has an empty or duplicate result",
                ));
            }
            Ok(ClientRoutineResultDefinition {
                name: result.name,
                r#type: convert_value_type(result.r#type)?,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ClientManualRoutineDefinition {
        routine_name: definition.routine_name,
        trigger_name: definition.trigger_name,
        display_name: definition.display_name,
        description: definition.description,
        scope,
        inputs,
        results,
        routine_revision: definition.routine_revision,
    })
}

fn convert_invocation_watch_update(
    response: waltr::grpc::waltr::routine::v1::WatchRoutineInvocationsResponse,
    request_id: &str,
) -> Result<ClientRoutineInvocationWatchUpdate, ClientError> {
    require_request_id(&response.request_id, request_id, "Routine invocation watch")?;
    match response.update {
        Some(watch_routine_invocations_response::Update::Snapshot(snapshot)) => {
            let invocations = convert_summaries(snapshot.invocations)?;
            validate_newest_first(&invocations)?;
            Ok(ClientRoutineInvocationWatchUpdate::Snapshot(
                ClientRoutineInvocationSnapshot {
                    invocations,
                    through_sequence: snapshot.through_sequence,
                    older_page_token: snapshot.older_page_token,
                },
            ))
        }
        Some(watch_routine_invocations_response::Update::Event(event)) => Ok(
            ClientRoutineInvocationWatchUpdate::Event(convert_event(event)?),
        ),
        None => Err(data_loss(
            "Routine invocation watch update is missing its shape",
        )),
    }
}

fn convert_summaries(
    summaries: Vec<waltr::grpc::waltr::routine::v1::RoutineInvocation>,
) -> Result<Vec<ClientRoutineInvocationSummary>, ClientError> {
    let mut ids = BTreeSet::new();
    summaries
        .into_iter()
        .map(|summary| {
            let summary = convert_summary(summary)?;
            if !ids.insert(summary.invocation_id.clone()) {
                return Err(data_loss(
                    "Routine history contains duplicate invocation IDs",
                ));
            }
            Ok(summary)
        })
        .collect()
}

fn convert_summary(
    summary: waltr::grpc::waltr::routine::v1::RoutineInvocation,
) -> Result<ClientRoutineInvocationSummary, ClientError> {
    if summary.admission_sequence == 0
        || summary.invocation_id.is_empty()
        || summary.routine_name.is_empty()
        || summary.routine_revision.is_empty()
        || summary.trigger_name.is_empty()
        || summary.correlation_id.is_empty()
        || summary.latest_sequence == 0
    {
        return Err(data_loss("Routine invocation summary is incomplete"));
    }
    let status = convert_invocation_status(summary.status)?;
    if status.is_terminal() != summary.completed_at_ms.is_some()
        || (status == ClientRoutineInvocationStatus::Admitted && summary.started_at_ms.is_some())
        || (status == ClientRoutineInvocationStatus::Succeeded && summary.terminal_error.is_some())
        || (!status.is_terminal() && summary.terminal_error.is_some())
    {
        return Err(data_loss(
            "Routine invocation summary has an illegal status shape",
        ));
    }
    Ok(ClientRoutineInvocationSummary {
        admission_sequence: summary.admission_sequence,
        invocation_id: summary.invocation_id,
        routine_name: summary.routine_name,
        routine_revision: summary.routine_revision,
        trigger_name: summary.trigger_name,
        trigger_kind: convert_trigger_kind(summary.trigger_kind)?,
        correlation_id: summary.correlation_id,
        metadata: summary.metadata.into_iter().collect(),
        source_cursor: summary.source_cursor,
        status,
        admitted_at_ms: summary.admitted_at_ms,
        started_at_ms: summary.started_at_ms,
        completed_at_ms: summary.completed_at_ms,
        updated_at_ms: summary.updated_at_ms,
        terminal_error: summary.terminal_error.map(convert_failure).transpose()?,
        latest_sequence: summary.latest_sequence,
    })
}

fn convert_detail(
    detail: waltr::grpc::waltr::routine::v1::RoutineInvocationDetail,
) -> Result<ClientRoutineInvocationDetail, ClientError> {
    let summary = convert_summary(
        detail
            .summary
            .ok_or_else(|| data_loss("Routine invocation detail is missing its summary"))?,
    )?;
    let mut task_names = BTreeSet::new();
    let tasks = detail
        .tasks
        .into_iter()
        .map(|task| {
            if task.name.is_empty()
                || task.r#type.is_empty()
                || !task_names.insert(task.name.clone())
            {
                return Err(data_loss(
                    "Routine invocation detail has an invalid task declaration",
                ));
            }
            let status = convert_task_status(task.status)?;
            if status == ClientRoutineTaskStatus::Pending
                && (!task.results.is_empty()
                    || !task.session_references.is_empty()
                    || task.error.is_some())
            {
                return Err(data_loss("Pending Routine task carries terminal data"));
            }
            Ok(ClientRoutineTask {
                name: task.name,
                r#type: task.r#type,
                status,
                results: convert_results(task.results)?,
                session_references: task.session_references.into_iter().collect(),
                error: task.error.map(convert_failure).transpose()?,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let results = convert_results(detail.results)?;
    let mut previous = 0;
    for event in detail.events {
        let event = convert_event(event)?;
        if event.invocation_id != summary.invocation_id || event.sequence <= previous {
            return Err(data_loss(
                "Routine detail event history is not strictly increasing",
            ));
        }
        previous = event.sequence;
    }
    if previous != 0 && previous != summary.latest_sequence {
        return Err(data_loss(
            "Routine detail event history does not reach its summary cursor",
        ));
    }
    Ok(ClientRoutineInvocationDetail {
        summary,
        tasks,
        results,
    })
}

#[allow(clippy::too_many_lines)]
fn convert_event(
    event: waltr::grpc::waltr::routine::v1::RoutineInvocationEvent,
) -> Result<ClientRoutineInvocationEvent, ClientError> {
    if event.sequence == 0 || event.invocation_id.is_empty() {
        return Err(data_loss(
            "Routine invocation event has an empty identity or cursor",
        ));
    }
    let kind = match RoutineEventKind::try_from(event.kind) {
        Ok(RoutineEventKind::InvocationAdmitted) => ClientRoutineEventKind::InvocationAdmitted,
        Ok(RoutineEventKind::InvocationStarted) => ClientRoutineEventKind::InvocationStarted,
        Ok(RoutineEventKind::TaskStarted) => ClientRoutineEventKind::TaskStarted,
        Ok(RoutineEventKind::TaskSucceeded) => ClientRoutineEventKind::TaskSucceeded,
        Ok(RoutineEventKind::TaskSkipped) => ClientRoutineEventKind::TaskSkipped,
        Ok(RoutineEventKind::TaskBlocked) => ClientRoutineEventKind::TaskBlocked,
        Ok(RoutineEventKind::TaskFailed) => ClientRoutineEventKind::TaskFailed,
        Ok(RoutineEventKind::TaskInterrupted) => ClientRoutineEventKind::TaskInterrupted,
        Ok(RoutineEventKind::InvocationSucceeded) => ClientRoutineEventKind::InvocationSucceeded,
        Ok(RoutineEventKind::InvocationFailed) => ClientRoutineEventKind::InvocationFailed,
        Ok(RoutineEventKind::InvocationInterrupted) => {
            ClientRoutineEventKind::InvocationInterrupted
        }
        _ => return Err(data_loss("Routine invocation event has an invalid kind")),
    };
    let is_task = matches!(
        kind,
        ClientRoutineEventKind::TaskStarted
            | ClientRoutineEventKind::TaskSucceeded
            | ClientRoutineEventKind::TaskSkipped
            | ClientRoutineEventKind::TaskBlocked
            | ClientRoutineEventKind::TaskFailed
            | ClientRoutineEventKind::TaskInterrupted
    );
    if is_task
        != event
            .task_name
            .as_ref()
            .is_some_and(|name| !name.is_empty())
    {
        return Err(data_loss(
            "Routine invocation event has an invalid task shape",
        ));
    }
    let is_admission = kind == ClientRoutineEventKind::InvocationAdmitted;
    if is_admission != event.admission.is_some() {
        return Err(data_loss(
            "Routine invocation event has an invalid admission shape",
        ));
    }
    let error_allowed = matches!(
        kind,
        ClientRoutineEventKind::TaskBlocked
            | ClientRoutineEventKind::TaskFailed
            | ClientRoutineEventKind::TaskInterrupted
            | ClientRoutineEventKind::InvocationFailed
            | ClientRoutineEventKind::InvocationInterrupted
    );
    if event.error.is_some() && !error_allowed {
        return Err(data_loss(
            "Routine invocation event has an unexpected error",
        ));
    }
    let values_allowed = matches!(
        kind,
        ClientRoutineEventKind::TaskSucceeded | ClientRoutineEventKind::InvocationSucceeded
    );
    if (!event.values.is_empty() && !values_allowed)
        || (!event.session_references.is_empty() && kind != ClientRoutineEventKind::TaskSucceeded)
    {
        return Err(data_loss(
            "Routine invocation event has unexpected result data",
        ));
    }
    Ok(ClientRoutineInvocationEvent {
        sequence: event.sequence,
        invocation_id: event.invocation_id,
        kind,
        task_name: event.task_name,
        occurred_at_ms: event.occurred_at_ms,
        error: event.error.map(convert_failure).transpose()?,
        values: convert_results(event.values)?,
        session_references: event.session_references.into_iter().collect(),
        admission: event.admission.map(convert_admission).transpose()?,
    })
}

fn convert_admission(
    admission: waltr::grpc::waltr::routine::v1::RoutineAdmission,
) -> Result<ClientRoutineAdmission, ClientError> {
    if admission.admission_sequence == 0 {
        return Err(data_loss("Routine admission sequence is zero"));
    }
    if admission.routine_name.is_empty()
        || admission.routine_revision.is_empty()
        || admission.trigger_name.is_empty()
        || admission.correlation_id.is_empty()
    {
        return Err(data_loss("Routine admission metadata is incomplete"));
    }
    let mut task_names = BTreeSet::new();
    let declared_tasks = admission
        .declared_tasks
        .into_iter()
        .map(|task| {
            if task.name.is_empty()
                || task.r#type.is_empty()
                || !task_names.insert(task.name.clone())
            {
                return Err(data_loss(
                    "Routine admission has an invalid task declaration",
                ));
            }
            Ok(ClientRoutineTaskDescriptor {
                name: task.name,
                r#type: task.r#type,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ClientRoutineAdmission {
        admission_sequence: admission.admission_sequence,
        routine_name: admission.routine_name,
        routine_revision: admission.routine_revision,
        trigger_name: admission.trigger_name,
        trigger_kind: convert_trigger_kind(admission.trigger_kind)?,
        correlation_id: admission.correlation_id,
        metadata: admission.metadata.into_iter().collect(),
        source_cursor: admission.source_cursor,
        admitted_at_ms: admission.admitted_at_ms,
        declared_tasks,
    })
}

fn convert_results(
    results: Vec<waltr::grpc::waltr::routine::v1::RoutineResultValue>,
) -> Result<Vec<ClientRoutineResult>, ClientError> {
    let mut names = BTreeSet::new();
    results
        .into_iter()
        .map(|result| {
            if result.name.is_empty() || !names.insert(result.name.clone()) {
                return Err(data_loss("Routine result has an empty or duplicate name"));
            }
            Ok(ClientRoutineResult {
                name: result.name,
                value: convert_value(result.value)?,
            })
        })
        .collect()
}

fn convert_failure(
    failure: waltr::grpc::waltr::routine::v1::RoutineFailure,
) -> Result<ClientRoutineFailure, ClientError> {
    if failure.code.is_empty() || failure.message.is_empty() {
        return Err(data_loss("Routine failure is incomplete"));
    }
    Ok(ClientRoutineFailure {
        code: failure.code,
        message: failure.message,
    })
}

fn convert_value_type(value: i32) -> Result<ClientRoutineValueType, ClientError> {
    match RoutineValueType::try_from(value) {
        Ok(RoutineValueType::String) => Ok(ClientRoutineValueType::String),
        Ok(RoutineValueType::Boolean) => Ok(ClientRoutineValueType::Boolean),
        _ => Err(data_loss("Routine definition has an invalid value type")),
    }
}

fn convert_trigger_kind(value: i32) -> Result<ClientRoutineTriggerKind, ClientError> {
    match RoutineTriggerKind::try_from(value) {
        Ok(RoutineTriggerKind::Manual) => Ok(ClientRoutineTriggerKind::Manual),
        Ok(RoutineTriggerKind::Event) => Ok(ClientRoutineTriggerKind::Event),
        _ => Err(data_loss("Routine invocation has an invalid trigger kind")),
    }
}

fn convert_invocation_status(value: i32) -> Result<ClientRoutineInvocationStatus, ClientError> {
    match RoutineInvocationStatus::try_from(value) {
        Ok(RoutineInvocationStatus::Admitted) => Ok(ClientRoutineInvocationStatus::Admitted),
        Ok(RoutineInvocationStatus::Running) => Ok(ClientRoutineInvocationStatus::Running),
        Ok(RoutineInvocationStatus::Succeeded) => Ok(ClientRoutineInvocationStatus::Succeeded),
        Ok(RoutineInvocationStatus::Failed) => Ok(ClientRoutineInvocationStatus::Failed),
        Ok(RoutineInvocationStatus::Interrupted) => Ok(ClientRoutineInvocationStatus::Interrupted),
        _ => Err(data_loss("Routine invocation has an invalid status")),
    }
}

fn convert_task_status(value: i32) -> Result<ClientRoutineTaskStatus, ClientError> {
    match RoutineTaskStatus::try_from(value) {
        Ok(RoutineTaskStatus::Pending) => Ok(ClientRoutineTaskStatus::Pending),
        Ok(RoutineTaskStatus::Running) => Ok(ClientRoutineTaskStatus::Running),
        Ok(RoutineTaskStatus::Succeeded) => Ok(ClientRoutineTaskStatus::Succeeded),
        Ok(RoutineTaskStatus::Skipped) => Ok(ClientRoutineTaskStatus::Skipped),
        Ok(RoutineTaskStatus::Blocked) => Ok(ClientRoutineTaskStatus::Blocked),
        Ok(RoutineTaskStatus::Failed) => Ok(ClientRoutineTaskStatus::Failed),
        Ok(RoutineTaskStatus::Interrupted) => Ok(ClientRoutineTaskStatus::Interrupted),
        _ => Err(data_loss("Routine task has an invalid status")),
    }
}

fn validate_newest_first(
    invocations: &[ClientRoutineInvocationSummary],
) -> Result<(), ClientError> {
    if invocations
        .windows(2)
        .any(|pair| pair[0].admission_sequence <= pair[1].admission_sequence)
    {
        return Err(data_loss("Routine history is not strictly newest-first"));
    }
    Ok(())
}

fn to_proto_value(value: ClientRoutineValue) -> routine_value::Value {
    match value {
        ClientRoutineValue::String(value) => routine_value::Value::StringValue(value),
        ClientRoutineValue::Boolean(value) => routine_value::Value::BooleanValue(value),
    }
}

fn convert_value(value: Option<RoutineValue>) -> Result<ClientRoutineValue, ClientError> {
    match value.and_then(|value| value.value) {
        Some(routine_value::Value::StringValue(value)) => Ok(ClientRoutineValue::String(value)),
        Some(routine_value::Value::BooleanValue(value)) => Ok(ClientRoutineValue::Boolean(value)),
        None => Err(data_loss("Routine result is missing its typed value")),
    }
}

fn require_request_id(actual: &str, expected: &str, context: &str) -> Result<(), ClientError> {
    if actual == expected {
        Ok(())
    } else {
        Err(data_loss(&format!(
            "{context} response request ID mismatch"
        )))
    }
}

fn data_loss(message: &str) -> ClientError {
    ClientError::new(format!("data_loss: {message}"))
}

#[cfg(test)]
mod tests {
    use waltr::grpc::waltr::routine::v1::{
        RoutineAdmission, RoutineInvocationEvent, WatchManualRoutinesResponse,
        WatchRoutineInvocationsResponse,
    };

    use super::*;

    #[test]
    fn transport_rejects_request_mismatch_and_missing_watch_shape() {
        let mismatch = convert_definition_snapshot(
            WatchManualRoutinesResponse {
                request_id: "other".to_owned(),
                digest: "digest".to_owned(),
                routines: Vec::new(),
            },
            "expected",
        )
        .unwrap_err();
        assert!(mismatch.message().contains("request ID mismatch"));

        let missing = convert_invocation_watch_update(
            WatchRoutineInvocationsResponse {
                request_id: "expected".to_owned(),
                update: None,
            },
            "expected",
        )
        .unwrap_err();
        assert!(missing.message().contains("missing its shape"));
    }

    #[test]
    fn transport_rejects_unknown_enums_duplicate_definitions_and_malformed_events() {
        let mut invalid = waltr::grpc::waltr::routine::v1::ManualRoutineDefinition {
            routine_name: "fork".to_owned(),
            trigger_name: "manual".to_owned(),
            routine_revision: "revision".to_owned(),
            scope: RoutineScope::Session as i32,
            ..Default::default()
        };
        invalid.scope = RoutineScope::Unspecified as i32;
        assert!(convert_definition(invalid).is_err());

        let definition = waltr::grpc::waltr::routine::v1::ManualRoutineDefinition {
            routine_name: "fork".to_owned(),
            trigger_name: "manual".to_owned(),
            routine_revision: "revision".to_owned(),
            scope: RoutineScope::Session as i32,
            ..Default::default()
        };
        assert!(
            convert_definition_snapshot(
                WatchManualRoutinesResponse {
                    request_id: "request".to_owned(),
                    routines: vec![definition.clone(), definition],
                    digest: "digest".to_owned(),
                },
                "request",
            )
            .unwrap_err()
            .message()
            .contains("duplicate identities")
        );

        let malformed = RoutineInvocationEvent {
            sequence: 1,
            invocation_id: "invocation".to_owned(),
            kind: RoutineEventKind::TaskStarted as i32,
            task_name: None,
            ..Default::default()
        };
        assert!(
            convert_event(malformed)
                .unwrap_err()
                .message()
                .contains("task shape")
        );

        let malformed = RoutineInvocationEvent {
            sequence: 1,
            invocation_id: "invocation".to_owned(),
            kind: RoutineEventKind::InvocationStarted as i32,
            admission: Some(RoutineAdmission::default()),
            ..Default::default()
        };
        assert!(
            convert_event(malformed)
                .unwrap_err()
                .message()
                .contains("admission shape")
        );

        let distinct_sequences = RoutineInvocationEvent {
            sequence: 7,
            invocation_id: "invocation".to_owned(),
            kind: RoutineEventKind::InvocationAdmitted as i32,
            admission: Some(RoutineAdmission {
                admission_sequence: 6,
                routine_name: "fork".to_owned(),
                routine_revision: "revision".to_owned(),
                trigger_name: "manual".to_owned(),
                trigger_kind: RoutineTriggerKind::Manual as i32,
                correlation_id: "request".to_owned(),
                ..RoutineAdmission::default()
            }),
            ..RoutineInvocationEvent::default()
        };
        let converted = convert_event(distinct_sequences).expect("distinct sequences are valid");
        assert_eq!(converted.sequence, 7);
        assert_eq!(converted.admission.unwrap().admission_sequence, 6);

        let zero = RoutineInvocationEvent {
            sequence: 7,
            invocation_id: "invocation".to_owned(),
            kind: RoutineEventKind::InvocationAdmitted as i32,
            admission: Some(RoutineAdmission {
                admission_sequence: 0,
                routine_name: "fork".to_owned(),
                routine_revision: "revision".to_owned(),
                trigger_name: "manual".to_owned(),
                trigger_kind: RoutineTriggerKind::Manual as i32,
                correlation_id: "request".to_owned(),
                ..RoutineAdmission::default()
            }),
            ..RoutineInvocationEvent::default()
        };
        assert!(
            convert_event(zero)
                .unwrap_err()
                .message()
                .contains("sequence is zero")
        );
    }
}
