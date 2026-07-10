//! Error type for shared client operations.

use std::fmt;

use crate::AuthTarget;

/// Error surfaced by client transports or state transitions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientError {
    message: String,
    auth_target: Option<AuthTarget>,
}

impl ClientError {
    /// Create a client error from a displayable message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            auth_target: None,
        }
    }

    /// Create an authorization-required client error for a specific target.
    #[must_use]
    pub fn authorization_required(message: impl Into<String>, target: AuthTarget) -> Self {
        Self {
            message: message.into(),
            auth_target: Some(target),
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
