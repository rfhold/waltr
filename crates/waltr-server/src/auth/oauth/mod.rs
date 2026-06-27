use std::collections::BTreeMap;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use waltr::grpc::waltr::{
    auth::v1::{BeginProviderAuthResponse, ProviderAuthMethod},
    config::v1::{
        OAuthProvider, OauthJwtMetadata, OauthTokenField, OauthTokenRequestStyle, Provider,
    },
};
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use tonic::Status;
use uuid::Uuid;

use super::OAuthCredential;

/// Resolved provider OAuth definition.
#[derive(Debug, Clone)]
pub(crate) struct ProviderOAuthDefinition {
    /// OAuth provider definition to use for login and refresh.
    pub definition: OAuthProvider,
}

/// Pending manual OAuth attempt.
#[derive(Debug, Clone)]
pub struct OAuthAttempt {
    /// Provider key this attempt belongs to.
    provider_key: String,
    definition: OAuthProvider,
    verifier: String,
    state: String,
    expires_at_ms: i64,
}

impl OAuthAttempt {
    /// Return the owning provider key.
    #[must_use]
    pub fn provider_key(&self) -> &str {
        &self.provider_key
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
    provider_key: String,
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
        provider_key,
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
            "Open the URL, approve access, then paste the final redirect URL or code.".to_owned(),
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
    if now_ms > attempt.expires_at_ms {
        return Err(Status::deadline_exceeded("OAuth attempt expired"));
    }
    let (code, state) = parse_manual_input(input);
    let code = code.ok_or_else(|| Status::invalid_argument("OAuth code is required"))?;
    if state.as_deref().is_some_and(|value| value != attempt.state) {
        return Err(Status::invalid_argument("OAuth state does not match"));
    }
    exchange_code(
        &attempt.definition,
        &code,
        &attempt.verifier,
        &attempt.state,
    )
    .await
}

/// Refresh an OAuth credential with a provider definition.
pub async fn refresh(
    definition: &OAuthProvider,
    credential: &OAuthCredential,
) -> Result<OAuthCredential, Status> {
    validate_definition(definition)?;
    let mut next = request_token(
        definition,
        TokenRequest::Refresh {
            refresh_token: &credential.refresh,
        },
    )
    .await?;
    if next.refresh.is_empty() {
        next.refresh.clone_from(&credential.refresh);
    }
    Ok(next)
}

async fn exchange_code(
    definition: &OAuthProvider,
    code: &str,
    verifier: &str,
    state: &str,
) -> Result<OAuthCredential, Status> {
    request_token(
        definition,
        TokenRequest::AuthorizationCode {
            code,
            verifier,
            state,
        },
    )
    .await
}

enum TokenRequest<'a> {
    AuthorizationCode {
        code: &'a str,
        verifier: &'a str,
        state: &'a str,
    },
    Refresh {
        refresh_token: &'a str,
    },
}

async fn request_token(
    definition: &OAuthProvider,
    token_request: TokenRequest<'_>,
) -> Result<OAuthCredential, Status> {
    let pairs = token_request_pairs(definition, token_request);
    let builder = Client::new().post(&definition.token_url);
    let response = match request_style(definition)? {
        OauthTokenRequestStyle::Json => builder.json(&pairs_to_json(&pairs)).send().await,
        OauthTokenRequestStyle::FormUrlencoded => builder.form(&pairs).send().await,
        OauthTokenRequestStyle::Unspecified => unreachable!("validated request style"),
    }
    .map_err(|error| Status::unauthenticated(format!("OAuth token request failed: {error}")))?;
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
        TokenRequest::Refresh { refresh_token } => vec![
            ("grant_type", "refresh_token"),
            ("client_id", definition.client_id.as_str()),
            ("refresh_token", refresh_token),
        ],
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
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(Status::unauthenticated(format!(
            "OAuth token endpoint returned {status}: {text}"
        )));
    }
    let token: TokenResponse = serde_json::from_str(&text).map_err(|error| {
        Status::unauthenticated(format!("OAuth token response was invalid: {error}"))
    })?;
    let metadata = extract_metadata(definition, &token)?;
    Ok(OAuthCredential {
        access: token.access_token,
        refresh: token.refresh_token.unwrap_or_default(),
        expires_at_ms: now_ms() + token.expires_in.unwrap_or(3600) * 1000 - 5 * 60 * 1000,
        metadata,
    })
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
        .map_or(0, |duration| duration.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::defaults::openai_codex_oauth_settings;
    use serde_json::json;

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
}
