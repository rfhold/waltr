use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    AgentSessionRunDisposition, SessionStatus, Suspension, ToolDefinition, ToolExecutionContext,
    ToolResult,
};

use super::TASK_TOOL_NAME;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TaskInput {
    pub(super) mode: Option<TaskMode>,
    pub(super) description: Option<String>,
    pub(super) prompt: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TaskMode {
    Sync,
    Async,
}

#[derive(Debug, Serialize)]
pub(super) struct TaskOutput {
    pub(super) task_id: String,
    pub(super) status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) result: Option<String>,
    pub(super) queued_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) run_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) disposition: Option<String>,
}

pub(super) fn task_definition() -> ToolDefinition {
    ToolDefinition::new(
        TASK_TOOL_NAME,
        "Run a subagent task. Sync mode waits for completion and returns the subagent result; async mode starts work and returns a task id.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["sync", "async"],
                    "description": "Sync waits for the subagent result; async returns after starting work.",
                    "default": "sync"
                },
                "description": {
                    "type": "string",
                    "description": "Short human-readable task label."
                },
                "prompt": {
                    "type": "string",
                    "description": "Instructions for the subagent."
                }
            }
        }),
    )
}

pub(super) fn task_result(
    context: &ToolExecutionContext,
    output: TaskOutput,
) -> crate::Result<ToolResult> {
    let content = serde_json::to_string(&output)?;
    Ok(context.result(content))
}

pub(super) fn status_label(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Finished => "finished",
        SessionStatus::Failed { .. } => "failed",
        SessionStatus::Suspended(Suspension::AwaitingExternalInput { .. }) => {
            "awaiting_external_input"
        }
        SessionStatus::Suspended(Suspension::AwaitingToolResults { .. }) => "awaiting_tool_results",
        SessionStatus::Suspended(Suspension::Interrupted { .. }) => "interrupted",
        SessionStatus::Idle => "idle",
        SessionStatus::BuildingRequest
        | SessionStatus::StreamingProvider
        | SessionStatus::EvaluatingToolCall
        | SessionStatus::RunningTool => "running",
    }
}

pub(super) fn disposition_label(disposition: &AgentSessionRunDisposition) -> &'static str {
    match disposition {
        AgentSessionRunDisposition::Started => "started",
        AgentSessionRunDisposition::ReplacedActive => "replaced_active",
        AgentSessionRunDisposition::Queued { .. } => "queued",
    }
}
