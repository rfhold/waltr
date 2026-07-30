#![allow(clippy::duration_suboptimal_units)]

use std::{
    collections::BTreeSet,
    fmt,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use axum::http::HeaderMap;
use futures_util::StreamExt as _;
use reqwest::{Url, header, redirect::Policy};
use sha2::{Digest as _, Sha256};
use url::Host;

use super::OAuthAuthorizationStoreError;
use crate::{
    Error, McpClientMetadataDocument, McpOAuthEndpointPolicy, Result, server::BoxFuture,
    validate_public_redirect_uri,
};

const DEFAULT_MAX_BODY_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_DNS_RESULTS: usize = 8;
const DEFAULT_DNS_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_RESPONSE_HEADERS: usize = 64;
const MAX_RESPONSE_HEADER_BYTES: usize = 16 * 1024;

/// Validated CIMD fetch result and safe freshness lifetime.
#[derive(Clone, Debug)]
pub struct OAuthFetchedClientMetadata {
    /// Parsed and validated document.
    pub document: McpClientMetadataDocument,
    /// Canonical JSON bytes used for the immutable digest.
    pub normalized_document: Vec<u8>,
    /// SHA-256 digest of the canonical document.
    pub document_digest: Vec<u8>,
    /// Bounded freshness selected from safe response metadata.
    pub freshness: Duration,
}

/// Object-safe CIMD retrieval seam.
pub trait OAuthClientMetadataFetcher: Send + Sync + 'static {
    /// Fetch, validate, and normalize one URL-shaped client identifier.
    fn fetch(&self, client_id: String) -> BoxFuture<Result<OAuthFetchedClientMetadata>>;
}

/// Sanitized CIMD retrieval outcome for host metrics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OAuthCimdFetchOutcome {
    /// Document was fetched and validated.
    Accepted,
    /// Destination, transport, or document validation failed.
    Rejected,
}

/// Sanitized DCR admission outcome for host metrics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OAuthRegistrationOutcome {
    /// Registration was accepted.
    Accepted,
    /// Metadata or policy validation rejected registration.
    Rejected,
    /// A registration rate limit was reached.
    Limited,
    /// Provisional or active-client capacity was reached.
    CapacityBlocked,
}

/// Optional host observer. Values never contain client IDs, request bodies, or secrets.
pub trait OAuthRegistrationObserver: Send + Sync + 'static {
    /// Observe CIMD retrieval by sanitized hostname and outcome.
    fn cimd_fetch(&self, hostname: &str, outcome: OAuthCimdFetchOutcome);
    /// Observe aggregate DCR outcomes without client labels.
    fn dynamic_registration(&self, outcome: OAuthRegistrationOutcome);
}

/// DNS resolution and destination admission policy for CIMD.
pub trait OAuthCimdDestinationPolicy: Send + Sync + 'static {
    /// Resolve and approve the complete address set before connecting.
    fn resolve(&self, destination: Url) -> BoxFuture<Result<Vec<SocketAddr>>>;
}

/// Production destination policy that rejects non-public IP address ranges.
#[derive(Clone, Copy, Debug, Default)]
pub struct PublicOAuthCimdDestinationPolicy;

impl OAuthCimdDestinationPolicy for PublicOAuthCimdDestinationPolicy {
    fn resolve(&self, destination: Url) -> BoxFuture<Result<Vec<SocketAddr>>> {
        Box::pin(async move {
            let host = destination_host(&destination)?;
            let port = destination
                .port_or_known_default()
                .ok_or_else(|| Error::protocol("CIMD destination requires a port"))?;
            let addresses = tokio::time::timeout(
                DEFAULT_DNS_TIMEOUT,
                tokio::net::lookup_host((host.as_str(), port)),
            )
            .await
            .map_err(|_| Error::protocol("CIMD DNS resolution timed out"))?
            .map_err(|_| Error::protocol("CIMD DNS resolution failed"))?
            .collect::<BTreeSet<_>>();
            if addresses.is_empty()
                || addresses.len() > DEFAULT_MAX_DNS_RESULTS
                || addresses.iter().any(|address| !is_public_ip(address.ip()))
            {
                return Err(Error::protocol("CIMD destination is prohibited"));
            }
            Ok(addresses.into_iter().collect())
        })
    }
}

