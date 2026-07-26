use std::collections::BTreeMap;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use waltr::grpc::waltr::{
    auth::v1::{BeginProviderAuthResponse, ProviderAuthMethod},
    config::v1::{
        OAuthProvider, OauthJwtMetadata, OauthTokenField, OauthTokenRequestStyle, Provider,
    },
};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use tonic::Status;
use uuid::Uuid;

use super::{
    AuthTarget, OAuthCredential,
    outbound_http::{
        OAUTH_RESPONSE_LIMIT, OutboundHttpPolicy, read_json_body, validate_response_peer,
    },
};

/// Resolved provider OAuth definition.
#[derive(Debug, Clone)]
pub(crate) struct ProviderOAuthDefinition {
    /// OAuth provider definition to use for login and refresh.
    pub definition: OAuthProvider,
}

/// Pending manual OAuth attempt.
#[derive(Clone)]
pub struct OAuthAttempt {
    /// Target this attempt belongs to.
    target: AuthTarget,
    definition: OAuthProvider,
    verifier: String,
    state: String,
    expires_at_ms: i64,
}

impl std::fmt::Debug for OAuthAttempt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthAttempt")
            .field("target", &self.target)
            .field("definition", &"<redacted>")
            .field("verifier", &"<redacted>")
            .field("state", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}
impl OAuthAttempt {
    /// Return the owning target.
    #[must_use]
    pub fn target(&self) -> &AuthTarget {
        &self.target
    }

    /// Return the OAuth definition used for the attempt.
    #[must_use]
    pub fn definition(&self) -> &OAuthProvider {
        &self.definition
    }

    /// Return the owning provider key when this is a provider attempt.
    #[must_use]
    pub fn provider_key(&self) -> Option<&str> {
        match &self.target {
            AuthTarget::Provider { key } => Some(key),
            AuthTarget::McpServer { .. } => None,
        }
    }
}

/// Return the active OAuth definition for a provider.
#[must_use]
pub(crate) fn provider_definition(
    _provider_key: &str,
    provider: &Provider,
) -> Option<ProviderOAuthDefinition> {
    if !provider.oauth_compatible {
        return None;
    }

    if let Some(settings) = &provider.oauth_settings {
        if !definition_is_empty(settings) {
            return Some(ProviderOAuthDefinition {
                definition: settings.clone(),
            });
        }
    }

    None
}

/// Return whether an OAuth definition is empty/defaulted.
#[must_use]
pub(crate) fn definition_is_empty(definition: &OAuthProvider) -> bool {
    definition.name.is_empty()
        && definition.authorize_url.is_empty()
        && definition.token_url.is_empty()
        && definition.client_id.is_empty()
        && definition.redirect_uri.is_empty()
        && definition.scopes.is_empty()
        && definition.token_request_style == OauthTokenRequestStyle::Unspecified as i32
        && definition.auth_url_params.is_empty()
        && definition.jwt_metadata.is_empty()
        && !definition.send_state_in_token_request
}

/// Build a manual OAuth authorization attempt from a provider definition.
pub fn begin_manual(
    target: AuthTarget,
    definition: OAuthProvider,
    now_ms: i64,
) -> Result<(OAuthAttempt, BeginProviderAuthResponse), Status> {
    validate_definition(&definition)?;
    let verifier = pkce_verifier();
    let challenge = pkce_challenge(&verifier);
    let state = Uuid::new_v4().to_string();
    let expires_at_ms = now_ms + 15 * 60 * 1000;
    let auth_url = authorize_url(&definition, &challenge, &state);
    let attempt = OAuthAttempt {
        target,
        definition,
        verifier,
        state,
        expires_at_ms,
    };
    let response = BeginProviderAuthResponse {
        method: ProviderAuthMethod::OauthManual as i32,
        auth_url,
        expires_at_ms,
        messages: vec![
            "Open the URL, approve access, then paste the final redirect URL including state."
                .to_owned(),
        ],
        ..BeginProviderAuthResponse::default()
    };
    Ok((attempt, response))
}

/// Exchange a manual OAuth callback input for a stored credential.
pub async fn complete_manual(
    attempt: &OAuthAttempt,
    input: &str,
    now_ms: i64,
) -> Result<OAuthCredential, Status> {
    complete_manual_with_policy(attempt, input, now_ms, OutboundHttpPolicy::production()).await
}

