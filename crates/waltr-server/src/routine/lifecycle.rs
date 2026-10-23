use std::collections::BTreeMap;

use serde_json::Value;
use tokio::sync::broadcast;

use crate::persistence::routine_invocation::{
    RoutineFailure, RoutineInvocationStore, RoutineTaskTransition, StoredRoutineResult,
    StoredRoutineValue,
};
use crate::resource_catalog::routine::RoutineResult;

use super::InvocationError;

#[derive(Clone, Debug)]
pub(crate) struct LifecycleRecorder {
    store: RoutineInvocationStore,
    invocation_id: String,
    wakeups: broadcast::Sender<()>,
    // Store appends CAS the invocation sequence; concurrent task transitions share one gate.
    writes: std::sync::Arc<tokio::sync::Mutex<()>>,
    transport_failure: std::sync::Arc<std::sync::Mutex<Option<RoutineFailure>>>,
    transport_completion: Vec<tokio::sync::watch::Receiver<bool>>,
    started: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl LifecycleRecorder {
    pub(crate) fn invocation_id(&self) -> &str {
        &self.invocation_id
    }

    pub(crate) fn new(
        store: RoutineInvocationStore,
        invocation_id: String,
        wakeups: broadcast::Sender<()>,
    ) -> Self {
        Self {
            store,
            invocation_id,
            wakeups,
            writes: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            transport_failure: Default::default(),
            transport_completion: Vec::new(),
            started: Default::default(),
        }
    }

    pub(crate) fn with_transport_completion(
        mut self,
        completion: Vec<tokio::sync::watch::Receiver<bool>>,
    ) -> Self {
        self.transport_completion = completion;
        self
    }

    async fn transport_terminal(&self) -> Result<bool, InvocationError> {
        let failure = self
            .transport_failure
            .lock()
            .expect("transport failure")
            .clone();
        if let Some(failure) = failure {
            if failure.code == "cancelled" {
                self.store
                    .finish_interruption(&self.invocation_id, failure, now_ms())
                    .await
                    .map_err(persistence_error)?;
            } else {
                self.store
                    .finish_failure(&self.invocation_id, failure, now_ms())
                    .await
                    .map_err(persistence_error)?;
            }
            self.wake();
            return Ok(true);
        }
        Ok(false)
    }

    pub(crate) async fn started(&self) -> Result<(), InvocationError> {
        let _write = self.writes.lock().await;
        if self.started.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }
        self.store
            .start_invocation(&self.invocation_id, now_ms())
            .await
            .map_err(persistence_error)?;
        self.started
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.wake();
        Ok(())
    }

    pub(crate) async fn task_started(&self, task: &str) -> Result<(), InvocationError> {
        self.transition(
            task,
            RoutineTaskTransition::Started {
                session_references: BTreeMap::new(),
            },
        )
        .await
    }

    pub(crate) async fn task_succeeded(
        &self,
        task: &str,
        values: &BTreeMap<String, Value>,
        declared_results: &[RoutineResult],
        session_references: BTreeMap<String, String>,
    ) -> Result<(), InvocationError> {
        self.transition(
            task,
            RoutineTaskTransition::Succeeded {
                values: stored_values(values, declared_results)?,
                session_references,
            },
        )
        .await
    }

    pub(crate) async fn task_skipped(&self, task: &str) -> Result<(), InvocationError> {
        self.transition(task, RoutineTaskTransition::Skipped).await
    }

    pub(crate) async fn task_blocked(
        &self,
        task: &str,
        failure: RoutineFailure,
    ) -> Result<(), InvocationError> {
        self.transition(task, RoutineTaskTransition::Blocked(failure))
            .await
    }

    pub(crate) async fn task_interrupted(
        &self,
        task: &str,
        failure: RoutineFailure,
    ) -> Result<(), InvocationError> {
        self.transition(task, RoutineTaskTransition::Interrupted(failure))
            .await
    }

    async fn transition(
        &self,
        task: &str,
        transition: RoutineTaskTransition,
    ) -> Result<(), InvocationError> {
        let _write = self.writes.lock().await;
        self.store
            .transition_task(&self.invocation_id, task, transition, now_ms())
            .await
            .map_err(persistence_error)?;
        self.wake();
        Ok(())
    }

