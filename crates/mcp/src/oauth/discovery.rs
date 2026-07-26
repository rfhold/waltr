use std::{
    collections::{BTreeSet, HashSet},
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use futures_util::StreamExt;
use reqwest::{Url, header::CONTENT_TYPE, redirect::Policy};
use serde::{
    Deserializer as _,
    de::{DeserializeOwned, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Number, Value};

use super::{McpAuthorizationServerMetadata, McpBearerChallenge, McpProtectedResourceMetadata};
use crate::{Error, Result};

const DEFAULT_MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESOLVED_ADDRESSES: usize = 16;

/// URL scheme policy for OAuth endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpOAuthEndpointPolicy {
    /// Require HTTPS for every endpoint.
    HttpsOnly,
    /// Permit HTTP only for literal loopback addresses and `localhost`.
    AllowLoopbackHttp,
}

/// Caller-supplied validation for OAuth discovery destinations and resolved addresses.
pub trait McpOAuthDestinationPolicy: Send + Sync {
    /// Validate a fully parsed destination before DNS resolution.
    ///
    /// # Errors
    ///
    /// Returns an error when the destination is not permitted.
    fn validate(&self, destination: &Url) -> Result<()>;

    /// Validate an address returned by DNS and later observed as the response peer.
    ///
    /// The default rejects non-public addresses. A test that needs loopback networking must
    /// explicitly override this method rather than weakening process-global policy.
    ///
    /// # Errors
    ///
    /// Returns an error when the address is not a public unicast destination.
    fn validate_address(&self, address: IpAddr) -> Result<()> {
        validate_public_address(address)
    }
}

#[derive(Debug)]
struct PublicDestination;

impl McpOAuthDestinationPolicy for PublicDestination {
    fn validate(&self, _destination: &Url) -> Result<()> {
        Ok(())
    }
}

/// Redirect-free, DNS-pinned, timeout-bounded, response-bounded OAuth discovery client.
#[derive(Clone)]
pub struct McpOAuthDiscoveryClient {
    endpoint_policy: McpOAuthEndpointPolicy,
    destination_policy: Arc<dyn McpOAuthDestinationPolicy>,
    max_response_bytes: usize,
}

impl fmt::Debug for McpOAuthDiscoveryClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpOAuthDiscoveryClient")
            .field("endpoint_policy", &self.endpoint_policy)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish_non_exhaustive()
    }
}

impl McpOAuthDiscoveryClient {
    /// Build a hardened OAuth discovery client with conservative default bounds.
    ///
    /// # Errors
    ///
    /// Returns an error if a redirect-free, proxy-free TLS client cannot be built.
    pub fn new(endpoint_policy: McpOAuthEndpointPolicy) -> Result<Self> {
        build_http_client(None)?;
        Ok(Self {
            endpoint_policy,
            destination_policy: Arc::new(PublicDestination),
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        })
    }

    /// Override the maximum accepted response body size.
    #[must_use]
    pub const fn with_max_response_bytes(mut self, max_response_bytes: usize) -> Self {
        self.max_response_bytes = max_response_bytes;
        self
    }

    /// Set explicit caller-owned destination and address policy.
    #[must_use]
    pub fn with_destination_policy(
        mut self,
        destination_policy: Arc<dyn McpOAuthDestinationPolicy>,
    ) -> Self {
        self.destination_policy = destination_policy;
        self
    }

    /// Fetch and validate protected-resource metadata.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error when every discovery location fails validation.
    pub async fn discover_protected_resource_metadata(
        &self,
        endpoint: &str,
        challenge: Option<&McpBearerChallenge>,
    ) -> Result<McpProtectedResourceMetadata> {
        for destination in protected_resource_discovery_urls(endpoint, challenge) {
            let Ok(metadata) = self
                .get_json::<McpProtectedResourceMetadata>(&destination)
                .await
            else {
                continue;
            };
            if metadata.validate_for_mcp_resource(endpoint).is_ok() {
                return Ok(metadata);
            }
        }
        Err(Error::protocol(
            "MCP protected resource metadata discovery failed",
        ))
    }

