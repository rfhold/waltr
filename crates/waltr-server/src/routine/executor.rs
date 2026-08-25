use std::{
    collections::{BTreeMap, HashSet},
    num::NonZeroU64,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use waltr_agent::{
    AgentDefinitionBundle, SessionCreateRequest, SessionId, SessionStart, SessionStartMessage,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::resource_catalog::routine::{RoutineResult, RoutineResultType, RoutineTask};

use super::{
    InvocationError,
    engine::{AdmittedInvocation, DestinationTaskTracker, short_digest},
    expression::{
        MAX_EXPANDED_PROMPT_BYTES, conditions_match, ensure_string_bound, scalar_string,
        substitute_scalar, substitute_string, validate_metadata_mutation,
    },
    host::RoutineHost,
    lifecycle::{LifecycleRecorder, public_failure},
};
use super::{RoutineInvocationResult, RoutineValue};

fn shutdown_error(operation: &str) -> InvocationError {
    InvocationError::new(
        "shutdown",
        format!("routine {operation} canceled during shutdown"),
    )
}

#[derive(Clone)]
enum TaskState {
    Succeeded(BTreeMap<String, Value>),
    Skipped,
    Failed,
}

struct ActiveChild {
    id: Option<SessionId>,
    active_children: Arc<tokio::sync::Mutex<HashSet<SessionId>>>,
    host: Arc<dyn RoutineHost>,
}

impl ActiveChild {
    async fn register(
        id: SessionId,
        active_children: &Arc<tokio::sync::Mutex<HashSet<SessionId>>>,
        host: &Arc<dyn RoutineHost>,
    ) -> Self {
        active_children.lock().await.insert(id.clone());
        Self {
            id: Some(id),
            active_children: Arc::clone(active_children),
            host: Arc::clone(host),
        }
    }

    async fn release(mut self) {
        let id = self.id.take().expect("active child should be armed");
        self.active_children.lock().await.remove(&id);
    }
}

impl Drop for ActiveChild {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else {
            return;
        };
        let active_children = Arc::clone(&self.active_children);
        let host = Arc::clone(&self.host);
        tokio::spawn(async move {
            match host.interrupt(&id).await {
                Ok(()) => {
                    active_children.lock().await.remove(&id);
                }
                Err(error) => {
                    tracing::warn!(session_id = %id.as_str(), %error, "failed to interrupt canceled routine child");
                }
            }
        });
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "sequential DAG execution keeps task state transitions together"
)]
pub(super) async fn execute_invocation(
    invocation: &AdmittedInvocation,
    host: &Arc<dyn RoutineHost>,
    active_children: &Arc<tokio::sync::Mutex<HashSet<SessionId>>>,
    child_admission_closed: &Arc<AtomicBool>,
    destination_tasks: &DestinationTaskTracker,
    cancellation: &CancellationToken,
    recorder: Option<&LifecycleRecorder>,
) -> Result<Vec<RoutineInvocationResult>, InvocationError> {
    if let Some(recorder) = recorder {
        recorder.started().await?;
    }
    let tasks = &invocation.routine.definition().spec.tasks;
    let mut states = BTreeMap::<String, TaskState>::new();
    while states.len() < tasks.len() {
        let mut progressed = false;
        for task in tasks {
            if cancellation.is_cancelled() {
                if let Some(recorder) = recorder {
                    recorder
                        .interrupted("server_shutdown", "routine interrupted by server shutdown")
                        .await?;
                }
                return Err(InvocationError::new(
                    "shutdown",
                    "routine invocation canceled during shutdown",
                ));
            }
            if states.contains_key(&task.name)
                || !task.run_after.iter().all(|name| states.contains_key(name))
            {
                continue;
            }
            progressed = true;
            if task
                .run_after
                .iter()
                .any(|name| !matches!(states.get(name), Some(TaskState::Succeeded(_))))
            {
                if let Some(recorder) = recorder {
                    recorder
                        .task_blocked(
                            &task.name,
                            crate::persistence::routine_invocation::RoutineFailure {
                                code: "dependency_failed".to_owned(),
                                message: "routine task dependency did not succeed".to_owned(),
                            },
                        )
                        .await?;
                }
                states.insert(task.name.clone(), TaskState::Failed);
                continue;
            }
            let available = successful_results(&states);
            if !conditions_match(&task.when, &invocation.context, &available) {
                if let Some(recorder) = recorder {
                    recorder.task_skipped(&task.name).await?;
                }
                states.insert(task.name.clone(), TaskState::Skipped);
                continue;
            }
            if let Some(recorder) = recorder {
                recorder.task_started(&task.name).await?;
            }
            let started = Instant::now();
            let result = execute_task(
                invocation,
                task,
                &available,
                host,
                active_children,
                child_admission_closed,
                destination_tasks,
                cancellation,
            )
            .await;
            match result {
                Ok(results) => {
                    if let Some(recorder) = recorder {
                        recorder
                            .task_succeeded(
                                &task.name,
                                &results.values,
                                &task.task_spec.results,
                                results.session_references.clone(),
                            )
                            .await?;
                    }
                    tracing::info!(
                        routine = invocation.routine.name(),
                        trigger = invocation.trigger,
                        correlation_id = invocation.correlation_id,
                        invocation_id = invocation.identity,
                        task = task.name,
                        status = "succeeded",
                        duration_ms = started.elapsed().as_millis(),
                        "routine task completed"
                    );
                    states.insert(task.name.clone(), TaskState::Succeeded(results.values));
                }
                Err(error) => {
                    tracing::warn!(
                        routine = invocation.routine.name(),
                        trigger = invocation.trigger,
                        correlation_id = invocation.correlation_id,
                        invocation_id = invocation.identity,
                        task = task.name,
                        status = "failed",
                        error_class = error.class,
                        duration_ms = started.elapsed().as_millis(),
                        "routine task failed"
                    );
                    states.insert(task.name.clone(), TaskState::Failed);
                    if let Some(recorder) = recorder {
                        if error.class == "routine_persistence" {
                            return Err(error);
                        }
                        if error.class == "shutdown" || error.class == "cancelled" {
                            let failure = crate::persistence::routine_invocation::RoutineFailure {
                                code: "server_shutdown".to_owned(),
                                message: "routine interrupted by server shutdown".to_owned(),
                            };
                            recorder.task_interrupted(&task.name, failure).await?;
                            recorder
                                .interrupted(
                                    "server_shutdown",
                                    "routine interrupted by server shutdown",
                                )
                                .await?;
                            return Err(error);
                        }
                        let blocked = tasks
                            .iter()
                            .filter(|candidate| {
                                !states.contains_key(&candidate.name)
                                    && depends_on(candidate, &task.name, tasks)
                            })
                            .map(|candidate| candidate.name.clone())
                            .collect::<Vec<_>>();
                        for blocked in blocked {
                            recorder
                                .task_blocked(&blocked, public_failure(&error))
                                .await?;
                            states.insert(blocked, TaskState::Failed);
                        }
                        recorder.task_failed(&task.name, &error).await?;
                    }
                    return Err(error);
                }
            }
        }
        if !progressed {
            return Err(InvocationError::new(
                "invalid_dag",
                "routine task graph made no progress",
            ));
        }
    }
    let available = successful_results(&states);
    let results = invocation
        .routine
        .definition()
        .spec
        .results
        .iter()
        .map(|result| {
            let value =
                substitute_scalar(&result.value, &invocation.context, &available, None, None)?;
            validate_result_type(result, &value)?;
            Ok(RoutineInvocationResult {
                name: result.name.clone(),
                value: RoutineValue::from_json(value)?,
            })
        })
        .collect::<Result<Vec<_>, InvocationError>>();
    match results {
        Ok(results) => {
            if let Some(recorder) = recorder {
                recorder
                    .succeeded(&results, &invocation.routine.definition().spec.results)
                    .await?;
            }
            Ok(results)
        }
        Err(error) => {
            if let Some(recorder) = recorder
                && error.class != "routine_persistence"
            {
                recorder.failed(&error).await?;
            }
            Err(error)
        }
    }
}

fn depends_on(task: &RoutineTask, dependency: &str, tasks: &[RoutineTask]) -> bool {
    task.run_after.iter().any(|name| {
        name == dependency
            || tasks
                .iter()
                .find(|candidate| candidate.name == *name)
                .is_some_and(|candidate| depends_on(candidate, dependency, tasks))
    })
}

#[derive(Debug)]
pub(super) struct TaskExecution {
    values: BTreeMap<String, Value>,
    session_references: BTreeMap<String, String>,
}

fn successful_results(
    states: &BTreeMap<String, TaskState>,
) -> BTreeMap<String, BTreeMap<String, Value>> {
    states
        .iter()
        .filter_map(|(name, state)| match state {
            TaskState::Succeeded(results) => Some((name.clone(), results.clone())),
            TaskState::Skipped | TaskState::Failed => None,
        })
        .collect()
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "validated task variants share result collection"
)]
pub(super) async fn execute_task(
    invocation: &AdmittedInvocation,
    task: &RoutineTask,
    task_results: &BTreeMap<String, BTreeMap<String, Value>>,
    host: &Arc<dyn RoutineHost>,
    active_children: &Arc<tokio::sync::Mutex<HashSet<SessionId>>>,
    child_admission_closed: &Arc<AtomicBool>,
    destination_tasks: &DestinationTaskTracker,
    cancellation: &CancellationToken,
) -> Result<TaskExecution, InvocationError> {
    if let Some(template) = &task.task_spec.session_template {
        let captured = template
            .source
            .as_ref()
            .map(|source| {
                let capture = invocation.context.source.as_ref().ok_or_else(|| {
                    InvocationError::new("source_capture", "captured source is unavailable")
                })?;
                let source_id = session_id_value(
                    &substitute_scalar(
                        &source.session,
                        &invocation.context,
                        task_results,
                        None,
                        None,
                    )?,
                    "template source session",
                )?;
                let cursor = substitute_scalar(
                    &source.through_cursor,
                    &invocation.context,
                    task_results,
                    None,
                    None,
                )?;
                if source_id != capture.source_id
                    || cursor.as_u64() != Some(capture.through_cursor.get())
                {
                    return Err(InvocationError::new(
                        "source_capture",
                        "source template does not identify the admitted capture",
                    ));
                }
                Ok(capture)
            })
            .transpose()?;
        let source_id = if let Some(capture) = captured {
            capture.source_id.clone()
        } else {
            let inference = template
                .inference
                .as_ref()
                .expect("validated non-source template has inference");
            session_id_value(
                &substitute_scalar(
                    &inference.inherit_from,
                    &invocation.context,
                    task_results,
                    None,
                    None,
                )?,
                "inference source session",
            )?
        };
        let system_prompt = substitute_string(
            &template.system_prompt.text,
            &invocation.context,
            task_results,
            None,
            None,
        )?;
        let user_prompt = substitute_string(
            &template.user_prompt.text,
            &invocation.context,
            task_results,
            None,
            None,
        )?;
        ensure_string_bound(
            &system_prompt,
            MAX_EXPANDED_PROMPT_BYTES,
            "expanded_prompt",
            "expanded system prompt",
        )?;
        ensure_string_bound(
            &user_prompt,
            MAX_EXPANDED_PROMPT_BYTES,
            "expanded_prompt",
            "expanded user prompt",
        )?;
        let initial_message = SessionStartMessage::user(user_prompt)
            .map_err(|error| InvocationError::new("expanded_prompt", error.to_string()))?;
        let bundle = agent_bundle(task, &system_prompt)?;
        let inference = if let Some(selection) = &template.inference {
            let inference_id = session_id_value(
                &substitute_scalar(
                    &selection.inherit_from,
                    &invocation.context,
                    task_results,
                    None,
                    None,
                )?,
                "inference source session",
            )?;
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(shutdown_error("source inference load")),
                inference = host.load_inference(&inference_id) => inference
                    .map_err(|error| InvocationError::new("source_load", error))?
                    .with_reasoning_disabled(),
            }
        } else if let Some(capture) = captured {
            capture.inference_settings.clone().with_reasoning_disabled()
        } else {
            unreachable!("validated agent template has source or inference")
        };
        let child_id = SessionId::new(format!(
            "routine_{}",
            short_digest(&format!("{}\0{}", invocation.identity, task.name))
        ));
        let mut metadata = BTreeMap::from([
            ("waltr.session.ephemeral".to_owned(), Value::Bool(true)),
            (
                "waltr.session.purpose".to_owned(),
                Value::String("routine-task".to_owned()),
            ),
            (
                "waltr.routine.name".to_owned(),
                Value::String(invocation.routine.name().to_owned()),
            ),
            (
                "waltr.routine.revision".to_owned(),
                Value::String(invocation.routine.source_etag().to_owned()),
            ),
            (
                "waltr.routine.trigger".to_owned(),
                Value::String(invocation.trigger.clone()),
            ),
            (
                "waltr.routine.task".to_owned(),
                Value::String(task.name.clone()),
            ),
            (
                "waltr.routine.invocation_id".to_owned(),
                Value::String(invocation.identity.clone()),
            ),
        ]);
        metadata.insert(
            "waltr.routine.source_session_id".to_owned(),
            Value::String(source_id.as_str().to_owned()),
        );
        // Ownership precedes every host await that can create or submit work. Cancellation may
        // drop an ambiguous host future, but final shutdown interruption still targets this ID.
        let child = ActiveChild::register(child_id.clone(), active_children, host).await;
        let start = match captured {
            None => SessionStart::Fresh {
                messages: vec![initial_message],
            },
            Some(capture) => SessionStart::Copy {
                copy: capture.copy.clone(),
                messages: vec![initial_message],
            },
        };
        let created = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(shutdown_error("child creation")),
            created = host.create_child(SessionCreateRequest {
                id: child_id.clone(),
                inference_settings: Some(inference),
                agent_definition: Some(bundle),
                metadata_values: metadata,
                start,
            }) => created,
        };
        if let Err(error) = created {
            return Err(InvocationError::new("child_create", error));
        }
        let max_turns = NonZeroU64::new(u64::from(
            task.task_spec
                .limits
                .as_ref()
                .expect("validated agent task has limits")
                .max_turns,
        ))
        .expect("validated maxTurns is nonzero");
        if child_admission_closed.load(Ordering::SeqCst) || cancellation.is_cancelled() {
            return Err(shutdown_error("child submission"));
        }
        let run = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(shutdown_error("child submission")),
            run = host.submit_child(child_id.clone(), max_turns) => run,
        };
        let run = match run {
            Ok(run) => run,
            Err(error) => {
                return Err(InvocationError::new("provider", error));
            }
        };
        let output = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                return Err(shutdown_error("child run"));
            }
            output = run => output,
        };
        child.release().await;
        let output = output
            .map_err(|error| InvocationError::new("provider", error))?
            .filter(|text| !text.is_empty())
            .ok_or_else(|| InvocationError::new("empty_output", "agent task produced no text"))?;
        let values = collect_results(
            &task.task_spec.results,
            &invocation.context,
            task_results,
            Some(&output),
            None,
        )?;
        Ok(TaskExecution {
            values,
            session_references: BTreeMap::from([(
                "helper".to_owned(),
                child_id.as_str().to_owned(),
            )]),
        })
    } else {
        let action = task
            .task_spec
            .action
            .as_ref()
            .expect("validated task has one executor");
        if let Some(metadata) = &action.session_metadata {
            if metadata.mutations.len() != 1 {
                return Err(InvocationError::new(
                    "unsupported_action",
                    "session metadata action must contain exactly one mutation",
                ));
            }
            let session_id = scalar_string(
                &substitute_scalar(
                    &metadata.session_id,
                    &invocation.context,
                    task_results,
                    None,
                    None,
                )?,
                "action session id",
            )?;
            let mutation = &metadata.mutations[0];
            let value = substitute_scalar(
                &mutation.value,
                &invocation.context,
                task_results,
                None,
                None,
            )?;
            validate_metadata_mutation(&mutation.key, &value)?;
            let session_id = SessionId::new(session_id);
            let changed = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(shutdown_error("metadata action")),
                changed = host.set_metadata_if_absent(&session_id, mutation.key.clone(), value) => changed
                    .map_err(|error| InvocationError::new("metadata_action", error))?,
            };
            tracing::info!(
                routine = invocation.routine.name(),
                trigger = invocation.trigger,
                correlation_id = invocation.correlation_id,
                invocation_id = invocation.identity,
                task = task.name,
                changed,
                "routine metadata action completed"
            );
            let values = collect_results(
                &task.task_spec.results,
                &invocation.context,
                task_results,
                None,
                Some(&json!({"sessionMetadata": {"changed": changed}})),
            )?;
            Ok(TaskExecution {
                values,
                session_references: BTreeMap::new(),
            })
        } else {
            execute_session_create(
                invocation,
                task,
                task_results,
                host,
                destination_tasks,
                cancellation,
            )
            .await
        }
    }
}

