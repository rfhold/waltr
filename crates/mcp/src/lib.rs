//! Generic Model Context Protocol primitives, client transports, and server helpers.

mod capabilities;
/// MCP client connection and transport helpers.
pub mod client;
mod error;
mod log;
/// MCP OAuth protected-resource discovery and bearer-token validation helpers.
pub mod oauth;
/// Opinionated progressive-workflow MCP server helpers.
#[cfg(feature = "progressive")]
pub mod progressive;
mod prompt;
/// JSON-RPC and MCP wire protocol helpers.
pub mod protocol;
mod resource;
/// MCP server handler traits and stdio/HTTP serving helpers.
pub mod server;
mod tool;
mod trace;
mod types;

pub use capabilities::{
    McpElicitationCapability, McpPromptsCapability, McpResourcesCapability, McpSamplingCapability,
    McpServerCapabilities, McpTasksCapability, McpToolsCapability,
};
pub use error::{Error, Result};
pub use log::McpLogLevel;
#[cfg(feature = "macros")]
pub use mcp_macros::mcp_server;
#[cfg(feature = "progressive")]
pub use mcp_macros::progressive_server;
#[cfg(feature = "oauth-authorization-server")]
pub use oauth::server::*;
pub use oauth::{
    McpAccessToken, McpAuthorizationCodeTokenRequest, McpAuthorizationError,
    McpAuthorizationErrorCode, McpAuthorizationErrorParameters, McpAuthorizationRequest,
    McpAuthorizationResponse, McpAuthorizationServerMetadata, McpAuthorizationSuccess,
    McpBearerChallenge, McpClientMetadataDocument, McpClientMetadataError,
    McpClientRegistrationError, McpClientRegistrationErrorCode, McpClientRegistrationResponse,
    McpJwtAccessTokenClaims, McpJwtAccessTokenHeader, McpJwtAudience, McpOAuthClock,
    McpOAuthDestinationPolicy, McpOAuthDiscoveryClient, McpOAuthEndpointPolicy, McpOAuthEntropy,
    McpOAuthGrantType, McpOAuthResponseType, McpOAuthSecret, McpOAuthTokenType,
    McpPkceChallengeMethod, McpPrincipalId, McpProtectedResourceMetadata, McpPublicClientIdentity,
    McpPublicClientRegistrationRequest, McpRefreshTokenRequest, McpRevocationError,
    McpRevocationErrorCode, McpRevocationRequest, McpRevocationResponse,
    McpRevocationTokenTypeHint, McpSystemOAuthClock, McpTokenEndpointAuthMethod, McpTokenError,
    McpTokenErrorCode, McpTokenResponse, McpTokenValidation, authorization_server_metadata_urls,
    discover_authorization_server_metadata, discover_protected_resource_metadata,
    protected_resource_metadata_urls, validate_public_redirect_uri,
};
pub use prompt::{
    McpPromptArgument, McpPromptDefinition, format_prompt_result, parse_prompt_definition,
};
pub use resource::{
    McpCompletionResult, McpResourceDefinition, McpResourceTemplateDefinition,
    format_resource_result, parse_completion_result, parse_resource_definition,
    parse_resource_template_definition,
};
pub use tool::{McpToolDefinition, McpToolTaskSupport, format_tool_result, parse_tool_definition};
pub use trace::McpTracePropagation;
pub use types::{
    McpCompletionRequest, McpIcon, McpProgressNotification, McpPromptList, McpPromptResult,
    McpResourceList, McpResourceResult, McpResourceTemplateList, McpRoot, McpToolCall, McpToolList,
    McpToolResult,
};
