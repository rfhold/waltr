//! Owned streaming Routine admission and protocol adaptation.

mod ingress;
mod protocol;
mod queue;

use crate::routine::telemetry::{self, AdmissionStage, admission_result, admission_sync};
use crate::{
    persistence::routine_invocation::{
        RoutineAdmission, RoutineFailure, RoutineTaskDescriptor, RoutineTriggerKind,
    },
    resource_catalog::{
        AgentDefinitionState,
        routine::{InteractionLimits, MaterializedRoutine, RoutineInteractionTrigger},
    },
    routine::{
        self, ManualRoutineHandle, RoutineValue,
        interaction::{self, Adapters, Event},
        lifecycle::{LifecycleRecorder, now_ms},
    },
    session::SessionHost,
};
use prost::Message as _;
use std::{collections::BTreeMap, pin::Pin, sync::Arc, time::Duration};
use tokio::sync::{Semaphore, mpsc};
use tokio_stream::{Stream, StreamExt as _, wrappers::ReceiverStream};
use tonic::{Request, Response, Status};
use tracing::Instrument as _;
use waltr::grpc::waltr::routine::v1::{
    self as wire, routine_interaction_service_server::RoutineInteractionService as Api,
};

type Responses = Pin<Box<dyn Stream<Item = Result<wire::InteractResponse, Status>> + Send>>;
type Requests = Pin<Box<dyn Stream<Item = Result<wire::InteractRequest, Status>> + Send>>;

enum InputRequests {
    Native(Requests, usize),
    Complete(Vec<wire::InteractionInput>),
}

/// Shared finite admission and owned protocol drivers for both interaction RPCs.
#[derive(Clone)]
pub struct RoutineInteractionService {
    catalog: AgentDefinitionState,
    engine: ManualRoutineHandle,
    adapters: Arc<dyn Adapters>,
    limits: InteractionLimits,
    capacity: Arc<Semaphore>,
}

impl std::fmt::Debug for RoutineInteractionService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoutineInteractionService")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

struct Prepared {
    routine: MaterializedRoutine,
    configuration: Arc<waltr::grpc::waltr::config::v1::Configuration>,
    trigger: RoutineInteractionTrigger,
    trigger_name: String,
    params: BTreeMap<String, serde_json::Value>,
    correlation: Option<String>,
}

impl RoutineInteractionService {
    /// Construct with explicit host limits; invalid or unrepresentable bounds are rejected.
    ///
    /// # Errors
    /// Returns invalid argument for invalid host limits.
    pub fn new(
        catalog: AgentDefinitionState,
        engine: ManualRoutineHandle,
        sessions: Arc<dyn SessionHost>,
        limits: InteractionLimits,
    ) -> Result<Self, Status> {
        Self::with_adapters(
            catalog,
            engine,
            Arc::new(interaction::SessionAdapters::new(sessions)),
            limits,
        )
    }

    pub(crate) fn with_adapters(
        catalog: AgentDefinitionState,
        engine: ManualRoutineHandle,
        adapters: Arc<dyn Adapters>,
        limits: InteractionLimits,
    ) -> Result<Self, Status> {
        Self::validate_limits(&limits)?;
        Ok(Self {
            catalog,
            engine,
            adapters,
            capacity: Arc::new(Semaphore::new(limits.active_interactions)),
            limits,
        })
    }

    pub(crate) fn validate_limits(limits: &InteractionLimits) -> Result<(), Status> {
        limits.validate().map_err(Status::invalid_argument)?;
        if [
            limits.edge_bytes,
            limits.queued_chunks,
            limits.active_interactions,
            limits.active_tasks_per_interaction,
        ]
        .iter()
        .any(|v| *v > Semaphore::MAX_PERMITS)
            || limits.edge_bytes > u32::MAX as usize
            || limits.complete_request_bytes > u32::MAX as usize
            || tokio::time::Instant::now()
                .checked_add(Duration::from_secs(limits.invocation_seconds))
                .is_none()
        {
            return Err(Status::invalid_argument(
                "interaction limits are not representable",
            ));
        }
        Ok(())
    }

