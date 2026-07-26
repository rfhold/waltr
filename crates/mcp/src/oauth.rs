use std::{collections::BTreeMap, time::SystemTime};

use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result};

/// RFC 9728 OAuth protected resource metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpProtectedResourceMetadata {
    /// Protected resource identifier.
    pub resource: String,
    /// OAuth authorization server issuer identifiers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authorization_servers: Vec<String>,
    /// Optional resource JWK set URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jwks_uri: Option<String>,
    /// Supported OAuth scope values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes_supported: Vec<String>,
    /// Supported bearer token presentation methods.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bearer_methods_supported: Vec<String>,
    /// Supported resource response signing algorithms.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resource_signing_alg_values_supported: Vec<String>,
    /// Human-readable protected resource name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_name: Option<String>,
    /// Human-readable protected resource documentation URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_documentation: Option<String>,
    /// Human-readable protected resource policy URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_policy_uri: Option<String>,
    /// Human-readable protected resource terms of service URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_tos_uri: Option<String>,
    /// Whether mutual-TLS-bound access tokens are supported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_client_certificate_bound_access_tokens: Option<bool>,
    /// Supported authorization details type values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authorization_details_types_supported: Vec<String>,
    /// Supported DPoP signing algorithms.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dpop_signing_alg_values_supported: Vec<String>,
    /// Whether DPoP-bound access tokens are required.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dpop_bound_access_tokens_required: Option<bool>,
    /// Signed metadata JWT.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signed_metadata: Option<String>,
    /// Unknown extension metadata fields.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl McpProtectedResourceMetadata {
    /// Create MCP protected resource metadata with required authorization servers.
    #[must_use]
    pub fn new(
        resource: impl Into<String>,
        authorization_servers: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            resource: resource.into(),
            authorization_servers: authorization_servers.into_iter().map(Into::into).collect(),
            jwks_uri: None,
            scopes_supported: Vec::new(),
            bearer_methods_supported: vec!["header".to_owned()],
            resource_signing_alg_values_supported: Vec::new(),
            resource_name: None,
            resource_documentation: None,
            resource_policy_uri: None,
            resource_tos_uri: None,
            tls_client_certificate_bound_access_tokens: None,
            authorization_details_types_supported: Vec::new(),
            dpop_signing_alg_values_supported: Vec::new(),
            dpop_bound_access_tokens_required: None,
            signed_metadata: None,
            extra: BTreeMap::new(),
        }
    }

    /// Set supported scopes.
    #[must_use]
    pub fn with_scopes(mut self, scopes: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.scopes_supported = scopes.into_iter().map(Into::into).collect();
        self
    }

    /// Set the display name.
    #[must_use]
    pub fn with_resource_name(mut self, name: impl Into<String>) -> Self {
        self.resource_name = Some(name.into());
        self
    }

    /// Validate MCP-required protected resource metadata invariants.
    pub fn validate_for_mcp_resource(&self, expected_resource: &str) -> Result<()> {
        if self.resource != expected_resource {
            return Err(Error::protocol(
                "MCP protected resource metadata resource mismatch",
            ));
        }
        if self.authorization_servers.is_empty() {
            return Err(Error::protocol(
                "MCP protected resource metadata requires authorization_servers",
            ));
        }
        if Url::parse(&self.resource)
            .ok()
            .and_then(|url| url.fragment().map(ToOwned::to_owned))
            .is_some()
        {
            return Err(Error::protocol(
                "MCP protected resource identifier must not contain a fragment",
            ));
        }
        Ok(())
    }
}

