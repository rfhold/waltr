use std::{
    collections::{BTreeSet, HashSet},
    error::Error,
    fmt,
    time::{Duration, SystemTime},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{
    Algorithm, DecodingKey, Validation, decode, decode_header,
    jwk::{Jwk, KeyAlgorithm},
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256, Sha384, Sha512};
use url::{Position, Url};

use super::{
    config::{OidcConfig, OidcTransportPolicy},
    jwks::JwksCache,
    principal::{AuthenticatedPrincipal, AuthenticationMethod},
};

const ALLOWED_ALGORITHMS: &[Algorithm] = &[
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];
const ID_TOKEN_CLOCK_SKEW_SECONDS: u64 = 60;
const MAX_ID_TOKEN_AGE_SECONDS: u64 = 10 * 60;
const DISCOVERY_RESPONSE_LIMIT: usize = 64 * 1024;
const TOKEN_RESPONSE_LIMIT: usize = 64 * 1024;

/// Authentication style selected for the authorization-code token request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenEndpointAuthMethod {
    /// HTTP Basic authentication with the client ID and secret.
    ClientSecretBasic,
    /// Client credentials in the form body.
    ClientSecretPost,
    /// Public-client request without a client secret.
    None,
}

/// Redirect-free HTTP client for OIDC back-channel requests.
#[derive(Debug, Clone)]
pub struct AccessAuthHttpClient(Client);

impl AccessAuthHttpClient {
    /// Build a client from caller-selected timeout and TLS settings while always disabling redirects.
    ///
    /// # Errors
    ///
    /// Returns an error when Reqwest cannot construct the client.
    pub fn build(builder: reqwest::ClientBuilder) -> Result<Self, reqwest::Error> {
        builder
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map(Self)
    }

    pub(super) const fn inner(&self) -> &Client {
        &self.0
    }
}

/// Validated OIDC discovery metadata and initial signing keys.
#[derive(Debug, Clone)]
pub struct DiscoveredOidcProvider {
    config: OidcConfig,
    client: AccessAuthHttpClient,
    authorization_endpoint: Url,
    token_endpoint: Url,
    token_endpoint_auth_method: TokenEndpointAuthMethod,
    id_token_signing_algorithms: BTreeSet<String>,
    access_token_signing_algorithms: BTreeSet<String>,
    jwks: JwksCache,
}

impl DiscoveredOidcProvider {
    /// Discover and strictly validate an OIDC provider, including its initial JWKS.
    ///
    /// # Errors
    ///
    /// Returns an error when discovery, metadata validation, or initial key loading fails.
    #[allow(
        clippy::too_many_lines,
        reason = "provider discovery is one ordered validation boundary"
    )]
    pub async fn discover(
        client: AccessAuthHttpClient,
        config: OidcConfig,
    ) -> Result<Self, OidcError> {
        let issuer_url = Url::parse(config.issuer())
            .map_err(|_| OidcError::Discovery("configured issuer is invalid"))?;
        enforce_secure_endpoint(&issuer_url, config.transport_policy())?;
        let discovery_urls = discovery_urls(&issuer_url)?;
        let mut response = client
            .inner()
            .get(discovery_urls[0].clone())
            .send()
            .await
            .map_err(|_| OidcError::Discovery("OIDC discovery request failed"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND && discovery_urls.len() == 2 {
            response = client
                .inner()
                .get(discovery_urls[1].clone())
                .send()
                .await
                .map_err(|_| OidcError::Discovery("OIDC discovery request failed"))?;
        }
        if !response.status().is_success() {
            return Err(OidcError::Discovery(
                "OIDC discovery endpoint returned an error",
            ));
        }
        let body = read_bounded_body(
            response,
            DISCOVERY_RESPONSE_LIMIT,
            "OIDC discovery response read failed",
            "OIDC discovery response exceeds the size limit",
        )
        .await?;
        let metadata = serde_json::from_slice::<ProviderMetadata>(&body)
            .map_err(|_| OidcError::Discovery("OIDC discovery document is invalid"))?;
        validate_discovered_issuer(config.issuer(), &metadata.issuer)?;
        if !metadata
            .response_types_supported
            .iter()
            .any(|value| value == "code")
            || metadata
                .grant_types_supported
                .as_ref()
                .is_some_and(|values| !values.iter().any(|value| value == "authorization_code"))
        {
            return Err(OidcError::Discovery(
                "provider does not support the authorization-code flow",
            ));
        }
        if !metadata
            .code_challenge_methods_supported
            .as_ref()
            .is_some_and(|values| values.iter().any(|value| value == "S256"))
        {
            return Err(OidcError::Discovery(
                "provider does not advertise PKCE S256 support",
            ));
        }
        let authorization_endpoint =
            endpoint(&metadata.authorization_endpoint, config.transport_policy())?;
        let token_endpoint = endpoint(&metadata.token_endpoint, config.transport_policy())?;
        let jwks_uri = endpoint(&metadata.jwks_uri, config.transport_policy())?;
        let id_token_signing_algorithms = metadata
            .id_token_signing_alg_values_supported
            .into_iter()
            .filter_map(|name| parse_allowed_algorithm(&name).map(|_| name))
            .collect::<BTreeSet<_>>();
        if id_token_signing_algorithms.is_empty() {
            return Err(OidcError::Discovery(
                "provider has no allowed asymmetric ID-token signing algorithm",
            ));
        }
        let access_token_signing_algorithms =
            match metadata.access_token_signing_alg_values_supported {
                Some(values) => {
                    let algorithms = values
                        .into_iter()
                        .filter_map(|name| parse_allowed_algorithm(&name).map(|_| name))
                        .collect::<BTreeSet<_>>();
                    if algorithms.is_empty() {
                        return Err(OidcError::Discovery(
                            "provider has no allowed asymmetric access-token signing algorithm",
                        ));
                    }
                    algorithms
                }
                None => ALLOWED_ALGORITHMS
                    .iter()
                    .map(|algorithm| algorithm_name(*algorithm).to_owned())
                    .collect(),
            };
        let token_endpoint_auth_method = select_token_auth(
            config.client_secret().is_some(),
            metadata.token_endpoint_auth_methods_supported.as_deref(),
        )?;
        let cache_algorithms = id_token_signing_algorithms
            .union(&access_token_signing_algorithms)
            .cloned()
            .collect::<HashSet<_>>();
        let jwks = JwksCache::initialize(client.clone(), jwks_uri, cache_algorithms).await?;
        Ok(Self {
            config,
            client,
            authorization_endpoint,
            token_endpoint,
            token_endpoint_auth_method,
            id_token_signing_algorithms,
            access_token_signing_algorithms,
            jwks,
        })
    }

    /// Build an authorization-code URL using caller-persisted state, nonce, and S256 challenge.
    ///
    /// # Errors
    ///
    /// Returns an error when state, nonce, or the PKCE challenge is invalid.
    pub fn authorization_url(
        &self,
        state: &str,
        nonce: &str,
        pkce_challenge: &str,
    ) -> Result<Url, OidcError> {
        for (name, value) in [
            ("state", state),
            ("nonce", nonce),
            ("PKCE challenge", pkce_challenge),
        ] {
            if value.is_empty() || value.chars().any(char::is_control) {
                return Err(OidcError::InvalidRequest(match name {
                    "state" => "authorization state is invalid",
                    "nonce" => "authorization nonce is invalid",
                    _ => "authorization PKCE challenge is invalid",
                }));
            }
        }
        let mut url = self.authorization_endpoint.clone();
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", self.config.client_id())
            .append_pair("redirect_uri", self.config.redirect_uri().as_str())
            .append_pair("scope", &self.config.requested_scopes().join(" "))
            .append_pair("state", state)
            .append_pair("nonce", nonce)
            .append_pair("code_challenge", pkce_challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(url)
    }

    /// Exchange an authorization code using the exact registered redirect URI and PKCE verifier.
    ///
    /// # Errors
    ///
    /// Returns an error when the request is invalid or the provider rejects or malforms the response.
    pub async fn exchange_code(
        &self,
        request: &AuthorizationCode<'_>,
    ) -> Result<OidcTokenResponse, OidcError> {
        if request.code.is_empty() || request.pkce_verifier.is_empty() {
            return Err(OidcError::InvalidRequest(
                "authorization code and PKCE verifier are required",
            ));
        }
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", request.code),
            ("redirect_uri", self.config.redirect_uri().as_str()),
            ("code_verifier", request.pkce_verifier),
        ];
        let mut builder = self.client.inner().post(self.token_endpoint.clone());
        match self.token_endpoint_auth_method {
            TokenEndpointAuthMethod::ClientSecretBasic => {
                builder = builder.basic_auth(self.config.client_id(), self.config.client_secret());
            }
            TokenEndpointAuthMethod::ClientSecretPost => {
                form.push(("client_id", self.config.client_id()));
                form.push((
                    "client_secret",
                    self.config
                        .client_secret()
                        .ok_or(OidcError::Configuration("client secret is missing"))?,
                ));
            }
            TokenEndpointAuthMethod::None => form.push(("client_id", self.config.client_id())),
        }
        let response = builder
            .form(&form)
            .send()
            .await
            .map_err(|_| OidcError::Provider("token endpoint request failed"))?;
        if !response.status().is_success() {
            return Err(OidcError::Provider(
                "token endpoint rejected the authorization code",
            ));
        }
        let body = read_bounded_body(
            response,
            TOKEN_RESPONSE_LIMIT,
            "token endpoint response read failed",
            "token endpoint response exceeds the size limit",
        )
        .await?;
        let token = serde_json::from_slice::<OidcTokenResponse>(&body)
            .map_err(|_| OidcError::Provider("token endpoint response is invalid"))?;
        if token.access_token.is_empty()
            || token.id_token.is_empty()
            || !token.token_type.eq_ignore_ascii_case("bearer")
        {
            return Err(OidcError::Provider(
                "token endpoint response is missing required bearer tokens",
            ));
        }
        Ok(token)
    }

    /// Create a verifier backed by this provider's rotating key cache.
    #[must_use]
    pub fn verifier(&self) -> OidcVerifier {
        OidcVerifier {
            issuer: self.config.issuer().to_owned(),
            client_id: self.config.client_id().to_owned(),
            api_audience: self.config.api_audience().to_owned(),
            id_token_signing_algorithms: self.id_token_signing_algorithms.clone(),
            access_token_signing_algorithms: self.access_token_signing_algorithms.clone(),
            jwks: self.jwks.clone(),
        }
    }

    /// Return the selected endpoint authentication method.
    #[must_use]
    pub const fn token_endpoint_auth_method(&self) -> TokenEndpointAuthMethod {
        self.token_endpoint_auth_method
    }
}

/// Inputs retained for an authorization-code token exchange.
#[derive(Debug, Clone, Copy)]
pub struct AuthorizationCode<'a> {
    /// Authorization code returned by the provider.
    pub code: &'a str,
    /// PKCE verifier paired with the authorization request challenge.
    pub pkce_verifier: &'a str,
}

