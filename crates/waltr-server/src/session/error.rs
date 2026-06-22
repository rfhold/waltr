use tonic::Status;

#[allow(clippy::needless_pass_by_value)]
pub(super) fn agent_error_to_status(error: waltr_agent::LlmError) -> Status {
    Status::internal(error.to_string())
}
