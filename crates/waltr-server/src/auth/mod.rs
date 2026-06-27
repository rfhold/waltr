//! Provider credential storage and OAuth coordination.

mod credential;
pub(crate) mod oauth;
mod service;
mod store;

pub use credential::{OAuthCredential, StoredCredential};
pub use service::AuthService;
pub use store::FileCredentialStore;
