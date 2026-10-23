//! Transport-independent interaction execution. Admission owns authorization, durable identity,
//! parameter validation, pinned snapshots, and the global active-interaction limit.
//! `start` owns every task until `Interaction::join`; dropping the output receiver cancels work.

mod adapters;
mod group;
mod reservation;
mod stream;
#[cfg(test)]
mod tests;

use super::telemetry::Operation;
use super::{
    InvocationError, RoutineInvocationResult, RoutineValue,
    engine::RoutineInvocationContext,
    expression::{conditions_match, substitute_scalar},
};
use crate::resource_catalog::routine::{
    InteractionLimits, MaterializedRoutine, RoutineStreamFormat, RoutineStreamRef, RoutineTask,
};
pub(crate) use adapters::SessionAdapters;
use waltr::grpc::waltr::config::v1::Configuration;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    sync::Arc,
    time::Duration,
};
pub(crate) use stream::{Chunk, Text};
use stream::{Frame, State};
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

/// Validate adapter capabilities against pinned configuration before durable admission.
/// This performs no network I/O and does not evaluate dynamic prompts or context budgets.
pub(crate) fn validate_capabilities(
    routine: &MaterializedRoutine,
    configuration: &Configuration,
) -> Result<(), Error> {
    adapters::validate_capabilities(routine, configuration)
}

