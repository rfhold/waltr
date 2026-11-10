use super::task_protocol_error;

// Public audit values only; private producer-bound provenance controls behavior.
pub(super) const TASK_PARENT_SESSION_ID_KEY: &str = "task.parent_session_id";
pub(super) const TASK_PARENT_TURN_KEY: &str = "task.parent_turn";
pub(super) const TASK_TOOL_CALL_ID_KEY: &str = "task.tool_call_id";
pub(super) const TASK_DESCRIPTION_KEY: &str = "task.description";
pub(super) const TASK_DEPTH_KEY: &str = "task.depth";

pub(super) fn required_non_empty<'a>(
    value: Option<&'a str>,
    field: &str,
) -> crate::agent_host::Result<&'a str> {
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
