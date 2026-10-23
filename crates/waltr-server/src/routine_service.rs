//! Durable Routine gRPC service.

use std::{collections::BTreeMap, pin::Pin};

use waltr::grpc::waltr::routine::v1::{
    GetRoutineInvocationRequest, GetRoutineInvocationResponse, ListRoutineInvocationsRequest,
    ListRoutineInvocationsResponse, ManualRoutineAccepted, ManualRoutineDefinition,
    RoutineAdmission as ProtoAdmission, RoutineEventKind as ProtoEventKind,
    RoutineFailure as ProtoFailure, RoutineFieldViolation, RoutineInputDefinition,
    RoutineInvocation, RoutineInvocationDetail, RoutineInvocationEvent as ProtoEvent,
    RoutineInvocationSnapshot, RoutineInvocationStatus as ProtoInvocationStatus,
    RoutineResultDefinition, RoutineResultValue, RoutineScope, RoutineTask,
    RoutineTaskDescriptor as ProtoTaskDescriptor, RoutineTaskStatus,
    RoutineTriggerKind as ProtoTriggerKind, RoutineValidationFailure,
    RoutineValue as ProtoRoutineValue, RoutineValueType, StartManualRoutineRequest,
    StartManualRoutineResponse, WatchManualRoutinesRequest, WatchManualRoutinesResponse,
    WatchRoutineInvocationsRequest, WatchRoutineInvocationsResponse,
    routine_service_server::RoutineService as RoutineServiceApi, routine_value,
    start_manual_routine_response, watch_routine_invocations_response,
};
use prost::Message as _;
use sha2::{Digest as _, Sha256};
use tokio_stream::{Stream, StreamExt as _, wrappers::BroadcastStream};
use tonic::{Request, Response, Status};

use crate::{
    persistence::routine_invocation::{
        RoutineEventKind, RoutineInvocationEvent, RoutineInvocationStatus, RoutineInvocationStore,
        RoutineInvocationSummary, RoutineStoreError, RoutineTriggerKind, StoredRoutineResult,
        StoredRoutineValue, validate_metadata,
    },
    resource_catalog::{
        AgentDefinitionState,
        routine::{RoutineManualScope, RoutineResultType},
    },
    routine::{InvocationError, ManualRoutineHandle, ManualRoutineInvocation, RoutineValue},
};

const DEFAULT_PAGE_SIZE: u32 = 50;
const WATCH_BATCH_SIZE: u32 = 256;

#[derive(Clone, Debug)]
/// Server adapter for Routine definitions, durable admission, and lifecycle history.
pub struct RoutineService {
    catalog: AgentDefinitionState,
    manual: ManualRoutineHandle,
    store: RoutineInvocationStore,
    wakeups: std::sync::Arc<tokio::sync::Mutex<tokio::sync::broadcast::Receiver<()>>>,
}

impl RoutineService {
    pub(crate) fn new(
        catalog: AgentDefinitionState,
        manual: ManualRoutineHandle,
        store: RoutineInvocationStore,
        wakeups: tokio::sync::broadcast::Receiver<()>,
    ) -> Self {
        Self {
            catalog,
            manual,
            store,
            wakeups: std::sync::Arc::new(tokio::sync::Mutex::new(wakeups)),
        }
    }

    async fn subscribe_wakeups(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.wakeups.lock().await.resubscribe()
    }
}

#[tonic::async_trait]
impl RoutineServiceApi for RoutineService {
    type WatchManualRoutinesStream =
        Pin<Box<dyn Stream<Item = Result<WatchManualRoutinesResponse, Status>> + Send>>;
    type WatchRoutineInvocationsStream =
        Pin<Box<dyn Stream<Item = Result<WatchRoutineInvocationsResponse, Status>> + Send>>;

    async fn watch_manual_routines(
        &self,
        request: Request<WatchManualRoutinesRequest>,
    ) -> Result<Response<Self::WatchManualRoutinesStream>, Status> {
        let request_id = request.into_inner().request_id;
        required_identifier(&request_id, "request_id")?;
        let updates = self.catalog.subscribe();
        Ok(Response::new(manual_routine_stream(
            self.catalog.clone(),
            updates,
            request_id,
        )))
    }