#[cfg(test)]
pub(crate) fn start_sessions(
    request: Request,
    sessions: Arc<dyn crate::session::SessionHost>,
) -> Result<Interaction, Error> {
    validate_capabilities(&request.routine, &request.configuration)?;
    start(request, Arc::new(SessionAdapters::new(sessions)))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Error {
    pub code: &'static str,
    pub message: String,
}
impl Error {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    fn cancelled() -> Self {
        Self::new("cancelled", "interaction work was cancelled")
    }
}
impl From<InvocationError> for Error {
    fn from(value: InvocationError) -> Self {
        Self::new(value.class, value.message)
    }
}

/// All formats are declared up front. Input handles exist only for supplied ports.
pub(crate) struct Request {
    pub id: String,
    pub routine: MaterializedRoutine,
    pub configuration: Arc<Configuration>,
    pub trigger: String,
    pub params: BTreeMap<String, Value>,
    pub opened_inputs: BTreeMap<String, RoutineStreamFormat>,
    pub limits: InteractionLimits,
    pub cancellation: CancellationToken,
    pub recorder: Option<super::lifecycle::LifecycleRecorder>,
}
#[derive(Debug)]
pub(crate) enum Event {
    Opened {
        port: String,
        format: RoutineStreamFormat,
    },
    Chunk {
        port: String,
        chunk: Chunk,
    },
    Closed {
        port: String,
        error: Option<Error>,
    },
    Completed(Result<Vec<RoutineInvocationResult>, Error>),
}
/// Exactly one serialized writer per input. `close` is required; EOF is not cancellation.
pub(crate) struct Input {
    sender: stream::Sender,
    cancel: CancellationToken,
    closed: bool,
    state: State,
    bytes: Arc<std::sync::atomic::AtomicUsize>,
    max_bytes: usize,
    failures: watch::Sender<Option<Error>>,
}
impl Input {
    pub(crate) async fn send(&mut self, chunk: Chunk) -> Result<(), Error> {
        self.send_accounted(chunk, true).await
    }
    /// Feed a complete request whose entire encoded byte budget was checked at admission.
    /// This retains stream/chunk validation and queue bounds without charging decoded bytes twice.
    pub(crate) async fn send_complete(&mut self, chunk: Chunk) -> Result<(), Error> {
        self.send_accounted(chunk, false).await
    }
    async fn send_accounted(&mut self, chunk: Chunk, charge: bool) -> Result<(), Error> {
        if self.closed {
            return Err(Error::new("invalid_stream", "input already closed"));
        }
        let validated = self.state.accept(&chunk).and_then(|()| {
            if !charge {
                return Ok(());
            }
            self.bytes
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |old| {
                        old.checked_add(chunk.bytes())
                            .filter(|n| *n <= self.max_bytes)
                    },
                )
                .map(|_| ())
                .map_err(|_| Error::new("resource_exhausted", "complete input byte limit exceeded"))
        });
        if let Err(error) = validated {
            self.fail(error.clone());
            return Err(error);
        }
        let result = self.sender.send(Frame::Chunk(chunk), &self.cancel).await;
        if let Err(error) = &result {
            self.fail(error.clone());
        }
        result
    }
    pub(crate) async fn close(&mut self) -> Result<(), Error> {
        if self.closed {
            return Err(Error::new("invalid_stream", "input already closed"));
        }
        if let Err(error) = self.state.close() {
            self.fail(error.clone());
            return Err(error);
        }
        self.closed = true;
        let result = self.sender.send(Frame::Close, &self.cancel).await;
        if let Err(error) = &result {
            self.fail(error.clone());
        }
        result
    }
    fn fail(&mut self, error: Error) {
        self.closed = true;
        self.failures.send_replace(Some(error));
    }
}
/// Receiver drop cancels even while the output queue is saturated.
pub(crate) struct Events {
    rx: mpsc::Receiver<Output>,
    terminal: mpsc::Receiver<Event>,
    cancel: CancellationToken,
}
struct Output {
    event: Event,
    fence: Option<stream::Fence>,
    _bytes: tokio::sync::OwnedSemaphorePermit,
}
impl Events {
    pub(crate) async fn recv(&mut self) -> Option<Event> {
        // The terminal lane has a fixed capacity of declared closes plus one completion.
        // Drain all queued data before reporting terminal state.
        loop {
            match self.rx.recv().await {
                Some(value) => {
                    let _publication = value
                        .fence
                        .as_ref()
                        .map(|f| f.gate.lock().expect("publication gate"));
                    if value.fence.as_ref().is_some_and(|f| f.token.is_cancelled()) {
                        continue;
                    }
                    return Some(value.event);
                }
                None => return self.terminal.recv().await,
            }
        }
    }
}
impl Drop for Events {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
pub(crate) struct Interaction {
    pub inputs: BTreeMap<String, Input>,
    pub events: Events,
    pub control: Control,
    owner: tokio::task::JoinHandle<()>,
}
/// Join ownership for service drivers that split input and output processing.
/// Drop `Events` or call `Control::cancel` before joining an undrained interaction.
pub(crate) struct Completion {
    owner: tokio::task::JoinHandle<()>,
}
impl Completion {
    pub(crate) async fn join(self) {
        drop(self.owner.await);
    }
}
impl Interaction {
    /// Split for independently polled transport readers/writers without detaching runtime cleanup.
    pub(crate) fn into_parts(self) -> (BTreeMap<String, Input>, Events, Control, Completion) {
        (
            self.inputs,
            self.events,
            self.control,
            Completion { owner: self.owner },
        )
    }
    /// Wait for owned cleanup after draining `events` through `Completed`.
    /// Use `shutdown` instead when the caller cannot continue draining.
    #[cfg(test)]
    pub(crate) async fn join(self) {
        drop(self.owner.await);
    }
    /// Cancel and await cleanup without draining the output queue.
    #[cfg(test)]
    pub(crate) async fn shutdown(self) {
        self.control.cancel();
        drop(self.events);
        drop(self.inputs);
        drop(self.owner.await);
    }
}
#[derive(Clone)]
pub(crate) struct Control {
    cancel: CancellationToken,
    #[cfg(test)]
    tasks: Arc<BTreeMap<String, CancellationToken>>,
}
impl Control {
    pub(crate) fn cancel(&self) {
        self.cancel.cancel();
    }
    /// Internal only. The initial RPC surface exposes whole-invocation cancellation.
    #[cfg(test)]
    pub(crate) fn cancel_task(&self, name: &str) -> bool {
        self.tasks.get(name).is_some_and(|token| {
            token.cancel();
            true
        })
    }
}

pub(crate) type AdapterFuture<'a> =
    Pin<Box<dyn Future<Output = Result<TaskResult, Error>> + Send + 'a>>;
/// Scriptable adapter boundary; production construction uses `SessionAdapters`.
pub(crate) trait Adapters: Send + Sync {
    fn execute<'a>(
        &'a self,
        task: &'a RoutineTask,
        context: &'a mut TaskContext,
    ) -> AdapterFuture<'a>;
}
#[derive(Default)]
pub(crate) struct TaskResult {
    pub values: BTreeMap<String, Value>,
    pub session_references: BTreeMap<String, String>,
}
pub(crate) struct TaskContext {
    pub request_id: String,
    pub routine_name: String,
    pub routine_revision: String,
    pub trigger: String,
    pub invocation_id: Option<String>,
    pub configuration: Arc<Configuration>,
    pub results: BTreeMap<String, BTreeMap<String, Value>>,
    pub cancellation: CancellationToken,
    pub limits: InteractionLimits,
    pub input: Option<stream::Receiver>,
    pub output: Publisher,
    expression: RoutineInvocationContext,
}
pub(crate) struct Publisher {
    senders: Vec<stream::Sender>,
    state: Option<State>,
    cancel: CancellationToken,
    fence: Option<stream::Fence>,
}
impl Publisher {
    /// Validation and budgets precede every per-consumer enqueue; slow consumers backpressure.
    pub(crate) async fn publish(&mut self, chunk: Chunk) -> Result<(), Error> {
        if self.cancel.is_cancelled() {
            return Err(Error::cancelled());
        }
        self.state
            .as_mut()
            .ok_or_else(|| Error::new("invalid_stream", "task has no output stream"))?
            .accept(&chunk)?;
        for sender in &self.senders {
            sender
                .send_fenced(
                    Frame::Chunk(chunk.clone()),
                    &self.cancel,
                    self.fence.as_ref(),
                )
                .await?;
        }
        Ok(())
    }
    async fn close(&mut self) -> Result<(), Error> {
        if let Some(state) = &mut self.state {
            state.close()?;
        }
        for sender in &self.senders {
            sender
                .send_fenced(Frame::Close, &self.cancel, self.fence.as_ref())
                .await?;
        }
        Ok(())
    }
}
#[derive(Clone)]
struct OutputSender {
    tx: mpsc::Sender<Output>,
    bytes: Arc<tokio::sync::Semaphore>,
    max: usize,
    queue_wait: Duration,
}
impl OutputSender {
    async fn send(&self, event: Event, cancel: &CancellationToken) -> Result<(), Error> {
        self.send_fenced(event, cancel, None).await
    }
    async fn send_fenced(
        &self,
        event: Event,
        cancel: &CancellationToken,
        fence: Option<stream::Fence>,
    ) -> Result<(), Error> {
        let bytes = match &event {
            Event::Chunk { chunk, .. } => chunk.bytes().max(1),
            _ => 1,
        };
        if bytes > self.max {
            return Err(Error::new(
                "resource_exhausted",
                "output chunk exceeds byte limit",
            ));
        }
        let Some((slot, permit)) =
            stream::capacity(&self.tx, &self.bytes, bytes, cancel, None, self.queue_wait).await?
        else {
            return Err(Error::cancelled());
        };
        slot.send(Output {
            event,
            fence,
            _bytes: permit,
        });
        Ok(())
    }
}
fn key(reference: &RoutineStreamRef) -> String {
    match reference {
        RoutineStreamRef::InteractionInput(n) => format!("input:{n}"),
        RoutineStreamRef::TaskOutput(n) => format!("task:{n}"),
    }
}

