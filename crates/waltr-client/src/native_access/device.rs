use super::{
    Client, Duration, LOGIN_TIMEOUT, LoginFailure, LoginTokens, MAX_TOKEN_BYTES,
    NativeAccessConfig, NativeAccessOutput, NativeAccessProgress, NativeAuthError,
    OAuthErrorResponse, ReadBodyError, TokenResponse, Url, bounded_body, decode_token_response,
    discover, http_client, login_tokens, parse_secure_url, requested_scopes, send_progress,
};
use serde::Deserialize;
use tokio::{sync::mpsc, time};

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const INTERVAL_STEP: Duration = Duration::from_secs(5);
const MAX_DISPLAY_URI_BYTES: usize = 2048;
const MAX_USER_CODE_BYTES: usize = 64;
// RFC 8628 §5.2 calls for high-entropy device codes. This local length floor
// keeps substring checks from mistaking ordinary display text for a tiny code.
const MIN_DEVICE_CODE_BYTES: usize = 16;
const MAX_DEVICE_CODE_BYTES: usize = 4096;
const EXPIRED: &str = "Waltr QR sign-in expired; start a new login";

#[derive(Deserialize)]
struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}

impl DeviceAuthorization {
    fn validate(&self) -> Result<Duration, NativeAuthError> {
        if !(MIN_DEVICE_CODE_BYTES..=MAX_DEVICE_CODE_BYTES).contains(&self.device_code.len())
            || !self.device_code.bytes().all(|byte| byte.is_ascii_graphic())
            || self.user_code.is_empty()
            || self.user_code.len() > MAX_USER_CODE_BYTES
            || self.user_code.trim() != self.user_code
            || !self
                .user_code
                .bytes()
                .all(|byte| byte == b' ' || byte.is_ascii_graphic())
            || self.user_code.contains(&self.device_code)
            || self.expires_in == 0
            || self.interval == Some(0)
        {
            return Err(NativeAuthError(
                "authorization server returned invalid device login details".to_owned(),
            ));
        }
        Ok(self.interval.map_or(INTERVAL_STEP, Duration::from_secs))
    }

    fn into_progress(
        self,
        authority: &Url,
        deadline: time::Instant,
    ) -> Result<(NativeAccessProgress, String), NativeAuthError> {
        let verification_uri = private_code_free_url(&self.verification_uri, &self.device_code)?;
        let verification_uri_complete = self
            .verification_uri_complete
            .as_deref()
            .map(|value| private_code_free_url(value, &self.device_code))
            .transpose()?;
        let origin = authority.origin();
        if verification_uri.origin() != origin
            || verification_uri_complete
                .as_ref()
                .is_some_and(|complete| complete.origin() != origin)
        {
            return Err(NativeAuthError(
                "device verification URLs do not match the authorization server".to_owned(),
            ));
        }
        let timeout_millis = u64::try_from(
            deadline
                .saturating_duration_since(time::Instant::now())
                .as_millis(),
        )
        .expect("login timeout fits in milliseconds");
        if timeout_millis == 0 {
            return Err(NativeAuthError(EXPIRED.to_owned()));
        }
        Ok((
            NativeAccessProgress::AwaitingDevice {
                verification_uri: verification_uri.into(),
                verification_uri_complete: verification_uri_complete.map(Into::into),
                user_code: self.user_code,
                timeout_millis,
            },
            self.device_code,
        ))
    }
}