/// Successful token endpoint response.
#[derive(Deserialize)]
pub struct OidcTokenResponse {
    access_token: String,
    id_token: String,
    token_type: String,
    expires_in: Option<u64>,
}

impl fmt::Debug for OidcTokenResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OidcTokenResponse")
            .field("access_token", &"[redacted]")
            .field("id_token", &"[redacted]")
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

impl OidcTokenResponse {
    /// Access token returned by the provider.
    #[must_use]
    pub fn access_token(&self) -> &str {
        &self.access_token
    }
    /// ID token returned by the provider.
    #[must_use]
    pub fn id_token(&self) -> &str {
        &self.id_token
    }
    /// OAuth token type.
    #[must_use]
    pub fn token_type(&self) -> &str {
        &self.token_type
    }
    /// Provider-reported access-token lifetime.
    #[must_use]
    pub const fn expires_in(&self) -> Option<u64> {
        self.expires_in
    }
}

/// ID-token and JWT access-token verifier.
#[derive(Debug, Clone)]
pub struct OidcVerifier {
    issuer: String,
    client_id: String,
    api_audience: String,
    id_token_signing_algorithms: BTreeSet<String>,
    access_token_signing_algorithms: BTreeSet<String>,
    jwks: JwksCache,
}

impl OidcVerifier {
    /// Discover signing keys for an access-token-only authorization domain.
    ///
    /// # Errors
    ///
    /// Returns an error when discovery metadata or the initial JWKS is invalid or unavailable.
    pub async fn discover_access(
        client: AccessAuthHttpClient,
        issuer: &str,
        audience: &str,
        policy: OidcTransportPolicy,
    ) -> Result<Self, OidcError> {
        let issuer_url =
            Url::parse(issuer).map_err(|_| OidcError::Discovery("configured issuer is invalid"))?;
        enforce_secure_endpoint(&issuer_url, policy)?;
        let urls = discovery_urls(&issuer_url)?;
        let mut response = client
            .inner()
            .get(urls[0].clone())
            .send()
            .await
            .map_err(|_| OidcError::Discovery("OIDC discovery request failed"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND && urls.len() == 2 {
            response = client
                .inner()
                .get(urls[1].clone())
                .send()
                .await
                .map_err(|_| OidcError::Discovery("OIDC discovery request failed"))?;
        }
        if !response.status().is_success() {
            return Err(OidcError::Discovery(
                "OIDC discovery endpoint returned an error",
            ));
        }
        let body = read_bounded_body(
            response,
            DISCOVERY_RESPONSE_LIMIT,
            "OIDC discovery response read failed",
            "OIDC discovery response exceeds the size limit",
        )
        .await?;
        let metadata = serde_json::from_slice::<AccessProviderMetadata>(&body)
            .map_err(|_| OidcError::Discovery("OIDC discovery document is invalid"))?;
        validate_discovered_issuer(issuer, &metadata.issuer)?;
        let jwks_uri = endpoint(&metadata.jwks_uri, policy)?;
        let access_token_signing_algorithms: BTreeSet<String> = metadata
            .access_token_signing_alg_values_supported
            .map_or_else(
                || {
                    ALLOWED_ALGORITHMS
                        .iter()
                        .map(|algorithm| algorithm_name(*algorithm).to_owned())
                        .collect()
                },
                |values| {
                    values
                        .into_iter()
                        .filter_map(|name| parse_allowed_algorithm(&name).map(|_| name))
                        .collect()
                },
            );
        if access_token_signing_algorithms.is_empty() {
            return Err(OidcError::Discovery(
                "provider has no allowed asymmetric access-token signing algorithm",
            ));
        }
        let jwks = JwksCache::initialize(
            client,
            jwks_uri,
            access_token_signing_algorithms.iter().cloned().collect(),
        )
        .await?;
        Ok(Self {
            issuer: issuer.to_owned(),
            client_id: String::new(),
            api_audience: audience.to_owned(),
            id_token_signing_algorithms: BTreeSet::new(),
            access_token_signing_algorithms,
            jwks,
        })
    }

    /// Rebind a verifier to another audience for the same issuer and JWKS domain.
    #[must_use]
    pub fn for_access_audience(&self, audience: &str) -> Self {
        let mut verifier = self.clone();
        audience.clone_into(&mut verifier.api_audience);
        verifier
    }

    /// Validate an ID token, including nonce and optional access-token hash.
    ///
    /// # Errors
    ///
    /// Returns an error when the token signature, claims, nonce, or access-token hash is invalid.
    pub async fn validate_id_token(
        &self,
        token: &str,
        expected_nonce: &str,
        access_token: Option<&str>,
    ) -> Result<AuthenticatedPrincipal, OidcError> {
        let (header, key) = self.header_and_key(token, TokenKind::Id).await?;
        let mut validation = token_validation(header.alg, &self.issuer, &self.client_id);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        let claims = decode::<IdTokenClaims>(
            token,
            &DecodingKey::from_jwk(&key)
                .map_err(|_| OidcError::InvalidToken("JWT signing key is invalid"))?,
            &validation,
        )
        .map_err(|_| OidcError::InvalidToken("ID token validation failed"))?
        .claims;
        let now = jsonwebtoken::get_current_timestamp();
        if claims.iat > now.saturating_add(ID_TOKEN_CLOCK_SKEW_SECONDS)
            || now.saturating_sub(claims.iat)
                > MAX_ID_TOKEN_AGE_SECONDS + ID_TOKEN_CLOCK_SKEW_SECONDS
        {
            return Err(OidcError::InvalidToken(
                "ID token iat is outside the allowed freshness window",
            ));
        }
        if claims.iss != self.issuer
            || claims.sub.is_empty()
            || claims.nonce.as_deref() != Some(expected_nonce)
        {
            return Err(OidcError::InvalidToken(
                "ID token identity or nonce is invalid",
            ));
        }
        if claims.aud.len() > 1 && claims.azp.as_deref() != Some(self.client_id.as_str())
            || claims
                .azp
                .as_ref()
                .is_some_and(|azp| azp != &self.client_id)
        {
            return Err(OidcError::InvalidToken("ID token azp is invalid"));
        }
        if let Some(expected) = claims.at_hash {
            let access_token = access_token.ok_or(OidcError::InvalidToken(
                "ID token at_hash cannot be validated without an access token",
            ))?;
            if !constant_time_eq(
                expected.as_bytes(),
                access_token_hash(access_token, header.alg)?.as_bytes(),
            ) {
                return Err(OidcError::InvalidToken("ID token at_hash is invalid"));
            }
        }
        Ok(AuthenticatedPrincipal::new(
            claims.iss,
            claims.sub,
            claims.scopes.into_scopes(),
            AuthenticationMethod::OidcIdToken,
        ))
    }

    /// Validate an RFC 9068-style JWT access token and its required scopes.
    ///
    /// # Errors
    ///
    /// Returns an error when the token signature, claims, audience, or required scopes are invalid.
    pub async fn validate_access_token(
        &self,
        token: &str,
        required_scopes: &[String],
    ) -> Result<AuthenticatedPrincipal, OidcError> {
        if token.split('.').count() != 3 {
            return Err(OidcError::InvalidToken(
                "opaque access tokens are not supported",
            ));
        }
        let (header, key) = self.header_and_key(token, TokenKind::Access).await?;
        let mut validation = token_validation(header.alg, &self.issuer, &self.api_audience);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        let claims = decode::<AccessTokenClaims>(
            token,
            &DecodingKey::from_jwk(&key)
                .map_err(|_| OidcError::InvalidToken("JWT signing key is invalid"))?,
            &validation,
        )
        .map_err(|_| OidcError::InvalidToken("access token validation failed"))?
        .claims;
        if claims.iss != self.issuer || claims.sub.is_empty() {
            return Err(OidcError::InvalidToken("access token identity is invalid"));
        }
        let scopes = claims.scopes.into_scopes();
        if required_scopes
            .iter()
            .any(|required| !scopes.contains(required))
        {
            return Err(OidcError::InsufficientScope);
        }
        if claims.exp <= jsonwebtoken::get_current_timestamp() {
            return Err(OidcError::InvalidToken("access token has expired"));
        }
        let expires_at = SystemTime::UNIX_EPOCH
            .checked_add(Duration::from_secs(claims.exp))
            .ok_or(OidcError::InvalidToken("access token expiry is invalid"))?;
        Ok(AuthenticatedPrincipal::bearer(
            claims.iss, claims.sub, scopes, expires_at,
        ))
    }

    async fn header_and_key(
        &self,
        token: &str,
        token_kind: TokenKind,
    ) -> Result<(jsonwebtoken::Header, Jwk), OidcError> {
        let header = decode_protected_header(token)?;
        let policy = match token_kind {
            TokenKind::Id => &self.id_token_signing_algorithms,
            TokenKind::Access => &self.access_token_signing_algorithms,
        };
        if !ALLOWED_ALGORITHMS.contains(&header.alg) || !policy.contains(algorithm_name(header.alg))
        {
            return Err(OidcError::InvalidToken(
                "JWT signing algorithm is not allowed",
            ));
        }
        match token_kind {
            TokenKind::Id
                if header
                    .typ
                    .as_deref()
                    .is_some_and(|value| !value.eq_ignore_ascii_case("JWT")) =>
            {
                return Err(OidcError::InvalidToken(
                    "ID token typ must be absent or JWT",
                ));
            }
            TokenKind::Access if header.typ.as_deref() != Some("at+jwt") => {
                return Err(OidcError::InvalidToken("access token typ must be at+jwt"));
            }
            TokenKind::Id | TokenKind::Access => {}
        }
        let kid = header
            .kid
            .as_deref()
            .ok_or(OidcError::InvalidToken("JWT kid is missing"))?;
        let key = self.jwks.key(kid).await?;
        if key
            .common
            .key_algorithm
            .is_some_and(|algorithm| !key_algorithm_matches(algorithm, header.alg))
        {
            return Err(OidcError::InvalidToken(
                "JWT algorithm does not match its signing key",
            ));
        }
        Ok((header, key))
    }

    #[cfg(test)]
    fn fixture(issuer: &str, client_id: &str, audience: &str, jwks: JwksCache) -> Self {
        Self {
            issuer: issuer.to_owned(),
            client_id: client_id.to_owned(),
            api_audience: audience.to_owned(),
            id_token_signing_algorithms: BTreeSet::from(["RS256".to_owned()]),
            access_token_signing_algorithms: BTreeSet::from(["RS256".to_owned()]),
            jwks,
        }
    }
}

#[derive(Deserialize)]
struct CriticalHeaders {
    #[serde(default)]
    crit: Vec<String>,
}

fn decode_protected_header(token: &str) -> Result<jsonwebtoken::Header, OidcError> {
    let mut segments = token.split('.');
    let encoded = segments
        .next()
        .filter(|segment| !segment.is_empty())
        .ok_or(OidcError::InvalidToken("JWT header is invalid"))?;
    if segments.next().is_none() || segments.next().is_none() || segments.next().is_some() {
        return Err(OidcError::InvalidToken("JWT header is invalid"));
    }
    let protected = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| OidcError::InvalidToken("JWT header is invalid"))?;
    let critical = serde_json::from_slice::<CriticalHeaders>(&protected)
        .map_err(|_| OidcError::InvalidToken("JWT critical headers are invalid"))?;
    let mut names = HashSet::with_capacity(critical.crit.len());
    if critical
        .crit
        .iter()
        .any(|name| name.is_empty() || !names.insert(name))
    {
        return Err(OidcError::InvalidToken("JWT critical headers are invalid"));
    }
    if !critical.crit.is_empty() {
        return Err(OidcError::InvalidToken(
            "JWT critical headers are not supported",
        ));
    }
    decode_header(token).map_err(|_| OidcError::InvalidToken("JWT header is invalid"))
}

#[derive(Debug, Deserialize)]
struct ProviderMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    response_types_supported: Vec<String>,
    grant_types_supported: Option<Vec<String>>,
    code_challenge_methods_supported: Option<Vec<String>>,
    token_endpoint_auth_methods_supported: Option<Vec<String>>,
    id_token_signing_alg_values_supported: Vec<String>,
    access_token_signing_alg_values_supported: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct AccessProviderMetadata {
    issuer: String,
    jwks_uri: String,
    access_token_signing_alg_values_supported: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize)]
