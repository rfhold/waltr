use std::{env, error::Error, fmt, net::IpAddr, time::Duration};

use futures_util::StreamExt as _;
use mcp::TrustedPrivateOAuthCimdDestinationPolicy;
use reqwest::{Client, Response, Url, header::CONTENT_TYPE};
use tonic::Status;

pub(super) const OAUTH_RESPONSE_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub(super) struct OutboundHttpPolicy {
    allow_loopback_http: bool,
    allow_private_https: bool,
    configured_private_origins: Vec<Url>,
    automatic_self_resource: Option<Url>,
    active_private_origins: Vec<Url>,
}

impl OutboundHttpPolicy {
    pub(super) fn production(
        configured_private_origins: Vec<Url>,
        automatic_self_resource: Option<Url>,
        allow_private_https: bool,
    ) -> Self {
        Self {
            allow_loopback_http: false,
            allow_private_https,
            active_private_origins: configured_private_origins.clone(),
            configured_private_origins,
            automatic_self_resource,
        }
    }

    #[cfg(test)]
    pub(super) const fn loopback_test() -> Self {
        Self {
            allow_loopback_http: true,
            allow_private_https: false,
            configured_private_origins: Vec::new(),
            automatic_self_resource: None,
            active_private_origins: Vec::new(),
        }
    }

    pub(super) async fn client_for(&self, value: &str) -> Result<(Url, Client), Status> {
        let url = self.validate_url(value)?;
        let host = url
            .host_str()
            .ok_or_else(|| Status::failed_precondition("OAuth endpoint requires a host"))?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| Status::failed_precondition("OAuth endpoint has no usable port"))?;
        let addresses = tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| Status::unavailable("OAuth endpoint DNS resolution failed"))?
            .collect::<Vec<_>>();
        if addresses.is_empty()
            || addresses
                .iter()
                .any(|address| !self.allows_address(&url, address.ip()))
        {
            return Err(Status::failed_precondition(
                "OAuth endpoint resolves to a disallowed destination",
            ));
        }
        let mut builder = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10));
        if host.parse::<IpAddr>().is_err() {
            builder = builder.resolve_to_addrs(host, &addresses);
        }
        let client = builder
            .build()
            .map_err(|_| Status::internal("failed to build outbound OAuth client"))?;
        Ok((url, client))
    }

    pub(super) const fn allows_loopback_http(&self) -> bool {
        self.allow_loopback_http
    }

    pub(super) fn trusted_private_origins(&self) -> &[Url] {
        &self.active_private_origins
    }

    pub(super) fn for_mcp_resource(&self, resource: &str) -> Self {
        let mut policy = self.clone();
        policy
            .active_private_origins
            .clone_from(&self.configured_private_origins);
        if Url::parse(resource).ok().as_ref() == self.automatic_self_resource.as_ref()
            && let Some(resource) = &self.automatic_self_resource
            && !policy.trusts_private_origin(resource)
        {
            policy.active_private_origins.push(resource.clone());
        }
        policy
    }

    pub(super) fn without_trusted_private_origins(&self) -> Self {
        let mut policy = self.clone();
        policy.active_private_origins.clear();
        policy
    }

    pub(super) async fn validate_destination(&self, value: &str) -> Result<Url, Status> {
        self.client_for(value).await.map(|(url, _)| url)
    }

    fn validate_url(&self, value: &str) -> Result<Url, Status> {
        let url = Url::parse(value)
            .map_err(|_| Status::failed_precondition("OAuth endpoint URL is invalid"))?;
        if url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.host_str().is_none()
        {
            return Err(Status::failed_precondition(
                "OAuth endpoint URL contains disallowed components",
            ));
        }
        let loopback = url
            .host_str()
            .and_then(|host| host.parse::<IpAddr>().ok())
            .is_some_and(|address| address.is_loopback())
            || url
                .host_str()
                .is_some_and(|host| host.eq_ignore_ascii_case("localhost"));
        if url.scheme() != "https"
            && !(self.allow_loopback_http && url.scheme() == "http" && loopback)
        {
            return Err(Status::failed_precondition(
                "OAuth endpoints require HTTPS except in explicit loopback test mode",
            ));
        }
        Ok(url)
    }

    fn allows_address(&self, url: &Url, address: IpAddr) -> bool {
        if address.is_loopback() {
            return self.allow_loopback_http;
        }
        if (self.allow_private_https || self.trusts_private_origin(url))
            && match address {
                IpAddr::V4(address) => address.is_private(),
                IpAddr::V6(address) => address.is_unique_local(),
            }
        {
            return true;
        }
        is_public_address(address)
    }

    fn trusts_private_origin(&self, url: &Url) -> bool {
        self.active_private_origins
            .iter()
            .any(|trusted| trusted.origin() == url.origin())
    }
}

