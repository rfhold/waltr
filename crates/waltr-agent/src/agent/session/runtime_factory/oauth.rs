use std::{
    collections::BTreeMap,
    net::IpAddr,
    time::{Duration, SystemTime},
};

use futures_util::StreamExt as _;
use mcp::{
    McpOAuthGrantType, McpOAuthSecret, McpRefreshTokenRequest, McpTokenError, McpTokenErrorCode,
    McpTokenResponse,
};
use reqwest::{Client, Response, Url, header::CONTENT_TYPE};

use super::AgentSessionRuntimeError;
use crate::{
    AuthenticationFailureReason, McpOAuthState, OAuthCredential, SessionOAuthProviderSettings,
    SessionOAuthTokenFieldTarget, SessionOAuthTokenRequestStyle,
};

const TOKEN_RESPONSE_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RefreshFailureDisposition {
    RetrySafe,
    ReauthorizationRequired,
    Indeterminate,
}

pub(super) struct OAuthRefreshError {
    pub(super) reason: AuthenticationFailureReason,
    pub(super) message: String,
    pub(super) disposition: RefreshFailureDisposition,
}

pub(super) async fn refresh_oauth(
    definition: &SessionOAuthProviderSettings,
    credential: &OAuthCredential,
) -> Result<OAuthCredential, AgentSessionRuntimeError> {
    let pairs = vec![
        ("grant_type", "refresh_token"),
        ("client_id", definition.client_id.as_str()),
        ("refresh_token", credential.refresh.as_str()),
    ];
    let builder = reqwest::Client::new().post(&definition.token_url);
    let response = match definition.token_request_style {
        SessionOAuthTokenRequestStyle::Json => builder.json(&pairs_to_json(&pairs)).send().await,
        SessionOAuthTokenRequestStyle::FormUrlencoded => builder.form(&pairs).send().await,
    }
    .map_err(|error| {
        AgentSessionRuntimeError::FailedPrecondition(format!("OAuth token request failed: {error}"))
    })?;
    let mut next = token_response_to_credential(definition, response).await?;
    if next.refresh.is_empty() {
        next.refresh.clone_from(&credential.refresh);
    }
    Ok(next)
}

pub(super) async fn refresh_mcp_oauth(
    credential: &McpOAuthState,
    allow_loopback_http: bool,
) -> Result<McpOAuthState, OAuthRefreshError> {
    let refresh_token = credential
        .token
        .refresh_token
        .as_deref()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            refresh_error(
                AuthenticationFailureReason::CredentialRequired,
                "MCP OAuth credential does not include a refresh token",
                RefreshFailureDisposition::ReauthorizationRequired,
            )
        })?;
    if credential.endpoints.token.is_empty() || credential.registration.client_id.is_empty() {
        return Err(refresh_error(
            AuthenticationFailureReason::ConfigurationRequired,
            "MCP OAuth refresh configuration is missing",
            RefreshFailureDisposition::RetrySafe,
        ));
    }

    let request = McpRefreshTokenRequest {
        grant_type: McpOAuthGrantType::RefreshToken,
        refresh_token: McpOAuthSecret::new(refresh_token),
        client_id: credential.registration.client_id.clone(),
        scope: None,
        resource: Some(credential.resource.clone()),
    };
    let (token_url, client) =
        oauth_client_for(&credential.endpoints.token, allow_loopback_http).await?;
    let response = client
        .post(token_url)
        .form(&request)
        .send()
        .await
        .map_err(|_| {
            refresh_error(
                AuthenticationFailureReason::NetworkFailed,
                "MCP OAuth token request failed",
                RefreshFailureDisposition::Indeterminate,
            )
        })?;
    validate_response_peer(&response, allow_loopback_http)?;
    let status = response.status();
    let body = read_json_body(response).await?;
    if !status.is_success() {
        let token_error = serde_json::from_slice::<McpTokenError>(&body).ok();
        let invalid_grant = token_error
            .as_ref()
            .is_some_and(|error| error.error == McpTokenErrorCode::InvalidGrant);
        let disposition = if status.is_server_error() {
            RefreshFailureDisposition::Indeterminate
        } else {
            RefreshFailureDisposition::ReauthorizationRequired
        };
        return Err(refresh_error(
            if invalid_grant {
                AuthenticationFailureReason::CredentialRejected
            } else {
                AuthenticationFailureReason::TokenExchangeFailed
            },
            if invalid_grant {
                "MCP OAuth refresh token was rejected"
            } else {
                "MCP OAuth token endpoint rejected the refresh request"
            },
            disposition,
        ));
    }
    let token: McpTokenResponse = serde_json::from_slice(&body).map_err(|_| {
        refresh_error(
            AuthenticationFailureReason::ResponseInvalid,
            "MCP OAuth token response was invalid",
            RefreshFailureDisposition::Indeterminate,
        )
    })?;
    if token.access_token.expose_secret().is_empty() {
        return Err(refresh_error(
            AuthenticationFailureReason::ResponseInvalid,
            "MCP OAuth token response did not include an access token",
            RefreshFailureDisposition::Indeterminate,
        ));
    }

    let mut next = credential.clone();
    next.token.access_token = token.access_token.expose_secret().to_owned();
    next.token.token_type = "Bearer".to_owned();
    if let Some(refresh_token) = token.refresh_token {
        if !refresh_token.expose_secret().is_empty() {
            next.token.refresh_token = Some(refresh_token.expose_secret().to_owned());
        }
    }
    if let Some(scope) = token.scope {
        next.token.granted_scopes = scope.split_whitespace().map(ToOwned::to_owned).collect();
    }
    if let Some(seconds) = token.expires_in {
        next.token.expires_at_ms = now_ms().saturating_add(
            i64::try_from(seconds)
                .unwrap_or(i64::MAX)
                .saturating_mul(1000),
        );
    } else {
        next.token.set_expiry_unknown();
    }
    next.token.refresh_generation = credential.token.refresh_generation.saturating_add(1);
    Ok(next)
}

