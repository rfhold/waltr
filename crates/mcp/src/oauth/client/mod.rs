//! OAuth client-side protocol and hardened HTTP discovery helpers.

pub use super::discovery::{
    McpOAuthDestinationPolicy, McpOAuthDiscoveryClient, McpOAuthEndpointPolicy,
};

use super::{McpClientRegistrationResponse, McpPublicClientRegistrationRequest};
use crate::Result;

/// Register an RFC 7591 public client and reject confidential or mismatched
/// registration responses.
///
/// # Errors
///
/// Returns an error when transport, response parsing, or public-client
/// validation fails.
pub async fn register_public_client(
    client: &McpOAuthDiscoveryClient,
    registration_endpoint: &str,
    request: &McpPublicClientRegistrationRequest,
) -> Result<McpClientRegistrationResponse> {
    let response = client
        .post_json::<_, McpClientRegistrationResponse>(registration_endpoint, request)
        .await?;
    response.validate_public_response(request)?;
    Ok(response)
}
