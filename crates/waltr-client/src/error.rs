//! Error type for shared client operations.

use std::fmt;

use crate::AuthTarget;

/// Machine-readable category for a client error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientErrorKind {
    /// A transport or operation error unrelated to authentication.
    Other,
    /// The caller must authenticate for access to Waltr itself.
    WaltrLoginRequired,
}

/// Error surfaced by client transports or state transitions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientError {
    message: String,
    authentication_failure: Option<OutboundAuthenticationFailure>,
    kind: ClientErrorKind,
}

/// Structured outbound authentication failure safe to expose to clients.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutboundAuthenticationFailure {
    /// Failure schema version.
    pub version: u32,
    /// Exact authentication domain enum value.
    pub domain: i32,
    /// Exact authentication failure reason enum value.
    pub reason: i32,
    /// Provider or MCP target that failed.
    pub target: AuthTarget,
    /// Sanitized upstream MCP context, when supplied.
    pub mcp_context: Option<McpAuthorizationContext>,
}

/// Sanitized upstream MCP authentication context safe for shell presentation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpAuthorizationContext {
    /// Protected MCP endpoint, when safe.
    pub endpoint: Option<String>,
    /// HTTP status associated with the challenge.
    pub http_status: u32,
    /// Protected-resource metadata URL, when safe.
    pub resource_metadata_url: Option<String>,
    /// Requested OAuth scope.
    pub scope: Option<String>,
    /// OAuth bearer error code.
    pub error: Option<String>,
    /// OAuth bearer error description.
    pub error_description: Option<String>,
    /// OAuth bearer error documentation URL, when safe.
    pub error_uri: Option<String>,
    /// Bearer challenge realm.
    pub realm: Option<String>,
}

impl OutboundAuthenticationFailure {
    pub(crate) fn from_proto(
        failure: waltr::grpc::waltr::auth::v1::AuthenticationFailure,
    ) -> Option<Self> {
        use waltr::grpc::waltr::auth::v1::{
            AuthenticationDomain, AuthenticationFailureReason, auth_target,
        };

        if failure.version != 1 {
            return None;
        }
        let domain = AuthenticationDomain::try_from(failure.domain).ok()?;
        if !matches!(
            domain,
            AuthenticationDomain::OutboundProvider | AuthenticationDomain::OutboundMcp
        ) {
            return None;
        }
        let reason = AuthenticationFailureReason::try_from(failure.reason).ok()?;
        if reason == AuthenticationFailureReason::Unspecified {
            return None;
        }
        let target = failure.target?;
        match (domain, target.target.as_ref()) {
            (
                AuthenticationDomain::OutboundProvider,
                Some(auth_target::Target::ProviderKey(key)),
            )
            | (AuthenticationDomain::OutboundMcp, Some(auth_target::Target::McpServerKey(key)))
                if !key.trim().is_empty() => {}
            _ => return None,
        }
        Some(Self {
            version: failure.version,
            domain: failure.domain,
            reason: failure.reason,
            target,
            mcp_context: failure.mcp_authorization.map(McpAuthorizationContext::from),
        })
    }
}

impl From<waltr::grpc::waltr::auth::v1::McpAuthorizationContext> for McpAuthorizationContext {
    fn from(context: waltr::grpc::waltr::auth::v1::McpAuthorizationContext) -> Self {
        Self {
            endpoint: crate::coordination::state::safe_http_url(&context.endpoint),
            http_status: context.http_status,
            resource_metadata_url: context
                .resource_metadata_url
                .and_then(|value| crate::coordination::state::safe_http_url(&value)),
            scope: context
                .scope
                .map(|value| crate::coordination::state::safe_text(&value, 1_024)),
            error: context
                .error
                .map(|value| crate::coordination::state::safe_text(&value, 256)),
            error_description: context
                .error_description
                .map(|value| crate::coordination::state::safe_text(&value, 1_024)),
            error_uri: context
                .error_uri
                .and_then(|value| crate::coordination::state::safe_http_url(&value)),
            realm: context
                .realm
                .map(|value| crate::coordination::state::safe_text(&value, 256)),
        }
    }
}

impl ClientError {
    /// Create a client error from a displayable message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            authentication_failure: None,
            kind: ClientErrorKind::Other,
        }
    }

    /// Attach a structured outbound authentication failure.
    #[must_use]
    pub fn outbound_authentication(
        message: impl Into<String>,
        failure: OutboundAuthenticationFailure,
    ) -> Self {
        Self {
            message: message.into(),
            authentication_failure: Some(failure),
            kind: ClientErrorKind::Other,
        }
    }

    /// Create an error indicating that Waltr access login is required.
    #[must_use]
    pub fn waltr_login_required(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            authentication_failure: None,
            kind: ClientErrorKind::WaltrLoginRequired,
        }
    }

    /// Return the human-readable error message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Return structured outbound authentication details, when supplied.
    #[must_use]
    pub const fn authentication_failure(&self) -> Option<&OutboundAuthenticationFailure> {
        self.authentication_failure.as_ref()
    }

    /// Return the machine-readable error category.
    #[must_use]
    pub const fn kind(&self) -> ClientErrorKind {
        self.kind
    }

    /// Return whether this error requires Waltr access login.
    #[must_use]
    pub const fn is_waltr_login_required(&self) -> bool {
        matches!(self.kind, ClientErrorKind::WaltrLoginRequired)
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ClientError {}

impl From<String> for ClientError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<&str> for ClientError {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}
