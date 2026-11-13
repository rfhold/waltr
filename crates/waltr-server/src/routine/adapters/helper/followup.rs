//! Invocation-local authority and joins; scheduling/history remain the ordinary coordinator's.
use super::*;
use crate::routine::ports::HelperTarget;
use futures_util::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use tokio::sync::{Notify, oneshot};
use waltr_agent::{AgentRuntime, AgentSessionRunInput, AgentSessionRunOutcome, SessionRecord};

type RunWait = Shared<BoxFuture<'static, Result<AgentSessionRunOutcome, PortError>>>;
type ResultWait = Shared<BoxFuture<'static, Result<HelperResult, PortError>>>;
pub(super) struct Opening {
    ready: oneshot::Sender<Arc<dyn HelperTarget>>,
    result: ResultWait,
}
pub(super) struct Group {
    pub cancel: CancellationToken,
    pub task: tokio::task::JoinHandle<()>,
}
struct State {
    sealed: bool,
    waits: Vec<RunWait>,
}
struct Target<S: SessionStore + Clone + Send + Sync + 'static> {
    sessions: SessionCoordinator<S>,
    id: SessionId,
    incarnation: String,
    limits: AgentRunLimits,
    deadline: tokio::time::Instant,
    cancel: CancellationToken,
    state: Arc<tokio::sync::Mutex<State>>,
    changed: Arc<Notify>,
    result: ResultWait,
}
impl<S: SessionStore + Clone + Send + Sync + 'static> Clone for Target<S> {
    fn clone(&self) -> Self {
        Self {
            sessions: self.sessions.clone(),
            id: self.id.clone(),
            incarnation: self.incarnation.clone(),
            limits: self.limits,
            deadline: self.deadline,
            cancel: self.cancel.clone(),
            state: self.state.clone(),
            changed: self.changed.clone(),
            result: self.result.clone(),
        }
    }
}
fn wait(submission: waltr_agent::AgentSessionRunSubmission) -> RunWait {
    async move {
        submission
            .handle
            .wait()
            .await
            .map_err(|_| PortError::Uncertain)
    }
    .boxed()
    .shared()
}
async fn join(waits: Vec<RunWait>) -> Result<AgentSessionRunOutcome, PortError> {
    let mut last = None;
    let mut failure = false;
    for wait in waits {
        match wait.await {
            Ok(outcome) if outcome.status == SessionStatus::Finished => last = Some(outcome),
            _ => failure = true,
        }
    }
    if failure {
        Err(PortError::Uncertain)
    } else {
        last.ok_or(PortError::Uncertain)
    }
}
impl<S: SessionStore + Clone + Send + Sync + 'static> HelperTarget for Target<S> {
    fn submit(&self, prompt: String) -> PortFuture<()> {
        let this = self.clone();
        Box::pin(async move {
            if prompt.trim().is_empty() || prompt.len() > 16384 {
                return Err(PortError::Invalid);
            }
            let mut state = this.state.lock().await;
            if state.sealed
                || this.cancel.is_cancelled()
                || tokio::time::Instant::now() >= this.deadline
                || state.waits.len() >= 17
            {
                return Err(PortError::Denied);
            }
            let submission = this
                .sessions
                .submit_owned_next_turn_prompt(
                    this.id.clone(),
                    &this.incarnation,
                    prompt,
                    this.limits,
                )
                .await
                .map_err(|_| PortError::Unavailable)?;
            state.waits.push(wait(submission));
            this.changed.notify_one();
            Ok(())
        })
    }
    fn idle(&self) -> PortFuture<()> {
        let this = self.clone();
        Box::pin(async move {
            let waits = this.state.lock().await.waits.clone();
            tokio::select! { biased;
                () = this.cancel.cancelled() => Err(PortError::Unavailable),
                () = tokio::time::sleep_until(this.deadline) => Err(PortError::Unavailable),
                result = join(waits) => result.map(|_| ()),
            }
        })
    }
    fn seal(&self) -> PortFuture<HelperResult> {
        let this = self.clone();
        Box::pin(async move {
            this.state.lock().await.sealed = true;
            this.changed.notify_one();
            this.result.await
        })
    }
}

