//! Provider authentication and opaque host failure envelopes.

use serde::{Deserialize, Serialize};

/// Machine-readable outbound authentication failure reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthenticationFailureReason {
    /// A usable credential is absent or expired.
    CredentialRequired,
    /// The upstream rejected a credential.
    CredentialRejected,
    /// An authorization-code or refresh exchange was rejected.
    TokenExchangeFailed,
    /// The authentication service could not be reached.
    NetworkFailed,
    /// The authentication service returned an invalid response.
    ResponseInvalid,
    /// Required OAuth registration or endpoint configuration is absent.
    ConfigurationRequired,
}

/// Durable, target-scoped outbound authentication failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthenticationFailure {
    /// Configured provider or host target that failed authentication.
    pub target: AuthTarget,
    /// Machine-readable reason for the failure.
    pub reason: AuthenticationFailureReason,
    /// Opaque host-owned authentication context, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_context: Option<serde_json::Value>,
}

/// Runtime/config target that can require authentication.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthTarget {
    /// Model provider credentials.
    Provider {
        /// Provider key from config.
        key: String,
    },
    /// Opaque host-owned authentication target.
    Host {
        /// Host-defined namespace.
        namespace: String,
        /// Host-defined target key.
        key: String,
    },
}

impl AuthTarget {
    /// Create a provider auth target.
    #[must_use]
    pub fn provider(key: impl Into<String>) -> Self {
        Self::Provider { key: key.into() }
    }

    /// Stable credential-store key for this target.
    #[must_use]
    pub fn credential_key(&self) -> String {
        match self {
            Self::Provider { key } => format!("provider:{key}"),
            Self::Host { namespace, key } => format!("{namespace}:{key}"),
        }
    }

    /// User/config key for display and lookup.
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Self::Provider { key } | Self::Host { key, .. } => key,
        }
    }
}
