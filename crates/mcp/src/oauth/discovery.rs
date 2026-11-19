use std::{collections::HashSet, fmt, time::Duration};

use futures_util::StreamExt;
use reqwest::{StatusCode, Url, header, redirect::Policy};
use serde::{
    Deserializer as _, Serialize,
    de::{DeserializeOwned, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Number, Value};
use url::Host;

use super::{McpAuthorizationServerMetadata, McpBearerChallenge, McpProtectedResourceMetadata};
use crate::{Error, Result};

const DEFAULT_MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_METADATA_REDIRECTS: usize = 3;

/// URL scheme policy for OAuth endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpOAuthEndpointPolicy {
    /// Require HTTPS for every endpoint.
    HttpsOnly,
    /// Permit HTTP only for literal loopback addresses and `localhost`.
    AllowLoopbackHttp,
}

/// Timeout- and response-bounded OAuth discovery and registration client.
#[derive(Clone)]
pub struct McpOAuthDiscoveryClient {
    endpoint_policy: McpOAuthEndpointPolicy,
    client: reqwest::Client,
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
    /// Returns an error if an HTTP client cannot be built.
    pub fn new(endpoint_policy: McpOAuthEndpointPolicy) -> Result<Self> {
        Ok(Self {
            endpoint_policy,
            client: build_http_client()?,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        })
    }

