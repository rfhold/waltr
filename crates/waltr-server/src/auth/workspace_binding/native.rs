use std::time::{Duration, SystemTime};

use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, de::DeserializeOwned};
use tonic::Status;
use url::{Position, Url};

use crate::access_auth::{AccessAuthHttpClient, OidcVerifier};

use super::super::{
    OAuthTokenSet, WorkspaceBindingOAuthContext,
    outbound_http::{
        OAUTH_RESPONSE_LIMIT, OutboundHttpPolicy, read_json_body, validate_response_peer,
    },
};

pub(super) const STEP: Duration = Duration::from_secs(5);
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

#[derive(Deserialize)]
struct ProtectedResource {
    resource: String,
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
    waltr_native_client: NativeClient,
}

#[derive(Deserialize)]
struct NativeClient {
    client_id: String,
    scopes: Vec<String>,
    audience: String,
}

#[derive(Deserialize)]
struct IssuerMetadata {
    issuer: String,
    authorization_endpoint: String,
    device_authorization_endpoint: Option<String>,
    token_endpoint: String,
    jwks_uri: String,
    access_token_signing_alg_values_supported: Option<Vec<String>>,
    grant_types_supported: Option<Vec<String>>,
}

pub(super) struct DeviceStart {
    pub(super) context: WorkspaceBindingOAuthContext,
    pub(super) device_code: String,
    pub(super) user_code: String,
    pub(super) verification_url: Url,
    pub(super) lifetime: Duration,
    pub(super) interval: Duration,
}

#[derive(Deserialize)]
struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}

pub(super) async fn begin(
    policy: &OutboundHttpPolicy,
    endpoint: &Url,
    binding_fingerprint: String,
    publication_incarnation: Option<String>,
) -> Result<DeviceStart, Status> {
    let mut url = endpoint.clone();
    url.set_path("/.well-known/oauth-protected-resource");
    let protected: ProtectedResource = successful(get(policy, url.as_str()).await?).await?;
    let [issuer] = protected.authorization_servers.as_slice() else {
        return Err(Status::failed_precondition(
            "destination must advertise exactly one native API issuer",
        ));
    };
    if policy.validate_url(&protected.resource)? != *endpoint {
        return Err(Status::failed_precondition(
            "protected resource does not match the workspace binding API root",
        ));
    }
    let native = protected.waltr_native_client;
    if !valid_public_field(&native.client_id)
        || !valid_public_field(&native.audience)
        || native.scopes.is_empty()
        || native.scopes.iter().any(|scope| !valid_scope(scope))
        || protected
            .scopes_supported
            .iter()
            .any(|scope| !valid_scope(scope) || !native.scopes.contains(scope))
    {
        return Err(Status::failed_precondition(
            "destination native API OAuth metadata is invalid",
        ));
    }
    let issuer_url = policy.validate_url(issuer)?;
    if issuer_url.query().is_some() {
        return Err(Status::failed_precondition(
            "destination native API issuer is invalid",
        ));
    }
    let metadata = discover_issuer(policy, &issuer_url).await?;
    if metadata.issuer != *issuer {
        return Err(Status::failed_precondition(
            "destination native API issuer does not match discovery",
        ));
    }
    if metadata
        .grant_types_supported
        .as_ref()
        .is_some_and(|grants| !grants.iter().any(|grant| grant == DEVICE_GRANT))
    {
        return Err(device_unsupported());
    }
    let authorization = policy.validate_url(&metadata.authorization_endpoint)?;
    let device_endpoint = metadata
        .device_authorization_endpoint
        .ok_or_else(device_unsupported)?;
    let token_endpoint = policy
        .validate_destination(&metadata.token_endpoint)
        .await?;
    let jwks_uri = policy.validate_destination(&metadata.jwks_uri).await?;
    let scope = native.scopes.join(" ");
    let (device_url, http) = policy.client_for(&device_endpoint).await?;
    let response = http
        .post(device_url)
        .form(&[
            ("client_id", native.client_id.as_str()),
            ("scope", &scope),
            ("resource", &protected.resource),
            ("audience", &native.audience),
        ])
        .send()
        .await
        .map_err(|_| Status::unavailable("device authorization request failed"))?;
    validate_response_peer(policy, &response)?;
    if !response.status().is_success() {
        let error = oauth_error(response).await?;
        return Err(match error.as_str() {
            "unsupported_grant_type" | "unauthorized_client" | "invalid_client" => {
                device_unsupported()
            }
            _ => Status::failed_precondition("issuer rejected device authorization"),
        });
    }
    let device: DeviceAuthorization = decode(response).await?;
    let verification_url = device.display_url(policy, &authorization)?;
    Ok(DeviceStart {
        context: WorkspaceBindingOAuthContext {
            endpoint: endpoint.clone(),
            binding_fingerprint,
            publication_incarnation,
            issuer: issuer.clone(),
            subject: String::new(),
            client_id: native.client_id,
            audience: native.audience,
            required_scopes: protected.scopes_supported,
            token_endpoint,
            jwks_uri,
            signing_algorithms: metadata.access_token_signing_alg_values_supported,
        },
        device_code: device.device_code,
        user_code: device.user_code,
        verification_url,
        lifetime: Duration::from_secs(device.expires_in),
        interval: device.interval.map_or(STEP, Duration::from_secs),
    })
}