#[derive(Debug)]
pub struct TrustedPrivateOriginsError(String);

impl fmt::Display for TrustedPrivateOriginsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for TrustedPrivateOriginsError {}

pub fn trusted_private_origins_from_env() -> Result<Vec<Url>, TrustedPrivateOriginsError> {
    match env::var("WALTR_MCP_OUTBOUND_OAUTH_TRUSTED_PRIVATE_ORIGINS") {
        Ok(value) => parse_trusted_private_origins(&value),
        Err(env::VarError::NotPresent) => Ok(Vec::new()),
        Err(env::VarError::NotUnicode(_)) => Err(TrustedPrivateOriginsError(
            "WALTR_MCP_OUTBOUND_OAUTH_TRUSTED_PRIVATE_ORIGINS must contain valid UTF-8".to_owned(),
        )),
    }
}

fn parse_trusted_private_origins(value: &str) -> Result<Vec<Url>, TrustedPrivateOriginsError> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    let mut origins = Vec::new();
    for entry in value.split(',') {
        let entry = entry.trim();
        let origin = Url::parse(entry).map_err(|_| {
            TrustedPrivateOriginsError(
                "WALTR_MCP_OUTBOUND_OAUTH_TRUSTED_PRIVATE_ORIGINS must contain comma-separated HTTPS origins"
                    .to_owned(),
            )
        })?;
        if origin.scheme() != "https"
            || origin.host_str().is_none()
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
        {
            return Err(TrustedPrivateOriginsError(
                "WALTR_MCP_OUTBOUND_OAUTH_TRUSTED_PRIVATE_ORIGINS must contain only HTTPS origins without paths, credentials, queries, or fragments"
                    .to_owned(),
            ));
        }
        TrustedPrivateOAuthCimdDestinationPolicy::new([origin.clone()]).map_err(|_| {
            TrustedPrivateOriginsError(
                "WALTR_MCP_OUTBOUND_OAUTH_TRUSTED_PRIVATE_ORIGINS contains a prohibited origin"
                    .to_owned(),
            )
        })?;
        if origins
            .iter()
            .any(|existing: &Url| existing.origin() == origin.origin())
        {
            return Err(TrustedPrivateOriginsError(
                "WALTR_MCP_OUTBOUND_OAUTH_TRUSTED_PRIVATE_ORIGINS contains a duplicate origin"
                    .to_owned(),
            ));
        }
        origins.push(origin);
    }
    Ok(origins)
}

pub(super) fn validate_response_peer(
    policy: &OutboundHttpPolicy,
    response: &Response,
) -> Result<(), Status> {
    let peer = response.remote_addr().ok_or_else(|| {
        Status::permission_denied("OAuth response peer address could not be verified")
    })?;
    if !policy.allows_address(response.url(), peer.ip()) {
        return Err(Status::permission_denied(
            "OAuth response came from a disallowed destination",
        ));
    }
    Ok(())
}