/// RFC 8414 OAuth authorization server metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpAuthorizationServerMetadata {
    /// Authorization server issuer identifier.
    pub issuer: String,
    /// Authorization endpoint URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_endpoint: Option<String>,
    /// Token endpoint URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    /// JWK set URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jwks_uri: Option<String>,
    /// Dynamic client registration endpoint URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
    /// Supported scope values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes_supported: Vec<String>,
    /// Supported response type values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub response_types_supported: Vec<String>,
    /// Supported grant type values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grant_types_supported: Vec<String>,
    /// Supported token endpoint auth methods.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub token_endpoint_auth_methods_supported: Vec<String>,
    /// Supported PKCE code challenge methods.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub code_challenge_methods_supported: Vec<String>,
    /// Whether client ID metadata documents are supported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id_metadata_document_supported: Option<bool>,
    /// Protected resources advertised by the authorization server.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protected_resources: Vec<String>,
    /// Unknown extension metadata fields.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// OAuth bearer challenge parsed from `WWW-Authenticate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpBearerChallenge {
    /// Protected resource metadata URL.
    pub resource_metadata: Option<String>,
    /// Required scope string.
    pub scope: Option<String>,
    /// Bearer error code.
    pub error: Option<String>,
    /// Human-readable error description.
    pub error_description: Option<String>,
    /// Error documentation URL.
    pub error_uri: Option<String>,
    /// Authentication realm.
    pub realm: Option<String>,
    /// Unknown challenge parameters.
    pub parameters: BTreeMap<String, String>,
}

impl McpBearerChallenge {
    /// Create an empty bearer challenge.
    #[must_use]
    pub fn new() -> Self {
        Self {
            resource_metadata: None,
            scope: None,
            error: None,
            error_description: None,
            error_uri: None,
            realm: None,
            parameters: BTreeMap::new(),
        }
    }

    /// Parse the first Bearer challenge from a `WWW-Authenticate` header value.
    #[must_use]
    pub fn parse(header_value: &str) -> Option<Self> {
        let value = header_value.trim();
        let rest = bearer_challenge_parameters(value)?;
        let rest = rest.trim();
        let mut challenge = Self::new();
        for (name, value) in parse_auth_params(rest.trim()) {
            match name.as_str() {
                "resource_metadata" => challenge.resource_metadata = Some(value),
                "scope" => challenge.scope = Some(value),
                "error" => challenge.error = Some(value),
                "error_description" => challenge.error_description = Some(value),
                "error_uri" => challenge.error_uri = Some(value),
                "realm" => challenge.realm = Some(value),
                _ => {
                    challenge.parameters.insert(name, value);
                }
            }
        }
        Some(challenge)
    }

    /// Parse the first Bearer challenge from multiple `WWW-Authenticate` header values.
    #[must_use]
    pub fn parse_any<'a>(header_values: impl IntoIterator<Item = &'a str>) -> Option<Self> {
        header_values.into_iter().find_map(Self::parse)
    }

    /// Format this challenge as a `WWW-Authenticate` header value.
    #[must_use]
    pub fn to_header_value(&self) -> String {
        let mut params = Vec::new();
        push_param(&mut params, "realm", self.realm.as_deref());
        push_param(
            &mut params,
            "resource_metadata",
            self.resource_metadata.as_deref(),
        );
        push_param(&mut params, "scope", self.scope.as_deref());
        push_param(&mut params, "error", self.error.as_deref());
        push_param(
            &mut params,
            "error_description",
            self.error_description.as_deref(),
        );
        push_param(&mut params, "error_uri", self.error_uri.as_deref());
        for (name, value) in &self.parameters {
            params.push(format!("{name}=\"{}\"", escape_quoted(value)));
        }
        if params.is_empty() {
            "Bearer".to_owned()
        } else {
            format!("Bearer {}", params.join(", "))
        }
    }
}

impl Default for McpBearerChallenge {
    fn default() -> Self {
        Self::new()
    }
}

/// Access token extracted from an HTTP Authorization header.
#[derive(Clone, PartialEq, Eq)]
pub struct McpAccessToken(pub String);

impl std::fmt::Debug for McpAccessToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("McpAccessToken([REDACTED])")
    }
}

/// Stable identity associated with an authorized MCP access token.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct McpPrincipalId(String);