async fn complete_manual_with_policy(
    attempt: &OAuthAttempt,
    input: &str,
    now_ms: i64,
    policy: OutboundHttpPolicy,
) -> Result<OAuthCredential, Status> {
    if now_ms > attempt.expires_at_ms {
        return Err(Status::deadline_exceeded("OAuth attempt expired"));
    }
    let (code, state) = parse_manual_input(input);
    let code = code.ok_or_else(|| Status::invalid_argument("OAuth code is required"))?;
    if state.as_deref() != Some(attempt.state.as_str()) {
        return Err(Status::invalid_argument("OAuth state does not match"));
    }
    exchange_code(
        &attempt.definition,
        &code,
        &attempt.verifier,
        &attempt.state,
        policy,
    )
    .await
}

async fn exchange_code(
    definition: &OAuthProvider,
    code: &str,
    verifier: &str,
    state: &str,
    policy: OutboundHttpPolicy,
) -> Result<OAuthCredential, Status> {
    request_token(
        definition,
        TokenRequest::AuthorizationCode {
            code,
            verifier,
            state,
        },
        policy,
    )
    .await
}

enum TokenRequest<'a> {
    AuthorizationCode {
        code: &'a str,
        verifier: &'a str,
        state: &'a str,
    },
}

async fn request_token(
    definition: &OAuthProvider,
    token_request: TokenRequest<'_>,
    policy: OutboundHttpPolicy,
) -> Result<OAuthCredential, Status> {
    let pairs = token_request_pairs(definition, token_request);
    let (token_url, client) = policy.client_for(&definition.token_url).await?;
    let builder = client.post(token_url);
    let response = match request_style(definition)? {
        OauthTokenRequestStyle::Json => builder.json(&pairs_to_json(&pairs)).send().await,
        OauthTokenRequestStyle::FormUrlencoded => builder.form(&pairs).send().await,
        OauthTokenRequestStyle::Unspecified => unreachable!("validated request style"),
    }
    .map_err(|error| Status::unavailable(format!("OAuth token request failed: {error}")))?;
    validate_response_peer(policy, &response)?;
    token_response_to_credential(definition, response).await
}

fn token_request_pairs<'a>(
    definition: &'a OAuthProvider,
    token_request: TokenRequest<'a>,
) -> Vec<(&'a str, &'a str)> {
    match token_request {
        TokenRequest::AuthorizationCode {
            code,
            verifier,
            state,
        } => {
            let mut pairs = vec![
                ("grant_type", "authorization_code"),
                ("client_id", definition.client_id.as_str()),
                ("code", code),
                ("redirect_uri", definition.redirect_uri.as_str()),
                ("code_verifier", verifier),
            ];
            if definition.send_state_in_token_request {
                pairs.push(("state", state));
            }
            pairs
        }
    }
}

fn pairs_to_json(pairs: &[(&str, &str)]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (key, value) in pairs {
        map.insert(
            (*key).to_owned(),
            serde_json::Value::String((*value).to_owned()),
        );
    }
    serde_json::Value::Object(map)
}

fn authorize_url(definition: &OAuthProvider, challenge: &str, state: &str) -> String {
    let mut params = vec![
        ("response_type".to_owned(), "code".to_owned()),
        ("client_id".to_owned(), definition.client_id.clone()),
        ("redirect_uri".to_owned(), definition.redirect_uri.clone()),
        ("scope".to_owned(), definition.scopes.join(" ")),
        ("code_challenge".to_owned(), challenge.to_owned()),
        ("code_challenge_method".to_owned(), "S256".to_owned()),
        ("state".to_owned(), state.to_owned()),
    ];
    params.extend(
        definition
            .auth_url_params
            .iter()
            .map(|param| (param.key.clone(), param.value.clone())),
    );
    query_url(&definition.authorize_url, &params)
}

fn query_url(base: &str, params: &[(String, String)]) -> String {
    let query = params
        .iter()
        .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{base}?{query}")
}

fn pkce_verifier() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                vec![byte as char]
            }
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
}

fn parse_manual_input(input: &str) -> (Option<String>, Option<String>) {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return (None, None);
    }
    let query = trimmed
        .split_once('?')
        .map_or(trimmed, |(_, query)| query)
        .split_once('#')
        .map_or_else(
            || trimmed.split_once('?').map_or(trimmed, |(_, query)| query),
            |(query, fragment)| if query.contains('=') { query } else { fragment },
        );
    let mut code = None;
    let mut state = None;
    for part in query.split('&') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key {
            "code" => code = Some(percent_decode(value)),
            "state" => state = Some(percent_decode(value)),
            _ => {}
        }
    }
    if code.is_none() {
        if let Some((raw_code, raw_state)) = trimmed.split_once('#') {
            code = Some(raw_code.to_owned());
            state = Some(raw_state.to_owned());
        } else {
            code = Some(trimmed.to_owned());
        }
    }
    (code, state)
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &value[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(if bytes[index] == b'+' {
            b' '
        } else {
            bytes[index]
        });
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    id_token: Option<String>,
}

