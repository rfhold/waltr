use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use super::{McpOAuthEndpointPolicy, discovery::validate_endpoint};
use crate::{Error, Result};

/// RFC 9728 OAuth protected resource metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Supported `DPoP` signing algorithms.
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
    ///
    /// # Errors
    ///
    /// Returns an error when the resource, authorization servers, or URL is invalid.
    pub fn validate_for_mcp_resource(&self, expected_resource: &str) -> Result<()> {
        if self.resource != expected_resource {
            return Err(Error::protocol(
                "MCP protected resource metadata resource mismatch",
            ));
        }
        if self.authorization_servers.is_empty()
            || self.authorization_servers.len() > 16
            || self.scopes_supported.len() > 256
            || self.extra.len() > 128
            || self
                .authorization_servers
                .iter()
                .chain(self.scopes_supported.iter())
                .any(|value| value.len() > 2048)
        {
            return Err(Error::protocol(
                "MCP protected resource metadata exceeds field limits",
            ));
        }
        let resource = Url::parse(&self.resource)
            .map_err(|_| Error::protocol("MCP protected resource identifier is invalid"))?;
        if resource.cannot_be_a_base()
            || !secure_or_loopback(&resource)
            || resource.host_str().is_none()
            || !resource.username().is_empty()
            || resource.password().is_some()
            || resource.fragment().is_some()
        {
            return Err(Error::protocol(
                "MCP protected resource identifier is invalid",
            ));
        }
        for issuer in &self.authorization_servers {
            let issuer = Url::parse(issuer).map_err(|_| {
                Error::protocol("MCP protected resource authorization server is invalid")
            })?;
            if issuer.cannot_be_a_base()
                || !secure_or_loopback(&issuer)
                || issuer.host_str().is_none()
                || !issuer.username().is_empty()
                || issuer.password().is_some()
                || issuer.query().is_some()
                || issuer.fragment().is_some()
            {
                return Err(Error::protocol(
                    "MCP protected resource authorization server is invalid",
                ));
            }
        }
        Ok(())
    }
}

fn secure_or_loopback(url: &Url) -> bool {
    url.scheme() == "https"
        || (url.scheme() == "http"
            && url.host_str().is_some_and(|host| {
                host.eq_ignore_ascii_case("localhost")
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            }))
}

/// RFC 8414 OAuth authorization server metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpAuthorizationServerMetadata {
    /// Authorization server issuer identifier.
    pub issuer: String,
    /// Authorization endpoint URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_endpoint: Option<String>,
    /// Token endpoint URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    /// Token revocation endpoint URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revocation_endpoint: Option<String>,
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
    /// Supported grant type values. Omission has the RFC 8414 default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grant_types_supported: Vec<String>,
    /// Supported token endpoint auth methods. Omission defaults to `client_secret_basic`.
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
    /// Supported access-token JWT signing algorithms.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub access_token_signing_alg_values_supported: Vec<String>,
    /// Whether authorization responses include the RFC 9207 `iss` parameter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_response_iss_parameter_supported: Option<bool>,
    /// Unknown extension metadata fields.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl McpAuthorizationServerMetadata {
    /// Create authorization-server metadata with an issuer and no advertised capabilities.
    #[must_use]
    pub fn new(issuer: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            authorization_endpoint: None,
            token_endpoint: None,
            revocation_endpoint: None,
            jwks_uri: None,
            registration_endpoint: None,
            scopes_supported: Vec::new(),
            response_types_supported: Vec::new(),
            grant_types_supported: Vec::new(),
            token_endpoint_auth_methods_supported: Vec::new(),
            code_challenge_methods_supported: Vec::new(),
            client_id_metadata_document_supported: None,
            protected_resources: Vec::new(),
            access_token_signing_alg_values_supported: Vec::new(),
            authorization_response_iss_parameter_supported: None,
            extra: BTreeMap::new(),
        }
    }

    /// Validate metadata for an MCP public Authorization Code client.
    ///
    /// RFC 8414 defaults an omitted grant list to `authorization_code` and `implicit`,
    /// but defaults an omitted token authentication list to `client_secret_basic`.
    ///
    /// # Errors
    ///
    /// Returns an error when the issuer, endpoints, or public-client capabilities are unsafe.
    pub fn validate_for_public_authorization_code_client(
        &self,
        expected_issuer: &str,
        endpoint_policy: McpOAuthEndpointPolicy,
    ) -> Result<()> {
        if self.issuer != expected_issuer {
            return Err(Error::protocol(
                "MCP authorization server metadata issuer mismatch",
            ));
        }
        self.validate_field_limits()?;
        let issuer = validate_endpoint("issuer", &self.issuer, endpoint_policy)?;
        if issuer.query().is_some() {
            return Err(Error::protocol(
                "MCP authorization server issuer must not contain a query",
            ));
        }
        let authorization_endpoint = self.authorization_endpoint.as_deref().ok_or_else(|| {
            Error::protocol("MCP authorization server metadata requires authorization_endpoint")
        })?;
        validate_endpoint(
            "authorization_endpoint",
            authorization_endpoint,
            endpoint_policy,
        )?;
        let token_endpoint = self.token_endpoint.as_deref().ok_or_else(|| {
            Error::protocol("MCP authorization server metadata requires token_endpoint")
        })?;
        validate_endpoint("token_endpoint", token_endpoint, endpoint_policy)?;
        for (name, endpoint) in [
            ("revocation_endpoint", self.revocation_endpoint.as_deref()),
            (
                "registration_endpoint",
                self.registration_endpoint.as_deref(),
            ),
            ("jwks_uri", self.jwks_uri.as_deref()),
        ] {
            if let Some(endpoint) = endpoint {
                validate_endpoint(name, endpoint, endpoint_policy)?;
            }
        }
        let authorization_code = self.grant_types_supported.is_empty()
            || self
                .grant_types_supported
                .iter()
                .any(|value| value == "authorization_code");
        if !authorization_code {
            return Err(Error::protocol(
                "MCP authorization server does not support authorization_code",
            ));
        }
        if !self
            .response_types_supported
            .iter()
            .any(|value| value == "code")
        {
            return Err(Error::protocol(
                "MCP authorization server does not support the code response type",
            ));
        }
        if !self
            .code_challenge_methods_supported
            .iter()
            .any(|value| value == "S256")
        {
            return Err(Error::protocol(
                "MCP authorization server does not support PKCE S256",
            ));
        }
        if !self
            .token_endpoint_auth_methods_supported
            .iter()
            .any(|value| value == "none")
        {
            return Err(Error::protocol(
                "MCP authorization server does not support public token clients",
            ));
        }
        Ok(())
    }

    fn validate_field_limits(&self) -> Result<()> {
        if self.response_types_supported.len() > 32
            || self.grant_types_supported.len() > 32
            || self.token_endpoint_auth_methods_supported.len() > 32
            || self.code_challenge_methods_supported.len() > 32
            || self.scopes_supported.len() > 256
            || self.protected_resources.len() > 256
            || self.extra.len() > 128
        {
            return Err(Error::protocol(
                "MCP authorization server metadata exceeds field limits",
            ));
        }
        Ok(())
    }
}
