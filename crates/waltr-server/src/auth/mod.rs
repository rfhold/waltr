//! Provider credential storage and OAuth coordination.

pub(crate) mod oauth;
mod outbound_http;
mod service;

pub use waltr_agent::{AuthTarget, CredentialStore, OAuthCredential, StoredCredential};
pub(crate) use outbound_http::{TrustedPrivateOriginsError, trusted_private_origins_from_env};
pub use service::AuthService;