fn pairs_to_json(pairs: &[(&str, &str)]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (key, value) in pairs {
        map.insert(
            (*key).to_owned(),
            serde_json::Value::String((*value).to_owned()),
        );
    }
    serde_json::Value::Object(map)
}

async fn token_response_to_credential(
    definition: &SessionOAuthProviderSettings,
    response: reqwest::Response,
) -> Result<OAuthCredential, AgentSessionRuntimeError> {
    let status = response.status();
    let body = response.text().await.map_err(|error| {
        AgentSessionRuntimeError::FailedPrecondition(format!(
            "OAuth token response read failed: {error}"
        ))
    })?;
    if !status.is_success() {
        return Err(AgentSessionRuntimeError::FailedPrecondition(format!(
            "OAuth token request returned {status}: {body}"
        )));
    }
    let value: serde_json::Value = serde_json::from_str(&body).map_err(|error| {
        AgentSessionRuntimeError::FailedPrecondition(format!(
            "OAuth token response was not JSON: {error}"
        ))
    })?;
    let mut credential = OAuthCredential {
        access: value
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        refresh: value
            .get("refresh_token")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        expires_at_ms: value
            .get("expires_in")
            .and_then(serde_json::Value::as_i64)
            .map_or(0, |seconds| now_ms() + seconds * 1000),
        metadata: BTreeMap::new(),
    };
    for field in &definition.token_fields {
        let Some(field_value) = value.get(&field.source) else {
            continue;
        };
        match field.target {
            SessionOAuthTokenFieldTarget::Access => {
                credential.access = field_value.as_str().unwrap_or_default().to_owned();
            }
            SessionOAuthTokenFieldTarget::Refresh => {
                credential.refresh = field_value.as_str().unwrap_or_default().to_owned();
            }
            SessionOAuthTokenFieldTarget::ExpiresAtMs => {
                credential.expires_at_ms = field_value.as_i64().unwrap_or_default();
            }
            SessionOAuthTokenFieldTarget::ExpiresInSeconds => {
                credential.expires_at_ms =
                    now_ms() + field_value.as_i64().unwrap_or_default() * 1000;
            }
            SessionOAuthTokenFieldTarget::Metadata => {
                credential
                    .metadata
                    .insert(field.source.clone(), json_scalar_to_string(field_value));
            }
        }
    }
    if credential.access.is_empty() {
        return Err(AgentSessionRuntimeError::FailedPrecondition(
            "OAuth token response did not include an access token".to_owned(),
        ));
    }
    Ok(credential)
}

fn json_scalar_to_string(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), ToOwned::to_owned)
}

