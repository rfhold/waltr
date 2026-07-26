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
            waltr_agent::AgentSessionRuntimeError::AuthorizationRequired {
                target,
                message,
                reason,
                mcp_authorization: _,
            } => {
                let target = match target {
                    waltr_agent::AuthTarget::Provider { key } => AuthTarget {
                        target: Some(auth_target::Target::ProviderKey(key)),
                    },
                    waltr_agent::AuthTarget::McpServer { key } => AuthTarget {
                        target: Some(auth_target::Target::McpServerKey(key)),
                    },
                };
                Status::with_details(
                    authentication_status_code(reason),
                    message,
                    target.encode_to_vec().into(),
                )
            }
        },
    }
}

const fn authentication_status_code(
    reason: waltr_agent::AuthenticationFailureReason,
) -> tonic::Code {
    match reason {
        waltr_agent::AuthenticationFailureReason::CredentialRequired
        | waltr_agent::AuthenticationFailureReason::CredentialRejected
        | waltr_agent::AuthenticationFailureReason::TokenExchangeFailed => {
            tonic::Code::Unauthenticated
        }
        waltr_agent::AuthenticationFailureReason::NetworkFailed => tonic::Code::Unavailable,
        waltr_agent::AuthenticationFailureReason::ResponseInvalid => tonic::Code::DataLoss,
        waltr_agent::AuthenticationFailureReason::ConfigurationRequired => {
            tonic::Code::FailedPrecondition
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_structured_auth_reason_without_changing_target_details() {
        let status = session_run_error_to_status(waltr_agent::AgentSessionRunError::Runtime(
            waltr_agent::AgentSessionRuntimeError::AuthorizationRequired {
                target: waltr_agent::AuthTarget::mcp_server("remote"),
                message: "token service unavailable".to_owned(),
                reason: waltr_agent::AuthenticationFailureReason::NetworkFailed,
                mcp_authorization: None,
            },
        ));

        assert_eq!(status.code(), tonic::Code::Unavailable);
        let target = AuthTarget::decode(status.details()).expect("auth target details");
        assert!(matches!(
            target.target,
            Some(auth_target::Target::McpServerKey(key)) if key == "remote"
        ));
    }
}
