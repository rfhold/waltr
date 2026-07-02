use crate::{AgentSessionRunOutcome, SessionId, SessionRecord};

use super::schema::status_label;

pub(super) fn async_task_completion_prompt(
    task_id: &SessionId,
    outcome: &AgentSessionRunOutcome,
    result: Option<&str>,
) -> String {
    format!(
        "Async task completed.\n\nTask ID: {}\nStatus: {}\n\nResult:\n{}",
        task_id.as_str(),
        status_label(&outcome.status),
        result.unwrap_or("")
    )
}

pub(super) fn async_task_error_prompt(task_id: &SessionId, error: &str) -> String {
    format!(
        "Async task completed.\n\nTask ID: {}\nStatus: failed\n\nError:\n{}",
        task_id.as_str(),
        error
    )
}

pub(super) fn latest_assistant_text_from_record(record: &SessionRecord) -> Option<String> {
    record.snapshot().turns.iter().rev().find_map(|turn| {
        let text = turn.assistant_texts().collect::<String>();
        (!text.is_empty()).then_some(text)
    })
}