fn device_unsupported() -> Status {
    Status::failed_precondition("destination native API does not support device authorization")
}

fn valid_public_field(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn valid_scope(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| matches!(byte, b'!' | b'#'..=b'[' | b']'..=b'~'))
}

async fn discover_issuer(
    policy: &OutboundHttpPolicy,
    issuer: &Url,
) -> Result<IssuerMetadata, Status> {
    let primary = format!(
        "{}/.well-known/openid-configuration",
        issuer.as_str().trim_end_matches('/')
    );
    let response = get(policy, &primary).await?;
    if response.status() == StatusCode::NOT_FOUND && issuer.path() != "/" {
        successful(
            get(
                policy,
                &format!(
                    "{}/.well-known/openid-configuration{}",
                    &issuer[..Position::BeforePath],
                    issuer.path(),
                ),
            )
            .await?,
        )
        .await
    } else {
        successful(response).await
    }
}

impl DeviceAuthorization {
    fn display_url(&self, policy: &OutboundHttpPolicy, authority: &Url) -> Result<Url, Status> {
        let invalid = || Status::data_loss("issuer returned invalid device authorization details");
        if !(16..=4096).contains(&self.device_code.len())
            || !self.device_code.bytes().all(|byte| byte.is_ascii_graphic())
            || !(1..=64).contains(&self.user_code.len())
            || self.user_code.trim() != self.user_code
            || !self
                .user_code
                .bytes()
                .all(|byte| byte == b' ' || byte.is_ascii_graphic())
            || self.user_code.contains(&self.device_code)
            || self.expires_in == 0
            || self.interval == Some(0)
        {
            return Err(invalid());
        }
        let parse = |raw: &str| {
            if raw.len() > 2048 || raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
                return Err(invalid());
            }
            let url = policy.validate_url(raw).map_err(|_| invalid())?;
            if url.as_str().len() > 2048
                || url.origin() != authority.origin()
                || [raw, url.as_str()].iter().any(|value| {
                    value.contains(&self.device_code) || decoded_contains(value, &self.device_code)
                })
            {
                return Err(invalid());
            }
            Ok(url)
        };
        let url = parse(&self.verification_uri)?;
        if let Some(complete) = &self.verification_uri_complete {
            parse(complete)?;
        }
        // The existing Config UI displays the user code separately.
        Ok(url)
    }
}

fn decoded_contains(value: &str, private: &str) -> bool {
    let mut remaining = value.as_bytes();
    let mut decoded = Vec::with_capacity(remaining.len());
    while let Some((&byte, rest)) = remaining.split_first() {
        if byte == b'%'
            && let [high, low, tail @ ..] = rest
            && let (Some(high), Some(low)) = (
                char::from(*high).to_digit(16),
                char::from(*low).to_digit(16),
            )
        {
            decoded.push(u8::try_from(high * 16 + low).expect("hex pair fits in a byte"));
            remaining = tail;
        } else {
            decoded.push(byte);
            remaining = rest;
        }
    }
    decoded
        .windows(private.len())
        .any(|window| window == private.as_bytes())
}

pub(super) enum DevicePoll {
    Authorized(TokenResponse),
    Pending,
    SlowDown,
    Timeout,
    Denied,
    Expired,
}

pub(super) async fn poll(
    policy: &OutboundHttpPolicy,
    context: &WorkspaceBindingOAuthContext,
    device_code: &str,
) -> Result<DevicePoll, Status> {
    let (url, client) = policy.client_for(context.token_endpoint.as_str()).await?;
    let response = match client
        .post(url)
        .form(&[
            ("grant_type", DEVICE_GRANT),
            ("client_id", &context.client_id),
            ("device_code", device_code),
        ])
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) if error.is_timeout() || error.is_connect() => return Ok(DevicePoll::Timeout),
        Err(_) => return Err(Status::unavailable("device token request failed")),
    };
    validate_response_peer(policy, &response)?;
    let status = response.status();
    if status.is_success() {
        return decode(response).await.map(DevicePoll::Authorized);
    }
    let error = oauth_error(response).await?;
    match (status, error.as_str()) {
        (StatusCode::BAD_REQUEST, "authorization_pending") => Ok(DevicePoll::Pending),
        (StatusCode::BAD_REQUEST | StatusCode::TOO_MANY_REQUESTS, "slow_down") => {
            Ok(DevicePoll::SlowDown)
        }
        (StatusCode::BAD_REQUEST | StatusCode::FORBIDDEN, "access_denied") => {
            Ok(DevicePoll::Denied)
        }
        (StatusCode::BAD_REQUEST, "expired_token") => Ok(DevicePoll::Expired),
        _ => Err(Status::failed_precondition(
            "issuer rejected device token request",
        )),
    }
}

pub(super) struct RefreshRequest {
    client: Client,
    url: Url,
    verifier: OidcVerifier,
}

