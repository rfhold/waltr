//! Error type for shared client operations.

use std::fmt;

use crate::{AuthTarget, McpAuthorizationRequirement};

/// Machine-readable category for a client error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientErrorKind {
    /// A transport or operation error unrelated to authentication.
    Other,
    /// Waltr requires provider or upstream MCP credentials.
    AuthorizationRequired,
    /// The caller must authenticate for access to Waltr itself.
    WaltrLoginRequired,
}

/// Error surfaced by client transports or state transitions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientError {
    message: String,
    auth_target: Option<AuthTarget>,
    mcp_authorization: Option<McpAuthorizationRequirement>,
    kind: ClientErrorKind,
}

impl ClientError {
    /// Create a client error from a displayable message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            auth_target: None,
            mcp_authorization: None,
            kind: ClientErrorKind::Other,
        }
    }

    /// Create an authorization-required client error for a specific target.
    #[must_use]
    pub fn authorization_required(message: impl Into<String>, target: AuthTarget) -> Self {
        Self {
            message: message.into(),
            auth_target: Some(target),
            mcp_authorization: None,
            kind: ClientErrorKind::AuthorizationRequired,
        }
    }

    /// Create an authorization-required error with sanitized MCP challenge context.
    #[must_use]
    pub fn mcp_authorization_required(
        message: impl Into<String>,
        target: AuthTarget,
        authorization: McpAuthorizationRequirement,
    ) -> Self {
        Self {
            message: message.into(),
            auth_target: Some(target),
            mcp_authorization: Some(authorization),
            kind: ClientErrorKind::AuthorizationRequired,
        }
    }

    /// Create an error indicating that Waltr access login is required.
    #[must_use]
    pub fn waltr_login_required(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            auth_target: None,
            mcp_authorization: None,
            kind: ClientErrorKind::WaltrLoginRequired,
        }
    }

    /// Return the human-readable error message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Return the auth target that must be completed before retrying, when known.
    #[must_use]
    pub const fn auth_target(&self) -> Option<&AuthTarget> {
        self.auth_target.as_ref()
    }

    /// Return sanitized MCP authorization context, when supplied.
    #[must_use]
    pub const fn mcp_authorization(&self) -> Option<&McpAuthorizationRequirement> {
        self.mcp_authorization.as_ref()
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
