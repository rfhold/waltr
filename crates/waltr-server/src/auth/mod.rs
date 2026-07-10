//! Provider credential storage and OAuth coordination.

pub(crate) mod oauth;
mod service;

pub use waltr_agent::{AuthTarget, FileCredentialStore, OAuthCredential, StoredCredential};
pub use service::AuthService;