/// Production destination policy that permits private addresses only for explicit HTTPS origins.
#[derive(Clone, Debug, Default)]
pub struct TrustedPrivateOAuthCimdDestinationPolicy {
    trusted_origins: Arc<Vec<Url>>,
}

impl TrustedPrivateOAuthCimdDestinationPolicy {
    /// Validate and store exact trusted origins.
    ///
    /// # Errors
    ///
    /// Returns an error unless every value is a credential-free HTTPS origin.
    pub fn new(origins: impl IntoIterator<Item = Url>) -> Result<Self> {
        let mut trusted_origins = Vec::new();
        for origin in origins {
            if origin.scheme() != "https"
                || origin.host_str().is_none()
                || !origin.username().is_empty()
                || origin.password().is_some()
                || origin.path() != "/"
                || origin.query().is_some()
                || origin.fragment().is_some()
                || !trusted_origin_host_is_permitted(&origin)
            {
                return Err(Error::protocol(
                    "trusted CIMD destination must be an HTTPS origin",
                ));
            }
            if !trusted_origins
                .iter()
                .any(|trusted: &Url| trusted.origin() == origin.origin())
            {
                trusted_origins.push(origin);
            }
        }
        Ok(Self {
            trusted_origins: Arc::new(trusted_origins),
        })
    }

    fn trusts(&self, destination: &Url) -> bool {
        self.trusted_origins
            .iter()
            .any(|trusted| trusted.origin() == destination.origin())
    }
}

fn trusted_origin_host_is_permitted(origin: &Url) -> bool {
    match origin.host() {
        Some(Host::Domain(host)) => !host.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address.is_private() || is_public_ip(IpAddr::V4(address)),
        Some(Host::Ipv6(address)) => address.is_unique_local() || is_public_ip(IpAddr::V6(address)),
        None => false,
    }
}

impl OAuthCimdDestinationPolicy for TrustedPrivateOAuthCimdDestinationPolicy {
    fn resolve(&self, destination: Url) -> BoxFuture<Result<Vec<SocketAddr>>> {
        let trusted = self.trusts(&destination);
        Box::pin(async move {
            let host = destination_host(&destination)?;
            let port = destination
                .port_or_known_default()
                .ok_or_else(|| Error::protocol("CIMD destination requires a port"))?;
            let addresses = tokio::time::timeout(
                DEFAULT_DNS_TIMEOUT,
                tokio::net::lookup_host((host.as_str(), port)),
            )
            .await
            .map_err(|_| Error::protocol("CIMD DNS resolution timed out"))?
            .map_err(|_| Error::protocol("CIMD DNS resolution failed"))?
            .collect::<BTreeSet<_>>();
            if addresses.is_empty()
                || addresses.len() > DEFAULT_MAX_DNS_RESULTS
                || addresses.iter().any(|address| {
                    !(is_public_ip(address.ip()) || trusted && is_private_ip(address.ip()))
                })
            {
                return Err(Error::protocol("CIMD destination is prohibited"));
            }
            Ok(addresses.into_iter().collect())
        })
    }
}

/// Explicit deterministic-test policy that permits loopback HTTP only.
#[derive(Clone, Copy, Debug, Default)]
pub struct LoopbackOAuthCimdDestinationPolicy;

impl OAuthCimdDestinationPolicy for LoopbackOAuthCimdDestinationPolicy {
    fn resolve(&self, destination: Url) -> BoxFuture<Result<Vec<SocketAddr>>> {
        Box::pin(async move {
            let host = destination_host(&destination)?;
            let port = destination
                .port_or_known_default()
                .ok_or_else(|| Error::protocol("CIMD destination requires a port"))?;
            let addresses = tokio::time::timeout(
                DEFAULT_DNS_TIMEOUT,
                tokio::net::lookup_host((host.as_str(), port)),
            )
            .await
            .map_err(|_| Error::protocol("CIMD DNS resolution timed out"))?
            .map_err(|_| Error::protocol("CIMD DNS resolution failed"))?
            .collect::<BTreeSet<_>>();
            if addresses.is_empty()
                || addresses.len() > DEFAULT_MAX_DNS_RESULTS
                || addresses.iter().any(|address| !address.ip().is_loopback())
            {
                return Err(Error::protocol("CIMD test destination is not loopback"));
            }
            Ok(addresses.into_iter().collect())
        })
    }
}

