#![allow(clippy::duration_suboptimal_units)]

use std::{fmt, sync::Arc, time::Duration};

use axum::http::HeaderMap;
use futures_util::StreamExt as _;
use reqwest::{Url, header, redirect::Policy};
use sha2::{Digest as _, Sha256};

use super::OAuthAuthorizationStoreError;
use crate::{Error, McpClientMetadataDocument, McpOAuthEndpointPolicy, Result, server::BoxFuture};

const DEFAULT_MAX_BODY_BYTES: usize = 64 * 1024;
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
    /// URL scheme policy for fetched and cached identifiers.
    fn endpoint_policy(&self) -> McpOAuthEndpointPolicy {
        McpOAuthEndpointPolicy::HttpsOnly
    }

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

/// Bounded CIMD HTTP fetcher that follows only same-origin metadata redirects.
#[derive(Clone)]
pub struct HardenedOAuthClientMetadataFetcher {
    endpoint_policy: McpOAuthEndpointPolicy,
    connect_timeout: Duration,
    total_timeout: Duration,
    max_body_bytes: usize,
    default_freshness: Duration,
    max_freshness: Duration,
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
    /// Create a production fetcher requiring HTTPS.
    #[must_use]
    pub fn production() -> Self {
        Self {
            endpoint_policy: McpOAuthEndpointPolicy::HttpsOnly,
            connect_timeout: Duration::from_secs(5),
            total_timeout: Duration::from_secs(10),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            default_freshness: Duration::from_secs(5 * 60),
            max_freshness: Duration::from_secs(60 * 60),
            observer: None,
        }
    }

    /// Additionally permit loopback HTTP metadata URLs for deterministic tests.
    #[must_use]
    pub fn loopback_for_tests() -> Self {
        Self {
            endpoint_policy: McpOAuthEndpointPolicy::AllowLoopbackHttp,
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

    /// Install a sanitized metrics observer.
    #[must_use]
    pub fn with_observer(mut self, observer: Arc<dyn OAuthRegistrationObserver>) -> Self {
        self.observer = Some(observer);
        self
    }
}

impl OAuthClientMetadataFetcher for HardenedOAuthClientMetadataFetcher {
    fn endpoint_policy(&self) -> McpOAuthEndpointPolicy {
        self.endpoint_policy
    }

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
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .connect_timeout(self.connect_timeout)
            .timeout(self.total_timeout)
            .build()
            .map_err(|_| Error::protocol("failed to build CIMD HTTP client"))?;
        let response = super::super::discovery::get_metadata(
            &client,
            destination,
            self.endpoint_policy,
            self.total_timeout,
            Some("application/json"),
        )
        .await?;
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
            .validate_public_client(client_id, self.endpoint_policy)
            .map_err(|_| Error::protocol("CIMD public client metadata is invalid"))?;
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
            .field("max_request_bytes", &self.max_request_bytes)
            .finish_non_exhaustive()
    }
}

impl Default for OAuthClientRegistrationOptions {
    fn default() -> Self {
        Self {
            metadata_fetcher: None,
            dynamic_registration: false,
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

impl From<OAuthAuthorizationStoreError> for Error {
    fn from(_: OAuthAuthorizationStoreError) -> Self {
        Self::protocol("OAuth registration persistence failed")
    }
}
