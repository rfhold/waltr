use std::{net::IpAddr, time::Duration};

use futures_util::StreamExt as _;
use reqwest::{Client, Response, Url, header::CONTENT_TYPE};
use tonic::Status;

pub(super) const OAUTH_RESPONSE_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub(super) struct OutboundHttpPolicy {
    allow_loopback_http: bool,
    allow_private_https: bool,
    trusted_private_origin: Option<Url>,
}

impl OutboundHttpPolicy {
    pub(super) fn production(trusted_private_origin: Option<Url>) -> Self {
        Self {
            allow_loopback_http: false,
            allow_private_https: std::env::var("WALTR_OAUTH_ALLOW_PRIVATE_TEST_DESTINATIONS")
                .is_ok_and(|value| value == "true"),
            trusted_private_origin,
        }
    }

    #[cfg(test)]
    pub(super) const fn loopback_test() -> Self {
        Self {
            allow_loopback_http: true,
            allow_private_https: false,
            trusted_private_origin: None,
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

    pub(super) const fn trusted_private_origin(&self) -> Option<&Url> {
        self.trusted_private_origin.as_ref()
    }

    pub(super) fn for_mcp_resource(&self, resource: &str) -> Self {
        let mut policy = self.clone();
        if Url::parse(resource).map_or(true, |url| {
            self.trusted_private_origin.as_ref() != Some(&url)
        }) {
            policy.trusted_private_origin = None;
        }
        policy
    }

    pub(super) fn without_trusted_private_origin(&self) -> Self {
        let mut policy = self.clone();
        policy.trusted_private_origin = None;
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
        self.trusted_private_origin
            .as_ref()
            .is_some_and(|trusted| trusted.origin() == url.origin())
    }
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
        let policy = OutboundHttpPolicy::production(None);
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
        let policy = OutboundHttpPolicy::production(Some(
            Url::parse("https://preview-waltr.example/mcp").expect("trusted URL"),
        ));
        let trusted = Url::parse("https://preview-waltr.example/token").expect("endpoint URL");
        let other_host = Url::parse("https://other.example/token").expect("endpoint URL");
        let other_port =
            Url::parse("https://preview-waltr.example:444/token").expect("endpoint URL");

        assert!(policy.allows_address(&trusted, "10.0.0.1".parse().expect("private IP")));
        assert!(policy.allows_address(&trusted, "fd00::1".parse().expect("private IP")));
        assert!(!policy.allows_address(&other_host, "10.0.0.1".parse().expect("private IP")));
        assert!(!policy.allows_address(&other_port, "10.0.0.1".parse().expect("private IP")));
        assert!(!policy.allows_address(&trusted, "127.0.0.1".parse().expect("loopback IP")));
        assert!(
            !policy.allows_address(&trusted, "169.254.169.254".parse().expect("link-local IP"))
        );
        assert!(
            policy
                .for_mcp_resource("https://other.example/mcp")
                .trusted_private_origin()
                .is_none()
        );
        assert!(
            policy
                .for_mcp_resource("https://preview-waltr.example/mcp")
                .trusted_private_origin()
                .is_some()
        );
        assert!(
            policy
                .for_mcp_resource("https://preview-waltr.example/other-mcp")
                .trusted_private_origin()
                .is_none()
        );
        assert!(
            policy
                .without_trusted_private_origin()
                .trusted_private_origin()
                .is_none()
        );
    }
}