    async fn start_manual_routine(
        &self,
        request: Request<StartManualRoutineRequest>,
    ) -> Result<Response<StartManualRoutineResponse>, Status> {
        let invocation = invocation_from_proto(request.into_inner())?;
        let request_id = invocation.request_id.clone();
        let outcome = match self.manual.start(invocation).await {
            Ok(invocation_id) => {
                start_manual_routine_response::Outcome::Accepted(ManualRoutineAccepted {
                    invocation_id,
                })
            }
            Err(error) if is_validation_failure(&error) => {
                start_manual_routine_response::Outcome::ValidationFailure(validation_failure(error))
            }
            Err(error) => return Err(invocation_error_to_status(&error)),
        };
        Ok(Response::new(StartManualRoutineResponse {
            request_id,
            outcome: Some(outcome),
        }))
    }

    async fn list_routine_invocations(
        &self,
        request: Request<ListRoutineInvocationsRequest>,
    ) -> Result<Response<ListRoutineInvocationsResponse>, Status> {
        let request = request.into_inner();
        required_identifier(&request.request_id, "request_id")?;
        let before = request
            .before_admission_sequence
            .map(cursor_to_i64)
            .transpose()?;
        let page_size = normalized_page_size(request.page_size);
        let metadata_filters = metadata_filters(request.metadata_filters)?;
        let rows = self
            .store
            .list(before, page_size.saturating_add(1), &metadata_filters)
            .await
            .map_err(store_status)?;
        let has_older = rows.len() > page_size as usize;
        let invocations = rows
            .into_iter()
            .take(page_size as usize)
            .map(summary_to_proto)
            .collect::<Vec<_>>();
        let older_page_token = has_older
            .then(|| invocations.last().map(|row| row.admission_sequence))
            .flatten();
        Ok(Response::new(ListRoutineInvocationsResponse {
            request_id: request.request_id,
            invocations,
            older_page_token,
        }))
    }

    async fn get_routine_invocation(
        &self,
        request: Request<GetRoutineInvocationRequest>,
    ) -> Result<Response<GetRoutineInvocationResponse>, Status> {
        let request = request.into_inner();
        required_identifier(&request.request_id, "request_id")?;
        required_identifier(&request.invocation_id, "invocation_id")?;
        let invocation = load_detail(&self.store, &request.invocation_id).await?;
        Ok(Response::new(GetRoutineInvocationResponse {
            request_id: request.request_id,
            invocation: Some(invocation),
        }))
    }

    async fn watch_routine_invocations(
        &self,
        request: Request<WatchRoutineInvocationsRequest>,
    ) -> Result<Response<Self::WatchRoutineInvocationsStream>, Status> {
        let request = request.into_inner();
        required_identifier(&request.request_id, "request_id")?;
        let mut wakeups = self.subscribe_wakeups().await;
        let store = self.store.clone();
        let request_id = request.request_id;
        let after = request.after_sequence.map(cursor_to_i64).transpose()?;
        let snapshot_size = normalized_page_size(request.snapshot_size);
        let metadata_filters = metadata_filters(request.metadata_filters)?;
        let (sender, receiver) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move {
            let mut cursor = if let Some(after) = after {
                after
            } else {
                let (through, rows) = loop {
                    let before = match store.high_water().await {
                        Ok(value) => value.unwrap_or(0),
                        Err(error) => {
                            drop(sender.send(Err(store_status(error))).await);
                            return;
                        }
                    };
                    let rows = match store
                        .list(None, snapshot_size.saturating_add(1), &metadata_filters)
                        .await
                    {
                        Ok(rows) => rows,
                        Err(error) => {
                            drop(sender.send(Err(store_status(error))).await);
                            return;
                        }
                    };
                    let after = match store.high_water().await {
                        Ok(value) => value.unwrap_or(0),
                        Err(error) => {
                            drop(sender.send(Err(store_status(error))).await);
                            return;
                        }
                    };
                    if before == after {
                        break (after, rows);
                    }
                };
                let has_older = rows.len() > snapshot_size as usize;
                let invocations = rows
                    .into_iter()
                    .take(snapshot_size as usize)
                    .map(summary_to_proto)
                    .collect::<Vec<_>>();
                let older_page_token = has_older
                    .then(|| invocations.last().map(|row| row.admission_sequence))
                    .flatten();
                let response = WatchRoutineInvocationsResponse {
                    request_id: request_id.clone(),
                    update: Some(watch_routine_invocations_response::Update::Snapshot(
                        RoutineInvocationSnapshot {
                            invocations,
                            through_sequence: u64::try_from(through).unwrap_or_default(),
                            older_page_token,
                        },
                    )),
                };
                if sender.send(Ok(response)).await.is_err() {
                    return;
                }
                through
            };
            loop {
                match store
                    .events_after(cursor, WATCH_BATCH_SIZE, &metadata_filters)
                    .await
                {
                    Ok(events) if !events.is_empty() => {
                        for event in events {
                            cursor = event.sequence;
                            let response = WatchRoutineInvocationsResponse {
                                request_id: request_id.clone(),
                                update: Some(watch_routine_invocations_response::Update::Event(
                                    event_to_proto(event),
                                )),
                            };
                            if sender.send(Ok(response)).await.is_err() {
                                return;
                            }
                        }
                    }
                    Ok(_) => match wakeups.recv().await {
                        Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                    },
                    Err(error) => {
                        drop(sender.send(Err(store_status(error))).await);
                        return;
                    }
                }
            }
        });
        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(receiver),
        )))
    }
}

