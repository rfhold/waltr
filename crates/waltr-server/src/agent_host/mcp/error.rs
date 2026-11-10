use waltr_agent::{AuthTarget, AuthenticationFailure, AuthenticationFailureReason, LlmError};

pub(super) fn client_error(error: mcp::Error, server: &str, endpoint: &str) -> LlmError {
    match error {
        mcp::Error::Authorization(error) => {
            let challenge = error.challenge.as_ref();
            let context = crate::auth::McpAuthorizationContext {
                endpoint: endpoint.into(),
                http_status: error.status.as_u16(),
                resource_metadata_url: challenge.and_then(|value| value.resource_metadata.clone()),
                scope: challenge.and_then(|value| value.scope.clone()),
                error: challenge.and_then(|value| value.error.clone()),
                error_description: challenge.and_then(|value| value.error_description.clone()),
                error_uri: challenge.and_then(|value| value.error_uri.clone()),
                realm: challenge.and_then(|value| value.realm.clone()),
            };
            LlmError::AuthenticationFailure {
                failure: AuthenticationFailure {
                    target: AuthTarget::Host {
                        namespace: "mcp".into(),
                        key: server.into(),
                    },
                    reason: AuthenticationFailureReason::CredentialRequired,
                    host_context: serde_json::to_value(context).ok(),
                },
                message: "upstream MCP authentication is required".into(),
            }
        }
        mcp::Error::BearerProvider(mcp::client::McpBearerProviderError::Authorization) => {
            LlmError::Authorization {
                provider: "mcp",
                message: "upstream MCP bearer authentication is required".into(),
                status: None,
            }
        }
        error => LlmError::Protocol {
            provider: "mcp",
            message: error.to_string(),
        },
    }
}