pub(super) async fn read_json_body(response: Response, limit: usize) -> Result<Vec<u8>, Status> {
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
        return Err(Status::data_loss("OAuth response content type is not JSON"));
    }
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(Status::resource_exhausted(
            "OAuth response exceeds the size limit",
        ));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|_| Status::unavailable("OAuth response body could not be read"))?;
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(Status::resource_exhausted(
                "OAuth response exceeds the size limit",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn is_public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let octets = address.octets();
            !(address.is_private()
                || address.is_loopback()
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
                return is_public_address(IpAddr::V4(mapped));
            }
            let segments = address.segments();
            !(address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || address.is_unique_local()
                || address.is_unicast_link_local()
                || (segments[0] == 0x2001 && segments[1] == 0x0db8))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn production_policy_rejects_private_metadata_and_loopback_destinations() {
        let policy = OutboundHttpPolicy::production(Vec::new(), None, false);
        for endpoint in [
            "http://127.0.0.1/token",
            "https://10.0.0.1/token",
            "https://169.254.169.254/latest/meta-data",
            "https://[fe80::1]/token",
        ] {
            assert!(policy.client_for(endpoint).await.is_err(), "{endpoint}");
        }
    }

    #[tokio::test]
    async fn loopback_http_requires_explicit_test_policy() {
        assert!(
            OutboundHttpPolicy::loopback_test()
                .client_for("http://127.0.0.1/token")
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn trusted_https_origin_accepts_only_private_addresses_on_that_origin() {
        let self_resource = Url::parse("https://preview-waltr.example/mcp").expect("trusted URL");
        let configured = Url::parse("https://configured.example").expect("configured origin");
        let policy = OutboundHttpPolicy::production(vec![configured], Some(self_resource), false);
        let policy = policy.for_mcp_resource("https://preview-waltr.example/mcp");
        let trusted = Url::parse("https://preview-waltr.example/token").expect("endpoint URL");
        let other_host = Url::parse("https://other.example/token").expect("endpoint URL");
        let other_port =
            Url::parse("https://preview-waltr.example:444/token").expect("endpoint URL");

        let configured = Url::parse("https://configured.example/token").expect("endpoint URL");
        assert!(policy.allows_address(&trusted, "10.0.0.1".parse().expect("private IP")));
        assert!(policy.allows_address(&configured, "10.0.0.1".parse().expect("private IP")));
        assert!(policy.allows_address(&trusted, "fd00::1".parse().expect("private IP")));
        assert!(!policy.allows_address(&other_host, "10.0.0.1".parse().expect("private IP")));
        assert!(!policy.allows_address(&other_port, "10.0.0.1".parse().expect("private IP")));
        assert!(!policy.allows_address(&trusted, "127.0.0.1".parse().expect("loopback IP")));
        assert!(
            !policy.allows_address(&trusted, "169.254.169.254".parse().expect("link-local IP"))
        );
        assert_eq!(
            policy
                .for_mcp_resource("https://other.example/mcp")
                .trusted_private_origins()
                .len(),
            1
        );
        assert_eq!(
            policy
                .for_mcp_resource("https://preview-waltr.example/mcp")
                .trusted_private_origins()
                .len(),
            2
        );
        assert_eq!(
            policy
                .for_mcp_resource("https://preview-waltr.example/other-mcp")
                .trusted_private_origins()
                .len(),
            1
        );
        assert!(
            policy
                .without_trusted_private_origins()
                .trusted_private_origins()
                .is_empty()
        );
    }

    #[test]
    fn trusted_origin_parser_requires_exact_https_origins() {
        let origins =
            parse_trusted_private_origins("https://private.example, https://other.example:444")
                .expect("valid origins");
        assert_eq!(origins.len(), 2);
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
            "https://private.example,",
            "https://PRIVATE.example:443,https://private.example",
        ] {
            assert!(parse_trusted_private_origins(invalid).is_err(), "{invalid}");
        }
    }
}
