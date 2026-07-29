use tonic::Status;

use waltr::grpc::waltr::auth::v1::{
    AuthTarget, AuthenticationDomain, AuthenticationFailure, AuthenticationFailureReason,
    McpAuthorizationContext, auth_target,
};
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
            waltr_agent::AgentSessionRuntimeError::Authentication {
                target,
                message,
                reason,
                mcp_authorization,
            } => {
                let details = authentication_failure_to_proto(waltr_agent::AuthenticationFailure {
                    target,
                    reason,
                    mcp_authorization,
                });
                Status::with_details(status_code(reason), message, details.encode_to_vec().into())
            }
        },
    }
}

pub(super) fn authentication_failure_to_proto(
    failure: waltr_agent::AuthenticationFailure,
) -> AuthenticationFailure {
    let (domain, target) = match failure.target {
        waltr_agent::AuthTarget::Provider { key } => (
            AuthenticationDomain::OutboundProvider,
            AuthTarget {
                target: Some(auth_target::Target::ProviderKey(key)),
            },
        ),
        waltr_agent::AuthTarget::McpServer { key } => (
            AuthenticationDomain::OutboundMcp,
            AuthTarget {
                target: Some(auth_target::Target::McpServerKey(key)),
            },
        ),
    };
    AuthenticationFailure {
        version: 1,
        domain: domain as i32,
        reason: proto_reason(failure.reason) as i32,
        target: Some(target),
        mcp_authorization: failure
            .mcp_authorization
            .map(|context| McpAuthorizationContext {
                endpoint: context.endpoint,
                http_status: u32::from(context.http_status),
                resource_metadata_url: context.resource_metadata_url,
                scope: context.scope,
                error: context.error,
                error_description: context.error_description,
                error_uri: context.error_uri,
                realm: context.realm,
            }),
    }
}

const fn proto_reason(
    reason: waltr_agent::AuthenticationFailureReason,
) -> AuthenticationFailureReason {
    match reason {
        waltr_agent::AuthenticationFailureReason::CredentialRequired => {
            AuthenticationFailureReason::CredentialRequired
        }
        waltr_agent::AuthenticationFailureReason::CredentialRejected => {
            AuthenticationFailureReason::CredentialRejected
        }
        waltr_agent::AuthenticationFailureReason::TokenExchangeFailed => {
            AuthenticationFailureReason::TokenExchangeFailed
        }
        waltr_agent::AuthenticationFailureReason::NetworkFailed => {
            AuthenticationFailureReason::NetworkFailed
        }
        waltr_agent::AuthenticationFailureReason::ResponseInvalid => {
            AuthenticationFailureReason::ResponseInvalid
        }
        waltr_agent::AuthenticationFailureReason::ConfigurationRequired => {
            AuthenticationFailureReason::ConfigurationRequired
        }
    }
}

const fn status_code(reason: waltr_agent::AuthenticationFailureReason) -> tonic::Code {
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
    fn mcp_requirement_encodes_domain_reason_target_and_challenge() {
        let status = session_run_error_to_status(waltr_agent::AgentSessionRunError::Runtime(
            waltr_agent::AgentSessionRuntimeError::Authentication {
                target: waltr_agent::AuthTarget::mcp_server("self"),
                message: "MCP authentication failed".to_owned(),
                reason: waltr_agent::AuthenticationFailureReason::CredentialRequired,
                mcp_authorization: Some(Box::new(waltr_agent::McpAuthorizationContext {
                    endpoint: "https://waltr.example.test/mcp".to_owned(),
                    http_status: 401,
                    resource_metadata_url: Some(
                        "https://waltr.example.test/.well-known/oauth-protected-resource/mcp"
                            .to_owned(),
                    ),
                    scope: Some("tools.read".to_owned()),
                    error: None,
                    error_description: None,
                    error_uri: None,
                    realm: None,
                })),
            },
        ));

        let failure = AuthenticationFailure::decode(status.details()).expect("typed details");
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
        assert_eq!(failure.version, 1);
        assert_eq!(
            AuthenticationDomain::try_from(failure.domain),
            Ok(AuthenticationDomain::OutboundMcp)
        );
        assert_eq!(
            AuthenticationFailureReason::try_from(failure.reason),
            Ok(AuthenticationFailureReason::CredentialRequired)
        );
        assert!(matches!(
            failure.target.and_then(|target| target.target),
            Some(auth_target::Target::McpServerKey(key)) if key == "self"
        ));
        let context = failure.mcp_authorization.expect("MCP context");
        assert_eq!(context.endpoint, "https://waltr.example.test/mcp");
        assert_eq!(context.scope.as_deref(), Some("tools.read"));
    }

    #[test]
    fn outbound_network_failure_is_unavailable_not_access_unauthenticated() {
        let status = session_run_error_to_status(waltr_agent::AgentSessionRunError::Runtime(
            waltr_agent::AgentSessionRuntimeError::Authentication {
                target: waltr_agent::AuthTarget::provider("openai"),
                message: "token service unavailable".to_owned(),
                reason: waltr_agent::AuthenticationFailureReason::NetworkFailed,
                mcp_authorization: None,
            },
        ));

        assert_eq!(status.code(), tonic::Code::Unavailable);
        let failure = AuthenticationFailure::decode(status.details()).expect("typed details");
        assert_eq!(
            AuthenticationDomain::try_from(failure.domain),
            Ok(AuthenticationDomain::OutboundProvider)
        );
        assert_eq!(
            AuthenticationFailureReason::try_from(failure.reason),
            Ok(AuthenticationFailureReason::NetworkFailed)
        );
    }
}