async fn oauth_client_for(
    value: &str,
    allow_loopback_http: bool,
) -> Result<(Url, Client), OAuthRefreshError> {
    let url = Url::parse(value).map_err(|_| refresh_configuration_error())?;
    let host = url.host_str().ok_or_else(refresh_configuration_error)?;
    let loopback_host = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || (url.scheme() != "https"
            && !(allow_loopback_http && url.scheme() == "http" && loopback_host))
    {
        return Err(refresh_configuration_error());
    }
    let port = url
        .port_or_known_default()
        .ok_or_else(refresh_configuration_error)?;
    let addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| {
            refresh_error(
                AuthenticationFailureReason::NetworkFailed,
                "MCP OAuth token endpoint DNS resolution failed",
                RefreshFailureDisposition::RetrySafe,
            )
        })?
        .collect::<Vec<_>>();
    if addresses.is_empty()
        || addresses
            .iter()
            .any(|address| !allows_address(address.ip(), allow_loopback_http))
    {
        return Err(refresh_configuration_error());
    }
    let mut builder = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10));
    if host.parse::<IpAddr>().is_err() {
        builder = builder.resolve_to_addrs(host, &addresses);
    }
    let client = builder.build().map_err(|_| {
        refresh_error(
            AuthenticationFailureReason::NetworkFailed,
            "failed to build MCP OAuth token client",
            RefreshFailureDisposition::RetrySafe,
        )
    })?;
    Ok((url, client))
}

fn validate_response_peer(
    response: &Response,
    allow_loopback_http: bool,
) -> Result<(), OAuthRefreshError> {
    if response
        .remote_addr()
        .is_some_and(|peer| !allows_address(peer.ip(), allow_loopback_http))
    {
        return Err(refresh_configuration_error());
    }
    Ok(())
}

async fn read_json_body(response: Response) -> Result<Vec<u8>, OAuthRefreshError> {
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .unwrap_or_default();
    if content_type != "application/json"
        && !(content_type.starts_with("application/") && content_type.ends_with("+json"))
    {
        return Err(refresh_error(
            AuthenticationFailureReason::ResponseInvalid,
            "MCP OAuth token response content type is not JSON",
            RefreshFailureDisposition::Indeterminate,
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > TOKEN_RESPONSE_LIMIT as u64)
    {
        return Err(oversized_response_error());
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| {
            refresh_error(
                AuthenticationFailureReason::NetworkFailed,
                "MCP OAuth token response read failed",
                RefreshFailureDisposition::Indeterminate,
            )
        })?;
        if chunk.len() > TOKEN_RESPONSE_LIMIT.saturating_sub(body.len()) {
            return Err(oversized_response_error());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn refresh_configuration_error() -> OAuthRefreshError {
    refresh_error(
        AuthenticationFailureReason::ConfigurationRequired,
        "MCP OAuth token endpoint is not a permitted destination",
        RefreshFailureDisposition::RetrySafe,
    )
}

fn oversized_response_error() -> OAuthRefreshError {
    refresh_error(
        AuthenticationFailureReason::ResponseInvalid,
        "MCP OAuth token response exceeds the size limit",
        RefreshFailureDisposition::Indeterminate,
    )
}

fn refresh_error(
    reason: AuthenticationFailureReason,
    message: impl Into<String>,
    disposition: RefreshFailureDisposition,
) -> OAuthRefreshError {
    OAuthRefreshError {
        reason,
        message: message.into(),
        disposition,
    }
}

fn allows_address(address: IpAddr, allow_loopback: bool) -> bool {
    if address.is_loopback() {
        return allow_loopback;
    }
    match address {
        IpAddr::V4(address) => {
            let octets = address.octets();
            !(address.is_private()
                || address.is_link_local()
                || address.is_unspecified()
                || address.is_multicast()
                || address.is_broadcast()
                || address.is_documentation()
                || octets[0] == 0
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
                || (octets[0] == 198 && matches!(octets[1], 18 | 19))
                || octets[0] >= 240)
        }
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4_mapped() {
                return allows_address(IpAddr::V4(mapped), allow_loopback);
            }
            let segments = address.segments();
            !(address.is_unspecified()
                || address.is_multicast()
                || address.is_unique_local()
                || address.is_unicast_link_local()
                || (segments[0] == 0x2001 && segments[1] == 0x0db8))
        }
    }
}

pub(super) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}