/// Start only after admission has validated scalar parameters and pinned the catalog snapshot.
/// Rejects a graph above the task limit instead of holding permits across live dependencies.
/// Service wiring: construct `Request`, call `start(request, Arc::new(SessionAdapters::new(host)))`,
/// forward chunks through the named `Input`s, drain `events`, then join on teardown.
pub(crate) fn start(request: Request, adapters: Arc<dyn Adapters>) -> Result<Interaction, Error> {
    request
        .limits
        .validate()
        .map_err(|m| Error::new("invalid_limits", m))?;
    if request.limits.edge_bytes > u32::MAX as usize
        || request.limits.edge_bytes > tokio::sync::Semaphore::MAX_PERMITS
        || request.limits.queued_chunks > tokio::sync::Semaphore::MAX_PERMITS
    {
        return Err(Error::new(
            "invalid_limits",
            "channel limits are not representable",
        ));
    }
    if tokio::time::Instant::now()
        .checked_add(Duration::from_secs(request.limits.invocation_seconds))
        .is_none()
    {
        return Err(Error::new(
            "invalid_limits",
            "invocation deadline is not representable",
        ));
    }
    let spec = &request.routine.definition().spec;
    if spec.tasks.len() > request.limits.active_tasks_per_interaction {
        return Err(Error::new(
            "resource_exhausted",
            "graph exceeds active task limit",
        ));
    }
    crate::resource_catalog::routine::validate_routine_models(
        request.routine.definition(),
        &request.configuration,
        "interaction",
    )
    .map_err(|_| Error::new("invalid_model", "pinned routine model selection is invalid"))?;
    let trigger = spec
        .triggers
        .iter()
        .find(|v| v.name == request.trigger)
        .and_then(|v| v.interaction.as_ref())
        .ok_or_else(|| Error::new("invalid_trigger", "interaction trigger not found"))?;
    if request.opened_inputs.iter().any(|(name, format)| {
        !trigger
            .input_ports
            .iter()
            .any(|p| p.name == *name && p.format == *format)
    }) || trigger
        .input_ports
        .iter()
        .any(|p| p.required && !request.opened_inputs.contains_key(&p.name))
    {
        return Err(Error::new(
            "invalid_input",
            "opened inputs differ from declared ports",
        ));
    }
    let cancel = request.cancellation.child_token();
    let tasks: Arc<BTreeMap<_, _>> = Arc::new(
        spec.tasks
            .iter()
            .map(|t| (t.name.clone(), cancel.child_token()))
            .collect(),
    );
    let control = Control {
        cancel: cancel.clone(),
        #[cfg(test)]
        tasks: tasks.clone(),
    };
    let mut routes: BTreeMap<String, Vec<stream::Sender>> = BTreeMap::new();
    let mut task_inputs = BTreeMap::new();
    for task in &spec.tasks {
        if let Some(reference) = task.task_spec.stream_input() {
            let (tx, rx) = stream::edge(&request.limits);
            let detached = tasks[&task.name].child_token();
            routes
                .entry(key(reference))
                .or_default()
                .push(tx.for_consumer(detached.clone()));
            task_inputs.insert(task.name.clone(), rx.with_detachment(detached));
        }
    }
    let mut output_inputs = Vec::new();
    for route in &spec.outputs {
        let (tx, rx) = stream::edge(&request.limits);
        routes.entry(key(&route.stream_from)).or_default().push(tx);
        output_inputs.push((route.port.clone(), rx));
    }
    let mut inputs = BTreeMap::new();
    let request_bytes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (input_failures, mut failures_rx) = watch::channel(None);
    let mut input_receivers = Vec::new();
    for (name, format) in &request.opened_inputs {
        let (sender, receiver) = stream::edge(&request.limits);
        inputs.insert(
            name.clone(),
            Input {
                sender,
                cancel: cancel.clone(),
                closed: false,
                state: State::new(format.clone(), request.limits.complete_request_bytes),
                bytes: request_bytes.clone(),
                max_bytes: request.limits.complete_request_bytes,
                failures: input_failures.clone(),
            },
        );
        input_receivers.push((name.clone(), format.clone(), receiver));
    }
    let output_ports = trigger.output_ports.clone();
    let (tx, rx) = mpsc::channel(request.limits.queued_chunks);
    let output = OutputSender {
        tx,
        bytes: Arc::new(tokio::sync::Semaphore::new(request.limits.edge_bytes)),
        max: request.limits.chunk_bytes,
        queue_wait: Duration::from_secs(request.limits.queue_wait_seconds),
    };
    let (terminal, terminal_rx) = mpsc::channel(output_ports.len() + 1);
    let events = Events {
        rx,
        terminal: terminal_rx,
        cancel: cancel.clone(),
    };
    let phases = Arc::new(std::sync::Mutex::new(
        spec.tasks
            .iter()
            .map(|t| (t.name.clone(), Phase::Pending))
            .collect::<BTreeMap<_, _>>(),
    ));
    let owner_cancel = cancel.clone();
    let operation = Operation::new(
        tracing::info_span!(target: "waltr_server::routine::telemetry", "routine.interaction.owner", outcome = tracing::field::Empty),
    );
    let owner_span = operation.span.clone();
    let owner = tokio::spawn(async move {
        let mut work = JoinSet::new();
        let result = tokio::select! { biased;
            () = owner_cancel.cancelled() => Err(Error::cancelled()),
            error = async { loop {
                if let Some(error) = failures_rx.borrow().clone() { return error; }
                if failures_rx.changed().await.is_err() { std::future::pending::<()>().await; }
            }} => Err(error),
            result = tokio::time::timeout(Duration::from_secs(request.limits.invocation_seconds), run(&request, adapters, &tasks, routes, task_inputs, input_receivers, output_inputs, &output_ports, &output, &owner_cancel, &mut work, &phases)) => result.unwrap_or_else(|_| Err(Error::new("deadline_exceeded", "interaction deadline exceeded"))),
        };
        owner_cancel.cancel();
        // Drain instead of aborting: adapters own interruption of their child/provider work.
        while work.join_next().await.is_some() {}
        drop(output);
        for port in &output_ports {
            if terminal
                .try_send(Event::Closed {
                    port: port.name.clone(),
                    error: result.as_ref().err().cloned(),
                })
                .is_err()
            {
                return;
            }
        }
        if let Some(recorder) = &request.recorder {
            let recorded = match &result {
                Ok(values) => {
                    recorder
                        .succeeded(values, &request.routine.definition().spec.results)
                        .await
                }
                Err(error) if error.code == "cancelled" => {
                    recorder
                        .interrupted("cancelled", "interaction cancelled")
                        .await
                }
                Err(error) => finish_failed(recorder, &phases, error).await,
            };
            if let Err(error) = recorded {
                drop(terminal.try_send(Event::Completed(Err(error.into()))));
                return;
            }
        }
        operation.result(&result);
        drop(terminal.try_send(Event::Completed(result)));
    }.instrument(owner_span));
    Ok(Interaction {
        inputs,
        events,
        control,
        owner,
    })
}