async fn token_response_to_credential(
    definition: &OAuthProvider,
    response: reqwest::Response,
) -> Result<OAuthCredential, Status> {
    let status = response.status();
    let text = read_json_body(response, OAUTH_RESPONSE_LIMIT).await?;
    if !status.is_success() {
        return Err(Status::unauthenticated(format!(
            "OAuth token endpoint returned {status}"
        )));
    }
    let token: TokenResponse = serde_json::from_slice(&text)
        .map_err(|error| Status::data_loss(format!("OAuth token response was invalid: {error}")))?;
    let metadata = extract_metadata(definition, &token)?;
    Ok(OAuthCredential {
        access: token.access_token,
        refresh: token.refresh_token.unwrap_or_default(),
        expires_at_ms: credential_expiry_ms(now_ms(), token.expires_in),
        metadata,
    })
}

fn credential_expiry_ms(now_ms: i64, expires_in: Option<i64>) -> i64 {
    now_ms
        .saturating_add(expires_in.unwrap_or(3600).saturating_mul(1000))
        .saturating_sub(5 * 60 * 1000)
}

fn extract_metadata(
    definition: &OAuthProvider,
    token: &TokenResponse,
) -> Result<BTreeMap<String, String>, Status> {
    let mut metadata = BTreeMap::new();
    for extractor in &definition.jwt_metadata {
        let value = extract_jwt_metadata(extractor, token);
        match value {
            Some(value) => {
                metadata.insert(extractor.metadata_key.clone(), value);
            }
            None if extractor.required => {
                return Err(Status::unauthenticated(format!(
                    "OAuth token response missing required metadata '{}'",
                    extractor.metadata_key
                )));
            }
            None => {}
        }
    }
    Ok(metadata)
}

fn extract_jwt_metadata(extractor: &OauthJwtMetadata, token: &TokenResponse) -> Option<String> {
    let jwt = match OauthTokenField::try_from(extractor.token).ok()? {
        OauthTokenField::Access => Some(token.access_token.as_str()),
        OauthTokenField::Id => token.id_token.as_deref(),
        OauthTokenField::Unspecified => None,
    }?;
    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    extractor
        .claim_names
        .iter()
        .find_map(|claim| jwt_claim_string(&value, claim))
        .map(ToOwned::to_owned)
}

fn jwt_claim_string<'a>(value: &'a serde_json::Value, claim: &str) -> Option<&'a str> {
    if let Some(value) = value.get(claim).and_then(|value| value.as_str()) {
        return Some(value);
    }

    claim.match_indices('.').rev().find_map(|(index, _)| {
        let (prefix, suffix) = claim.split_at(index);
        let suffix = suffix.trim_start_matches('.');
        let nested = value.get(prefix)?;
        claim_path_string(nested, suffix)
    })
}

fn claim_path_string<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a str> {
    let mut current = value;
    for segment in path.split('.') {
        current = current.get(segment)?;
    }
    current.as_str()
}

fn validate_definition(definition: &OAuthProvider) -> Result<(), Status> {
    if definition.authorize_url.trim().is_empty()
        || definition.token_url.trim().is_empty()
        || definition.client_id.trim().is_empty()
        || definition.redirect_uri.trim().is_empty()
    {
        return Err(Status::failed_precondition(
            "OAuth provider definition is incomplete",
        ));
    }
    request_style(definition)?;
    validate_front_channel_url(&definition.authorize_url)?;
    validate_front_channel_url(&definition.token_url)?;
    Ok(())
}