/// Explicit deterministic-test policy that permits private HTTPS destinations.
#[derive(Clone, Copy, Debug, Default)]
pub struct PrivateNetworkOAuthCimdDestinationPolicy;

impl OAuthCimdDestinationPolicy for PrivateNetworkOAuthCimdDestinationPolicy {
    fn resolve(&self, destination: Url) -> BoxFuture<Result<Vec<SocketAddr>>> {
        Box::pin(async move {
            let host = destination_host(&destination)?;
            let port = destination
                .port_or_known_default()
                .ok_or_else(|| Error::protocol("CIMD destination requires a port"))?;
            let addresses = tokio::time::timeout(
                DEFAULT_DNS_TIMEOUT,
                tokio::net::lookup_host((host.as_str(), port)),
            )
            .await
            .map_err(|_| Error::protocol("CIMD DNS resolution timed out"))?
            .map_err(|_| Error::protocol("CIMD DNS resolution failed"))?
            .collect::<BTreeSet<_>>();
            if addresses.is_empty()
                || addresses.len() > DEFAULT_MAX_DNS_RESULTS
                || addresses.iter().any(|address| !is_private_ip(address.ip()))
            {
                return Err(Error::protocol("CIMD test destination is not private"));
            }
            Ok(addresses.into_iter().collect())
        })
    }
}

const fn is_private_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_private(),
        IpAddr::V6(address) => address.is_unique_local(),
    }
}

/// DNS-pinned, redirect-disabled, bounded CIMD HTTP fetcher.
#[derive(Clone)]
pub struct HardenedOAuthClientMetadataFetcher {
    endpoint_policy: McpOAuthEndpointPolicy,
    destination_policy: Arc<dyn OAuthCimdDestinationPolicy>,
    connect_timeout: Duration,
    total_timeout: Duration,
    max_body_bytes: usize,
    default_freshness: Duration,
    max_freshness: Duration,
    allow_loopback_redirects: bool,
    observer: Option<Arc<dyn OAuthRegistrationObserver>>,
}

impl fmt::Debug for HardenedOAuthClientMetadataFetcher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HardenedOAuthClientMetadataFetcher")
            .field("endpoint_policy", &self.endpoint_policy)
            .field("max_body_bytes", &self.max_body_bytes)
            .field("max_freshness", &self.max_freshness)
            .finish_non_exhaustive()
    }
}

impl HardenedOAuthClientMetadataFetcher {
    /// Create a production fetcher requiring HTTPS and public destinations.
    #[must_use]
    pub fn production() -> Self {
        Self {
            endpoint_policy: McpOAuthEndpointPolicy::HttpsOnly,
            destination_policy: Arc::new(PublicOAuthCimdDestinationPolicy),
            connect_timeout: Duration::from_secs(5),
            total_timeout: Duration::from_secs(10),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            default_freshness: Duration::from_secs(5 * 60),
            max_freshness: Duration::from_secs(60 * 60),
            allow_loopback_redirects: false,
            observer: None,
        }
    }

    /// Create a loopback-only HTTP fetcher for deterministic tests.
    #[must_use]
    pub fn loopback_for_tests() -> Self {
        Self {
            endpoint_policy: McpOAuthEndpointPolicy::AllowLoopbackHttp,
            destination_policy: Arc::new(LoopbackOAuthCimdDestinationPolicy),
            ..Self::production()
        }
    }

    /// Override response and freshness bounds.
    #[must_use]
    pub const fn with_bounds(
        mut self,
        max_body_bytes: usize,
        default_freshness: Duration,
        max_freshness: Duration,
    ) -> Self {
        self.max_body_bytes = max_body_bytes;
        self.default_freshness = default_freshness;
        self.max_freshness = max_freshness;
        self
    }

    /// Override the resolver/admission policy.
    #[must_use]
    pub fn with_destination_policy(mut self, policy: Arc<dyn OAuthCimdDestinationPolicy>) -> Self {
        self.destination_policy = policy;
        self
    }