pub(super) async fn login(
    config: NativeAccessConfig,
    outputs: mpsc::Sender<NativeAccessOutput>,
) -> Result<LoginTokens, LoginFailure> {
    if config.client_id.trim().is_empty() {
        return Err(NativeAuthError("native OIDC client ID is required".to_owned()).into());
    }
    let client = http_client()?;
    send_progress(&outputs, NativeAccessProgress::Discovering).await;
    let provider = discover(&client, &config.endpoint).await?;
    let metadata = provider
        .device_authorization_endpoint
        .as_ref()
        .ok_or(LoginFailure::DeviceUnsupported)?;
    let endpoint = metadata.as_str().ok_or_else(|| {
        NativeAuthError("device authorization endpoint is not a valid URL".to_owned())
    })?;
    let endpoint = parse_secure_url(endpoint, "device authorization endpoint")?;
    let scopes = requested_scopes(&provider, &config)?.join(" ");
    let mut form = vec![
        ("client_id", config.client_id.as_str()),
        ("scope", scopes.as_str()),
        ("resource", provider.resource.as_str()),
    ];
    if let Some(audience) = &config.audience {
        form.push(("audience", audience));
    }
    let started = time::Instant::now();
    let authorization = authorize_device(&client, endpoint, &form).await?;
    let deadline = started + LOGIN_TIMEOUT.min(Duration::from_secs(authorization.expires_in));
    let interval = authorization.validate()?;
    let (progress, device_code) =
        authorization.into_progress(&provider.authorization_endpoint, deadline)?;

    time::timeout_at(deadline, async {
        send_progress(&outputs, progress).await;
        poll(
            &client,
            &provider.token_endpoint,
            &config.client_id,
            &device_code,
            interval,
            deadline,
        )
        .await
    })
    .await
    .map_err(|_| NativeAuthError(EXPIRED.to_owned()))?
}

async fn authorize_device(
    client: &Client,
    endpoint: Url,
    form: &[(&str, &str)],
) -> Result<DeviceAuthorization, LoginFailure> {
    let response = client
        .post(endpoint)
        .form(form)
        .send()
        .await
        .map_err(|_| NativeAuthError("device authorization request failed".to_owned()))?;
    let status = response.status();
    let bytes = bounded_body(response, MAX_TOKEN_BYTES)
        .await
        .map_err(NativeAuthError::from)?;
    if !status.is_success() {
        if status.is_client_error()
            && serde_json::from_slice::<OAuthErrorResponse>(&bytes).is_ok_and(|error| {
                matches!(
                    error.error.as_str(),
                    "unsupported_grant_type" | "unauthorized_client" | "invalid_client"
                )
            })
        {
            return Err(LoginFailure::DeviceUnsupported);
        }
        return Err(NativeAuthError(
            "authorization server rejected the device authorization request".to_owned(),
        )
        .into());
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| NativeAuthError("device authorization response was invalid".to_owned()).into())
}

fn verification_url(value: &str) -> Result<Url, NativeAuthError> {
    let invalid = || NativeAuthError("device verification URL is invalid or oversized".to_owned());
    if value.len() > MAX_DISPLAY_URI_BYTES
        || value.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(invalid());
    }
    let url = parse_secure_url(value, "device verification URL")?;
    if url.as_str().len() > MAX_DISPLAY_URI_BYTES {
        return Err(invalid());
    }
    Ok(url)
}

fn private_code_free_url(value: &str, device_code: &str) -> Result<Url, NativeAuthError> {
    let url = verification_url(value)?;
    // Check both the supplied and normalized URL. Decode one percent-encoding
    // layer, so code in a path or query cannot be exposed through escaping.
    if [value, url.as_str()].iter().any(|uri| {
        uri.contains(device_code)
            || percent_decoded_contains(uri.as_bytes(), device_code.as_bytes())
    }) {
        return Err(NativeAuthError(
            "device verification URL contains private login data".to_owned(),
        ));
    }
    Ok(url)
}

fn percent_decoded_contains(mut input: &[u8], needle: &[u8]) -> bool {
    if !input.contains(&b'%') {
        return false;
    }
    let hex = |byte| char::from(byte).to_digit(16);
    let mut decoded = Vec::with_capacity(input.len());
    while let [byte, rest @ ..] = input {
        if let [b'%', high, low, tail @ ..] = input
            && let (Some(high), Some(low)) = (hex(*high), hex(*low))
            && let Ok(byte) = u8::try_from(high * 16 + low)
        {
            decoded.push(byte);
            input = tail;
        } else {
            decoded.push(*byte);
            input = rest;
        }
    }
    decoded.windows(needle.len()).any(|window| window == needle)
}

enum PollResult {
    Pending,
    SlowDown,
    RetryConnection,
    Authorized(TokenResponse),
}