pub(super) fn agent_bundle(
    task: &RoutineTask,
    system_prompt: &str,
) -> Result<AgentDefinitionBundle, InvocationError> {
    let parse_entries = |values: &[String], kind: &'static str| {
        values
            .iter()
            .map(|value| {
                serde_yaml_ng::from_str::<serde_yaml_ng::Value>(value).map_err(|error| {
                    InvocationError::new(kind, format!("invalid task YAML entry: {error}"))
                })
            })
            .collect::<Result<Vec<_>, _>>()
    };
    let features = parse_entries(
        task.task_spec.features.as_deref().unwrap_or_default(),
        "agent_features",
    )?;
    let mut permissions = parse_entries(
        task.task_spec.permissions.as_deref().unwrap_or_default(),
        "agent_permissions",
    )?;
    for tool in task.task_spec.tools.as_deref().unwrap_or_default() {
        permissions.push(
            serde_yaml_ng::to_value(json!({
                "name": tool,
                "permission": "allow"
            }))
            .expect("JSON values serialize as YAML"),
        );
    }
    let source = serde_yaml_ng::to_string(&json!({
        "apiVersion": waltr_agent::AGENT_DEFINITION_API_VERSION,
        "kind": "Agent",
        "metadata": { "name": "routine-task" },
        "spec": {
            "systemPrompt": system_prompt,
            "features": features,
            "permissions": permissions,
        }
    }))
    .map_err(|error| InvocationError::new("agent_bundle", error.to_string()))?;
    let bundle = AgentDefinitionBundle::new("routine-task", vec![source]);
    bundle
        .resolve()
        .map_err(|error| InvocationError::new("agent_bundle", error.to_string()))?;
    Ok(bundle)
}