impl RefreshRequest {
    pub(super) async fn prepare(
        policy: &OutboundHttpPolicy,
        context: &WorkspaceBindingOAuthContext,
    ) -> Result<Self, Status> {
        let (url, client) = policy.client_for(context.token_endpoint.as_str()).await?;
        let verifier = verifier(policy, context).await?;
        Ok(Self {
            client,
            url,
            verifier,
        })
    }

    pub(super) async fn exchange(
        self,
        policy: &OutboundHttpPolicy,
        context: &WorkspaceBindingOAuthContext,
        refresh_token: &str,
    ) -> Result<OAuthTokenSet, Status> {
        let response = self
            .client
            .post(self.url)
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", &context.client_id),
                ("refresh_token", refresh_token),
            ])
            .send()
            .await
            .map_err(|_| Status::unavailable("workspace binding refresh outcome is unknown"))?;
        validate_response_peer(policy, &response)?;
        if !response.status().is_success() {
            return Err(Status::unauthenticated(
                "workspace binding refresh was rejected",
            ));
        }
        let token: TokenResponse = decode(response).await?;
        let (_, token) = token
            .validate(&self.verifier, context, Some(refresh_token))
            .await?;
        Ok(token)
    }
}

#[derive(Deserialize)]
pub(super) struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    token_type: String,
    expires_in: Option<u64>,
}

impl TokenResponse {
    pub(super) async fn validate_initial(
        self,
        policy: &OutboundHttpPolicy,
        context: &mut WorkspaceBindingOAuthContext,
    ) -> Result<OAuthTokenSet, Status> {
        let verifier = verifier(policy, context).await?;
        let (subject, token) = self.validate(&verifier, context, None).await?;
        context.subject = subject;
        Ok(token)
    }

    async fn validate(
        self,
        verifier: &OidcVerifier,
        context: &WorkspaceBindingOAuthContext,
        prior_refresh: Option<&str>,
    ) -> Result<(String, OAuthTokenSet), Status> {
        let invalid =
            || Status::unauthenticated("issuer returned an invalid native API bearer token");
        if !self.token_type.eq_ignore_ascii_case("bearer")
            || self.access_token.is_empty()
            || !self
                .access_token
                .bytes()
                .all(|byte| byte.is_ascii_graphic())
            || self.refresh_token.as_ref().is_some_and(|token| {
                token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_graphic())
            })
            || self.expires_in == Some(0)
        {
            return Err(invalid());
        }
        let received_at = SystemTime::now();
        let principal = verifier
            .validate_access_token(&self.access_token, &context.required_scopes)
            .await
            .map_err(|_| invalid())?;
        if principal.issuer() != context.issuer
            || (!context.subject.is_empty() && principal.subject() != context.subject)
        {
            return Err(Status::unauthenticated(
                "workspace binding OAuth account changed",
            ));
        }
        let jwt_expiry = principal.bearer_expires_at().ok_or_else(invalid)?;
        let expiry = self
            .expires_in
            .and_then(|seconds| received_at.checked_add(Duration::from_secs(seconds)))
            .map_or(jwt_expiry, |advertised| advertised.min(jwt_expiry));
        if expiry <= SystemTime::now() {
            return Err(invalid());
        }
        let expires_at_ms = expiry
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_millis()).ok())
            .ok_or_else(invalid)?;
        Ok((
            principal.subject().to_owned(),
            OAuthTokenSet::bearer(
                self.access_token,
                self.refresh_token
                    .or_else(|| prior_refresh.map(ToOwned::to_owned)),
                Some(expires_at_ms),
            ),
        ))
    }
}

async fn verifier(
    policy: &OutboundHttpPolicy,
    context: &WorkspaceBindingOAuthContext,
) -> Result<OidcVerifier, Status> {
    let (url, client) = policy.client_for(context.jwks_uri.as_str()).await?;
    OidcVerifier::from_access_jwks(
        AccessAuthHttpClient::from_pinned_client(client),
        (url, context.signing_algorithms.clone()),
        &context.issuer,
        &context.audience,
        Some(&context.client_id),
    )
    .await
    .map_err(|_| Status::unavailable("workspace binding issuer signing keys are unavailable"))
}

async fn get(policy: &OutboundHttpPolicy, url: &str) -> Result<Response, Status> {
    let (url, client) = policy.client_for(url).await?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| Status::unavailable("native API OAuth discovery failed"))?;
    validate_response_peer(policy, &response)?;
    Ok(response)
}

async fn successful<T: DeserializeOwned>(response: Response) -> Result<T, Status> {
    if !response.status().is_success() {
        return Err(Status::failed_precondition(
            "native API OAuth metadata is unavailable",
        ));
    }
    decode(response).await
}

async fn decode<T: DeserializeOwned>(response: Response) -> Result<T, Status> {
    let body = read_json_body(response, OAUTH_RESPONSE_LIMIT).await?;
    serde_json::from_slice(&body)
        .map_err(|_| Status::data_loss("native API OAuth response is invalid"))
}

async fn oauth_error(response: Response) -> Result<String, Status> {
    #[derive(Deserialize)]
    struct OAuthError {
        error: String,
    }
    decode::<OAuthError>(response).await.map(|body| body.error)
}