use super::task_policy::{
    DependencyReadiness, Outcomes, TaskOutcome, dependency_readiness, successful_results,
};
#[derive(Clone)]
enum Phase {
    Pending,
    Running,
    Finished,
    Failed(Error),
}
type Phases = Arc<std::sync::Mutex<BTreeMap<String, Phase>>>;
async fn finish_failed(
    recorder: &super::lifecycle::LifecycleRecorder,
    phases: &Phases,
    error: &Error,
) -> Result<(), InvocationError> {
    let phases = phases.lock().expect("task phases").clone();
    let failed = phases.iter().find(|(_, phase)| matches!(phase, Phase::Failed(error) if error.code != "cancelled" && error.code != "task_interrupted"));
    for (name, phase) in &phases {
        if failed.is_some_and(|(failed, _)| failed == name) {
            continue;
        }
        let failure = crate::persistence::routine_invocation::RoutineFailure {
            code: error.code.into(),
            message: error.message.clone(),
        };
        match phase {
            Phase::Pending => recorder.task_blocked(name, failure).await?,
            Phase::Running | Phase::Failed(_) => recorder.task_interrupted(name, failure).await?,
            Phase::Finished => (),
        }
    }
    if let Some((name, Phase::Failed(task_error))) = failed {
        recorder
            .task_failed(
                name,
                &InvocationError::new(task_error.code, &task_error.message),
            )
            .await
    } else {
        recorder
            .failed(&InvocationError::new(error.code, &error.message))
            .await
    }
}