    /// Explicitly permit native loopback HTTP redirect URIs in fetched metadata.
    #[must_use]
    pub const fn with_loopback_redirects(mut self, allow: bool) -> Self {
        self.allow_loopback_redirects = allow;
        self
    }

    /// Install a sanitized metrics observer.
    #[must_use]
    pub fn with_observer(mut self, observer: Arc<dyn OAuthRegistrationObserver>) -> Self {
        self.observer = Some(observer);
        self
    }
}

impl OAuthClientMetadataFetcher for HardenedOAuthClientMetadataFetcher {
    fn fetch(&self, client_id: String) -> BoxFuture<Result<OAuthFetchedClientMetadata>> {
        let this = self.clone();
        Box::pin(async move {
            let hostname = Url::parse(&client_id)
                .ok()
                .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
                .unwrap_or_default();
            let result = this.fetch_document(&client_id).await;
            if let Some(observer) = &this.observer {
                observer.cimd_fetch(
                    &hostname,
                    if result.is_ok() {
                        OAuthCimdFetchOutcome::Accepted
                    } else {
                        OAuthCimdFetchOutcome::Rejected
                    },
                );
            }
            result
        })
    }
}

impl HardenedOAuthClientMetadataFetcher {
    #[allow(clippy::too_many_lines)]
    async fn fetch_document(&self, client_id: &str) -> Result<OAuthFetchedClientMetadata> {
        let destination = super::super::discovery::validate_endpoint(
            "client metadata URL",
            client_id,
            self.endpoint_policy,
        )?;
        let addresses = self.destination_policy.resolve(destination.clone()).await?;
        let host = destination_host(&destination)?;
        let approved = addresses
            .iter()
            .map(SocketAddr::ip)
            .collect::<BTreeSet<_>>();
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .no_proxy()
            .connect_timeout(self.connect_timeout)
            .timeout(self.total_timeout)
            .resolve_to_addrs(&host, &addresses)
            .build()
            .map_err(|_| Error::protocol("failed to build CIMD HTTP client"))?;
        let response = client
            .get(destination.clone())
            .header(header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| Error::protocol("CIMD HTTP request failed"))?;
        if response.url() != &destination || response.status().is_redirection() {
            return Err(Error::protocol("CIMD HTTP redirect refused"));
        }
        if !response.status().is_success() {
            return Err(Error::protocol("CIMD HTTP response was unsuccessful"));
        }
        if response.headers().len() > MAX_RESPONSE_HEADERS
            || response
                .headers()
                .iter()
                .fold(0_usize, |size, (name, value)| {
                    size.saturating_add(name.as_str().len())
                        .saturating_add(value.as_bytes().len())
                })
                > MAX_RESPONSE_HEADER_BYTES
        {
            return Err(Error::protocol("CIMD response headers exceed limit"));
        }
        if response
            .remote_addr()
            .is_none_or(|peer| !approved.contains(&peer.ip()))
        {
            return Err(Error::protocol("CIMD connected peer was not approved"));
        }
        let media_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .unwrap_or_default();
        let media_type = media_type.to_ascii_lowercase();
        if media_type != "application/json"
            && !(media_type.starts_with("application/") && media_type.ends_with("+json"))
        {
            return Err(Error::protocol("CIMD response content type is not JSON"));
        }
        if response.content_length().is_some_and(|length| {
            usize::try_from(length).map_or(true, |length| length > self.max_body_bytes)
        }) {
            return Err(Error::protocol("CIMD response body exceeds limit"));
        }
        let freshness = cache_freshness(
            response.headers(),
            self.default_freshness,
            self.max_freshness,
        );
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| Error::protocol("CIMD response body failed"))?;
            if bytes.len().saturating_add(chunk.len()) > self.max_body_bytes {
                return Err(Error::protocol("CIMD response body exceeds limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let document: McpClientMetadataDocument =
            super::super::discovery::parse_unique_json(&bytes)
                .map_err(|_| Error::protocol("CIMD response JSON is invalid"))?;
        document
            .validate_public_client(client_id, self.allow_loopback_redirects)
            .map_err(|_| Error::protocol("CIMD public client metadata is invalid"))?;
        for redirect in &document.redirect_uris {
            validate_public_redirect_uri(redirect, self.allow_loopback_redirects)
                .map_err(|_| Error::protocol("CIMD redirect URI is invalid"))?;
        }
        let normalized_document = serde_json::to_vec(&document)
            .map_err(|_| Error::protocol("CIMD normalization failed"))?;
        let document_digest = Sha256::digest(&normalized_document).to_vec();
        Ok(OAuthFetchedClientMetadata {
            document,
            normalized_document,
            document_digest,
            freshness,
        })
    }
}

/// Trusted-source identity seam for DCR rate limiting.
pub trait OAuthRegistrationSourceResolver: Send + Sync + 'static {
    /// Return a stable trusted source identity, or `None` for global-only limits.
    fn trusted_source(&self, headers: &HeaderMap) -> Option<String>;
}

/// Generic registration and CIMD policy installed on the hosted server.
#[derive(Clone)]
pub struct OAuthClientRegistrationOptions {
    /// Optional CIMD fetcher. `None` disables URL client IDs.
    pub metadata_fetcher: Option<Arc<dyn OAuthClientMetadataFetcher>>,
    /// Enable the configured RFC 7591 registration endpoint.
    pub dynamic_registration: bool,
    /// Explicit native loopback redirect policy.
    pub allow_loopback_redirects: bool,
    /// Maximum registration JSON body.
    pub max_request_bytes: usize,
    /// Maximum display or URI field length.
    pub max_field_bytes: usize,
    /// Provisional registration lifetime.
    pub provisional_lifetime: Duration,
    /// Inactive promoted-client lifetime.
    pub inactive_client_lifetime: Duration,
    /// Maximum provisional dynamic registrations.
    pub max_provisional_clients: usize,
    /// Maximum promoted dynamic clients.
    pub max_active_dynamic_clients: usize,
    /// Per-principal promoted-client limit.
    pub max_clients_per_principal: usize,
    /// Global registrations per fixed window.
    pub global_rate_limit: usize,
    /// Trusted-source registrations per fixed window.
    pub source_rate_limit: usize,
    /// Fixed rate-limit window.
    pub rate_limit_window: Duration,
    /// Optional host-validated source identity resolver.
    pub source_resolver: Option<Arc<dyn OAuthRegistrationSourceResolver>>,
    /// Optional sanitized outcome observer.
    pub observer: Option<Arc<dyn OAuthRegistrationObserver>>,
}

impl fmt::Debug for OAuthClientRegistrationOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthClientRegistrationOptions")
            .field("cimd_enabled", &self.metadata_fetcher.is_some())
            .field("dynamic_registration", &self.dynamic_registration)
            .field("allow_loopback_redirects", &self.allow_loopback_redirects)
            .field("max_request_bytes", &self.max_request_bytes)
            .finish_non_exhaustive()
    }
}

