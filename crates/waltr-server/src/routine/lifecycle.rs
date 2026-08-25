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
pub(super) struct LifecycleRecorder {
    store: RoutineInvocationStore,
    invocation_id: String,
    wakeups: broadcast::Sender<()>,
}

impl LifecycleRecorder {
    pub(super) fn new(
        store: RoutineInvocationStore,
        invocation_id: String,
        wakeups: broadcast::Sender<()>,
    ) -> Self {
        Self {
            store,
            invocation_id,
            wakeups,
        }
    }

    pub(super) async fn started(&self) -> Result<(), InvocationError> {
        self.store
            .start_invocation(&self.invocation_id, now_ms())
            .await
            .map_err(persistence_error)?;
        self.wake();
        Ok(())
    }

    pub(super) async fn task_started(&self, task: &str) -> Result<(), InvocationError> {
        self.transition(
            task,
            RoutineTaskTransition::Started {
                session_references: BTreeMap::new(),
            },
        )
        .await
    }

    pub(super) async fn task_succeeded(
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

    pub(super) async fn task_skipped(&self, task: &str) -> Result<(), InvocationError> {
        self.transition(task, RoutineTaskTransition::Skipped).await
    }

    pub(super) async fn task_blocked(
        &self,
        task: &str,
        failure: RoutineFailure,
    ) -> Result<(), InvocationError> {
        self.transition(task, RoutineTaskTransition::Blocked(failure))
            .await
    }

    pub(super) async fn task_interrupted(
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
        self.store
            .transition_task(&self.invocation_id, task, transition, now_ms())
            .await
            .map_err(persistence_error)?;
        self.wake();
        Ok(())
    }

    pub(super) async fn task_failed(
        &self,
        task: &str,
        error: &InvocationError,
    ) -> Result<(), InvocationError> {
        self.store
            .fail_task_and_invocation(&self.invocation_id, task, public_failure(error), now_ms())
            .await
            .map_err(persistence_error)?;
        self.wake();
        Ok(())
    }

    pub(super) async fn succeeded(
        &self,
        values: &[super::RoutineInvocationResult],
        declared_results: &[RoutineResult],
    ) -> Result<(), InvocationError> {
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
        self.store
            .finish_success(&self.invocation_id, values, now_ms())
            .await
            .map_err(persistence_error)?;
        self.wake();
        Ok(())
    }

    pub(super) async fn failed(&self, error: &InvocationError) -> Result<(), InvocationError> {
        self.store
            .finish_failure(&self.invocation_id, public_failure(error), now_ms())
            .await
            .map_err(persistence_error)?;
        self.wake();
        Ok(())
    }

    pub(super) async fn interrupted(
        &self,
        code: &str,
        message: &str,
    ) -> Result<(), InvocationError> {
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

pub(super) fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

pub(super) fn public_failure(error: &InvocationError) -> RoutineFailure {
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