/// Start a whole connected live component only after every external completion prerequisite.
/// Virtual input vertices connect sibling consumers just like a task producer's fanout does.
fn live_components(tasks: &[RoutineTask]) -> Vec<BTreeSet<String>> {
    let mut adjacency: BTreeMap<String, BTreeSet<String>> = tasks
        .iter()
        .map(|t| (format!("task:{}", t.name), BTreeSet::new()))
        .collect();
    for task in tasks {
        if let Some(source) = task.task_spec.stream_input() {
            let target = format!("task:{}", task.name);
            let source = key(source);
            adjacency
                .entry(target.clone())
                .or_default()
                .insert(source.clone());
            adjacency.entry(source).or_default().insert(target);
        }
    }
    let mut remaining: BTreeSet<_> = adjacency.keys().cloned().collect();
    let mut components = Vec::new();
    while let Some(first) = remaining.pop_first() {
        let mut component = BTreeSet::from([first.clone()]);
        let mut visit = vec![first];
        while let Some(vertex) = visit.pop() {
            for neighbor in &adjacency[&vertex] {
                if component.insert(neighbor.clone()) {
                    remaining.remove(neighbor);
                    visit.push(neighbor.clone());
                }
            }
        }
        let members: BTreeSet<_> = tasks
            .iter()
            .filter(|t| component.contains(&format!("task:{}", t.name)))
            .map(|t| t.name.clone())
            .collect();
        components.push(members);
    }
    components
}
fn component_prerequisites(tasks: &[RoutineTask]) -> BTreeMap<String, BTreeSet<String>> {
    let mut prerequisites = BTreeMap::new();
    for members in live_components(tasks) {
        let external: BTreeSet<_> = tasks
            .iter()
            .filter(|t| members.contains(&t.name))
            .flat_map(|t| t.run_after.iter())
            .filter(|n| !members.contains(*n))
            .cloned()
            .collect();
        for member in members {
            prerequisites.insert(member, external.clone());
        }
    }
    prerequisites
}
#[allow(clippy::too_many_arguments)]
async fn run(
    request: &Request,
    adapters: Arc<dyn Adapters>,
    tokens: &Arc<BTreeMap<String, CancellationToken>>,
    mut routes: BTreeMap<String, Vec<stream::Sender>>,
    mut task_inputs: BTreeMap<String, stream::Receiver>,
    inputs: Vec<(String, RoutineStreamFormat, stream::Receiver)>,
    outputs: Vec<(String, stream::Receiver)>,
    ports: &[crate::resource_catalog::routine::RoutineOutputPort],
    output: &OutputSender,
    cancel: &CancellationToken,
    work: &mut JoinSet<Result<(), Error>>,
    phases: &Phases,
) -> Result<Vec<RoutineInvocationResult>, Error> {
    if let Some(recorder) = &request.recorder {
        recorder.started().await?;
    }
    for port in ports {
        output
            .send(
                Event::Opened {
                    port: port.name.clone(),
                    format: port.format.clone(),
                },
                cancel,
            )
            .await?;
    }
    for (name, format, mut input) in inputs {
        let mut publisher = Publisher {
            senders: routes.remove(&format!("input:{name}")).unwrap_or_default(),
            state: Some(State::new(format, request.limits.complete_request_bytes)),
            cancel: cancel.clone(),
            fence: None,
        };
        let token = cancel.clone();
        let seconds = request.limits.input_seconds;
        work.spawn(
            async move {
                tokio::time::timeout(Duration::from_secs(seconds), async {
                    loop {
                        match input.recv(&token).await? {
                            Frame::Chunk(chunk) => {
                                publisher.publish(chunk).await?;
                            }
                            Frame::Close => {
                                publisher.close().await?;
                                return Ok(());
                            }
                            Frame::Failed(error) => return Err(error),
                        }
                    }
                })
                .await
                .unwrap_or_else(|_| Err(Error::new("deadline_exceeded", "input duration exceeded")))
            }
            .in_current_span(),
        );
    }
    for (port, mut receiver) in outputs {
        let output = output.clone();
        let cancel = cancel.clone();
        work.spawn(
            async move {
                loop {
                    let (frame, fence) = receiver.recv_fenced(&cancel).await?;
                    if fence.as_ref().is_some_and(|f| f.token.is_cancelled()) {
                        if matches!(frame, Frame::Close) {
                            return Err(Error::new(
                                "task_interrupted",
                                "output generation was replaced",
                            ));
                        }
                        continue;
                    }
                    match frame {
                        Frame::Chunk(chunk) => {
                            output
                                .send_fenced(
                                    Event::Chunk {
                                        port: port.clone(),
                                        chunk,
                                    },
                                    &cancel,
                                    fence,
                                )
                                .await?
                        }
                        Frame::Close => return Ok(()),
                        Frame::Failed(error) => return Err(error),
                    }
                }
            }
            .in_current_span(),
        );
    }
    let (results_tx, results_rx) = watch::channel(Outcomes::new());
    let groups: BTreeMap<_, _> = request
        .routine
        .definition()
        .spec
        .work_groups
        .iter()
        .filter(|g| g.on_busy != crate::resource_catalog::routine::RoutineWorkGroupBusy::Queue)
        .map(|g| (g.name.clone(), group::Group::new(g)))
        .collect();
    let expression = RoutineInvocationContext {
        event: None,
        trigger: serde_json::json!({"name": request.trigger}),
        params: request.params.clone(),
        source: None,
        source_values: Value::Null,
    };
    let queue_limits: BTreeMap<_, _> = request
        .routine
        .definition()
        .spec
        .work_groups
        .iter()
        .filter(|g| g.on_busy == crate::resource_catalog::routine::RoutineWorkGroupBusy::Queue)
        .map(|g| (g.name.clone(), g.max_queued.unwrap_or(0)))
        .collect();
    let queue_names: BTreeSet<_> = queue_limits.keys().cloned().collect();
    let reservations = reservation::Reservations::new(queue_limits);
    let mut admissions = BTreeMap::new();
    for members in live_components(&request.routine.definition().spec.tasks) {
        let cell = Arc::new(tokio::sync::OnceCell::new());
        let tasks: Vec<_> = request
            .routine
            .definition()
            .spec
            .tasks
            .iter()
            .filter(|t| members.contains(&t.name))
            .cloned()
            .collect();
        for name in members {
            admissions.insert(name, (cell.clone(), tasks.clone()));
        }
    }
    let prerequisites = component_prerequisites(&request.routine.definition().spec.tasks);
    for task in &request.routine.definition().spec.tasks {
        let dependencies = prerequisites[&task.name].clone();
        let task = task.clone();
        let mut ready = results_rx.clone();
        let results_tx = results_tx.clone();
        let adapters = adapters.clone();
        let recorder = request.recorder.clone();
        let phases = phases.clone();
        let token = tokens[&task.name].clone();
        let group = task
            .work_group
            .as_ref()
            .and_then(|n| groups.get(n))
            .cloned();
        let (admission, component_tasks) = admissions[&task.name].clone();
        let reservations = reservations.clone();
        let queue_names = queue_names.clone();
        let publisher = Publisher {
            senders: routes
                .remove(&format!("task:{}", task.name))
                .unwrap_or_default(),
            state: task
                .task_spec
                .stream_output_format()
                .map(|format| State::new(format, request.limits.complete_request_bytes)),
            cancel: token.clone(),
            fence: Some(stream::Fence {
                token: CancellationToken::new(),
                gate: Arc::new(std::sync::Mutex::new(())),
            }),
        };
        let mut context = TaskContext {
            request_id: request.id.clone(),
            routine_name: request.routine.name().to_owned(),
            routine_revision: request.routine.source_etag().to_owned(),
            trigger: request.trigger.clone(),
            invocation_id: request
                .recorder
                .as_ref()
                .map(|recorder| recorder.invocation_id().to_owned()),
            configuration: request.configuration.clone(),
            results: BTreeMap::new(),
            cancellation: token.clone(),
            limits: request.limits.clone(),
            input: task_inputs.remove(&task.name),
            output: publisher,
            expression: expression.clone(),
        };
        let invocation = cancel.clone();
        let kind = if task.task_spec.transcribe.is_some() {
            "transcribe"
        } else if task.task_spec.synthesize.is_some() {
            "synthesize"
        } else if task.task_spec.collect.is_some() {
            "collect"
        } else {
            "session_template"
        };
        let operation = Operation::new(
            tracing::info_span!(target: "waltr_server::routine::telemetry", "routine.interaction.task", task_kind = kind, outcome = tracing::field::Empty),
        );
        let task_span = operation.span.clone();
        work.spawn(async move {
            let result = async {
                loop {
                    let values = ready.borrow().clone();
                    match dependency_readiness(&dependencies, &values) {
                        DependencyReadiness::Ready => { context.results = successful_results(&values); break; }
                        DependencyReadiness::Blocked => return Err(Error::new("dependency_failed", "task prerequisite failed or was interrupted")),
                        DependencyReadiness::Waiting => {}
                    }
                    tokio::select! { biased;
                        () = token.cancelled() => return Err(Error::cancelled()),
                        result = ready.changed() => result.map_err(|_| Error::new("dependency_failed", "task dependency unavailable"))?,
                    }
                }
                if token.is_cancelled() { return Err(Error::cancelled()); }
                if !conditions_match(&task.when, &context.expression, &context.results) {
                    if let Some(input) = context.input.take() { input.detach(); }
                    context.output.close().await?;
                    if let Some(recorder) = &recorder { recorder.task_skipped(&task.name).await?; }
                    phases.lock().expect("task phases").insert(task.name.clone(), Phase::Finished);
                    results_tx.send_modify(|values| { values.insert(task.name.clone(), TaskOutcome::Skipped); });
                    return Ok(());
                }
                let resources = component_tasks.iter()
                    .filter(|t| conditions_match(&t.when, &context.expression, &context.results))
                    .filter_map(|t| t.work_group.as_ref()).filter(|n| queue_names.contains(*n)).cloned().collect();
                let _reservation = tokio::select! { biased;
                    () = token.cancelled() => return Err(Error::cancelled()),
                    reserved = admission.get_or_try_init(|| reservations.enter(resources, &invocation)) => reserved?,
                };
                let lease = if let Some(group) = group { Some(group.enter(&token).await?) } else { None };
                if let Some(lease) = &lease { context.cancellation = lease.token.clone(); context.output.cancel = lease.token.clone(); context.output.fence = Some(lease.fence()); }
                if let Some(recorder) = &recorder { recorder.task_started(&task.name).await?; }
                phases.lock().expect("task phases").insert(task.name.clone(), Phase::Running);
                let result = adapters.execute(&task, &mut context).await?;
                context.output.close().await?;
                if let Some(recorder) = &recorder { recorder.task_succeeded(&task.name, &result.values, &task.task_spec.results, result.session_references).await?; }
                phases.lock().expect("task phases").insert(task.name.clone(), Phase::Finished);
                results_tx.send_modify(|values| { values.insert(task.name.clone(), TaskOutcome::Succeeded(result.values)); });
                Ok(())
            }.await;
            if let Err(mut error) = result {
                if context.cancellation.is_cancelled() && !invocation.is_cancelled() {
                    error = Error::new("task_interrupted", "task cancelled or work group generation replaced");
                }
                // Unblock live consumers explicitly, never turn an interrupted producer into EOF.
                if error.code == "task_interrupted" {
                    token.cancel();
                    if let Some(fence) = &context.output.fence {
                        let _publication = fence.gate.lock().expect("publication gate");
                        fence.token.cancel();
                    }
                    for sender in &context.output.senders {
                        drop(sender.send(Frame::Failed(error.clone()), &invocation).await);
                    }
                }
                let outcome = if error.code == "task_interrupted" || error.code == "cancelled" {
                    TaskOutcome::Interrupted
                } else {
                    TaskOutcome::Failed
                };
                results_tx.send_modify(|values| { values.insert(task.name.clone(), outcome); });
                let mut phases = phases.lock().expect("task phases");
                if matches!(phases.get(&task.name), Some(Phase::Running)) { phases.insert(task.name.clone(), Phase::Failed(error.clone())); }
                operation.result::<()>(&Err(error.clone()));
                return Err(error);
            }
            operation.outcome("success");
            Ok(())
        }.instrument(task_span));
    }
    drop(admissions);
    drop(results_tx);
    let mut interrupted = None;
    while let Some(result) = work.join_next().await {
        match result.map_err(|_| Error::new("task_panicked", "interaction task failed to join"))? {
            Err(error) if error.code == "task_interrupted" => {
                interrupted = Some(error);
            }
            other => other?,
        }
    }
    if let Some(error) = interrupted {
        return Err(error);
    }
    let values = successful_results(&results_rx.borrow());
    request
        .routine
        .definition()
        .spec
        .results
        .iter()
        .map(|declared| {
            let value = substitute_scalar(&declared.value, &expression, &values, None, None)?;
            super::executor::validate_result_type(declared, &value)?;
            Ok(RoutineInvocationResult {
                name: declared.name.clone(),
                value: RoutineValue::from_json(value)?,
            })
        })
        .collect::<Result<Vec<_>, InvocationError>>()
        .map_err(Into::into)
}