    fn prepare(&self, open: wire::OpenInteraction) -> Result<Prepared, String> {
        if open.source_session_id.is_some() {
            return Err("source sessions are not supported by interaction execution".into());
        }
        if open.routine_name.is_empty()
            || open.trigger_name.is_empty()
            || open
                .request_id
                .as_ref()
                .is_some_and(|v| v.is_empty() || v.len() > 1024 || v.chars().count() > 512)
        {
            return Err("invalid interaction identity".into());
        }
        let snapshot = self.catalog.snapshot();
        let routine = snapshot
            .routine(&open.routine_name)
            .cloned()
            .ok_or("interaction routine was not found")?;
        interaction::validate_capabilities(&routine, snapshot.configuration())
            .map_err(|error| error.message)?;
        let spec = &routine.definition().spec;
        if spec.source_session.is_some() {
            return Err("source sessions are not supported by interaction execution".into());
        }
        if spec.tasks.len() > self.limits.active_tasks_per_interaction {
            return Err("routine exceeds active task limit".into());
        }
        let trigger = spec
            .triggers
            .iter()
            .find(|v| v.name == open.trigger_name)
            .and_then(|v| v.interaction.clone())
            .ok_or("interaction trigger was not found")?;
        if trigger.output_ports.len() >= Semaphore::MAX_PERMITS {
            return Err("routine output count is not representable".into());
        }
        let mut values = BTreeMap::new();
        for param in open.params {
            let value = match param.value.and_then(|v| v.value) {
                Some(wire::routine_value::Value::StringValue(v)) => RoutineValue::String(v),
                Some(wire::routine_value::Value::BooleanValue(v)) => RoutineValue::Boolean(v),
                None => return Err("scalar parameter value is required".into()),
            };
            if values.insert(param.name, value).is_some() {
                return Err("duplicate scalar parameter".into());
            }
        }
        let params =
            routine::validate_scalar_inputs(routine.definition(), &trigger.inputs, &values)
                .map_err(|v| v.message)?;
        crate::resource_catalog::routine::validate_routine_models(
            routine.definition(),
            snapshot.configuration(),
            "interaction",
        )
        .map_err(|_| "invalid pinned model selection")?;
        Ok(Prepared {
            routine,
            configuration: Arc::new(snapshot.configuration().clone()),
            trigger,
            trigger_name: open.trigger_name,
            params,
            correlation: open.request_id,
        })
    }

    async fn launch(
        &self,
        prepared: Prepared,
        requests: InputRequests,
        operation: &telemetry::Operation,
    ) -> Result<Response<Responses>, Status> {
        let permit = self
            .capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("active interaction limit reached"))?;
        let (tx, rx) = mpsc::channel::<queue::QueuedResponse>(self.limits.queued_chunks);
        let (terminal_tx, terminal_rx) = mpsc::channel(prepared.trigger.output_ports.len() + 1);
        let limits = self.limits.clone();
        let adapters = self.adapters.clone();
        self.engine
            .own_interaction(move |owner| async move {
                let _permit = permit;
                let operation = telemetry::Operation::new(tracing::info_span!(target: "waltr_server::routine::telemetry", "routine.interaction", outcome = tracing::field::Empty));
                drive(owner, prepared, requests, adapters, limits, tx, terminal_tx, &operation)
                    .instrument(operation.span.clone()).await;
            })
            .await
            .map_err(|error| Status::unavailable(error.message))?;
        operation.outcome("accepted");
        Ok(Response::new(Box::pin(
            ReceiverStream::new(rx)
                .map(|queued| queued.response)
                .chain(ReceiverStream::new(terminal_rx)),
        )))
    }
}

#[tonic::async_trait]
impl Api for RoutineInteractionService {
    type InteractStream = Responses;
    type RunStream = Responses;