struct IdTokenClaims {
    iss: String,
    sub: String,
    aud: Audience,
    exp: u64,
    iat: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    nbf: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nonce: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    azp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    at_hash: Option<String>,
    #[serde(flatten)]
    scopes: ScopeClaims,
}

#[derive(Debug, Clone, Copy)]
enum TokenKind {
    Id,
    Access,
}

#[derive(Debug, Deserialize, Serialize)]
struct AccessTokenClaims {
    iss: String,
    sub: String,
    aud: Audience,
    exp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    nbf: Option<u64>,
    #[serde(flatten)]
    scopes: ScopeClaims,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    const fn len(&self) -> usize {
        match self {
            Self::One(_) => 1,
            Self::Many(values) => values.len(),
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ScopeClaims {
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scp: Option<ScopeValue>,
}

impl ScopeClaims {
    fn into_scopes(self) -> BTreeSet<String> {
        let mut scopes = self
            .scope
            .into_iter()
            .flat_map(|value| {
                value
                    .split_ascii_whitespace()
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect::<BTreeSet<_>>();
        if let Some(scp) = self.scp {
            scopes.extend(match scp {
                ScopeValue::Text(value) => value
                    .split_ascii_whitespace()
                    .map(ToOwned::to_owned)
                    .collect(),
                ScopeValue::List(values) => values,
            });
        }
        scopes
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum ScopeValue {
    Text(String),
    List(Vec<String>),
}

fn token_validation(algorithm: Algorithm, issuer: &str, audience: &str) -> Validation {
    let mut validation = Validation::new(algorithm);
    validation.validate_nbf = true;
    validation.set_issuer(&[issuer]);
    validation.set_audience(&[audience]);
    validation
}

fn endpoint(value: &str, policy: OidcTransportPolicy) -> Result<Url, OidcError> {
    let url =
        Url::parse(value).map_err(|_| OidcError::Discovery("provider endpoint URL is invalid"))?;
    enforce_secure_endpoint(&url, policy)?;
    Ok(url)
}

fn discovery_urls(issuer: &Url) -> Result<Vec<Url>, OidcError> {
    let primary = Url::parse(&format!(
        "{}/.well-known/openid-configuration",
        issuer.as_str().trim_end_matches('/')
    ))
    .map_err(|_| OidcError::Discovery("OIDC discovery URL is invalid"))?;
    let issuer_path = issuer.path();
    let suffix = if issuer_path == "/" { "" } else { issuer_path };
    let fallback = Url::parse(&format!(
        "{}/.well-known/openid-configuration{suffix}",
        &issuer[..Position::BeforePath]
    ))
    .map_err(|_| OidcError::Discovery("OIDC discovery URL is invalid"))?;
    if primary == fallback {
        Ok(vec![primary])
    } else {
        Ok(vec![primary, fallback])
    }
}

pub(super) async fn read_bounded_body(
    mut response: reqwest::Response,
    limit: usize,
    read_error: &'static str,
    oversized_error: &'static str,
) -> Result<Vec<u8>, OidcError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(OidcError::Provider(oversized_error));
    }
    let mut body = Vec::with_capacity(response.content_length().map_or(0, |length| {
        usize::try_from(length.min(limit as u64)).unwrap_or(limit)
    }));
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| OidcError::Provider(read_error))?
    {
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(OidcError::Provider(oversized_error));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn validate_discovered_issuer(configured: &str, discovered: &str) -> Result<(), OidcError> {
    if configured == discovered {
        Ok(())
    } else {
        Err(OidcError::Discovery(
            "discovered issuer does not exactly match configured issuer",
        ))
    }
}

fn enforce_secure_endpoint(url: &Url, policy: OidcTransportPolicy) -> Result<(), OidcError> {
    if policy.allows(url) {
        Ok(())
    } else {
        Err(OidcError::Discovery(
            "OIDC endpoints must use HTTPS except for explicitly allowed loopback development",
        ))
    }
}

fn select_token_auth(
    has_secret: bool,
    advertised: Option<&[String]>,
) -> Result<TokenEndpointAuthMethod, OidcError> {
    let supports = |method: &str| {
        advertised.map_or(method == "client_secret_basic", |values| {
            values.iter().any(|value| value == method)
        })
    };
    if has_secret && supports("client_secret_basic") {
        return Ok(TokenEndpointAuthMethod::ClientSecretBasic);
    }
    if has_secret && supports("client_secret_post") {
        return Ok(TokenEndpointAuthMethod::ClientSecretPost);
    }
    if !has_secret && supports("none") {
        return Ok(TokenEndpointAuthMethod::None);
    }
    Err(OidcError::Discovery(
        "provider has no compatible token endpoint authentication method",
    ))
}

fn parse_allowed_algorithm(name: &str) -> Option<Algorithm> {
    ALLOWED_ALGORITHMS
        .iter()
        .copied()
        .find(|algorithm| algorithm_name(*algorithm) == name)
}

const fn algorithm_name(algorithm: Algorithm) -> &'static str {
    match algorithm {
        Algorithm::RS256 => "RS256",
        Algorithm::RS384 => "RS384",
        Algorithm::RS512 => "RS512",
        Algorithm::PS256 => "PS256",
        Algorithm::PS384 => "PS384",
        Algorithm::PS512 => "PS512",
        Algorithm::ES256 => "ES256",
        Algorithm::ES384 => "ES384",
        Algorithm::HS256 => "HS256",
        Algorithm::HS384 => "HS384",
        Algorithm::HS512 => "HS512",
        Algorithm::EdDSA => "EdDSA",
    }
}

const fn key_algorithm_matches(key: KeyAlgorithm, token: Algorithm) -> bool {
    matches!(
        (key, token),
        (KeyAlgorithm::RS256, Algorithm::RS256)
            | (KeyAlgorithm::RS384, Algorithm::RS384)
            | (KeyAlgorithm::RS512, Algorithm::RS512)
            | (KeyAlgorithm::PS256, Algorithm::PS256)
            | (KeyAlgorithm::PS384, Algorithm::PS384)
            | (KeyAlgorithm::PS512, Algorithm::PS512)
            | (KeyAlgorithm::ES256, Algorithm::ES256)
            | (KeyAlgorithm::ES384, Algorithm::ES384)
            | (KeyAlgorithm::EdDSA, Algorithm::EdDSA)
    )
}

fn access_token_hash(access_token: &str, algorithm: Algorithm) -> Result<String, OidcError> {
    let digest = match algorithm {
        Algorithm::RS256 | Algorithm::PS256 | Algorithm::ES256 => {
            Sha256::digest(access_token.as_bytes()).to_vec()
        }
        Algorithm::RS384 | Algorithm::PS384 | Algorithm::ES384 => {
            Sha384::digest(access_token.as_bytes()).to_vec()
        }
        Algorithm::RS512 | Algorithm::PS512 => Sha512::digest(access_token.as_bytes()).to_vec(),
        _ => {
            return Err(OidcError::InvalidToken(
                "at_hash uses an unsupported signing algorithm",
            ));
        }
    };
    Ok(URL_SAFE_NO_PAD.encode(&digest[..digest.len() / 2]))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

/// OIDC setup, provider, or token-validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OidcError {
    /// Local configuration is inconsistent.
    Configuration(&'static str),
    /// Discovery metadata is unavailable or unsafe.
    Discovery(&'static str),
    /// An authorization input is malformed.
    InvalidRequest(&'static str),
    /// The provider request or response failed without exposing its body.
    Provider(&'static str),
    /// A token failed cryptographic or claims validation.
    InvalidToken(&'static str),
    /// A JWT referenced no currently trusted key.
    UnknownKeyId,
    /// A valid access token lacks a required scope.
    InsufficientScope,
}

impl fmt::Display for OidcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(message)
            | Self::Discovery(message)
            | Self::InvalidRequest(message)
            | Self::Provider(message)
            | Self::InvalidToken(message) => formatter.write_str(message),
            Self::UnknownKeyId => formatter.write_str("JWT kid is not present in the issuer JWKS"),
            Self::InsufficientScope => formatter.write_str("access token lacks a required scope"),
        }
    }
}

impl Error for OidcError {}

#[cfg(test)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::duration_suboptimal_units,
    clippy::needless_pass_by_value,
    clippy::needless_raw_string_hashes,
    clippy::too_many_lines,
    reason = "OIDC fixtures keep token construction and timing boundaries explicit"
)]
mod tests {
    use super::*;
    use std::{
        convert::Infallible,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use axum::{
        Json, Router,
        body::{Body, Bytes},
        extract::State,
        http::{StatusCode, header},
        response::{IntoResponse as _, Response},
        routing::{get, post},
    };
    use futures_util::stream;
    use jsonwebtoken::{EncodingKey, Header, encode, jwk::JwkSet};
    use serde_json::{Value, json};

    const TEST_PRIVATE_KEY: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCNNdGQvErjmqYo
wx04OKzHiIwdhBvAj9adzAH726wXfUhR7ACBxW1Exalb1glGQDvXc2w/5x+zg3K6
vOWgn3nmLA0YmbMEXr8TnXlz5AXIAATLIWm2SkWpyBkEVRkYeyXPwCWBTKxLKyGM
4I0bXlUSD6CN0vAMlbVGu9BAL7DVgePeyBlX9dZ9hbLjTVPkZzs53Gy1UrW4iPYu
DnG6y8oYIQG4N1hmZOODO5CoXasYVZULuvhqDcLzHEF/AClFm2Tte9lpzeQC1gMJ
WUfIVu2SZqk43Ju4OKMlfPBV9L3rHQjs+hdz5zq1I9/7tak20gOgvEdwjlC/rq1+
w5188JzzAgMBAAECggEAAq1rU1etZHJ3mUt9eZJYS7SKUlgGccKNWX8NhQnCoMxP
x83dlnDuyP30mri0DJ9BAzcWHtCtXB9giatriaUymboveuvGg8hjziqi+m3b7FtU
9wMtewWoQvJ32+Lc3n0xHKbXQHA1ut3nkRnwP0wjDMrs6D6UGf8jc2LPo5HD64EG
Ye973DBz/tqIiRYviBYOgD1dgQR0ZqM6QHbVkQ0IJbB3wQNR8zDlubShCHk1ccwz
O1k3CNYI41+r6OQ46yW9KmYyztgzMiep0/lE4C9Qqq+OQyyPG8/F0+qvUKXsJjwx
F/ddPM6VN4iaP8QzwS8ajtm5enVc5Mo0lGZDrQkJcQKBgQDBVaD3Kd+UdpympajV
YcfPru66ZqIm+FUcIjIEyWINCSg90z4w+zzz4ajnB9YktMGRlKgHz95mUm72W2iZ
SAd13HixLlvVWWKYTQH9mdJ+bWzUhzP3XzhO94s3S3oS2Unj3vewKnvwLVKbm54x
VyqF0yldNFbMJYrb/J9T+BXqRwKBgQC6+w9vpXsKm8yd1J2NagyFF1U7uJ9nWc8y
EDOMcJOdPyXrHOpYH5xidubh4Vvt+4iU2QOt15yDOifAfAaAjuP2ZCcCTORXQ+nV
Lb3GUQaGYGvlT2zm9RuAlavV1KDKmvY+F8WueFU5/lSPgu0w2FB6hCRTtcCKuC6h
JOE8ovnh9QKBgQCwhz8yb0l2hdcDUr+WFnIbb98K1Cu66YTM62xAcqDUxke4K+nd
T4mYJbPTP5SIb4NPKi1C9MpdI2euCki3rM0jEOwi2/F+HN+psKbgqFNKnR+B7vHE
LW18rQqwsnoEO+L77dq7QA9qffevnagT5UMubmCWefPG84CW3VOdmChqgQKBgEpe
JKuRM7jqcp2rvOZL8aLVvu675Zqjm8cbroUDGZMHVLuIiqR2HisDg5z/lyK2UoJx
NgdkpDmFXd4u9h8Xtg9/kgxWLNOQjWZehQZln6S+oOtZhcTuY/vmi6vGoBU1QQ89
QUC9V5hzUYqq1lABGTFBcXlh5qzFyLYLBxKo954pAoGAUtBAanvamVMGl0x3HYa9
BHysmfq4utptmwJp6bCV0oqunuJqhSwIyGPlTxkoJKDwfXgSsWoEDePd6nHrm4TG
HpmPp186M8HC6SSmdVh3CSIuSzO3DzHyDCV9k8UzzVZiCqBVm5Vv3m5w2SAQPduP
ZbAq2L8jXEetO4oBq+h/bLE=
-----END PRIVATE KEY-----"#;

    fn test_jwks(kid: &str) -> JwkSet {
        serde_json::from_value(json!({"keys": [{
            "kty": "RSA",
            "kid": kid,
            "use": "sig",
            "alg": "RS256",
            "n": "jTXRkLxK45qmKMMdODisx4iMHYQbwI_WncwB-9usF31IUewAgcVtRMWpW9YJRkA713NsP-cfs4NyurzloJ955iwNGJmzBF6_E515c-QFyAAEyyFptkpFqcgZBFUZGHslz8AlgUysSyshjOCNG15VEg-gjdLwDJW1RrvQQC-w1YHj3sgZV_XWfYWy401T5Gc7OdxstVK1uIj2Lg5xusvKGCEBuDdYZmTjgzuQqF2rGFWVC7r4ag3C8xxBfwApRZtk7XvZac3kAtYDCVlHyFbtkmapONybuDijJXzwVfS96x0I7PoXc-c6tSPf-7WpNtIDoLxHcI5Qv66tfsOdfPCc8w",
            "e": "AQAB"
        }]}))
        .unwrap()
    }

    fn unrestricted_test_jwks(kid: &str) -> JwkSet {
        let mut set = test_jwks(kid);
        set.keys[0].common.key_algorithm = None;
        set
    }

    fn verifier() -> OidcVerifier {
        OidcVerifier::fixture(
            "https://issuer.example",
            "waltr-client",
            "waltr-api",
            JwksCache::fixture(test_jwks("test-key")).unwrap(),
        )
    }

    fn sign(claims: Value, typ: &str, kid: &str) -> String {
        sign_with_typ(claims, Some(typ), kid)
    }

    fn sign_with_typ(claims: Value, typ: Option<&str>, kid: &str) -> String {
        sign_with_algorithm(claims, typ, kid, Algorithm::RS256)
    }

    fn sign_with_algorithm(
        mut claims: Value,
        typ: Option<&str>,
        kid: &str,
        algorithm: Algorithm,
    ) -> String {
        if claims.get("exp").is_none() {
            claims["exp"] = json!(jsonwebtoken::get_current_timestamp() + 300);
        }
        if claims.get("iat").is_none() && claims.get("nonce").is_some() {
            claims["iat"] = json!(jsonwebtoken::get_current_timestamp());
        }
        let mut header = Header::new(algorithm);
        header.kid = Some(kid.to_owned());
        header.typ = typ.map(ToOwned::to_owned);
        encode(
            &header,
            &claims,
            &EncodingKey::from_rsa_pem(TEST_PRIVATE_KEY.as_bytes()).unwrap(),
        )
        .unwrap()
    }

    fn sign_with_protected_header(mut claims: Value, protected_header: &str) -> String {
        if claims.get("exp").is_none() {
            claims["exp"] = json!(jsonwebtoken::get_current_timestamp() + 300);
        }
        if claims.get("iat").is_none() && claims.get("nonce").is_some() {
            claims["iat"] = json!(jsonwebtoken::get_current_timestamp());
        }
        let header = URL_SAFE_NO_PAD.encode(protected_header.as_bytes());
        let claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        let message = format!("{header}.{claims}");
        let signature = jsonwebtoken::crypto::sign(
            message.as_bytes(),
            &EncodingKey::from_rsa_pem(TEST_PRIVATE_KEY.as_bytes()).unwrap(),
            Algorithm::RS256,
        )
        .unwrap();
        format!("{message}.{signature}")
    }

    fn access_claims() -> Value {
        json!({
            "iss": "https://issuer.example",
            "sub": "subject-1",
            "aud": "waltr-api",
            "scope": "waltr:read mcp:use"
        })
    }

    fn id_claims() -> Value {
        json!({
            "iss": "https://issuer.example",
            "sub": "subject-1",
            "aud": ["waltr-client", "other"],
            "azp": "waltr-client",
            "nonce": "expected-nonce"
        })
    }

    fn streamed_body(chunks: Vec<Vec<u8>>) -> Body {
        Body::from_stream(stream::iter(
            chunks
                .into_iter()
                .map(|chunk| Ok::<_, Infallible>(Bytes::from(chunk))),
        ))
    }

    async fn spawn_server(router: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (format!("http://{address}"), server)
    }

    fn local_config(issuer: &str) -> OidcConfig {
        OidcConfig::new(
            issuer,
            "waltr-client",
            None,
            "http://127.0.0.1:14333/oidc/callback",
            "waltr-api",
            vec!["openid".to_owned()],
            Duration::from_secs(60),
            true,
            true,
        )
        .unwrap()
    }

    fn http_client() -> AccessAuthHttpClient {
        AccessAuthHttpClient::build(Client::builder()).unwrap()
    }

    fn token_provider(token_endpoint: Url) -> DiscoveredOidcProvider {
        DiscoveredOidcProvider {
            config: local_config("http://127.0.0.1:8080"),
            client: http_client(),
            authorization_endpoint: Url::parse("http://127.0.0.1:8080/authorize").unwrap(),
            token_endpoint,
            token_endpoint_auth_method: TokenEndpointAuthMethod::None,
            id_token_signing_algorithms: BTreeSet::from(["RS256".to_owned()]),
            access_token_signing_algorithms: BTreeSet::from(["RS256".to_owned()]),
            jwks: JwksCache::fixture(test_jwks("test-key")).unwrap(),
        }
    }

    #[test]
    fn endpoint_security_only_allows_explicit_loopback_http() {
        assert!(
            enforce_secure_endpoint(
                &Url::parse("https://issuer.example").unwrap(),
                OidcTransportPolicy::HttpsOnly
            )
            .is_ok()
        );
        assert!(
            enforce_secure_endpoint(
                &Url::parse("http://127.0.0.1:8080").unwrap(),
                OidcTransportPolicy::LoopbackHttp
            )
            .is_ok()
        );
        assert!(
            enforce_secure_endpoint(
                &Url::parse("http://issuer.example").unwrap(),
                OidcTransportPolicy::LoopbackHttp
            )
            .is_err()
        );
        assert!(
            enforce_secure_endpoint(
                &Url::parse("http://localhost:8080").unwrap(),
                OidcTransportPolicy::HttpsOnly
            )
            .is_err()
        );
    }

    #[test]
    fn discovery_issuer_match_is_exact() {
        assert!(
            validate_discovered_issuer("https://issuer.example", "https://issuer.example").is_ok()
        );
        assert!(
            validate_discovered_issuer("https://issuer.example", "https://issuer.example/")
                .is_err()
        );
    }

    #[test]
    fn discovery_urls_try_standard_path_then_raw_prefix_fallback() {
        assert_eq!(
            discovery_urls(&Url::parse("https://issuer.example").unwrap()).unwrap(),
            [Url::parse("https://issuer.example/.well-known/openid-configuration").unwrap()]
        );
        assert_eq!(
            discovery_urls(&Url::parse("https://issuer.example/tenant%2Fblue/sub").unwrap())
                .unwrap(),
            [
                Url::parse(
                    "https://issuer.example/tenant%2Fblue/sub/.well-known/openid-configuration"
                )
                .unwrap(),
                Url::parse(
                    "https://issuer.example/.well-known/openid-configuration/tenant%2Fblue/sub"
                )
                .unwrap(),
            ]
        );
    }

    #[tokio::test]
    async fn discovery_stream_accepts_normal_body_and_rejects_oversized_body() {
        async fn metadata(State(base): State<String>) -> Response {
            let body = serde_json::to_vec(&json!({
                "issuer": base,
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
                "jwks_uri": format!("{base}/jwks"),
                "response_types_supported": ["code"],
                "grant_types_supported": ["authorization_code"],
                "code_challenge_methods_supported": ["S256"],
                "token_endpoint_auth_methods_supported": ["none"],
                "id_token_signing_alg_values_supported": ["RS256"]
            }))
            .unwrap();
            Response::new(streamed_body(vec![
                body[..body.len() / 2].to_vec(),
                body[body.len() / 2..].to_vec(),
            ]))
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}");
        let router = Router::new()
            .route("/.well-known/openid-configuration", get(metadata))
            .route("/jwks", get(|| async { Json(test_jwks("test-key")) }))
            .with_state(base.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        assert!(
            DiscoveredOidcProvider::discover(http_client(), local_config(&base))
                .await
                .is_ok()
        );
        server.abort();

        let router = Router::new().route(
            "/.well-known/openid-configuration",
            get(|| async {
                Response::new(streamed_body(vec![
                    vec![b' '; DISCOVERY_RESPONSE_LIMIT],
                    vec![b'x'],
                ]))
            }),
        );
        let (base, server) = spawn_server(router).await;
        let error = DiscoveredOidcProvider::discover(http_client(), local_config(&base))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            OidcError::Provider("OIDC discovery response exceeds the size limit")
        );
        server.abort();
    }

    #[tokio::test]
    async fn access_only_discovery_does_not_require_browser_provider_metadata() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let issuer = format!("http://{address}");
        let metadata_issuer = issuer.clone();
        let router = Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(move || {
                    let issuer = metadata_issuer.clone();
                    async move {
                        Json(json!({
                            "issuer": issuer.clone(),
                            "jwks_uri": format!("{issuer}/jwks"),
                            "access_token_signing_alg_values_supported": ["RS256"]
                        }))
                    }
                }),
            )
            .route("/jwks", get(|| async { Json(test_jwks("test-key")) }));
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        let verifier = OidcVerifier::discover_access(
            http_client(),
            &issuer,
            "api-audience",
            OidcTransportPolicy::LoopbackHttp,
        )
        .await
        .unwrap();
        let token = sign(
            json!({
                "iss": issuer,
                "sub": "subject",
                "aud": "api-audience"
            }),
            "at+jwt",
            "test-key",
        );
        verifier.validate_access_token(&token, &[]).await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn path_issuer_uses_standard_location_then_404_only_prefix_fallback() {
        async fn metadata(State(base): State<String>) -> Json<Value> {
            let issuer = format!("{base}/tenant%2Fblue");
            Json(json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
                "jwks_uri": format!("{base}/jwks"),
                "response_types_supported": ["code"],
                "code_challenge_methods_supported": ["S256"],
                "token_endpoint_auth_methods_supported": ["none"],
                "id_token_signing_alg_values_supported": ["RS256"]
            }))
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}");
        let router = Router::new()
            .route(
                "/tenant%2Fblue/.well-known/openid-configuration",
                get(metadata),
            )
            .route("/jwks", get(|| async { Json(test_jwks("test-key")) }))
            .with_state(base.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        DiscoveredOidcProvider::discover(
            http_client(),
            local_config(&format!("{base}/tenant%2Fblue")),
        )
        .await
        .unwrap();
        server.abort();

        let fallback_hits = Arc::new(AtomicUsize::new(0));
        let hits = Arc::clone(&fallback_hits);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}");
        let router = Router::new()
            .route(
                "/.well-known/openid-configuration/tenant",
                get(move || {
                    let hits = Arc::clone(&hits);
                    let base = base.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(json!({
                            "issuer": format!("{base}/tenant"),
                            "authorization_endpoint": format!("{base}/authorize"),
                            "token_endpoint": format!("{base}/token"),
                            "jwks_uri": format!("{base}/jwks"),
                            "response_types_supported": ["code"],
                            "code_challenge_methods_supported": ["S256"],
                            "token_endpoint_auth_methods_supported": ["none"],
                            "id_token_signing_alg_values_supported": ["RS256"]
                        }))
                    }
                }),
            )
            .route("/jwks", get(|| async { Json(test_jwks("test-key")) }));
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        DiscoveredOidcProvider::discover(
            http_client(),
            local_config(&format!("http://{address}/tenant")),
        )
        .await
        .unwrap();
        assert_eq!(fallback_hits.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn malformed_or_incomplete_primary_discovery_never_falls_back() {
        for document in [
            json!({}),
            json!({
                "issuer": "wrong",
                "authorization_endpoint": "http://127.0.0.1/authorize",
                "token_endpoint": "http://127.0.0.1/token",
                "jwks_uri": "http://127.0.0.1/jwks",
                "response_types_supported": ["code"],
                "token_endpoint_auth_methods_supported": ["none"],
                "id_token_signing_alg_values_supported": ["RS256"]
            }),
        ] {
            let fallback_hits = Arc::new(AtomicUsize::new(0));
            let hits = Arc::clone(&fallback_hits);
            let router = Router::new()
                .route(
                    "/tenant/.well-known/openid-configuration",
                    get(move || {
                        let document = document.clone();
                        async move { Json(document) }
                    }),
                )
                .route(
                    "/.well-known/openid-configuration/tenant",
                    get(move || {
                        hits.fetch_add(1, Ordering::SeqCst);
                        async { StatusCode::INTERNAL_SERVER_ERROR }
                    }),
                );
            let (base, server) = spawn_server(router).await;
            assert!(
                DiscoveredOidcProvider::discover(
                    http_client(),
                    local_config(&format!("{base}/tenant"))
                )
                .await
                .is_err()
            );
            assert_eq!(fallback_hits.load(Ordering::SeqCst), 0);
            server.abort();
        }
    }

    #[tokio::test]
    async fn issuer_mismatch_and_non_404_status_never_trigger_discovery_fallback() {
        for mismatch in [true, false] {
            let fallback_hits = Arc::new(AtomicUsize::new(0));
            let hits = Arc::clone(&fallback_hits);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let base = format!("http://{address}");
            let primary_base = base.clone();
            let router = Router::new()
                .route(
                    "/tenant/.well-known/openid-configuration",
                    get(move || {
                        let base = primary_base.clone();
                        async move {
                            if mismatch {
                                let mut metadata = valid_metadata(&base, format!("{base}/jwks"));
                                metadata["issuer"] = json!(format!("{base}/other"));
                                Json(metadata).into_response()
                            } else {
                                StatusCode::INTERNAL_SERVER_ERROR.into_response()
                            }
                        }
                    }),
                )
                .route(
                    "/.well-known/openid-configuration/tenant",
                    get(move || {
                        hits.fetch_add(1, Ordering::SeqCst);
                        async { StatusCode::OK }
                    }),
                );
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            assert!(
                DiscoveredOidcProvider::discover(
                    http_client(),
                    local_config(&format!("{base}/tenant")),
                )
                .await
                .is_err()
            );
            assert_eq!(fallback_hits.load(Ordering::SeqCst), 0);
            server.abort();
        }
    }

    #[tokio::test]
    async fn discovery_timeout_never_triggers_fallback() {
        let fallback_hits = Arc::new(AtomicUsize::new(0));
        let hits = Arc::clone(&fallback_hits);
        let router = Router::new()
            .route(
                "/tenant/.well-known/openid-configuration",
                get(|| async {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    StatusCode::OK
                }),
            )
            .route(
                "/.well-known/openid-configuration/tenant",
                get(move || {
                    hits.fetch_add(1, Ordering::SeqCst);
                    async { StatusCode::OK }
                }),
            );
        let (base, server) = spawn_server(router).await;
        let client =
            AccessAuthHttpClient::build(Client::builder().timeout(Duration::from_millis(25)))
                .unwrap();
        assert!(
            DiscoveredOidcProvider::discover(client, local_config(&format!("{base}/tenant")),)
                .await
                .is_err()
        );
        assert_eq!(fallback_hits.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[tokio::test]
    async fn missing_empty_pkce_and_symmetric_access_metadata_fail_discovery() {
        for pkce in [None, Some(Vec::<String>::new())] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let base = format!("http://{address}");
            let mut document = valid_metadata(&base, format!("{base}/jwks"));
            if let Some(pkce) = pkce {
                document["code_challenge_methods_supported"] = json!(pkce);
            } else {
                document
                    .as_object_mut()
                    .unwrap()
                    .remove("code_challenge_methods_supported");
            }
            let router = Router::new().route(
                "/.well-known/openid-configuration",
                get(move || {
                    let document = document.clone();
                    async move { Json(document) }
                }),
            );
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            assert!(
                DiscoveredOidcProvider::discover(http_client(), local_config(&base))
                    .await
                    .is_err()
            );
            server.abort();
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}");
        let mut document = valid_metadata(&base, format!("{base}/jwks"));
        document["access_token_signing_alg_values_supported"] = json!(["HS256"]);
        let router = Router::new().route(
            "/.well-known/openid-configuration",
            get(move || {
                let document = document.clone();
                async move { Json(document) }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        assert!(
            DiscoveredOidcProvider::discover(http_client(), local_config(&base))
                .await
                .is_err()
        );
        server.abort();
    }

    #[tokio::test]
    async fn discovery_and_jwks_redirect_targets_are_never_contacted() {
        let target_hits = Arc::new(AtomicUsize::new(0));
        let hits = Arc::clone(&target_hits);
        let target = Router::new().fallback(move || {
            hits.fetch_add(1, Ordering::SeqCst);
            async { StatusCode::OK }
        });
        let (target_base, target_server) = spawn_server(target).await;
        let location = format!("{target_base}/stolen");
        let source = Router::new().route(
            "/.well-known/openid-configuration",
            get(move || {
                let location = location.clone();
                async move {
                    (
                        StatusCode::TEMPORARY_REDIRECT,
                        [(header::LOCATION, location)],
                    )
                }
            }),
        );
        let (base, source_server) = spawn_server(source).await;
        assert!(
            DiscoveredOidcProvider::discover(http_client(), local_config(&base))
                .await
                .is_err()
        );
        assert_eq!(target_hits.load(Ordering::SeqCst), 0);
        source_server.abort();

        let location = format!("{target_base}/jwks");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}");
        let metadata_base = base.clone();
        let source = Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(move || {
                    let base = metadata_base.clone();
                    async move { Json(valid_metadata(&base, format!("{base}/jwks"))) }
                }),
            )
            .route(
                "/jwks",
                get(move || {
                    let location = location.clone();
                    async move { (StatusCode::FOUND, [(header::LOCATION, location)]) }
                }),
            );
        let source_server =
            tokio::spawn(async move { axum::serve(listener, source).await.unwrap() });
        assert!(
            DiscoveredOidcProvider::discover(http_client(), local_config(&base))
                .await
                .is_err()
        );
        assert_eq!(target_hits.load(Ordering::SeqCst), 0);
        source_server.abort();
        target_server.abort();
    }

    fn valid_metadata(base: &str, jwks_uri: String) -> Value {
        json!({
            "issuer": base,
            "authorization_endpoint": format!("{base}/authorize"),
            "token_endpoint": format!("{base}/token"),
            "jwks_uri": jwks_uri,
            "response_types_supported": ["code"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none"],
            "id_token_signing_alg_values_supported": ["RS256"]
        })
    }

    #[tokio::test]
    async fn token_redirect_does_not_replay_code_verifier_or_client_secret() {
        let target_hits = Arc::new(AtomicUsize::new(0));
        let hits = Arc::clone(&target_hits);
        let target = Router::new().fallback(move || {
            hits.fetch_add(1, Ordering::SeqCst);
            async { StatusCode::OK }
        });
        let (target_base, target_server) = spawn_server(target).await;
        let location = format!("{target_base}/stolen");
        let source = Router::new().route(
            "/token",
            post(move || {
                let location = location.clone();
                async move {
                    (
                        StatusCode::TEMPORARY_REDIRECT,
                        [(header::LOCATION, location)],
                    )
                }
            }),
        );
        let (base, source_server) = spawn_server(source).await;
        let config = OidcConfig::new(
            "http://127.0.0.1:8080",
            "secret-client",
            Some("client-secret".to_owned()),
            "http://127.0.0.1:14333/oidc/callback",
            "api",
            vec!["openid".to_owned()],
            Duration::from_secs(60),
            true,
            true,
        )
        .unwrap();
        let provider = DiscoveredOidcProvider {
            config,
            client: http_client(),
            authorization_endpoint: Url::parse("http://127.0.0.1:8080/authorize").unwrap(),
            token_endpoint: Url::parse(&format!("{base}/token")).unwrap(),
            token_endpoint_auth_method: TokenEndpointAuthMethod::ClientSecretBasic,
            id_token_signing_algorithms: BTreeSet::from(["RS256".to_owned()]),
            access_token_signing_algorithms: BTreeSet::from(["RS256".to_owned()]),
            jwks: JwksCache::fixture(test_jwks("test-key")).unwrap(),
        };
        assert!(
            provider
                .exchange_code(&AuthorizationCode {
                    code: "secret-code",
                    pkce_verifier: "secret-verifier",
                })
                .await
                .is_err()
        );
        assert_eq!(target_hits.load(Ordering::SeqCst), 0);
        source_server.abort();
        target_server.abort();
    }

    #[tokio::test]
    async fn token_stream_accepts_normal_body_and_rejects_oversized_body() {
        let normal = serde_json::to_vec(&json!({
            "access_token": "access",
            "id_token": "id",
            "token_type": "Bearer",
            "expires_in": 300
        }))
        .unwrap();
        let router = Router::new()
            .route(
                "/normal",
                post(move || {
                    let normal = normal.clone();
                    async move { Response::new(streamed_body(vec![normal])) }
                }),
            )
            .route(
                "/large",
                post(|| async {
                    Response::new(streamed_body(vec![
                        vec![b' '; TOKEN_RESPONSE_LIMIT],
                        vec![b'x'],
                    ]))
                }),
            );
        let (base, server) = spawn_server(router).await;
        let input = AuthorizationCode {
            code: "code",
            pkce_verifier: "verifier",
        };
        let token = token_provider(Url::parse(&format!("{base}/normal")).unwrap())
            .exchange_code(&input)
            .await
            .unwrap();
        assert_eq!(token.access_token(), "access");

        let error = token_provider(Url::parse(&format!("{base}/large")).unwrap())
            .exchange_code(&input)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            OidcError::Provider("token endpoint response exceeds the size limit")
        );
        server.abort();
    }

    #[test]
    fn endpoint_auth_selection_is_deterministic() {
        let methods = vec![
            "client_secret_post".to_owned(),
            "client_secret_basic".to_owned(),
        ];
        assert_eq!(
            select_token_auth(true, Some(&methods)).unwrap(),
            TokenEndpointAuthMethod::ClientSecretBasic
        );
        assert!(select_token_auth(false, None).is_err());
        assert_eq!(
            select_token_auth(false, Some(&["none".to_owned()])).unwrap(),
            TokenEndpointAuthMethod::None
        );
        assert!(select_token_auth(true, Some(&["none".to_owned()])).is_err());
        assert!(select_token_auth(false, Some(&["client_secret_basic".to_owned()])).is_err());
    }

    #[test]
    fn access_token_hash_uses_left_half_of_signing_hash() {
        assert_eq!(
            access_token_hash("access-token", Algorithm::RS256).unwrap(),
            "Pxa-1wifRlPl7yG_0oJNfw"
        );
    }

    #[tokio::test]
    async fn validates_id_token_nonce_audience_azp_and_at_hash() {
        let access_token = "access-token";
        let mut claims = id_claims();
        claims["at_hash"] = json!(access_token_hash(access_token, Algorithm::RS256).unwrap());
        let principal = verifier()
            .validate_id_token(
                &sign(claims, "JWT", "test-key"),
                "expected-nonce",
                Some(access_token),
            )
            .await
            .unwrap();
        assert_eq!(principal.subject(), "subject-1");

        assert!(
            verifier()
                .validate_id_token(&sign(id_claims(), "JWT", "test-key"), "wrong-nonce", None)
                .await
                .is_err()
        );
        let mut wrong_audience = id_claims();
        wrong_audience["aud"] = json!(["another", "other"]);
        assert!(
            verifier()
                .validate_id_token(
                    &sign(wrong_audience, "JWT", "test-key"),
                    "expected-nonce",
                    None
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn id_token_requires_fresh_iat() {
        let now = jsonwebtoken::get_current_timestamp();
        let mut stale = id_claims();
        stale["iat"] = json!(now - MAX_ID_TOKEN_AGE_SECONDS - ID_TOKEN_CLOCK_SKEW_SECONDS - 120);
        assert!(
            verifier()
                .validate_id_token(&sign(stale, "JWT", "test-key"), "expected-nonce", None)
                .await
                .is_err()
        );

        let mut future = id_claims();
        future["iat"] = json!(now + ID_TOKEN_CLOCK_SKEW_SECONDS + 120);
        assert!(
            verifier()
                .validate_id_token(&sign(future, "JWT", "test-key"), "expected-nonce", None)
                .await
                .is_err()
        );

        let token = {
            let mut header = Header::new(Algorithm::RS256);
            header.kid = Some("test-key".to_owned());
            encode(
                &header,
                &json!({
                    "iss": "https://issuer.example",
                    "sub": "subject-1",
                    "aud": "waltr-client",
                    "exp": now + 300,
                    "nonce": "expected-nonce"
                }),
                &EncodingKey::from_rsa_pem(TEST_PRIVATE_KEY.as_bytes()).unwrap(),
            )
            .unwrap()
        };
        assert!(
            verifier()
                .validate_id_token(&token, "expected-nonce", None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn id_token_typ_accepts_absent_or_jwt_and_rejects_token_confusion() {
        for typ in [None, Some("JWT"), Some("jwt"), Some("Jwt")] {
            let token = sign_with_typ(id_claims(), typ, "test-key");
            verifier()
                .validate_id_token(&token, "expected-nonce", None)
                .await
                .unwrap();
        }
        for typ in [Some("at+jwt"), Some("application/jwt"), Some("unrelated")] {
            let token = sign_with_typ(id_claims(), typ, "test-key");
            assert!(
                verifier()
                    .validate_id_token(&token, "expected-nonce", None)
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn validates_access_token_issuer_audience_expiry_signature_and_scopes() {
        let required = vec!["mcp:use".to_owned()];
        let valid = sign(access_claims(), "at+jwt", "test-key");
        let principal = verifier()
            .validate_access_token(&valid, &required)
            .await
            .unwrap();
        assert!(principal.has_scope("mcp:use"));

        let mut wrong_issuer = access_claims();
        wrong_issuer["iss"] = json!("https://attacker.example");
        assert!(
            verifier()
                .validate_access_token(&sign(wrong_issuer, "at+jwt", "test-key"), &required)
                .await
                .is_err()
        );

        let mut wrong_audience = access_claims();
        wrong_audience["aud"] = json!("another-api");
        assert!(
            verifier()
                .validate_access_token(&sign(wrong_audience, "at+jwt", "test-key"), &required)
                .await
                .is_err()
        );

        let mut expired = access_claims();
        expired["exp"] = json!(jsonwebtoken::get_current_timestamp() - 120);
        assert!(
            verifier()
                .validate_access_token(&sign(expired, "at+jwt", "test-key"), &required)
                .await
                .is_err()
        );

        let mut at_expiry = access_claims();
        at_expiry["exp"] = json!(jsonwebtoken::get_current_timestamp());
        assert!(
            verifier()
                .validate_access_token(&sign(at_expiry, "at+jwt", "test-key"), &required)
                .await
                .is_err(),
            "post-decode expiry check must reject tokens within JWT validation leeway"
        );

        let mut not_yet_valid = access_claims();
        not_yet_valid["nbf"] = json!(jsonwebtoken::get_current_timestamp() + 120);
        assert!(
            verifier()
                .validate_access_token(&sign(not_yet_valid, "at+jwt", "test-key"), &required)
                .await
                .is_err()
        );
        assert!(
            verifier()
                .validate_access_token(&sign(access_claims(), "JWT", "test-key"), &required)
                .await
                .is_err()
        );

        let mut invalid_signature = valid;
        invalid_signature.replace_range(
            invalid_signature.rfind('.').unwrap() + 1..,
            "invalid-signature",
        );
        assert!(
            verifier()
                .validate_access_token(&invalid_signature, &required)
                .await
                .is_err()
        );

        assert!(matches!(
            verifier()
                .validate_access_token(&sign(access_claims(), "at+jwt", "unknown"), &required)
                .await,
            Err(OidcError::UnknownKeyId)
        ));
        assert!(matches!(
            verifier()
                .validate_access_token(
                    &sign(access_claims(), "at+jwt", "test-key"),
                    &["admin".to_owned()]
                )
                .await,
            Err(OidcError::InsufficientScope)
        ));
        assert!(
            verifier()
                .validate_access_token("opaque", &[])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn access_tokens_are_rejected_across_issuer_and_audience_domains() {
        let keys = JwksCache::fixture(test_jwks("test-key")).unwrap();
        let api = OidcVerifier::fixture(
            "https://api-issuer.example",
            "browser",
            "api-audience",
            keys.clone(),
        );
        let mcp = OidcVerifier::fixture(
            "https://mcp-issuer.example",
            "browser",
            "mcp-audience",
            keys,
        );
        let api_token = sign(
            json!({
                "iss": "https://api-issuer.example",
                "sub": "subject",
                "aud": "api-audience",
                "scope": "api:use"
            }),
            "at+jwt",
            "test-key",
        );
        let mcp_token = sign(
            json!({
                "iss": "https://mcp-issuer.example",
                "sub": "subject",
                "aud": "mcp-audience",
                "scope": "mcp:use"
            }),
            "at+jwt",
            "test-key",
        );

        api.validate_access_token(&api_token, &["api:use".to_owned()])
            .await
            .unwrap();
        mcp.validate_access_token(&mcp_token, &["mcp:use".to_owned()])
            .await
            .unwrap();
        assert!(api.validate_access_token(&mcp_token, &[]).await.is_err());
        assert!(mcp.validate_access_token(&api_token, &[]).await.is_err());

        let api_audience_with_only_mcp_scope = sign(
            json!({
                "iss": "https://api-issuer.example",
                "sub": "subject",
                "aud": "api-audience",
                "scope": "mcp:use"
            }),
            "at+jwt",
            "test-key",
        );
        assert!(matches!(
            api.validate_access_token(&api_audience_with_only_mcp_scope, &["api:use".to_owned()])
                .await,
            Err(OidcError::InsufficientScope)
        ));

        let same_issuer_wrong_audience = api.for_access_audience("mcp-audience");
        assert!(
            same_issuer_wrong_audience
                .validate_access_token(&api_token, &[])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn id_and_access_token_algorithm_policies_are_independent() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}");
        let mut document = valid_metadata(&base, format!("{base}/jwks"));
        document["access_token_signing_alg_values_supported"] = json!(["RS384"]);
        let router = Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(move || {
                    let document = document.clone();
                    async move { Json(document) }
                }),
            )
            .route(
                "/jwks",
                get(|| async { Json(unrestricted_test_jwks("test-key")) }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let verifier = DiscoveredOidcProvider::discover(http_client(), local_config(&base))
            .await
            .unwrap()
            .verifier();
        let mut id_claims = id_claims();
        id_claims["iss"] = json!(base);
        verifier
            .validate_id_token(
                &sign_with_algorithm(id_claims, Some("JWT"), "test-key", Algorithm::RS256),
                "expected-nonce",
                None,
            )
            .await
            .unwrap();
        let mut access_claims = access_claims();
        access_claims["iss"] = json!(base);
        verifier
            .validate_access_token(
                &sign_with_algorithm(
                    access_claims.clone(),
                    Some("at+jwt"),
                    "test-key",
                    Algorithm::RS384,
                ),
                &[],
            )
            .await
            .unwrap();
        assert!(
            verifier
                .validate_access_token(
                    &sign_with_algorithm(
                        access_claims,
                        Some("at+jwt"),
                        "test-key",
                        Algorithm::RS256,
                    ),
                    &[],
                )
                .await
                .is_err()
        );
        server.abort();
    }

    #[tokio::test]
    async fn missing_none_symmetric_and_unsupported_algorithms_are_rejected() {
        let claims = access_claims();
        let symmetric = {
            let mut header = Header::new(Algorithm::HS256);
            header.kid = Some("test-key".to_owned());
            header.typ = Some("at+jwt".to_owned());
            encode(&header, &claims, &EncodingKey::from_secret(b"secret")).unwrap()
        };
        assert!(
            verifier()
                .validate_access_token(&symmetric, &[])
                .await
                .is_err()
        );

        for header in [
            json!({"typ":"at+jwt","kid":"test-key"}),
            json!({"alg":"none","typ":"at+jwt","kid":"test-key"}),
            json!({"alg":"RSA-OAEP","typ":"at+jwt","kid":"test-key"}),
        ] {
            let token = format!(
                "{}.{}.signature",
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap()),
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
            );
            assert!(verifier().validate_access_token(&token, &[]).await.is_err());
        }
    }

    #[tokio::test]
    async fn signed_id_and_access_tokens_reject_unsupported_critical_headers_before_key_lookup() {
        let id = sign_with_protected_header(
            id_claims(),
            r#"{"alg":"RS256","typ":"JWT","kid":"unknown","crit":["exp"]}"#,
        );
        assert_eq!(
            verifier()
                .validate_id_token(&id, "expected-nonce", None)
                .await,
            Err(OidcError::InvalidToken(
                "JWT critical headers are not supported"
            ))
        );

        let access = sign_with_protected_header(
            access_claims(),
            r#"{"alg":"RS256","typ":"at+jwt","kid":"unknown","crit":["exp"]}"#,
        );
        assert_eq!(
            verifier().validate_access_token(&access, &[]).await,
            Err(OidcError::InvalidToken(
                "JWT critical headers are not supported"
            ))
        );
    }

    #[tokio::test]
    async fn malformed_duplicate_and_invalid_critical_headers_fail_closed() {
        let malformed_id = sign_with_protected_header(
            id_claims(),
            r#"{"alg":"RS256","typ":"JWT","kid":"test-key","crit":null}"#,
        );
        assert_eq!(
            verifier()
                .validate_id_token(&malformed_id, "expected-nonce", None)
                .await,
            Err(OidcError::InvalidToken("JWT critical headers are invalid"))
        );

        for protected in [
            r#"{"alg":"RS256","typ":"at+jwt","kid":"test-key","crit":null}"#,
            r#"{"alg":"RS256","typ":"at+jwt","kid":"test-key","crit":"exp"}"#,
            r#"{"alg":"RS256","typ":"at+jwt","kid":"test-key","crit":[1]}"#,
            r#"{"alg":"RS256","typ":"at+jwt","kid":"test-key","crit":[],"crit":[]}"#,
            r#"{"alg":"RS256","typ":"at+jwt","kid":"test-key","crit":[""]}"#,
            r#"{"alg":"RS256","typ":"at+jwt","kid":"test-key","crit":["exp","exp"]}"#,
        ] {
            let token = sign_with_protected_header(access_claims(), protected);
            assert!(
                matches!(
                    verifier().validate_access_token(&token, &[]).await,
                    Err(OidcError::InvalidToken(message))
                        if message.starts_with("JWT critical headers")
                ),
                "{protected}"
            );
        }
    }
}
