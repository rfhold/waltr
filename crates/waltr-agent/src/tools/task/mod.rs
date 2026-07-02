//! Provider-facing subagent task tool.

use std::{collections::BTreeMap, fmt};

use serde_json::{Value, json};

use crate::{
    AgentSessionRunDisposition, AgentSessionRunHandle, AgentTool, LlmError, RunConflictPolicy,
    SessionCoordinator, SessionId, SessionRuntimeSettings, SessionStore, ToolExecutionContext,
    ToolResult,
};

/// Provider-facing tool name for spawning subagent tasks.
pub const TASK_TOOL_NAME: &str = "task";

/// Default number of subagent levels allowed by the task tool.
///
/// A value of `1` allows root agents to spawn subagents, but prevents those
/// subagents from spawning additional subagents.
pub const DEFAULT_MAX_SUBAGENT_DEPTH: usize = 1;

mod async_completion;
mod metadata;
mod schema;
mod visibility;

use async_completion::{
    async_task_completion_prompt, async_task_error_prompt, latest_assistant_text_from_record,
};
use metadata::{
    TASK_DEPTH_KEY, TASK_DESCRIPTION_KEY, TASK_PARENT_SESSION_ID_KEY, TASK_PARENT_TURN_KEY,
    TASK_TOOL_CALL_ID_KEY, metadata_depth, required_non_empty, sanitize_task_id_part,
};
use schema::{
    TaskInput, TaskMode, TaskOutput, disposition_label, status_label, task_definition, task_result,
};
use visibility::TaskVisibilityMiddleware;

/// Configuration for the provider-facing task tool.
#[derive(Clone)]
pub struct TaskConfig<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    sessions: SessionCoordinator<S>,
    max_subagent_depth: usize,
}