fn manual_routine_stream(
    catalog: AgentDefinitionState,
    updates: tokio::sync::broadcast::Receiver<crate::agent_definition::PublishedState>,
    request_id: String,
) -> Pin<Box<dyn Stream<Item = Result<WatchManualRoutinesResponse, Status>> + Send>> {
    let initial = manual_definitions(&catalog.snapshot());
    let mut last = definition_digest(&initial);
    let first = tokio_stream::once(Ok(WatchManualRoutinesResponse {
        request_id: request_id.clone(),
        routines: initial,
        digest: last.clone(),
    }));
    let updates = BroadcastStream::new(updates).filter_map(move |publication| {
        match publication {
            Ok(_) | Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(_)) => {}
        }
        let routines = manual_definitions(&catalog.snapshot());
        let digest = definition_digest(&routines);
        if digest == last {
            return None;
        }
        last.clone_from(&digest);
        Some(Ok(WatchManualRoutinesResponse {
            request_id: request_id.clone(),
            routines,
            digest,
        }))
    });
    Box::pin(first.chain(updates))
}

fn manual_definitions(
    snapshot: &crate::resource_catalog::AgentDefinitionSnapshot,
) -> Vec<ManualRoutineDefinition> {
    let mut routines = Vec::new();
    for routine in snapshot.routines() {
        let definition = routine.definition();
        for trigger in &definition.spec.triggers {
            let Some(manual) = &trigger.manual else {
                continue;
            };
            let inputs = manual
                .inputs
                .iter()
                .map(|input| {
                    let parameter = definition
                        .spec
                        .params
                        .iter()
                        .find(|parameter| parameter.name == input.name)
                        .expect("materialization validates manual input references");
                    RoutineInputDefinition {
                        name: parameter.name.clone(),
                        r#type: proto_value_type(parameter.r#type) as i32,
                        required: parameter.required,
                        minimum_string_length: parameter
                            .validation
                            .as_ref()
                            .and_then(|validation| validation.min_length)
                            .and_then(|value| u64::try_from(value).ok()),
                        string_pattern: parameter
                            .validation
                            .as_ref()
                            .and_then(|validation| validation.pattern.clone()),
                        default_value: parameter.default.as_ref().map(|value| ProtoRoutineValue {
                            value: Some(match value {
                                crate::resource_catalog::routine::RoutineScalar::String(value) => {
                                    routine_value::Value::StringValue(value.clone())
                                }
                                crate::resource_catalog::routine::RoutineScalar::Boolean(value) => {
                                    routine_value::Value::BooleanValue(*value)
                                }
                                crate::resource_catalog::routine::RoutineScalar::Integer(_) => {
                                    unreachable!(
                                        "validated Routine parameter defaults are string or boolean"
                                    )
                                }
                            }),
                        }),
                    }
                })
                .collect();
            routines.push(ManualRoutineDefinition {
                routine_name: routine.name().to_owned(),
                trigger_name: trigger.name.clone(),
                display_name: manual.display_name.clone(),
                description: definition.spec.description.clone(),
                scope: match manual.scope {
                    RoutineManualScope::Session => RoutineScope::Session as i32,
                },
                inputs,
                results: definition
                    .spec
                    .results
                    .iter()
                    .map(|result| RoutineResultDefinition {
                        name: result.name.clone(),
                        r#type: proto_value_type(result.r#type) as i32,
                    })
                    .collect(),
                routine_revision: routine.source_etag().to_owned(),
            });
        }
    }
    routines
}