impl McpPrincipalId {
    /// Create a nonempty principal identifier no larger than 256 UTF-8 bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when the identifier is empty or exceeds 256 UTF-8 bytes.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty() || value.len() > 256 {
            return Err(Error::protocol(
                "MCP principal identifier must contain 1 to 256 UTF-8 bytes",
            ));
        }
        Ok(Self(value))
    }

    /// Borrow the principal identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for McpPrincipalId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Result returned by a Streamable HTTP bearer token validator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTokenValidation {
    /// Token is valid for this resource and request.
    Authorized {
        /// Stable identity used to bind Streamable HTTP sessions.
        principal_id: McpPrincipalId,
        /// Credential expiry used to bound active streams.
        expires_at: Option<SystemTime>,
    },
    /// Token is absent, invalid, expired, or not audience-bound to this resource.
    Unauthorized {
        /// Optional human-readable reason.
        error_description: Option<String>,
    },
    /// Token is valid but lacks required scopes.
    InsufficientScope {
        /// Scopes required to satisfy the request.
        required_scopes: Vec<String>,
        /// Optional human-readable reason.
        error_description: Option<String>,
    },
    /// Authorization state could not be checked.
    Unavailable,
}

/// OAuth authorization failure returned by HTTP clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpAuthorizationError {
    /// HTTP status code returned by the server.
    pub status: StatusCode,
    /// Parsed bearer challenge, if present.
    pub challenge: Option<McpBearerChallenge>,
}

impl std::fmt::Display for McpAuthorizationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "MCP HTTP authorization failed with status {}",
            self.status
        )
    }
}

impl std::error::Error for McpAuthorizationError {}

/// Build RFC 9728 well-known protected resource metadata URLs for an MCP endpoint.
#[must_use]
pub fn protected_resource_metadata_urls(endpoint: &str) -> Vec<String> {
    let Ok(url) = Url::parse(endpoint) else {
        return Vec::new();
    };
    let mut urls = Vec::new();
    if let Some(path_url) = protected_resource_metadata_url_for_path(&url) {
        urls.push(path_url);
    }
    if let Some(root_url) = protected_resource_metadata_url_for_root(&url)
        && !urls.contains(&root_url)
    {
        urls.push(root_url);
    }
    urls
}

/// Build MCP authorization server discovery URLs for an issuer.
#[must_use]
pub fn authorization_server_metadata_urls(issuer: &str) -> Vec<String> {
    let Ok(url) = Url::parse(issuer) else {
        return Vec::new();
    };
    let path = url.path().trim_matches('/');
    let mut urls = Vec::new();
    if path.is_empty() {
        urls.push(with_path(&url, "/.well-known/oauth-authorization-server"));
        urls.push(with_path(&url, "/.well-known/openid-configuration"));
    } else {
        urls.push(with_path(
            &url,
            &format!("/.well-known/oauth-authorization-server/{path}"),
        ));
        urls.push(with_path(
            &url,
            &format!("/.well-known/openid-configuration/{path}"),
        ));
        urls.push(with_path(
            &url,
            &format!("/{path}/.well-known/openid-configuration"),
        ));
    }
    urls
}

/// Fetch and validate protected resource metadata for an MCP endpoint.
pub async fn discover_protected_resource_metadata(
    client: &reqwest::Client,
    endpoint: &str,
    challenge: Option<&McpBearerChallenge>,
) -> Result<McpProtectedResourceMetadata> {
    let urls = if let Some(metadata_url) = challenge.and_then(|challenge| {
        challenge
            .resource_metadata
            .as_deref()
            .filter(|value| !value.trim().is_empty())
    }) {
        vec![metadata_url.to_owned()]
    } else {
        protected_resource_metadata_urls(endpoint)
    };
    for url in urls {
        let response = client.get(&url).send().await?;
        if !response.status().is_success() {
            continue;
        }
        let metadata = response.json::<McpProtectedResourceMetadata>().await?;
        if metadata.validate_for_mcp_resource(endpoint).is_ok() {
            return Ok(metadata);
        }
    }
    Err(Error::protocol(
        "MCP protected resource metadata discovery failed",
    ))
}