struct PinnedRuntime {
    record: SessionRecord,
    runtime: AgentRuntime,
}
impl waltr_agent::AgentHostRuntimeFactory for PinnedRuntime {
    fn build<'a>(&'a self, record: &'a SessionRecord) -> waltr_agent::HostRuntimeFuture<'a> {
        Box::pin(async move {
            if record.id != self.record.id
                || record.metadata.incarnation != self.record.metadata.incarnation
                || record.agent_definition != self.record.agent_definition
                || record.inference_settings != self.record.inference_settings
            {
                return Err(waltr_agent::LlmError::Protocol {
                    provider: "routine",
                    message: "helper target identity changed".into(),
                });
            }
            Ok(self.runtime.clone())
        })
    }
}

pub(super) fn open<S: SessionStore + Clone + Send + Sync + 'static>(
    runner: CoordinatorHelperRunner<S>,
    request: HelperRequest,
    payload: HelperPayload,
    cancellation: CancellationToken,
) -> PortFuture<Arc<dyn HelperTarget>> {
    Box::pin(async move {
        if !payload.attachments.is_empty() {
            return Err(PortError::UnsupportedCapability);
        }
        let mut groups = runner.groups.lock().await;
        if runner.owners.lock().await.closed || groups.len() >= 16 {
            return Err(PortError::Denied);
        }
        let (ready, opened) = oneshot::channel();
        let (completed, result) = oneshot::channel();
        let result = async move { result.await.map_err(|_| PortError::Uncertain)? }
            .boxed()
            .shared();
        let cancel = cancellation.child_token();
        let execution = runner.run_internal_open(
            request,
            Some(payload),
            cancel.clone(),
            Some(Opening { ready, result }),
        );
        let task = runner.runtime.spawn(async move {
            drop(completed.send(execution.await));
        });
        groups.push(Group { cancel, task });
        drop(groups);
        opened.await.map_err(|_| PortError::Unavailable)
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run<S: SessionStore + Clone + Send + Sync + 'static>(
    runner: &CoordinatorHelperRunner<S>,
    record: &SessionRecord,
    runtime: AgentRuntime,
    limits: AgentRunLimits,
    cancel: CancellationToken,
    deadline: tokio::time::Instant,
    opening: Opening,
) -> Result<AgentSessionRunOutcome, PortError> {
    let factory = runner
        .factory
        .session_factory(runner.sessions.clone())
        .with_host_runtime_factory(Arc::new(PinnedRuntime {
            record: record.clone(),
            runtime,
        }));
    let sessions = runner.sessions.clone().with_runtime_factory(factory);
    let incarnation = record
        .metadata
        .incarnation
        .clone()
        .ok_or(PortError::Invalid)?;
    let first = sessions
        .submit_owned_input(
            record.id.clone(),
            &incarnation,
            AgentSessionRunInput::Continue,
            limits,
        )
        .await
        .map_err(|_| PortError::Unavailable)?;
    let target = Target {
        sessions,
        id: record.id.clone(),
        incarnation,
        limits,
        cancel: cancel.clone(),
        deadline,
        state: Arc::new(tokio::sync::Mutex::new(State {
            sealed: false,
            waits: vec![wait(first)],
        })),
        changed: Arc::new(Notify::new()),
        result: opening.result,
    };
    if opening.ready.send(Arc::new(target.clone())).is_err() {
        cancel.cancel();
    }
    let drain = async {
        loop {
            let changed = target.changed.notified();
            let (sealed, waits) = {
                let state = target.state.lock().await;
                (state.sealed, state.waits.clone())
            };
            let outcome = join(waits).await?;
            if sealed {
                return Ok(outcome);
            }
            changed.await;
        }
    };
    let result = tokio::select! { biased;
        () = cancel.cancelled() => Err(PortError::Unavailable),
        () = tokio::time::sleep_until(deadline) => Err(PortError::Unavailable),
        result = drain => result,
    };
    if result.is_err() {
        let waits = {
            let mut state = target.state.lock().await;
            state.sealed = true;
            state.waits.clone()
        };
        let interrupted = target
            .sessions
            .interrupt_owned(&target.id, &target.incarnation)
            .await;
        drop(join(waits).await);
        interrupted.map_err(|_| PortError::Uncertain)?;
    }
    result
}