    async fn interact(
        &self,
        request: Request<tonic::Streaming<wire::InteractRequest>>,
    ) -> Result<Response<Responses>, Status> {
        let operation = telemetry::rpc(request.metadata(), false);
        let result = async {
            let mut requests = request.into_inner();
            let first = admission_result(AdmissionStage::OpenRead, requests.message())
                .await?
                .ok_or_else(|| Status::invalid_argument("first request must open interaction"))?;
            let initial_bytes = first.encoded_len();
            if initial_bytes > self.limits.complete_request_bytes {
                return Err(Status::resource_exhausted(
                    "open request exceeds byte limit",
                ));
            }
            let Some(wire::interact_request::Event::Open(open)) = first.event else {
                return Ok(validation(
                    "first request must open interaction",
                    &operation,
                ));
            };
            let prepared = match admission_sync(AdmissionStage::Prepare, || self.prepare(open)) {
                Ok(v) => v,
                Err(error) => return Ok(validation(error, &operation)),
            };
            self.launch(
                prepared,
                InputRequests::Native(Box::pin(requests), initial_bytes),
                &operation,
            )
            .await
        }
        .instrument(operation.span.clone())
        .await;
        operation.span.record(
            "grpc_status",
            result
                .as_ref()
                .err()
                .map_or(0, |status| status.code() as i32),
        );
        if result.is_err() {
            operation.outcome("failure");
        }
        result
    }

    async fn run(
        &self,
        request: Request<wire::RunInteractionRequest>,
    ) -> Result<Response<Responses>, Status> {
        let operation = telemetry::rpc(request.metadata(), true);
        let result = async {
            let request = request.into_inner();
            if request.encoded_len() > self.limits.complete_request_bytes {
                return Err(Status::resource_exhausted(
                    "complete request exceeds byte limit",
                ));
            }
            let Some(open) = request.open else {
                return Ok(validation("open is required", &operation));
            };
            let prepared = match admission_sync(AdmissionStage::Prepare, || self.prepare(open)) {
                Ok(v) => v,
                Err(error) => return Ok(validation(error, &operation)),
            };
            let requests =
                match protocol::complete_inputs(&prepared.trigger, request.inputs, &self.limits) {
                    Ok(v) => v,
                    Err(error) => return Ok(validation(error, &operation)),
                };
            self.launch(prepared, InputRequests::Complete(requests), &operation)
                .await
        }
        .instrument(operation.span.clone())
        .await;
        operation.span.record(
            "grpc_status",
            result
                .as_ref()
                .err()
                .map_or(0, |status| status.code() as i32),
        );
        if result.is_err() {
            operation.outcome("failure");
        }
        result
    }
}

fn validation(message: impl Into<String>, operation: &telemetry::Operation) -> Response<Responses> {
    operation.outcome("rejected");
    let message = message.into();
    Response::new(Box::pin(tokio_stream::iter([Ok(wire::InteractResponse {
        event: Some(wire::interact_response::Event::ValidationFailure(
            wire::RoutineValidationFailure {
                message,
                violations: Vec::new(),
            },
        )),
    })])))
}