/// Fetch and validate authorization server metadata for an issuer.
pub async fn discover_authorization_server_metadata(
    client: &reqwest::Client,
    issuer: &str,
) -> Result<McpAuthorizationServerMetadata> {
    for url in authorization_server_metadata_urls(issuer) {
        let response = client.get(url).send().await?;
        if !response.status().is_success() {
            continue;
        }
        let metadata = response.json::<McpAuthorizationServerMetadata>().await?;
        if metadata.issuer == issuer {
            return Ok(metadata);
        }
    }
    Err(Error::protocol(
        "MCP authorization server metadata discovery failed",
    ))
}

fn protected_resource_metadata_url_for_path(url: &Url) -> Option<String> {
    let path = url.path().trim_start_matches('/');
    if path.is_empty() && url.query().is_none() {
        return None;
    }
    let well_known_path = if path.is_empty() {
        "/.well-known/oauth-protected-resource".to_owned()
    } else {
        format!("/.well-known/oauth-protected-resource/{path}")
    };
    Some(with_path_and_query(url, &well_known_path, true))
}

fn protected_resource_metadata_url_for_root(url: &Url) -> Option<String> {
    Some(with_path_and_query(
        url,
        "/.well-known/oauth-protected-resource",
        false,
    ))
}

fn with_path(url: &Url, path: &str) -> String {
    with_path_and_query(url, path, false)
}

fn with_path_and_query(url: &Url, path: &str, preserve_query: bool) -> String {
    let mut url = url.clone();
    url.set_path(path);
    if !preserve_query {
        url.set_query(None);
    }
    url.set_fragment(None);
    url.to_string()
}

fn bearer_challenge_parameters(value: &str) -> Option<&str> {
    let mut in_quote = false;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if in_quote => escaped = true,
            '"' => in_quote = !in_quote,
            _ => {}
        }
        if in_quote {
            continue;
        }
        let candidate = &value[index..];
        let boundary = index == 0
            || value[..index]
                .chars()
                .next_back()
                .is_some_and(|previous| previous == ',' || previous.is_ascii_whitespace());
        if boundary && candidate.len() >= "Bearer".len() {
            let (scheme, rest) = candidate.split_at("Bearer".len());
            if scheme.eq_ignore_ascii_case("Bearer")
                && (rest.is_empty()
                    || rest.starts_with(|character: char| character.is_ascii_whitespace()))
            {
                return Some(rest);
            }
        }
    }
    None
}

fn push_param(params: &mut Vec<String>, name: &str, value: Option<&str>) {
    if let Some(value) = value {
        params.push(format!("{name}=\"{}\"", escape_quoted(value)));
    }
}

fn escape_quoted(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn parse_auth_params(value: &str) -> Vec<(String, String)> {
    let mut params = Vec::new();
    let mut chars = value.chars().peekable();
    loop {
        while chars
            .peek()
            .is_some_and(|character| character.is_ascii_whitespace() || *character == ',')
        {
            chars.next();
        }
        let mut name = String::new();
        while chars
            .peek()
            .is_some_and(|character| *character != '=' && *character != ',')
        {
            name.push(chars.next().expect("peeked character exists"));
        }
        let name = name.trim().to_owned();
        if name.is_empty() || chars.next() != Some('=') {
            break;
        }
        while chars
            .peek()
            .is_some_and(|character| character.is_ascii_whitespace())
        {
            chars.next();
        }
        let value = if chars.peek() == Some(&'"') {
            chars.next();
            let mut output = String::new();
            while let Some(character) = chars.next() {
                match character {
                    '"' => break,
                    '\\' => {
                        if let Some(escaped) = chars.next() {
                            output.push(escaped);
                        }
                    }
                    _ => output.push(character),
                }
            }
            output
        } else {
            let mut output = String::new();
            while chars.peek().is_some_and(|character| *character != ',') {
                output.push(chars.next().expect("peeked character exists"));
            }
            output.trim().to_owned()
        };
        params.push((name, value));
    }
    params
}
