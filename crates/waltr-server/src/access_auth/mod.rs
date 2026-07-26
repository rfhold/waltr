//! OIDC-backed authentication for access to the Waltr server.

mod config;
mod handlers;
mod jwks;
mod lease;
mod middleware;
mod oidc;
mod principal;
mod service;
mod store;

pub use config::{AccessAuthConfig, AccessAuthConfigError, AccessAuthMode, OidcConfig};
pub use handlers::{access_auth_router, api_protected_resource};
pub use middleware::{GrpcAccessAuthLayer, GrpcTransportLayer, require_browser_session};
pub use oidc::{
    AccessAuthHttpClient, AuthorizationCode, DiscoveredOidcProvider, OidcError, OidcTokenResponse,
    OidcVerifier, TokenEndpointAuthMethod,
};
pub use principal::{AuthenticatedPrincipal, AuthenticationMethod};
pub use service::{AccessAuthError, AccessAuthenticator, RequestAuthFailure};
pub use store::{
    AccessAuthStore, AccessAuthStoreError, ActiveBrowserSession, BrowserSession,
    BrowserSessionTokenDigest, CleanupResult, ConsumedLoginAttempt, LoginAttempt,
    LoginCorrelationDigest, LoginStateDigest, MAX_ACTIVE_LOGIN_ATTEMPTS,
};
