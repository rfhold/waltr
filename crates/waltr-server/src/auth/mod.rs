//! Provider credential storage and OAuth coordination.

mod credentials;
pub(crate) mod oauth;
mod outbound_http;
pub mod refresh;
mod service;
mod types;
mod workspace_binding;

pub use credentials::*;
pub(crate) use outbound_http::{TrustedPrivateOriginsError, trusted_private_origins_from_env};
pub use service::AuthService;
pub(crate) use service::OutboundOAuthConfig;
pub use types::*;
pub use workspace_binding::WorkspaceBindingCredentialLease;