fn validate_front_channel_url(value: &str) -> Result<(), Status> {
    let url = url::Url::parse(value)
        .map_err(|_| Status::failed_precondition("OAuth endpoint URL is invalid"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(Status::failed_precondition(
            "OAuth endpoints must be absolute HTTPS URLs without credentials or fragments",
        ));
    }
    Ok(())
}

fn request_style(definition: &OAuthProvider) -> Result<OauthTokenRequestStyle, Status> {
    match OauthTokenRequestStyle::try_from(definition.token_request_style) {
        Ok(OauthTokenRequestStyle::Json) => Ok(OauthTokenRequestStyle::Json),
        Ok(OauthTokenRequestStyle::FormUrlencoded) => Ok(OauthTokenRequestStyle::FormUrlencoded),
        Ok(OauthTokenRequestStyle::Unspecified) | Err(_) => Err(Status::failed_precondition(
            "OAuth provider token request style is unsupported",
        )),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use crate::config::defaults::openai_codex_oauth_settings;
    use axum::{
        Json, Router,
        http::{StatusCode, header},
        response::IntoResponse,
        routing::post,
    };
    use serde_json::json;

    async fn malicious_token_server(oversized: bool) -> (String, Arc<AtomicUsize>) {
        let redirected_requests = Arc::new(AtomicUsize::new(0));
        let redirected = Arc::clone(&redirected_requests);
        let app = if oversized {
            Router::new().route(
                "/token",
                post(|| async {
                    (
                        [(header::CONTENT_TYPE, "application/json")],
                        "x".repeat(OAUTH_RESPONSE_LIMIT + 1),
                    )
                }),
            )
        } else {
            Router::new()
                .route(
                    "/token",
                    post(|| async {
                        (
                            StatusCode::TEMPORARY_REDIRECT,
                            [(header::LOCATION, "/secret-sink")],
                        )
                            .into_response()
                    }),
                )
                .route(
                    "/secret-sink",
                    post(move || {
                        let redirected = Arc::clone(&redirected);
                        async move {
                            redirected.fetch_add(1, Ordering::SeqCst);
                            Json(json!({"access_token": "stolen"}))
                        }
                    }),
                )
        };
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}/token"), redirected_requests)
    }

    fn loopback_provider_attempt(token_url: String) -> OAuthAttempt {
        let mut definition = openai_codex_oauth_settings();
        definition.token_url = token_url;
        OAuthAttempt {
            target: AuthTarget::provider("openai-codex"),
            definition,
            verifier: "verifier".to_owned(),
            state: "state".to_owned(),
            expires_at_ms: 10_000,
        }
    }

    #[test]
    fn jwt_claim_string_reads_nested_url_claim_path() {
        let claims = json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "account-123"
            }
        });

        assert_eq!(
            jwt_claim_string(&claims, "https://api.openai.com/auth.chatgpt_account_id"),
            Some("account-123")
        );
    }

    #[test]
    fn codex_definition_extracts_account_id_from_id_token() {
        let payload = json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "account-123"
            }
        });
        let payload = URL_SAFE_NO_PAD.encode(payload.to_string());
        let token = TokenResponse {
            access_token: "not-a-jwt".to_owned(),
            refresh_token: None,
            expires_in: Some(3600),
            id_token: Some(format!("header.{payload}.signature")),
        };

        let metadata =
            extract_metadata(&openai_codex_oauth_settings(), &token).expect("metadata extracts");

        assert_eq!(metadata.get("account_id"), Some(&"account-123".to_owned()));
    }

    #[tokio::test]
    async fn provider_manual_completion_requires_state() {
        let (attempt, _) = begin_manual(
            AuthTarget::provider("openai-codex"),
            openai_codex_oauth_settings(),
            1_000,
        )
        .unwrap();

        let error = complete_manual(&attempt, "code=secret", 1_001)
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn provider_code_exchange_refuses_redirect_without_replaying_code() {
        let (token_url, redirected_requests) = malicious_token_server(false).await;
        let attempt = loopback_provider_attempt(token_url);
        let error = complete_manual_with_policy(
            &attempt,
            "code=code-secret&state=state",
            1_001,
            OutboundHttpPolicy::loopback_test(),
        )
        .await
        .unwrap_err();

        assert_eq!(error.code(), tonic::Code::DataLoss);
        assert_eq!(redirected_requests.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn provider_code_exchange_rejects_oversized_token_response() {
        let (token_url, _) = malicious_token_server(true).await;
        let attempt = loopback_provider_attempt(token_url);
        let error = complete_manual_with_policy(
            &attempt,
            "code=code-secret&state=state",
            1_001,
            OutboundHttpPolicy::loopback_test(),
        )
        .await
        .unwrap_err();

        assert_eq!(error.code(), tonic::Code::ResourceExhausted);
    }

    #[test]
    fn provider_expiry_arithmetic_saturates() {
        assert_eq!(
            credential_expiry_ms(i64::MAX, Some(i64::MAX)),
            i64::MAX - 300_000
        );
    }
}
