//! Provider credential storage and OAuth coordination.

mod credentials;
pub(crate) mod oauth;
mod outbound_http;
pub mod refresh;
mod remote;
pub(crate) use remote::route_id as remote_route_id;
mod service;
mod types;

pub use credentials::*;
pub use remote::RemoteCredentialLease;
pub use service::AuthService;
pub(crate) use service::now_ms;
pub(crate) use service::OutboundOAuthConfig;
pub use types::*;
