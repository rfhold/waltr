use std::collections::BTreeMap;

use serde_json::Value;

use super::task_protocol_error;

pub(super) const TASK_PARENT_SESSION_ID_KEY: &str = "waltr.task.parent_session_id";
pub(super) const TASK_PARENT_TURN_KEY: &str = "waltr.task.parent_turn";
pub(super) const TASK_TOOL_CALL_ID_KEY: &str = "waltr.task.tool_call_id";
pub(super) const TASK_DESCRIPTION_KEY: &str = "waltr.task.description";
pub(super) const TASK_DEPTH_KEY: &str = "waltr.task.depth";

pub(super) fn metadata_depth(values: &BTreeMap<String, Value>) -> Option<usize> {
    values
        .get(TASK_DEPTH_KEY)
        .and_then(Value::as_u64)
        .and_then(|depth| usize::try_from(depth).ok())
}

pub(super) fn required_non_empty<'a>(
    value: Option<&'a str>,
    field: &str,
) -> crate::Result<&'a str> {
    let value = value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| task_protocol_error(format!("{field} is required")))?;
    Ok(value)
}

pub(super) fn sanitize_task_id_part(value: &str) -> String {
    let sanitized = value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-') {
                char::from(byte)
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "unknown".to_owned()
    } else {
        sanitized
    }
}