async fn execute_session_create(
    invocation: &AdmittedInvocation,
    task: &RoutineTask,
    task_results: &BTreeMap<String, BTreeMap<String, Value>>,
    host: &Arc<dyn RoutineHost>,
    destination_tasks: &DestinationTaskTracker,
    cancellation: &CancellationToken,
) -> Result<TaskExecution, InvocationError> {
    let create = task
        .task_spec
        .action
        .as_ref()
        .and_then(|action| action.session_create.as_ref())
        .expect("validated create action");
    let capture = invocation.context.source.as_ref().ok_or_else(|| {
        InvocationError::new(
            "source_capture",
            "sessionCreate requires an admitted source capture",
        )
    })?;
    let inherited_id = session_id_value(
        &substitute_scalar(
            &create.inherit_from.session,
            &invocation.context,
            task_results,
            None,
            None,
        )?,
        "sessionCreate inheritance source",
    )?;
    if inherited_id != capture.source_id {
        return Err(InvocationError::new(
            "source_capture",
            "sessionCreate inheritance does not identify the admitted source",
        ));
    }
    let mut metadata = BTreeMap::new();
    for (key, scalar) in &create.metadata {
        let value = substitute_scalar(scalar, &invocation.context, task_results, None, None)?;
        validate_metadata_mutation(key, &value)?;
        metadata.insert(key.clone(), value);
    }
    let first_prompt = substitute_string(
        &create.first_user_message.text,
        &invocation.context,
        task_results,
        None,
        None,
    )?;
    ensure_string_bound(
        &first_prompt,
        MAX_EXPANDED_PROMPT_BYTES,
        "expanded_prompt",
        "expanded first user message",
    )?;
    let initial_message = SessionStartMessage::user(first_prompt)
        .map_err(|error| InvocationError::new("expanded_prompt", error.to_string()))?;
    let id = SessionId::generate();
    let mut bundle = capture.agent_definition.clone();
    bundle.pending_switch_audit = None;
    let request = SessionCreateRequest {
        id: id.clone(),
        inference_settings: Some(capture.inference_settings.clone()),
        agent_definition: Some(bundle),
        metadata_values: metadata,
        start: SessionStart::Fresh {
            messages: vec![initial_message],
        },
    };
    let create_host = Arc::clone(host);
    let creation =
        destination_tasks.spawn(async move { create_host.create_destination(request).await });
    creation
        .await
        .map_err(|error| {
            let message = if error.is_cancelled() {
                "routine destination creation task was cancelled"
            } else if error.is_panic() {
                "routine destination creation task panicked"
            } else {
                "routine destination creation task failed to join"
            };
            InvocationError::new("destination_create_join", message)
        })?
        .map_err(|error| InvocationError::new("destination_create", error))?;
    // Atomic creation is the ownership boundary: cancellation and start failure retain the
    // prompt-durable ordinary destination and still return its identity.
    if cancellation.is_cancelled() {
        tracing::warn!(
            session_id = id.as_str(),
            "Routine destination retained after cancellation following prompt durability"
        );
    } else if let Err(error) = host.start_destination(id.clone()).await {
        tracing::warn!(session_id = id.as_str(), %error, "Routine destination start failed after prompt durability");
    }
    let values = collect_results(
        &task.task_spec.results,
        &invocation.context,
        task_results,
        None,
        Some(&json!({"sessionCreate": {"session": {"id": id.as_str()}}})),
    )?;
    Ok(TaskExecution {
        values,
        session_references: BTreeMap::from([("destination".to_owned(), id.as_str().to_owned())]),
    })
}