impl PollResult {
    fn next_interval(&self, interval: Duration) -> Duration {
        match self {
            Self::SlowDown => interval.saturating_add(INTERVAL_STEP),
            Self::RetryConnection => interval.saturating_mul(2),
            Self::Pending | Self::Authorized(_) => interval,
        }
    }
}

async fn poll(
    client: &Client,
    endpoint: &Url,
    client_id: &str,
    device_code: &str,
    mut interval: Duration,
    deadline: time::Instant,
) -> Result<LoginTokens, LoginFailure> {
    loop {
        let now = time::Instant::now();
        let wait = interval.min(deadline.saturating_duration_since(now));
        time::sleep_until(now + wait).await;
        if time::Instant::now() >= deadline {
            return Err(NativeAuthError(EXPIRED.to_owned()).into());
        }
        let result = poll_once(client, endpoint, client_id, device_code).await?;
        if time::Instant::now() >= deadline {
            return Err(NativeAuthError(EXPIRED.to_owned()).into());
        }
        if let PollResult::Authorized(token) = result {
            return login_tokens(token, endpoint, client_id).map_err(LoginFailure::from);
        }
        interval = result.next_interval(interval);
    }
}

async fn poll_once(
    client: &Client,
    endpoint: &Url,
    client_id: &str,
    device_code: &str,
) -> Result<PollResult, LoginFailure> {
    let response = match client
        .post(endpoint.clone())
        .form(&[
            ("grant_type", DEVICE_GRANT),
            ("client_id", client_id),
            ("device_code", device_code),
        ])
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) if error.is_timeout() || error.is_connect() => {
            return Ok(PollResult::RetryConnection);
        }
        Err(_) => return Err(NativeAuthError("device token request failed".to_owned()).into()),
    };
    let status = response.status();
    let bytes = match bounded_body(response, MAX_TOKEN_BYTES).await {
        Ok(bytes) => bytes,
        Err(ReadBodyError::Transport(error)) if error.is_timeout() => {
            return Ok(PollResult::RetryConnection);
        }
        Err(error) => return Err(NativeAuthError::from(error).into()),
    };
    if status.is_success() {
        return Ok(PollResult::Authorized(decode_token_response(&bytes)?));
    }
    if status.is_client_error()
        && let Ok(error) = serde_json::from_slice::<OAuthErrorResponse>(&bytes)
    {
        return match error.error.as_str() {
            "authorization_pending" => Ok(PollResult::Pending),
            "slow_down" => Ok(PollResult::SlowDown),
            "access_denied" => {
                Err(NativeAuthError("Waltr QR sign-in was denied".to_owned()).into())
            }
            "expired_token" => Err(NativeAuthError(EXPIRED.to_owned()).into()),
            "unsupported_grant_type" => Err(LoginFailure::DeviceUnsupported),
            _ => Err(
                NativeAuthError("authorization server rejected the device login".to_owned()).into(),
            ),
        };
    }
    Err(NativeAuthError("authorization server rejected the device login".to_owned()).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device_authorization() -> DeviceAuthorization {
        DeviceAuthorization {
            device_code: "private-device-code".to_owned(),
            user_code: "CODE-123".to_owned(),
            verification_uri: "https://id.example/activate".to_owned(),
            verification_uri_complete: None,
            expires_in: 60,
            interval: None,
        }
    }

    #[test]
    fn defaults_poll_interval_and_retains_backoff_for_later_pending_responses() {
        let interval = device_authorization().validate().unwrap();
        assert_eq!(interval, Duration::from_secs(5));
        let interval = PollResult::SlowDown.next_interval(interval);
        assert_eq!(interval, Duration::from_secs(10));
        let interval = PollResult::RetryConnection.next_interval(interval);
        assert_eq!(interval, Duration::from_secs(20));
        assert_eq!(PollResult::Pending.next_interval(interval), interval);
        assert_eq!(
            PollResult::RetryConnection.next_interval(Duration::MAX),
            Duration::MAX
        );
    }

    #[test]
    fn display_urls_are_bounded_and_safe_to_open() {
        for url in [
            "http://id.example/activate",
            "http://127.0.0.2/activate",
            "javascript:alert(1)",
            "https://name:secret@id.example/activate",
            "https://id.example/activate#fragment",
            "https://id.example/act\nivate",
        ] {
            assert!(verification_url(url).is_err(), "{url}");
        }
        assert!(verification_url("http://127.0.0.1/activate").is_ok());
        assert!(verification_url("http://[::1]/activate").is_ok());
        assert!(
            verification_url(&format!(
                "https://id.example/{}",
                "x".repeat(MAX_DISPLAY_URI_BYTES)
            ))
            .is_err()
        );

        let authority = Url::parse("https://id.example/authorize").unwrap();
        let deadline = time::Instant::now() + Duration::from_secs(5);
        let authorization = device_authorization();
        assert!(matches!(
            authorization.into_progress(&authority, deadline).unwrap().0,
            NativeAccessProgress::AwaitingDevice {
                verification_uri_complete: None,
                ..
            }
        ));
        let mut authorization = device_authorization();
        authorization.verification_uri_complete = Some("https://other.example/activate".to_owned());
        assert!(authorization.into_progress(&authority, deadline).is_err());
        let mut authorization = device_authorization();
        authorization.verification_uri = "https://other.example/activate".to_owned();
        assert!(authorization.into_progress(&authority, deadline).is_err());
    }

    #[test]
    fn rejects_unusable_display_details_and_poll_timings() {
        for code in [
            "".to_owned(),
            "code\u{1b}[1m".to_owned(),
            "private-device-code".to_owned(),
            "prefix-private-device-code-suffix".to_owned(),
            "X".repeat(MAX_USER_CODE_BYTES + 1),
        ] {
            let mut authorization = device_authorization();
            authorization.user_code = code;
            assert!(authorization.validate().is_err());
        }
        let mut authorization = device_authorization();
        authorization.device_code = "X".repeat(MIN_DEVICE_CODE_BYTES - 1);
        assert!(authorization.validate().is_err());
        authorization.device_code.push('X');
        assert!(authorization.validate().is_ok());
        let mut authorization = device_authorization();
        authorization.interval = Some(0);
        assert!(authorization.validate().is_err());
        authorization.interval = Some(1);
        authorization.expires_in = 0;
        assert!(authorization.validate().is_err());
    }

    #[test]
    fn rejects_literal_or_percent_encoded_private_code_in_either_verification_url() {
        let authority = Url::parse("https://id.example/authorize").unwrap();
        let deadline = time::Instant::now() + Duration::from_secs(5);
        for uri in [
            "https://id.example/activate/prefix-private-device-code-suffix",
            "https://id.example/activate?prefix-private-device-code=1",
            "https://id.example/activate/prefix-%70rivate%2ddevice-code-suffix",
            "https://id.example/activate?code=prefix-%70rivate-device%2Dcode-suffix",
        ] {
            for complete in [false, true] {
                let mut authorization = device_authorization();
                if complete {
                    authorization.verification_uri_complete = Some(uri.to_owned());
                } else {
                    authorization.verification_uri = uri.to_owned();
                }
                assert_eq!(
                    authorization
                        .into_progress(&authority, deadline)
                        .err()
                        .expect("private code must be rejected")
                        .to_string(),
                    "device verification URL contains private login data",
                );
            }
        }
    }

    #[test]
    fn accepts_authentik_style_verification_url_with_user_code() {
        let authority = Url::parse("https://id.example/application/o/authorize/").unwrap();
        let deadline = time::Instant::now() + Duration::from_secs(5);
        let mut authorization = device_authorization();
        authorization.verification_uri = "https://id.example/device".to_owned();
        authorization.verification_uri_complete =
            Some("https://id.example/device?code=CODE-123".to_owned());
        assert!(matches!(
            authorization.into_progress(&authority, deadline).unwrap().0,
            NativeAccessProgress::AwaitingDevice {
                verification_uri_complete: Some(url),
                user_code,
                ..
            } if url.ends_with("?code=CODE-123") && user_code == "CODE-123"
        ));
    }
}