    /// Fetch and validate authorization-server metadata.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error when RFC 8414 and OIDC fallback discovery fail.
    pub async fn discover_authorization_server_metadata(
        &self,
        issuer: &str,
    ) -> Result<McpAuthorizationServerMetadata> {
        for destination in authorization_server_metadata_urls(issuer) {
            let Ok(metadata) = self
                .get_json::<McpAuthorizationServerMetadata>(&destination)
                .await
            else {
                continue;
            };
            if metadata
                .validate_for_public_authorization_code_client(issuer, self.endpoint_policy)
                .is_ok()
            {
                return Ok(metadata);
            }
        }
        Err(Error::protocol(
            "MCP authorization server metadata discovery failed",
        ))
    }

    /// Fetch bounded JSON after validating, resolving, and pinning the destination.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error for unsafe destinations, DNS, HTTP, peer, content, or JSON.
    pub async fn get_json<T: DeserializeOwned>(&self, destination: &str) -> Result<T> {
        let url = validate_endpoint("HTTP destination", destination, self.endpoint_policy)?;
        self.destination_policy.validate(&url)?;
        let addresses = resolve_destination(&url, self.destination_policy.as_ref()).await?;
        let client = build_http_client(Some((&url, &addresses)))?;
        let response = client
            .get(url.clone())
            .send()
            .await
            .map_err(|_| Error::protocol("OAuth HTTP request failed"))?;
        parse_json_response(
            response,
            &url,
            &addresses,
            self.destination_policy.as_ref(),
            self.max_response_bytes,
        )
        .await
    }
}

/// Build RFC 9728 well-known protected resource metadata URLs for an MCP endpoint.
#[must_use]
pub fn protected_resource_metadata_urls(endpoint: &str) -> Vec<String> {
    let Ok(url) = parse_discovery_base(endpoint, true) else {
        return Vec::new();
    };
    let mut urls = Vec::new();
    if let Some(path_url) = protected_resource_metadata_url_for_path(&url) {
        urls.push(path_url);
    }
    let root_url = with_path_and_query(&url, "/.well-known/oauth-protected-resource", false);
    if !urls.contains(&root_url) {
        urls.push(root_url);
    }
    urls
}

/// Build RFC 8414 and `OpenID Connect` discovery URLs for an issuer.
#[must_use]
pub fn authorization_server_metadata_urls(issuer: &str) -> Vec<String> {
    let Ok(url) = parse_discovery_base(issuer, false) else {
        return Vec::new();
    };
    let path = url.path().trim_matches('/');
    if path.is_empty() {
        return vec![
            with_path(&url, "/.well-known/oauth-authorization-server"),
            with_path(&url, "/.well-known/openid-configuration"),
        ];
    }
    vec![
        with_path(
            &url,
            &format!("/.well-known/oauth-authorization-server/{path}"),
        ),
        with_path(&url, &format!("/.well-known/openid-configuration/{path}")),
        with_path(&url, &format!("/{path}/.well-known/openid-configuration")),
    ]
}

/// Fetch protected-resource metadata with hardened default HTTPS/public-address policy.
///
/// The supplied client is retained for source compatibility but is intentionally not used because
/// its redirect, proxy, DNS, and timeout behavior cannot be verified.
///
/// # Errors
///
/// Returns an error when discovery fails or no valid metadata is found.
pub async fn discover_protected_resource_metadata(
    _client: &reqwest::Client,
    endpoint: &str,
    challenge: Option<&McpBearerChallenge>,
) -> Result<McpProtectedResourceMetadata> {
    McpOAuthDiscoveryClient::new(McpOAuthEndpointPolicy::HttpsOnly)?
        .discover_protected_resource_metadata(endpoint, challenge)
        .await
}

/// Fetch authorization-server metadata with hardened default HTTPS/public-address policy.
///
/// The supplied client is retained for source compatibility but is intentionally not used because
/// its redirect, proxy, DNS, and timeout behavior cannot be verified.
///
/// # Errors
///
/// Returns an error when discovery fails or no matching issuer is found.
pub async fn discover_authorization_server_metadata(
    _client: &reqwest::Client,
    issuer: &str,
) -> Result<McpAuthorizationServerMetadata> {
    McpOAuthDiscoveryClient::new(McpOAuthEndpointPolicy::HttpsOnly)?
        .discover_authorization_server_metadata(issuer)
        .await
}

