use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Durable provider credential stored outside shared config.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StoredCredential {
    /// OAuth access and refresh tokens.
    OAuth(OAuthCredential),
}

/// OAuth token material and non-secret metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCredential {
    /// OAuth access token.
    pub access: String,
    /// OAuth refresh token.
    pub refresh: String,
    /// Expiration timestamp in Unix milliseconds.
    pub expires_at_ms: i64,
    /// Non-secret provider-specific metadata.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}