fn response(event: wire::interact_response::Event) -> wire::InteractResponse {
    wire::InteractResponse { event: Some(event) }
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    owner: routine::InteractionOwnership,
    prepared: Prepared,
    requests: InputRequests,
    adapters: Arc<dyn Adapters>,
    limits: InteractionLimits,
    tx: mpsc::Sender<queue::QueuedResponse>,
    terminal_tx: mpsc::Sender<Result<wire::InteractResponse, Status>>,
    operation: &telemetry::Operation,
) {
    let id = format!("rinv_{}", uuid::Uuid::new_v4().simple());
    let admission = RoutineAdmission {
        invocation_id: id.clone(),
        execution_id: id.clone(),
        routine_name: prepared.routine.name().into(),
        routine_revision: prepared.routine.source_etag().into(),
        trigger_name: prepared.trigger_name.clone(),
        trigger_kind: RoutineTriggerKind::Interaction,
        correlation_id: prepared.correlation.unwrap_or_else(|| id.clone()),
        metadata: BTreeMap::new(),
        source_cursor: None,
        declared_tasks: prepared
            .routine
            .definition()
            .spec
            .tasks
            .iter()
            .map(|task| RoutineTaskDescriptor {
                name: task.name.clone(),
                task_type: if task.task_spec.transcribe.is_some() {
                    "transcribe"
                } else if task.task_spec.synthesize.is_some() {
                    "synthesize"
                } else if task.task_spec.collect.is_some() {
                    "collect"
                } else {
                    "session_template"
                }
                .into(),
            })
            .collect(),
        admitted_at_ms: now_ms(),
    };
    // Commit is never abandoned: disconnect during the transaction is handled after its outcome.
    if admission_result(
        AdmissionStage::DurableAdmission,
        owner.store.admit(admission),
    )
    .await
    .is_err()
    {
        operation.outcome("failure");
        drop(
            tx.try_send(queue::QueuedResponse::control(Err(Status::unavailable(
                "interaction admission persistence failed",
            )))),
        );
        owner.persistence_failed().await;
        return;
    }
    let _ = owner.wakeups.send(());
    let (input_finished, input_completion) = tokio::sync::watch::channel(false);
    let mut remaining_outputs = prepared.trigger.output_ports.len();
    let (output_finished, output_completion) = tokio::sync::watch::channel(remaining_outputs == 0);
    let recorder = LifecycleRecorder::new(owner.store.clone(), id.clone(), owner.wakeups.clone())
        .with_transport_completion(vec![input_completion, output_completion]);
    if tx.is_closed() || owner.cancellation.is_cancelled() {
        operation.outcome("cancelled");
        if recorder
            .interrupted("cancelled", "interaction cancelled before acceptance")
            .await
            .is_err()
        {
            owner.persistence_failed().await;
        }
        return;
    }
    // Input drivers may fail immediately; their task transitions require Running history.
    if admission_result(AdmissionStage::DurableStart, recorder.started())
        .await
        .is_err()
    {
        operation.outcome("failure");
        drop(
            tx.try_send(queue::QueuedResponse::control(Err(Status::unavailable(
                "interaction start persistence failed",
            )))),
        );
        owner.persistence_failed().await;
        return;
    }
    // This queue is empty, so acceptance cannot be overtaken by runtime output.
    if admission_sync(AdmissionStage::AcceptedEnqueue, || {
        tx.try_send(queue::QueuedResponse::control(Ok(response(
            wire::interact_response::Event::Accepted(wire::InteractionAccepted {
                invocation_id: id.clone(),
            }),
        ))))
    })
    .is_err()
    {
        operation.outcome("cancelled");
        if recorder
            .interrupted("cancelled", "interaction acceptance was not delivered")
            .await
            .is_err()
        {
            owner.persistence_failed().await;
        }
        return;
    }
    let cancellation = owner.cancellation.child_token();
    let runtime = interaction::start(
        interaction::Request {
            id,
            routine: prepared.routine,
            configuration: prepared.configuration,
            trigger: prepared.trigger_name,
            params: prepared.params,
            opened_inputs: prepared
                .trigger
                .input_ports
                .iter()
                .map(|p| (p.name.clone(), p.format.clone()))
                .collect(),
            limits: limits.clone(),
            cancellation: cancellation.clone(),
            recorder: Some(recorder.clone()),
        },
        adapters,
    );
    let runtime = match runtime {
        Ok(v) => v,
        Err(error) => {
            operation.result::<()>(&Err(error.clone()));
            if recorder
                .interrupted(error.code, &error.message)
                .await
                .is_err()
            {
                owner.persistence_failed().await;
            }
            drop(terminal_tx.try_send(Ok(protocol::completed(Err(error)))));
            return;
        }
    };
    let (inputs, mut events, control, completion) = runtime.into_parts();
    let (failure_tx, failure_rx) = tokio::sync::watch::channel(None::<interaction::Error>);
    let reader_cancel = cancellation.clone();
    let reader = async {
        let _result = protocol::read_inputs(
            requests,
            inputs,
            &prepared.trigger,
            &limits,
            &reader_cancel,
            &input_finished,
            |error| {
                // Latch before dropping any input driver can race runtime terminal recording.
                recorder.fail_transport(RoutineFailure {
                    code: error.code.into(),
                    message: error.message.clone(),
                });
                failure_tx.send_replace(Some(error.clone()));
                control.cancel();
                reader_cancel.cancel();
                input_finished.send_replace(true);
            },
        )
        .await;
    };
    let mut runtime_terminal = false;
    let response_credits = Arc::new(Semaphore::new(limits.edge_bytes));
    let invocation_deadline =
        tokio::time::Instant::now() + Duration::from_secs(limits.invocation_seconds);
    let writer = async {
        let mut ports = BTreeMap::new();
        let mut saturated = false;
        loop {
            let event = tokio::select! { biased;
                () = owner.cancellation.cancelled() => {
                    recorder.fail_transport(RoutineFailure { code: "cancelled".into(), message: "server shutdown".into() });
                    input_finished.send_replace(true);
                    output_finished.send_replace(true);
                    control.cancel(); break;
                },
                () = tx.closed() => {
                    recorder.fail_transport(RoutineFailure { code: "cancelled".into(), message: "interaction response dropped".into() });
                    input_finished.send_replace(true);
                    output_finished.send_replace(true);
                    control.cancel(); break;
                },
                event = events.recv() => event,
            };
            let Some(event) = event else {
                break;
            };
            if let Event::Completed(result) = &event {
                operation.result(result);
            }
            let persistence_failed = matches!(&event, Event::Completed(Err(error)) if error.code == "routine_persistence");
            let terminal = matches!(event, Event::Completed(_));
            let terminal_lane = matches!(event, Event::Closed { .. } | Event::Completed(_));
            let output_closed = matches!(event, Event::Closed { .. });
            let opened_port = match &event {
                Event::Opened { port, .. } => Some(port.clone()),
                _ => None,
            };
            let wire = protocol::output(event, &mut ports, failure_rx.borrow().clone());
            if persistence_failed {
                owner.persistence_failed().await;
            }
            if let Some(wire) = wire {
                if terminal_lane {
                    drop(terminal_tx.try_send(Ok(wire)));
                } else if !saturated {
                    if let Err(error) = queue::send(
                        &tx,
                        &response_credits,
                        wire,
                        &cancellation,
                        &limits,
                        invocation_deadline,
                    )
                    .await
                    {
                        if let Some(port) = opened_port {
                            ports.remove(&port);
                        }
                        let error = failure_rx.borrow().clone().unwrap_or(error);
                        recorder.fail_transport(RoutineFailure {
                            code: error.code.into(),
                            message: error.message.clone(),
                        });
                        failure_tx.send_replace(Some(error));
                        input_finished.send_replace(true);
                        control.cancel();
                        saturated = true;
                    }
                }
            }
            if output_closed {
                remaining_outputs = remaining_outputs.saturating_sub(1);
                if remaining_outputs == 0 {
                    // Runtime emits closes before recording success; drain validation first.
                    output_finished.send_replace(true);
                }
            }
            if terminal {
                runtime_terminal = true;
                break;
            }
        }
        cancellation.cancel();
    };
    tokio::join!(reader, writer);
    control.cancel();
    if !runtime_terminal {
        while let Some(event) = events.recv().await {
            if let Event::Completed(result) = event {
                operation.result(&result);
                if result.is_err_and(|error| error.code == "routine_persistence") {
                    owner.persistence_failed().await;
                }
                break;
            }
        }
    }
    drop(events);
    completion.join().await;
}