impl<S> TaskConfig<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    /// Create a task tool configuration.
    #[must_use]
    pub fn new(sessions: SessionCoordinator<S>) -> Self {
        Self {
            sessions,
            max_subagent_depth: DEFAULT_MAX_SUBAGENT_DEPTH,
        }
    }

    /// Set how many subagent levels this task tool may create.
    ///
    /// `1` allows root agents to spawn subagents and prevents subagents from
    /// spawning subagents. `2` allows one nested subagent level, and so on.
    #[must_use]
    pub const fn with_max_subagent_depth(mut self, max_subagent_depth: usize) -> Self {
        self.max_subagent_depth = max_subagent_depth;
        self
    }

    /// Return the configured maximum subagent depth.
    #[must_use]
    pub const fn max_subagent_depth(&self) -> usize {
        self.max_subagent_depth
    }

    /// Convert this configuration into an agent tool.
    #[must_use]
    pub fn into_agent_tool(self) -> AgentTool {
        AgentTool::new_async(task_definition(), move |context| {
            let config = self.clone();
            Box::pin(async move { config.handle(context).await })
        })
    }

    /// Create middleware that hides the provider-facing task tool at max depth.
    pub(crate) const fn visibility_middleware(&self) -> TaskVisibilityMiddleware {
        TaskVisibilityMiddleware {
            max_subagent_depth: self.max_subagent_depth,
        }
    }

    async fn handle(&self, context: ToolExecutionContext) -> crate::Result<ToolResult> {
        let input: TaskInput =
            serde_json::from_value(context.call().arguments.clone()).map_err(invalid_task_input)?;
        self.run_task(context, input).await
    }

    async fn run_task(
        &self,
        context: ToolExecutionContext,
        input: TaskInput,
    ) -> crate::Result<ToolResult> {
        let prompt = required_non_empty(input.prompt.as_deref(), "prompt")?.to_owned();
        let mode = input.mode.unwrap_or(TaskMode::Sync);
        let task_id = self.resolve_task_id(&context);
        let child_depth = self.child_depth(&context).await;
        self.ensure_depth_allowed(child_depth)?;
        self.ensure_child_session(
            &task_id,
            &context,
            input.description.as_deref(),
            child_depth,
        )
        .await?;

        let policy = match mode {
            TaskMode::Sync | TaskMode::Async => RunConflictPolicy::QueueIfActive,
        };
        let submission = self
            .sessions
            .submit_prompt(task_id.clone(), prompt, policy)
            .await?;

        match mode {
            TaskMode::Sync => {
                let outcome = tokio::select! {
                    outcome = submission.handle.wait() => outcome?,
                    () = context.cancellation_token().cancelled() => {
                        self.sessions.interrupt(&task_id).await?;
                        return Err(task_protocol_error("task was canceled"));
                    }
                };
                let result = match outcome.assistant_text {
                    Some(text) => Some(text),
                    None => self
                        .sessions
                        .load(&task_id)
                        .await
                        .ok()
                        .and_then(|session| latest_assistant_text_from_record(&session.record)),
                };
                let output = TaskOutput {
                    task_id: task_id.as_str().to_owned(),
                    status: status_label(&outcome.status).to_owned(),
                    result,
                    queued_count: self
                        .sessions
                        .active_state(&task_id)
                        .await
                        .map_or(0, |state| state.queued_count),
                    run_id: Some(outcome.run_id),
                    disposition: Some(disposition_label(&submission.disposition).to_owned()),
                };
                task_result(&context, output)
            }
            TaskMode::Async => {
                let run_id = submission.run_id;
                let disposition = submission.disposition.clone();
                let active_state = self.sessions.active_state(&task_id).await;
                self.spawn_async_completion(&context, task_id.clone(), submission.handle)
                    .await;
                let output = TaskOutput {
                    task_id: task_id.as_str().to_owned(),
                    status: match disposition {
                        AgentSessionRunDisposition::Queued { .. } => "queued",
                        AgentSessionRunDisposition::Started
                        | AgentSessionRunDisposition::ReplacedActive => "running",
                    }
                    .to_owned(),
                    result: None,
                    queued_count: active_state.map_or(0, |state| state.queued_count),
                    run_id: Some(run_id),
                    disposition: Some(disposition_label(&disposition).to_owned()),
                };
                task_result(&context, output)
            }
        }
    }

    async fn spawn_async_completion(
        &self,
        context: &ToolExecutionContext,
        task_id: SessionId,
        handle: AgentSessionRunHandle,
    ) {
        let Some(parent_session_id) = context.session_id().cloned() else {
            return;
        };
        if self.sessions.load(&parent_session_id).await.is_err() {
            tracing::debug!(
                task_id = %task_id.as_str(),
                parent_session_id = %parent_session_id.as_str(),
                "skipping async task completion callback for unmanaged parent session"
            );
            return;
        }

        let sessions = self.sessions.clone();
        tokio::spawn(async move {
            let message = match handle.wait().await {
                Ok(outcome) => {
                    let result =
                        if let Some(text) = outcome.assistant_text.clone() {
                            Some(text)
                        } else {
                            sessions.load(&task_id).await.ok().and_then(|session| {
                                latest_assistant_text_from_record(&session.record)
                            })
                        };
                    async_task_completion_prompt(&task_id, &outcome, result.as_deref())
                }
                Err(error) => async_task_error_prompt(&task_id, &error.to_string()),
            };
            if sessions.load(&parent_session_id).await.is_err() {
                tracing::debug!(
                    task_id = %task_id.as_str(),
                    parent_session_id = %parent_session_id.as_str(),
                    "skipping async task completion callback for missing parent session"
                );
                return;
            }
            if let Err(error) = sessions
                .submit_prompt(
                    parent_session_id.clone(),
                    message,
                    RunConflictPolicy::QueueIfActive,
                )
                .await
            {
                tracing::warn!(
                    task_id = %task_id.as_str(),
                    parent_session_id = %parent_session_id.as_str(),
                    %error,
                    "failed to queue async task completion callback"
                );
            }
        });
    }

    fn resolve_task_id(&self, context: &ToolExecutionContext) -> SessionId {
        if let Some(parent_session_id) = context.session_id() {
            let turn = context.turn().unwrap_or_default();
            return SessionId::new(format!(
                "task_{}_{}_{}",
                sanitize_task_id_part(parent_session_id.as_str()),
                turn,
                sanitize_task_id_part(&context.call().id)
            ));
        }
        SessionId::generate()
    }

    async fn ensure_child_session(
        &self,
        task_id: &SessionId,
        context: &ToolExecutionContext,
        description: Option<&str>,
        child_depth: usize,
    ) -> crate::Result<()> {
        if self.sessions.load(task_id).await.is_ok() {
            return Ok(());
        }

        let metadata = self.child_metadata(context, description, child_depth).await;
        let runtime_settings = self.child_runtime_settings(context).await;
        match self
            .sessions
            .create_with_id_and_runtime_settings(task_id.clone(), runtime_settings, metadata)
            .await
        {
            Ok(_) => Ok(()),
            Err(error) => {
                if self.sessions.load(task_id).await.is_ok() {
                    Ok(())
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn child_metadata(
        &self,
        context: &ToolExecutionContext,
        description: Option<&str>,
        child_depth: usize,
    ) -> BTreeMap<String, Value> {
        let mut values = if let Some(parent_session_id) = context.session_id() {
            self.sessions
                .load(parent_session_id)
                .await
                .map(|session| session.record.metadata.values)
                .unwrap_or_default()
        } else {
            BTreeMap::new()
        };
        if let Some(parent_session_id) = context.session_id() {
            values.insert(
                TASK_PARENT_SESSION_ID_KEY.to_owned(),
                json!(parent_session_id.as_str()),
            );
        }
        if let Some(turn) = context.turn() {
            values.insert(TASK_PARENT_TURN_KEY.to_owned(), json!(turn));
        }
        values.insert(
            TASK_TOOL_CALL_ID_KEY.to_owned(),
            json!(context.call().id.as_str()),
        );
        if let Some(description) = description.filter(|value| !value.trim().is_empty()) {
            values.insert(TASK_DESCRIPTION_KEY.to_owned(), json!(description));
        }
        values.insert(TASK_DEPTH_KEY.to_owned(), json!(child_depth));
        values
    }

    async fn child_depth(&self, context: &ToolExecutionContext) -> usize {
        let parent_depth = if let Some(parent_session_id) = context.session_id() {
            self.sessions
                .load(parent_session_id)
                .await
                .ok()
                .and_then(|session| metadata_depth(&session.record.metadata.values))
                .unwrap_or_default()
        } else {
            0
        };
        parent_depth.saturating_add(1)
    }

    fn ensure_depth_allowed(&self, child_depth: usize) -> crate::Result<()> {
        if child_depth <= self.max_subagent_depth {
            return Ok(());
        }
        Err(task_protocol_error(format!(
            "subagent depth limit exceeded: requested depth {child_depth}, max depth {}",
            self.max_subagent_depth
        )))
    }

    async fn child_runtime_settings(
        &self,
        context: &ToolExecutionContext,
    ) -> Option<SessionRuntimeSettings> {
        let parent_session_id = context.session_id()?;
        self.sessions
            .load(parent_session_id)
            .await
            .ok()
            .and_then(|session| session.record.runtime_settings)
    }
}

impl<S> fmt::Debug for TaskConfig<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("TaskConfig").finish_non_exhaustive()
    }
}

fn invalid_task_input(error: serde_json::Error) -> LlmError {
    task_protocol_error(format!("invalid task input: {error}"))
}

fn task_protocol_error(message: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        provider: TASK_TOOL_NAME,
        message: message.into(),
    }
}