impl Default for OAuthClientRegistrationOptions {
    fn default() -> Self {
        Self {
            metadata_fetcher: None,
            dynamic_registration: false,
            allow_loopback_redirects: false,
            max_request_bytes: DEFAULT_MAX_BODY_BYTES,
            max_field_bytes: 2_048,
            provisional_lifetime: Duration::from_secs(10 * 60),
            inactive_client_lifetime: Duration::from_secs(30 * 24 * 60 * 60),
            max_provisional_clients: 1_000,
            max_active_dynamic_clients: 10_000,
            max_clients_per_principal: 20,
            global_rate_limit: 120,
            source_rate_limit: 20,
            rate_limit_window: Duration::from_secs(60),
            source_resolver: None,
            observer: None,
        }
    }
}

pub(super) fn duration_ms(duration: Duration) -> Result<i64> {
    i64::try_from(duration.as_millis())
        .map_err(|_| Error::protocol("OAuth registration lifetime is invalid"))
}

fn cache_freshness(headers: &HeaderMap, fallback: Duration, maximum: Duration) -> Duration {
    let cache_control = headers
        .get(header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok());
    let no_cache = cache_control.is_some_and(|value| {
        value.split(',').any(|directive| {
            matches!(
                directive.trim().to_ascii_lowercase().as_str(),
                "no-cache" | "no-store"
            )
        })
    });
    let max_age = if no_cache {
        Duration::ZERO
    } else {
        cache_control
            .and_then(|value| {
                value.split(',').find_map(|directive| {
                    directive
                        .trim()
                        .strip_prefix("max-age=")
                        .and_then(|seconds| seconds.parse::<u64>().ok())
                })
            })
            .map_or(fallback, Duration::from_secs)
    };
    max_age.min(maximum).max(Duration::from_secs(1))
}