fn definition_digest(definitions: &[ManualRoutineDefinition]) -> String {
    let mut digest = Sha256::new();
    for definition in definitions {
        digest.update(definition.encode_to_vec());
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn invocation_from_proto(
    request: StartManualRoutineRequest,
) -> Result<ManualRoutineInvocation, Status> {
    required_identifier(&request.request_id, "request_id")?;
    required_identifier(&request.routine_name, "routine_name")?;
    required_identifier(&request.trigger_name, "trigger_name")?;
    required_identifier(&request.source_session_id, "source_session_id")?;
    let mut inputs = BTreeMap::new();
    for input in request.inputs {
        required_identifier(&input.name, "inputs.name")?;
        let value = input
            .value
            .and_then(|value| value.value)
            .ok_or_else(|| Status::invalid_argument("inputs.value is required"))?;
        let value = match value {
            routine_value::Value::StringValue(value) => RoutineValue::String(value),
            routine_value::Value::BooleanValue(value) => RoutineValue::Boolean(value),
        };
        if inputs.insert(input.name, value).is_some() {
            return Err(Status::invalid_argument("input names must be unique"));
        }
    }
    Ok(ManualRoutineInvocation {
        request_id: request.request_id,
        routine_name: request.routine_name,
        trigger_name: request.trigger_name,
        session_id: waltr_agent::SessionId::new(request.source_session_id),
        inputs,
    })
}

fn detail_to_proto(
    summary: RoutineInvocationSummary,
    events: Vec<RoutineInvocationEvent>,
) -> RoutineInvocationDetail {
    let mut tasks = summary
        .declared_tasks
        .iter()
        .map(|descriptor| RoutineTask {
            name: descriptor.name.clone(),
            r#type: descriptor.task_type.clone(),
            status: RoutineTaskStatus::Pending as i32,
            results: Vec::new(),
            session_references: std::collections::HashMap::new(),
            error: None,
        })
        .collect::<Vec<_>>();
    let mut results = Vec::new();
    for event in &events {
        if let Some(task_name) = &event.task_name
            && let Some(task) = tasks.iter_mut().find(|task| task.name == *task_name)
        {
            task.status = task_status(event.kind) as i32;
            task.results = event
                .values
                .as_ref()
                .map(|values| values_to_proto(values))
                .unwrap_or_default();
            task.session_references = event
                .session_references
                .clone()
                .unwrap_or_default()
                .into_iter()
                .collect();
            task.error = event.error.as_ref().map(failure_to_proto);
        }
        if event.kind == RoutineEventKind::InvocationSucceeded {
            results = event
                .values
                .as_ref()
                .map(|values| values_to_proto(values))
                .unwrap_or_default();
        }
    }
    RoutineInvocationDetail {
        summary: Some(summary_to_proto(summary)),
        tasks,
        results,
        events: events.into_iter().map(event_to_proto).collect(),
    }
}

async fn load_detail(
    store: &RoutineInvocationStore,
    invocation_id: &str,
) -> Result<RoutineInvocationDetail, Status> {
    let detail = store.detail(invocation_id).await.map_err(store_status)?;
    Ok(detail_to_proto(detail.summary, detail.events))
}

fn summary_to_proto(summary: RoutineInvocationSummary) -> RoutineInvocation {
    RoutineInvocation {
        admission_sequence: u64::try_from(summary.admission_sequence).unwrap_or_default(),
        invocation_id: summary.invocation_id,
        routine_name: summary.routine_name,
        routine_revision: summary.routine_revision,
        trigger_name: summary.trigger_name,
        trigger_kind: match summary.trigger_kind {
            RoutineTriggerKind::Manual => ProtoTriggerKind::Manual,
            RoutineTriggerKind::Event => ProtoTriggerKind::Event,
            RoutineTriggerKind::Interaction => ProtoTriggerKind::Interaction,
        } as i32,
        correlation_id: summary.correlation_id,
        source_cursor: summary.source_cursor,
        status: match summary.status {
            RoutineInvocationStatus::Admitted => ProtoInvocationStatus::Admitted,
            RoutineInvocationStatus::Running => ProtoInvocationStatus::Running,
            RoutineInvocationStatus::Succeeded => ProtoInvocationStatus::Succeeded,
            RoutineInvocationStatus::Failed => ProtoInvocationStatus::Failed,
            RoutineInvocationStatus::Interrupted => ProtoInvocationStatus::Interrupted,
        } as i32,
        admitted_at_ms: summary.admitted_at_ms,
        started_at_ms: summary.started_at_ms,
        completed_at_ms: summary.completed_at_ms,
        updated_at_ms: summary.updated_at_ms,
        terminal_error: summary.terminal_error.as_ref().map(failure_to_proto),
        latest_sequence: u64::try_from(summary.latest_sequence).unwrap_or_default(),
        metadata: summary.metadata.into_iter().collect(),
    }
}

fn event_to_proto(event: RoutineInvocationEvent) -> ProtoEvent {
    let sequence = u64::try_from(event.sequence)
        .expect("persisted routine event sequences must be nonnegative");
    ProtoEvent {
        sequence,
        invocation_id: event.invocation_id,
        kind: match event.kind {
            RoutineEventKind::InvocationAdmitted => ProtoEventKind::InvocationAdmitted,
            RoutineEventKind::InvocationStarted => ProtoEventKind::InvocationStarted,
            RoutineEventKind::TaskStarted => ProtoEventKind::TaskStarted,
            RoutineEventKind::TaskSucceeded => ProtoEventKind::TaskSucceeded,
            RoutineEventKind::TaskSkipped => ProtoEventKind::TaskSkipped,
            RoutineEventKind::TaskBlocked => ProtoEventKind::TaskBlocked,
            RoutineEventKind::TaskFailed => ProtoEventKind::TaskFailed,
            RoutineEventKind::TaskInterrupted => ProtoEventKind::TaskInterrupted,
            RoutineEventKind::InvocationSucceeded => ProtoEventKind::InvocationSucceeded,
            RoutineEventKind::InvocationFailed => ProtoEventKind::InvocationFailed,
            RoutineEventKind::InvocationInterrupted => ProtoEventKind::InvocationInterrupted,
        } as i32,
        task_name: event.task_name,
        occurred_at_ms: event.occurred_at_ms,
        error: event.error.as_ref().map(failure_to_proto),
        values: event
            .values
            .as_ref()
            .map(|values| values_to_proto(values))
            .unwrap_or_default(),
        session_references: event
            .session_references
            .unwrap_or_default()
            .into_iter()
            .collect(),
        admission: event.admission.map(|admission| {
            let admission_sequence = u64::try_from(admission.admission_sequence)
                .expect("persisted aggregate admission sequences must be nonnegative");
            assert_ne!(
                admission_sequence, 0,
                "persisted aggregate admission sequence must be nonzero"
            );
            ProtoAdmission {
                routine_name: admission.routine_name,
                routine_revision: admission.routine_revision,
                trigger_name: admission.trigger_name,
                trigger_kind: match admission.trigger_kind {
                    RoutineTriggerKind::Manual => ProtoTriggerKind::Manual,
                    RoutineTriggerKind::Event => ProtoTriggerKind::Event,
                    RoutineTriggerKind::Interaction => ProtoTriggerKind::Interaction,
                } as i32,
                correlation_id: admission.correlation_id,
                source_cursor: admission.source_cursor,
                admitted_at_ms: admission.admitted_at_ms,
                declared_tasks: admission
                    .declared_tasks
                    .into_iter()
                    .map(|task| ProtoTaskDescriptor {
                        name: task.name,
                        r#type: task.task_type,
                    })
                    .collect(),
                admission_sequence,
                metadata: admission.metadata.into_iter().collect(),
            }
        }),
    }
}

fn values_to_proto(values: &[StoredRoutineResult]) -> Vec<RoutineResultValue> {
    values
        .iter()
        .map(|result| RoutineResultValue {
            name: result.name.clone(),
            value: Some(ProtoRoutineValue {
                value: Some(match &result.value {
                    StoredRoutineValue::String(value) => {
                        routine_value::Value::StringValue(value.clone())
                    }
                    StoredRoutineValue::Boolean(value) => {
                        routine_value::Value::BooleanValue(*value)
                    }
                }),
            }),
        })
        .collect()
}

fn is_validation_failure(error: &InvocationError) -> bool {
    matches!(
        error.class,
        "input"
            | "routine_not_found"
            | "trigger_not_found"
            | "source_not_found"
            | "source_precondition"
            | "source_capture"
    )
}

fn validation_failure(error: InvocationError) -> RoutineValidationFailure {
    let (field_path, message) = match error.class {
        "input" => {
            let field = error
                .message
                .split_once('`')
                .and_then(|(_, rest)| rest.split_once('`'))
                .map_or_else(|| "inputs".to_owned(), |(name, _)| format!("inputs.{name}"));
            (field, error.message)
        }
        "routine_not_found" => (
            "routine_name".to_owned(),
            "manual routine was not found".to_owned(),
        ),
        "trigger_not_found" => (
            "trigger_name".to_owned(),
            "manual routine trigger was not found".to_owned(),
        ),
        "source_not_found" => (
            "source_session_id".to_owned(),
            "source session was not found".to_owned(),
        ),
        "source_precondition" | "source_capture" => (
            "source_session_id".to_owned(),
            "source session has no usable completed state".to_owned(),
        ),
        _ => unreachable!("caller filters validation classes"),
    };
    RoutineValidationFailure {
        message: "routine invocation is invalid".to_owned(),
        violations: vec![RoutineFieldViolation {
            field_path,
            message,
        }],
    }
}

fn failure_to_proto(
    failure: &crate::persistence::routine_invocation::RoutineFailure,
) -> ProtoFailure {
    ProtoFailure {
        code: failure.code.clone(),
        message: failure.message.clone(),
    }
}

const fn task_status(kind: RoutineEventKind) -> RoutineTaskStatus {
    match kind {
        RoutineEventKind::TaskStarted => RoutineTaskStatus::Running,
        RoutineEventKind::TaskSucceeded => RoutineTaskStatus::Succeeded,
        RoutineEventKind::TaskSkipped => RoutineTaskStatus::Skipped,
        RoutineEventKind::TaskBlocked => RoutineTaskStatus::Blocked,
        RoutineEventKind::TaskFailed => RoutineTaskStatus::Failed,
        RoutineEventKind::TaskInterrupted => RoutineTaskStatus::Interrupted,
        _ => RoutineTaskStatus::Unspecified,
    }
}

const fn proto_value_type(value: RoutineResultType) -> RoutineValueType {
    match value {
        RoutineResultType::String => RoutineValueType::String,
        RoutineResultType::Boolean => RoutineValueType::Boolean,
    }
}

fn normalized_page_size(value: u32) -> u32 {
    if value == 0 {
        DEFAULT_PAGE_SIZE
    } else {
        // The store accepts at most 256 rows, reserving one row detects an older page.
        value.min(255)
    }
}

fn metadata_filters(
    filters: std::collections::HashMap<String, String>,
) -> Result<BTreeMap<String, String>, Status> {
    let filters = filters.into_iter().collect::<BTreeMap<_, _>>();
    validate_metadata(&filters).map_err(store_status)?;
    Ok(filters)
}

fn cursor_to_i64(value: u64) -> Result<i64, Status> {
    i64::try_from(value).map_err(|_| Status::invalid_argument("routine cursor exceeds i64::MAX"))
}

fn store_status(error: RoutineStoreError) -> Status {
    match error {
        RoutineStoreError::Invalid(message) => Status::invalid_argument(message),
        RoutineStoreError::NotFound => Status::not_found("routine invocation was not found"),
        RoutineStoreError::Conflict => {
            Status::failed_precondition("routine invocation state changed")
        }
        RoutineStoreError::Persistence => Status::internal("routine invocation persistence failed"),
    }
}

fn invocation_error_to_status(error: &InvocationError) -> Status {
    let (code, message) = match error.class {
        "input" => (tonic::Code::InvalidArgument, "routine inputs are invalid"),
        "routine_not_found" => (tonic::Code::NotFound, "manual routine was not found"),
        "trigger_not_found" => (
            tonic::Code::NotFound,
            "manual routine trigger was not found",
        ),
        "source_not_found" => (tonic::Code::NotFound, "source session was not found"),
        "source_precondition" | "source_capture" => (
            tonic::Code::FailedPrecondition,
            "source session has no usable completed state",
        ),
        "source_exhausted"
        | "input_exhausted"
        | "expanded_string"
        | "expanded_value"
        | "expanded_prompt"
        | "metadata_key_size"
        | "metadata_value_size" => (
            tonic::Code::ResourceExhausted,
            "routine invocation exceeded a resource limit",
        ),
        "shutdown" | "reconciling" => (tonic::Code::Unavailable, "routine engine is unavailable"),
        _ => (tonic::Code::Internal, "routine invocation failed"),
    };
    Status::new(code, message)
}

fn required_identifier(value: &str, field: &str) -> Result<(), Status> {
    if value.trim().is_empty() {
        Err(Status::invalid_argument(format!(
            "{field} must not be empty"
        )))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn state() -> AgentDefinitionState {
        let materializer = crate::catalog::CatalogMaterializer::load(
            &crate::test_support::default_catalog_loader(),
        )
        .await
        .unwrap();
        let root = crate::test_support::temp_root();
        let resource_dir = root.join(".waltr/resources/mcp-servers");
        std::fs::create_dir_all(&resource_dir).unwrap();
        std::fs::write(
            resource_dir.join("waltr.yaml"),
            "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata:\n  name: waltr\nspec:\n  transport: streamableHttp\n  url: https://waltr.example/base\n  path: /mcp\n",
        )
        .unwrap();
        AgentDefinitionState::load_unified(root, materializer).unwrap()
    }

    #[tokio::test]
    async fn definition_watch_recovers_from_lag_with_authoritative_projection() {
        let state = state().await;
        let updates = state.subscribe();
        let mut stream = manual_routine_stream(state.clone(), updates, "lag".to_owned());
        let initial = stream.next().await.unwrap().unwrap();
        assert_eq!(
            initial
                .routines
                .iter()
                .find(|routine| routine.routine_name == "fork")
                .unwrap()
                .description,
            "Compact a durable session snapshot into a new ordinary session."
        );

        let crate::resource_catalog::AgentDefinitionInspection::Resource(detail) =
            state.inspect(Some("routines/fork.yaml")).unwrap()
        else {
            panic!("routine detail expected")
        };
        let source = detail.source.unwrap();
        let mut etag = detail.persisted_etag;
        for index in 0..40 {
            let replacement = source.replace(
                "Compact a durable session snapshot into a new ordinary session.",
                &format!("Lag revision {index}."),
            );
            etag = state
                .replace("routines/fork.yaml", replacement, &etag)
                .await
                .unwrap()
                .persisted_etag;
        }

        let recovered = stream.next().await.unwrap().unwrap();
        assert_eq!(
            recovered
                .routines
                .iter()
                .find(|routine| routine.routine_name == "fork")
                .unwrap()
                .description,
            "Lag revision 39."
        );
        assert_ne!(recovered.digest, initial.digest);
    }

    fn admission(
        id: &str,
        tasks: &[&str],
    ) -> crate::persistence::routine_invocation::RoutineAdmission {
        crate::persistence::routine_invocation::RoutineAdmission {
            invocation_id: id.to_owned(),
            execution_id: format!("execution-{id}"),
            routine_name: "routine".to_owned(),
            routine_revision: "revision".to_owned(),
            trigger_name: "manual".to_owned(),
            trigger_kind: RoutineTriggerKind::Manual,
            correlation_id: format!("request-{id}"),
            metadata: BTreeMap::from([(
                "waltr.routine.source_session_id".to_owned(),
                "source".to_owned(),
            )]),
            source_cursor: Some("7".to_owned()),
            declared_tasks: tasks
                .iter()
                .map(
                    |name| crate::persistence::routine_invocation::RoutineTaskDescriptor {
                        name: (*name).to_owned(),
                        task_type: "session_template".to_owned(),
                    },
                )
                .collect(),
            admitted_at_ms: 50,
        }
    }

    #[tokio::test]
    async fn history_and_replay_preserve_distinct_interaction_trigger_kind() {
        let persistence = crate::persistence::Persistence::initialize(
            crate::persistence::DatabaseConfig::for_tests().unwrap(),
        )
        .await
        .unwrap();
        let store = persistence.routine_invocation_store();
        for (id, kind, expected) in [
            (
                "manual",
                RoutineTriggerKind::Manual,
                ProtoTriggerKind::Manual,
            ),
            ("event", RoutineTriggerKind::Event, ProtoTriggerKind::Event),
            (
                "interaction",
                RoutineTriggerKind::Interaction,
                ProtoTriggerKind::Interaction,
            ),
        ] {
            let mut input = admission(id, &[]);
            input.trigger_kind = kind;
            store.admit(input).await.unwrap();
            let detail = load_detail(&store, id).await.unwrap();
            assert_eq!(detail.summary.unwrap().trigger_kind, expected as i32);
            assert_eq!(
                detail.events[0].admission.as_ref().unwrap().trigger_kind,
                expected as i32
            );
        }
        assert_eq!(ProtoTriggerKind::Interaction as i32, 3);
    }

    #[tokio::test]
    async fn replay_projects_true_aggregate_admission_sequence() {
        let persistence = crate::persistence::Persistence::initialize(
            crate::persistence::DatabaseConfig::for_tests().unwrap(),
        )
        .await
        .unwrap();
        let store = persistence.routine_invocation_store();
        store.admit(admission("first", &[])).await.unwrap();
        store.start_invocation("first", 60).await.unwrap();
        store.finish_success("first", Vec::new(), 70).await.unwrap();
        let (second, second_event) = store
            .admit(admission("second", &["second", "first"]))
            .await
            .unwrap();
        assert_ne!(second.admission_sequence, second_event.sequence);

        let restarted = persistence.routine_invocation_store();
        let replayed = restarted
            .events_after(second_event.sequence - 1, 1, &BTreeMap::new())
            .await
            .unwrap()
            .remove(0);
        let projected = event_to_proto(replayed);
        let admission = projected.admission.unwrap();
        assert_eq!(admission.routine_name, "routine");
        assert_eq!(admission.routine_revision, "revision");
        assert_eq!(
            admission.admission_sequence,
            u64::try_from(second.admission_sequence).unwrap()
        );
        assert_ne!(admission.admission_sequence, projected.sequence);
        assert_ne!(admission.admission_sequence, 0);
        assert_eq!(
            admission.metadata.get("waltr.routine.source_session_id"),
            Some(&"source".to_owned())
        );
        assert_eq!(
            admission
                .declared_tasks
                .iter()
                .map(|task| task.name.as_str())
                .collect::<Vec<_>>(),
            ["second", "first"]
        );
    }

    #[test]
    fn replay_projection_keeps_result_order() {
        let projected = event_to_proto(RoutineInvocationEvent {
            sequence: 41,
            invocation_id: "invocation".to_owned(),
            kind: RoutineEventKind::TaskSucceeded,
            task_name: Some("task".to_owned()),
            occurred_at_ms: 50,
            error: None,
            values: Some(vec![
                StoredRoutineResult {
                    name: "zeta".to_owned(),
                    value: StoredRoutineValue::String("first".to_owned()),
                },
                StoredRoutineResult {
                    name: "alpha".to_owned(),
                    value: StoredRoutineValue::Boolean(true),
                },
            ]),
            session_references: None,
            admission: None,
        });
        assert_eq!(
            projected
                .values
                .iter()
                .map(|result| result.name.as_str())
                .collect::<Vec<_>>(),
            ["zeta", "alpha"]
        );
    }

    #[tokio::test]
    async fn detail_projection_uses_consistent_store_contract_and_maps_not_found() {
        let persistence = crate::persistence::Persistence::initialize(
            crate::persistence::DatabaseConfig::for_tests().unwrap(),
        )
        .await
        .unwrap();
        let store = persistence.routine_invocation_store();
        let (summary, _) = store.admit(admission("detail", &[])).await.unwrap();
        let detail = load_detail(&store, "detail").await.unwrap();
        assert_eq!(
            detail.summary.as_ref().unwrap().admission_sequence,
            u64::try_from(summary.admission_sequence).unwrap()
        );
        assert_eq!(detail.events.len(), 1);

        let error = load_detail(&store, "missing").await.unwrap_err();
        assert_eq!(error.code(), tonic::Code::NotFound);
    }
}
