//! Provider-neutral OAuth protocol, discovery, and validation primitives.

mod challenge;
/// OAuth client-side helpers.
pub mod client;
mod discovery;
mod metadata;
mod protocol;
/// OAuth authorization-server seams.
pub mod server;

pub use challenge::{
    McpAccessToken, McpAuthorizationError, McpBearerChallenge, McpPrincipalId, McpTokenValidation,
};
pub use discovery::{
    McpOAuthDestinationPolicy, McpOAuthDiscoveryClient, McpOAuthEndpointPolicy,
    authorization_server_metadata_urls, discover_authorization_server_metadata,
    discover_protected_resource_metadata, protected_resource_metadata_urls,
};
pub use metadata::{McpAuthorizationServerMetadata, McpProtectedResourceMetadata};
pub use protocol::{
    McpAuthorizationCodeTokenRequest, McpAuthorizationErrorCode, McpAuthorizationErrorParameters,
    McpAuthorizationRequest, McpAuthorizationResponse, McpAuthorizationSuccess,
    McpClientMetadataDocument, McpClientMetadataError, McpOAuthGrantType, McpOAuthResponseType,
    McpOAuthSecret, McpOAuthTokenType, McpPkceChallengeMethod, McpPublicClientIdentity,
    McpRefreshTokenRequest, McpTokenEndpointAuthMethod, McpTokenError, McpTokenErrorCode,
    McpTokenResponse, validate_public_redirect_uri,
};
pub use server::McpOAuthEntropy;
#[cfg(feature = "oauth-authorization-server")]
pub use server::*;