fn destination_host(destination: &Url) -> Result<String> {
    match destination.host() {
        Some(url::Host::Ipv6(address)) => Ok(address.to_string()),
        Some(host) => Ok(host.to_string()),
        None => Err(Error::protocol("CIMD destination requires a host")),
    }
}

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            !(address.is_private()
                || address.is_loopback()
                || address.is_link_local()
                || address.is_multicast()
                || address.is_broadcast()
                || address.is_unspecified()
                || address.octets()[0] == 0
                || address.octets()[0] >= 240
                || address.octets() == [169, 254, 169, 254]
                || address.octets()[0] == 100 && (64..=127).contains(&address.octets()[1])
                || address.octets()[0] == 192 && address.octets()[1] == 0
                || address.octets()[0..3] == [192, 0, 2]
                || address.octets()[0..3] == [198, 51, 100]
                || address.octets()[0..3] == [203, 0, 113]
                || address.octets()[0] == 198 && (18..=19).contains(&address.octets()[1])
                || address.octets()[0] == 192
                    && address.octets()[1] == 88
                    && address.octets()[2] == 99)
        }
        IpAddr::V6(address) => {
            if let Some(ipv4) = address.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(ipv4));
            }
            let segments = address.segments();
            !(address.is_loopback()
                || address.is_multicast()
                || address.is_unspecified()
                || address.is_unique_local()
                || address.is_unicast_link_local()
                || segments[0] == 0x2001 && matches!(segments[1], 0x0000..=0x01ff | 0x0db8)
                || segments[0] == 0x0100 && segments[1..4] == [0, 0, 0]
                || segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2] == 0x0001
                || segments[0] & 0xffc0 == 0xfec0
                || segments[0] == 0x2002
                || segments[0] & 0xfff0 == 0x3ff0)
        }
    }
}

impl From<OAuthAuthorizationStoreError> for Error {
    fn from(_: OAuthAuthorizationStoreError) -> Self {
        Self::protocol("OAuth registration persistence failed")
    }
}

#[cfg(test)]
mod destination_policy_tests {
    use super::*;

    #[tokio::test]
    async fn trusted_private_origins_are_exact_and_keep_prohibited_addresses_blocked() {
        let policy = TrustedPrivateOAuthCimdDestinationPolicy::new([
            Url::parse("https://10.1.2.3").expect("private IPv4 origin"),
            Url::parse("https://[fd00::1]").expect("private IPv6 origin"),
        ])
        .expect("trusted origins");

        assert!(
            policy
                .resolve(Url::parse("https://10.1.2.3/client").expect("destination"))
                .await
                .is_ok()
        );
        assert!(
            policy
                .resolve(Url::parse("https://[fd00::1]/client").expect("destination"))
                .await
                .is_ok()
        );
        assert!(
            policy
                .resolve(Url::parse("https://10.1.2.3:444/client").expect("destination"))
                .await
                .is_err()
        );
        assert!(
            policy
                .resolve(Url::parse("https://127.0.0.1/client").expect("destination"))
                .await
                .is_err()
        );
        assert!(
            policy
                .resolve(Url::parse("https://169.254.169.254/client").expect("destination"))
                .await
                .is_err()
        );
        assert!(
            policy
                .resolve(Url::parse("https://1.1.1.1/client").expect("destination"))
                .await
                .is_ok()
        );
    }

    #[test]
    fn trusted_private_origin_configuration_requires_origins() {
        for invalid in [
            "http://private.example",
            "https://private.example/path",
            "https://user@private.example",
            "https://private.example?query",
            "https://private.example#fragment",
            "https://localhost",
            "https://127.0.0.1",
            "https://169.254.169.254",
            "https://[::1]",
        ] {
            assert!(
                TrustedPrivateOAuthCimdDestinationPolicy::new([
                    Url::parse(invalid).expect("absolute URL")
                ])
                .is_err(),
                "{invalid}"
            );
        }
    }
}