fn session_id_value(value: &Value, description: &str) -> Result<SessionId, InvocationError> {
    value
        .as_str()
        .or_else(|| value.get("id").and_then(Value::as_str))
        .map(SessionId::new)
        .ok_or_else(|| {
            InvocationError::new(
                "substitution_type",
                format!("{description} must be a session id or object with id"),
            )
        })
}

fn collect_results(
    declared: &[RoutineResult],
    context: &super::engine::RoutineInvocationContext,
    task_results: &BTreeMap<String, BTreeMap<String, Value>>,
    output: Option<&str>,
    action: Option<&Value>,
) -> Result<BTreeMap<String, Value>, InvocationError> {
    declared
        .iter()
        .map(|result| {
            let value = substitute_scalar(&result.value, context, task_results, output, action)?;
            validate_result_type(result, &value)?;
            Ok((result.name.clone(), value))
        })
        .collect()
}

fn validate_result_type(result: &RoutineResult, value: &Value) -> Result<(), InvocationError> {
    let matches = matches!(
        (result.r#type, value),
        (RoutineResultType::String, Value::String(_))
            | (RoutineResultType::Boolean, Value::Bool(_))
    );
    if matches {
        Ok(())
    } else {
        Err(InvocationError::new(
            "result_type",
            format!("result `{}` does not match its declared type", result.name),
        ))
    }
}