pub(super) fn validate_endpoint(
    name: &str,
    value: &str,
    policy: McpOAuthEndpointPolicy,
) -> Result<Url> {
    if value.trim() != value || value.contains('\\') {
        return Err(Error::protocol(format!("invalid OAuth {name}")));
    }
    let url = Url::parse(value).map_err(|_| Error::protocol(format!("invalid OAuth {name}")))?;
    if url.cannot_be_a_base() || url.fragment().is_some() {
        return Err(Error::protocol(format!(
            "OAuth {name} must be an absolute URL without a fragment"
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::protocol(format!(
            "OAuth {name} must not contain credentials"
        )));
    }
    let host = url
        .host_str()
        .ok_or_else(|| Error::protocol(format!("OAuth {name} requires a host")))?;
    let valid_scheme = url.scheme() == "https"
        || policy == McpOAuthEndpointPolicy::AllowLoopbackHttp
            && url.scheme() == "http"
            && is_loopback_host(host);
    if !valid_scheme {
        return Err(Error::protocol(format!(
            "OAuth {name} must use HTTPS or explicitly allowed loopback HTTP"
        )));
    }
    Ok(url)
}

fn build_http_client(resolution: Option<(&Url, &[SocketAddr])>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .redirect(Policy::none())
        .no_proxy()
        .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
        .timeout(DEFAULT_RESPONSE_TIMEOUT);
    if let Some((url, addresses)) = resolution {
        let host = url
            .host_str()
            .ok_or_else(|| Error::protocol("OAuth HTTP destination requires a host"))?;
        builder = builder.resolve_to_addrs(host, addresses);
    }
    builder
        .build()
        .map_err(|_| Error::protocol("failed to build hardened OAuth HTTP client"))
}

async fn resolve_destination(
    destination: &Url,
    policy: &dyn McpOAuthDestinationPolicy,
) -> Result<Vec<SocketAddr>> {
    let host = destination
        .host_str()
        .ok_or_else(|| Error::protocol("OAuth HTTP destination requires a host"))?;
    let port = destination
        .port_or_known_default()
        .ok_or_else(|| Error::protocol("OAuth HTTP destination requires a known port"))?;
    let resolved = tokio::time::timeout(
        DEFAULT_CONNECT_TIMEOUT,
        tokio::net::lookup_host((host, port)),
    )
    .await
    .map_err(|_| Error::protocol("OAuth HTTP DNS resolution timed out"))?
    .map_err(|_| Error::protocol("OAuth HTTP DNS resolution failed"))?;
    let mut addresses = BTreeSet::new();
    for address in resolved {
        policy.validate_address(address.ip())?;
        addresses.insert(address);
        if addresses.len() > MAX_RESOLVED_ADDRESSES {
            return Err(Error::protocol(
                "OAuth HTTP DNS response contains too many addresses",
            ));
        }
    }
    if addresses.is_empty() {
        return Err(Error::protocol("OAuth HTTP DNS response was empty"));
    }
    Ok(addresses.into_iter().collect())
}

async fn parse_json_response<T: DeserializeOwned>(
    response: reqwest::Response,
    destination: &Url,
    addresses: &[SocketAddr],
    policy: &dyn McpOAuthDestinationPolicy,
    max_response_bytes: usize,
) -> Result<T> {
    if response.url() != destination {
        return Err(Error::protocol("OAuth HTTP redirect refused"));
    }
    let peer = response
        .remote_addr()
        .ok_or_else(|| Error::protocol("OAuth HTTP response peer is unavailable"))?;
    policy.validate_address(peer.ip())?;
    if !addresses.iter().any(|address| address.ip() == peer.ip()) {
        return Err(Error::protocol(
            "OAuth HTTP response peer was not DNS-pinned",
        ));
    }
    if !response.status().is_success() {
        return Err(Error::protocol("OAuth HTTP response was unsuccessful"));
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .unwrap_or_default();
    let json_content_type = content_type.eq_ignore_ascii_case("application/json")
        || content_type.len() > "application/+json".len()
            && content_type[.."application/".len()].eq_ignore_ascii_case("application/")
            && content_type[content_type.len() - "+json".len()..].eq_ignore_ascii_case("+json");
    if !json_content_type {
        return Err(Error::protocol(
            "OAuth HTTP response content type is not JSON",
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > max_response_bytes as u64)
    {
        return Err(Error::protocol("OAuth HTTP response body exceeds limit"));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| Error::protocol("OAuth HTTP response body failed"))?;
        if body.len().saturating_add(chunk.len()) > max_response_bytes {
            return Err(Error::protocol("OAuth HTTP response body exceeds limit"));
        }
        body.extend_from_slice(&chunk);
    }
    let mut deserializer = serde_json::Deserializer::from_slice(&body);
    let value = deserializer
        .deserialize_any(UniqueValueVisitor)
        .map_err(|_| Error::protocol("OAuth HTTP response JSON is invalid"))?;
    deserializer
        .end()
        .map_err(|_| Error::protocol("OAuth HTTP response JSON is invalid"))?;
    T::deserialize(value).map_err(|_| Error::protocol("OAuth HTTP response JSON is invalid"))
}

struct UniqueValueVisitor;

impl<'de> Visitor<'de> for UniqueValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> std::result::Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> std::result::Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> std::result::Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E: serde::de::Error>(self, value: f64) -> std::result::Result<Value, E> {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> std::result::Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> std::result::Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_none<E>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> std::result::Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(UniqueValueSeed)? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut object: A) -> std::result::Result<Value, A::Error> {
        let mut values = Map::new();
        let mut keys = HashSet::new();
        while let Some(key) = object.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(serde::de::Error::custom("duplicate JSON object key"));
            }
            values.insert(key, object.next_value_seed(UniqueValueSeed)?);
        }
        Ok(Value::Object(values))
    }
}

struct UniqueValueSeed;

impl<'de> serde::de::DeserializeSeed<'de> for UniqueValueSeed {
    type Value = Value;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<Value, D::Error> {
        deserializer.deserialize_any(UniqueValueVisitor)
    }
}

fn validate_public_address(address: IpAddr) -> Result<()> {
    let public = match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    };
    if public {
        Ok(())
    } else {
        Err(Error::protocol(
            "OAuth HTTP destination resolved to a non-public address",
        ))
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, ..] = address.octets();
    !(a == 0
        || a == 10
        || a == 127
        || a >= 224
        || a == 100 && (64..=127).contains(&b)
        || a == 169 && b == 254
        || a == 172 && (16..=31).contains(&b)
        || a == 192 && b == 0
        || a == 192 && b == 168
        || a == 198 && ((18..=19).contains(&b) || b == 51 && c == 100)
        || a == 203 && b == 0 && c == 113)
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(address) = address.to_ipv4() {
        return is_public_ipv4(address);
    }
    let segments = address.segments();
    !address.is_unspecified()
        && !address.is_loopback()
        && !address.is_multicast()
        && segments[0] & 0xfe00 != 0xfc00
        && segments[0] & 0xffc0 != 0xfe80
        && segments[0] & 0xffc0 != 0xfec0
        && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
}

fn protected_resource_discovery_urls(
    endpoint: &str,
    challenge: Option<&McpBearerChallenge>,
) -> Vec<String> {
    challenge
        .and_then(|challenge| {
            challenge
                .resource_metadata
                .as_deref()
                .filter(|value| !value.trim().is_empty())
        })
        .map_or_else(
            || protected_resource_metadata_urls(endpoint),
            |metadata_url| vec![metadata_url.to_owned()],
        )
}

fn parse_discovery_base(value: &str, allow_query: bool) -> Result<Url> {
    if value.trim() != value || value.contains('\\') {
        return Err(Error::protocol("invalid OAuth discovery base URL"));
    }
    let url = Url::parse(value).map_err(|_| Error::protocol("invalid OAuth discovery base URL"))?;
    if url.cannot_be_a_base()
        || !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !allow_query && url.query().is_some()
    {
        return Err(Error::protocol("invalid OAuth discovery base URL"));
    }
    Ok(url)
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
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
