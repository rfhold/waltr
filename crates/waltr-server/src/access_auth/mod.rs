//! OIDC-backed authentication for access to the Waltr server.

mod config;
mod device;
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
pub(crate) use lease::LeaseMonitor;
pub(crate) use middleware::{guard_response, lease_end_response};
pub use oidc::{
    AccessAuthHttpClient, AuthorizationCode, DiscoveredOidcProvider, OidcError, OidcTokenResponse,
    OidcVerifier, TokenEndpointAuthMethod,
};
pub use principal::{AuthenticatedPrincipal, AuthenticationMethod};
pub use service::{AccessAuthError, AccessAuthenticator, RequestAuthFailure};
pub use store::{
    AccessAuthStore, AccessAuthStoreError, ActiveBrowserSession, BrowserSession,
    BrowserSessionTokenDigest, CleanupResult, ConsumedLoginAttempt, LoginAttempt,
    LoginCorrelationDigest, LoginStateDigest, MAX_ACTIVE_LOGIN_ATTEMPTS, McpOAuthLoginContinuation,
};

const fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