    pub(crate) async fn task_failed(
        &self,
        task: &str,
        error: &InvocationError,
    ) -> Result<(), InvocationError> {
        let _write = self.writes.lock().await;
        self.store
            .fail_task_and_invocation(&self.invocation_id, task, public_failure(error), now_ms())
            .await
            .map_err(persistence_error)?;
        self.wake();
        Ok(())
    }

    pub(crate) async fn succeeded(
        &self,
        values: &[super::RoutineInvocationResult],
        declared_results: &[RoutineResult],
    ) -> Result<(), InvocationError> {
        for mut completion in self.transport_completion.clone() {
            while !*completion.borrow_and_update() {
                if completion.changed().await.is_err() {
                    break;
                }
            }
        }
        let values = declared_results
            .iter()
            .map(|declared| {
                let result = values
                    .iter()
                    .find(|result| result.name == declared.name)
                    .ok_or_else(|| {
                        InvocationError::new("result_type", "declared routine result is missing")
                    })?;
                let value = match &result.value {
                    super::RoutineValue::String(value) => StoredRoutineValue::String(value.clone()),
                    super::RoutineValue::Boolean(value) => StoredRoutineValue::Boolean(*value),
                };
                Ok(StoredRoutineResult {
                    name: declared.name.clone(),
                    value,
                })
            })
            .collect::<Result<Vec<_>, InvocationError>>()?;
        let _write = self.writes.lock().await;
        if self.transport_terminal().await? {
            return Ok(());
        }
        self.store
            .finish_success(&self.invocation_id, values, now_ms())
            .await
            .map_err(persistence_error)?;
        self.wake();
        Ok(())
    }

    pub(crate) async fn failed(&self, error: &InvocationError) -> Result<(), InvocationError> {
        let _write = self.writes.lock().await;
        self.store
            .finish_failure(&self.invocation_id, public_failure(error), now_ms())
            .await
            .map_err(persistence_error)?;
        self.wake();
        Ok(())
    }

    /// Transport validation failure cancels runtime work but is not user cancellation.
    pub(crate) fn fail_transport(&self, failure: RoutineFailure) {
        let mut current = self.transport_failure.lock().expect("transport failure");
        if current.is_none() {
            *current = Some(failure);
        }
    }

    pub(crate) async fn interrupted(
        &self,
        code: &str,
        message: &str,
    ) -> Result<(), InvocationError> {
        let _write = self.writes.lock().await;
        if self.transport_terminal().await? {
            return Ok(());
        }
        self.store
            .finish_interruption(
                &self.invocation_id,
                RoutineFailure {
                    code: code.to_owned(),
                    message: message.to_owned(),
                },
                now_ms(),
            )
            .await
            .map_err(persistence_error)?;
        self.wake();
        Ok(())
    }

    fn wake(&self) {
        drop(self.wakeups.send(()));
    }
}

pub(crate) fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

pub(crate) fn public_failure(error: &InvocationError) -> RoutineFailure {
    let code = match error.class {
        "shutdown" | "cancelled" => "interrupted",
        "input" => "invalid_input",
        "provider" | "empty_output" => "task_execution_failed",
        "routine_persistence" => "persistence_failed",
        _ => "routine_failed",
    };
    let message = match code {
        "interrupted" => "routine execution was interrupted",
        "invalid_input" => "routine input was invalid",
        "task_execution_failed" => "routine task execution failed",
        "persistence_failed" => "routine lifecycle persistence failed",
        _ => "routine execution failed",
    };
    RoutineFailure {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

fn stored_values(
    values: &BTreeMap<String, Value>,
    declared_results: &[RoutineResult],
) -> Result<Vec<StoredRoutineResult>, InvocationError> {
    declared_results
        .iter()
        .map(|result| {
            let value = values.get(&result.name).ok_or_else(|| {
                InvocationError::new("result_type", "declared routine result is missing")
            })?;
            let value = match value {
                Value::String(value) => StoredRoutineValue::String(value.clone()),
                Value::Bool(value) => StoredRoutineValue::Boolean(*value),
                _ => {
                    return Err(InvocationError::new(
                        "result_type",
                        "routine result is not a persistable scalar",
                    ));
                }
            };
            Ok(StoredRoutineResult {
                name: result.name.clone(),
                value,
            })
        })
        .collect()
}

fn persistence_error(_: impl std::fmt::Display) -> InvocationError {
    InvocationError::new(
        "routine_persistence",
        "routine lifecycle persistence failed",
    )
}
