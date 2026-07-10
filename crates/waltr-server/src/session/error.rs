use tonic::Status;

use waltr::grpc::waltr::auth::v1::{AuthTarget, auth_target};
use prost::Message;

#[allow(clippy::needless_pass_by_value)]
pub(super) fn agent_error_to_status(error: waltr_agent::LlmError) -> Status {
    Status::internal(error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn session_run_error_to_status(error: waltr_agent::AgentSessionRunError) -> Status {
    match error {
        waltr_agent::AgentSessionRunError::AlreadyRunning { .. } => {
            Status::failed_precondition("session already has an active run")
        }
        waltr_agent::AgentSessionRunError::Canceled { .. } => Status::cancelled(error.to_string()),
        waltr_agent::AgentSessionRunError::OutcomeDropped { .. } => {
            Status::internal(error.to_string())
        }
        waltr_agent::AgentSessionRunError::Agent(error) => agent_error_to_status(error),
        waltr_agent::AgentSessionRunError::Runtime(runtime_error) => match runtime_error {
            waltr_agent::AgentSessionRuntimeError::FailedPrecondition(message) => {
                Status::failed_precondition(message)
            }
            waltr_agent::AgentSessionRuntimeError::Internal(message) => Status::internal(message),
            waltr_agent::AgentSessionRuntimeError::Agent(error) => agent_error_to_status(error),
            waltr_agent::AgentSessionRuntimeError::AuthorizationRequired { target, message } => {
                let target = match target {
                    waltr_agent::AuthTarget::Provider { key } => AuthTarget {
                        target: Some(auth_target::Target::ProviderKey(key)),
                    },
                    waltr_agent::AuthTarget::McpServer { key } => AuthTarget {
                        target: Some(auth_target::Target::McpServerKey(key)),
                    },
                };
                Status::with_details(
                    tonic::Code::Unauthenticated,
                    message,
                    target.encode_to_vec().into(),
                )
            }
        },
    }
}
