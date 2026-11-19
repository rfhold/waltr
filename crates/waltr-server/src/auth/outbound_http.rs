use std::time::Duration;

use futures_util::StreamExt as _;
use reqwest::{
    Client, Response, StatusCode, Url,
    header::{CONTENT_TYPE, LOCATION},
    redirect::Policy,
};
use tonic::Status;

pub(super) const OAUTH_RESPONSE_LIMIT: usize = 64 * 1024;
const METADATA_REDIRECT_LIMIT: usize = 5;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy)]
pub(super) struct OutboundHttpPolicy {
    allow_loopback_http: bool,
}

impl OutboundHttpPolicy {
    pub(super) const fn production() -> Self {
        Self::new(false)
    }

    #[cfg(test)]
    pub(super) const fn loopback_test() -> Self {
        Self::new(true)
    }

    pub(super) const fn new(allow_loopback_http: bool) -> Self {
        Self {
            allow_loopback_http,
        }
    }

    pub(super) fn client_for(self, value: &str) -> Result<(Url, Client), Status> {
        let url = self.validate_url(value)?;
        let client = Self::client_builder()
            .redirect(Policy::none())
            .build()
            .map_err(|_| Status::internal("failed to build outbound OAuth client"))?;
        Ok((url, client))
    }

    pub(super) async fn get_metadata(
        self,
        value: &str,
        request_error: &'static str,
    ) -> Result<Response, Status> {
        let (mut url, client) = self.client_for(value)?;
        let deadline = tokio::time::Instant::now() + REQUEST_TIMEOUT;
        let mut remaining = METADATA_REDIRECT_LIMIT;
        loop {
            let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
            if timeout.is_zero() {
                return Err(Status::unavailable(request_error));
            }
            let response = client
                .get(url.clone())
                .timeout(timeout)
                .send()
                .await
                .map_err(|_| Status::unavailable(request_error))?;
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
            if remaining == 0 {
                return Err(Status::failed_precondition(
                    "OAuth metadata redirected too many times",
                ));
            }
            let mut locations = response.headers().get_all(LOCATION).iter();
            url = locations
                .next()
                .and_then(|value| value.to_str().ok())
                .filter(|location| !location.is_empty() && locations.next().is_none())
                .and_then(|location| self.metadata_redirect_target(&url, location))
                .ok_or_else(|| {
                    Status::failed_precondition("OAuth metadata redirect URL is invalid")
                })?;
            remaining -= 1;
        }
    }

    fn metadata_redirect_target(self, previous: &Url, location: &str) -> Option<Url> {
        let target = previous.join(location).ok()?;
        if previous.origin() != target.origin() {
            return None;
        }
        self.validate_url(target.as_str()).ok()
    }

    pub(super) fn client_builder() -> reqwest::ClientBuilder {
        crate::outbound_http::client_builder()
            .referer(false)
            .connect_timeout(Duration::from_secs(5))
            .timeout(REQUEST_TIMEOUT)
    }

    pub(super) fn validate_url(self, value: &str) -> Result<Url, Status> {
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
        if url.scheme() != "https"
            && !(self.allow_loopback_http
                && url.scheme() == "http"
                && match url.host() {
                    Some(url::Host::Ipv4(address)) => address.is_loopback(),
                    Some(url::Host::Ipv6(address)) => address.is_loopback(),
                    Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
                    None => false,
                })
        {
            return Err(Status::failed_precondition("OAuth endpoints require HTTPS"));
        }
        Ok(url)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_endpoint_url_shape_is_checked_before_a_request() {
        let policy = OutboundHttpPolicy::production();
        for endpoint in [
            "https://127.0.0.1/token",
            "https://10.0.0.1/token",
            "https://issuer.example.invalid/token?tenant=one",
        ] {
            assert!(policy.client_for(endpoint).is_ok(), "{endpoint}");
        }
        for endpoint in [
            "http://127.0.0.1/token",
            "https://name:password@issuer.example/token",
            "https://issuer.example/token#fragment",
        ] {
            assert!(policy.client_for(endpoint).is_err(), "{endpoint}");
        }
    }

    #[test]
    fn metadata_redirects_stay_on_the_same_origin() {
        let policy = OutboundHttpPolicy::loopback_test();
        let https = Url::parse("https://127.0.0.1/metadata").unwrap();
        for location in [
            "http://127.0.0.1/metadata",
            "https://localhost/metadata",
            "https://127.0.0.1:8443/metadata",
        ] {
            assert!(policy.validate_url(location).is_ok());
            assert!(policy.metadata_redirect_target(&https, location).is_none());
        }
        assert!(
            policy
                .metadata_redirect_target(&https, "https://127.0.0.1:443/other")
                .is_some()
        );
    }

    #[tokio::test]
    async fn only_metadata_gets_follow_clean_bounded_redirects() {
        use axum::{Router, extract::Path, response::Redirect, routing::get};

        let app = Router::new()
            .route(
                "/step/{step}",
                get(|Path(step): Path<usize>| async move {
                    Redirect::temporary(&format!("/step/{}", step + 1))
                }),
            )
            .route(
                "/metadata",
                get(|headers: axum::http::HeaderMap| async move {
                    assert!(headers.get("referer").is_none());
                    assert!(headers.get("authorization").is_none());
                    "metadata"
                }),
            )
            .route("/clean", get(|| async { Redirect::temporary("/metadata") }))
            .route(
                "/unclean",
                get(|| async { Redirect::temporary("/metadata#fragment") }),
            )
            .route(
                "/effect",
                axum::routing::post(|| async { Redirect::temporary("/metadata") }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let policy = OutboundHttpPolicy::loopback_test();

        assert_eq!(
            policy
                .get_metadata(&format!("{base}/clean"), "test request failed")
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "metadata"
        );

        assert_eq!(
            policy
                .get_metadata(&format!("{base}/unclean"), "test request failed")
                .await
                .unwrap_err()
                .message(),
            "OAuth metadata redirect URL is invalid"
        );
        assert_eq!(
            policy
                .get_metadata(&format!("{base}/step/0"), "test request failed")
                .await
                .unwrap_err()
                .message(),
            "OAuth metadata redirected too many times"
        );

        let (url, client) = policy.client_for(&format!("{base}/effect")).unwrap();
        assert!(
            client
                .post(url)
                .send()
                .await
                .unwrap()
                .status()
                .is_redirection()
        );
        server.abort();
    }
}