    /// Override the maximum accepted response body size.
    #[must_use]
    pub const fn with_max_response_bytes(mut self, max_response_bytes: usize) -> Self {
        self.max_response_bytes = max_response_bytes;
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

    /// Fetch bounded metadata JSON, following bounded same-origin redirects.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error for unsafe URLs, HTTP, content, or JSON.
    pub async fn get_json<T: DeserializeOwned>(&self, destination: &str) -> Result<T> {
        let url = validate_endpoint("HTTP destination", destination, self.endpoint_policy)?;
        let response = get_metadata(
            &self.client,
            url,
            self.endpoint_policy,
            DEFAULT_RESPONSE_TIMEOUT,
            None,
        )
        .await?;
        parse_json_response(response, self.max_response_bytes).await
    }

    /// Send JSON to a validated URL without following redirects and parse a bounded response.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error for unsafe URLs, HTTP, content, or JSON.
    pub async fn post_json<T: Serialize + Sync + ?Sized, R: DeserializeOwned>(
        &self,
        destination: &str,
        value: &T,
    ) -> Result<R> {
        let url = validate_endpoint("HTTP destination", destination, self.endpoint_policy)?;
        let response = self
            .client
            .post(url)
            .json(value)
            .send()
            .await
            .map_err(|_| Error::protocol("OAuth HTTP request failed"))?;
        parse_json_response(response, self.max_response_bytes).await
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

/// Fetch protected-resource metadata with the default HTTPS-only URL policy.
///
/// The supplied client is retained for source compatibility but is intentionally not used because
/// its redirect and timeout behavior cannot be verified.
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

/// Fetch authorization-server metadata with the default HTTPS-only URL policy.
///
/// The supplied client is retained for source compatibility but is intentionally not used because
/// its redirect and timeout behavior cannot be verified.
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
        .host()
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

fn build_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(Policy::none())
        .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
        .timeout(DEFAULT_RESPONSE_TIMEOUT)
        .build()
        .map_err(|_| Error::protocol("failed to build OAuth HTTP client"))
}

// Use a no-follow client; only this metadata GET path may issue another request.
pub(super) async fn get_metadata(
    client: &reqwest::Client,
    mut url: Url,
    endpoint_policy: McpOAuthEndpointPolicy,
    timeout: Duration,
    accept: Option<&str>,
) -> Result<reqwest::Response> {
    let start = std::time::Instant::now();
    let origin = url.origin();
    let mut redirects_remaining = MAX_METADATA_REDIRECTS;
    loop {
        let remaining = timeout
            .checked_sub(start.elapsed())
            .ok_or_else(|| Error::protocol("OAuth metadata request timed out"))?;
        let mut request = client.get(url.clone()).timeout(remaining);
        if let Some(accept) = accept {
            request = request.header(header::ACCEPT, accept);
        }
        let response = request
            .send()
            .await
            .map_err(|_| Error::protocol("OAuth metadata request failed"))?;
        if !matches!(
            response.status(),
            StatusCode::MOVED_PERMANENTLY
                | StatusCode::FOUND
                | StatusCode::SEE_OTHER
                | StatusCode::TEMPORARY_REDIRECT
                | StatusCode::PERMANENT_REDIRECT
        ) {
            return Ok(response);
        }
        redirects_remaining = redirects_remaining
            .checked_sub(1)
            .ok_or_else(|| Error::protocol("OAuth metadata redirect limit exceeded"))?;
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .filter(|value| {
                !value.is_empty()
                    && value.trim() == *value
                    && !value.contains('\\')
                    && !value.chars().any(char::is_control)
            })
            .ok_or_else(|| Error::protocol("OAuth metadata redirect location is invalid"))?;
        let target = url
            .join(location)
            .map_err(|_| Error::protocol("OAuth metadata redirect location is invalid"))?;
        url = validate_endpoint("metadata redirect", target.as_str(), endpoint_policy)?;
        if url.origin() != origin {
            return Err(Error::protocol(
                "OAuth metadata redirect must stay on the same origin",
            ));
        }
    }
}

async fn parse_json_response<T: DeserializeOwned>(
    response: reqwest::Response,
    max_response_bytes: usize,
) -> Result<T> {
    if !response.status().is_success() {
        return Err(Error::protocol("OAuth HTTP response was unsuccessful"));
    }
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
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
    parse_unique_json(&body)
}

pub(super) fn parse_unique_json<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
    let mut deserializer = serde_json::Deserializer::from_slice(body);
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

pub(super) fn is_loopback_host(host: Host<&str>) -> bool {
    match host {
        Host::Domain(host) => host.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(address) => address.is_loopback(),
        Host::Ipv6(address) => address.is_loopback(),
    }
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

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{
        Json, Router,
        response::Redirect,
        routing::{get, post},
    };
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::*;

    #[tokio::test]
    async fn oauth_metadata_redirects_are_same_origin_and_writes_never_follow() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let alternate = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let alternate_origin = format!("http://{}", alternate.local_addr().unwrap());
        let redirected_requests = Arc::new(AtomicUsize::new(0));
        let observe_write = redirected_requests.clone();
        let observe_cross_origin = redirected_requests.clone();
        let alternate_task = tokio::spawn(async move {
            let endpoint = Router::new().route(
                "/json",
                get(move || async move {
                    observe_cross_origin.fetch_add(1, Ordering::Relaxed);
                    Json(json!({}))
                }),
            );
            axum::serve(alternate, endpoint).await.unwrap();
        });
        let server = Router::new()
            .route("/read", get(|| async { Redirect::temporary("/json") }))
            .route("/json", get(|| async { Json(json!({"ok": true})) }))
            .route(
                "/cross-origin",
                get(
                    move || async move { Redirect::temporary(&format!("{alternate_origin}/json")) },
                ),
            )
            .route("/write", post(|| async { Redirect::temporary("/written") }))
            .route(
                "/written",
                post(move || async move {
                    observe_write.fetch_add(1, Ordering::Relaxed);
                    Json(json!({}))
                }),
            );
        let task = tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });
        let discovery =
            McpOAuthDiscoveryClient::new(McpOAuthEndpointPolicy::AllowLoopbackHttp).unwrap();

        assert_eq!(
            discovery
                .get_json::<Value>(&format!("{origin}/read"))
                .await
                .unwrap(),
            json!({"ok": true})
        );
        assert!(
            discovery
                .get_json::<Value>(&format!("{origin}/cross-origin"))
                .await
                .is_err()
        );
        assert!(
            discovery
                .post_json::<_, Value>(&format!("{origin}/write"), &json!({"secret": "test"}))
                .await
                .is_err()
        );
        assert_eq!(redirected_requests.load(Ordering::Relaxed), 0);
        task.abort();
        alternate_task.abort();
    }
}
