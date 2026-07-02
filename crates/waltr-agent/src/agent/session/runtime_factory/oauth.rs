use std::{collections::BTreeMap, time::SystemTime};

use super::AgentSessionRuntimeError;
use crate::{
    OAuthCredential, SessionOAuthProviderSettings, SessionOAuthTokenFieldTarget,
    SessionOAuthTokenRequestStyle,
};

pub(super) async fn refresh_oauth(
    definition: &SessionOAuthProviderSettings,
    credential: &OAuthCredential,
) -> Result<OAuthCredential, AgentSessionRuntimeError> {
    let pairs = vec![
        ("grant_type", "refresh_token"),
        ("client_id", definition.client_id.as_str()),
        ("refresh_token", credential.refresh.as_str()),
    ];
    let builder = reqwest::Client::new().post(&definition.token_url);
    let response = match definition.token_request_style {
        SessionOAuthTokenRequestStyle::Json => builder.json(&pairs_to_json(&pairs)).send().await,
        SessionOAuthTokenRequestStyle::FormUrlencoded => builder.form(&pairs).send().await,
    }
    .map_err(|error| {
        AgentSessionRuntimeError::FailedPrecondition(format!("OAuth token request failed: {error}"))
    })?;
    let mut next = token_response_to_credential(definition, response).await?;
    if next.refresh.is_empty() {
        next.refresh.clone_from(&credential.refresh);
    }
    Ok(next)
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

async fn token_response_to_credential(
    definition: &SessionOAuthProviderSettings,
    response: reqwest::Response,
) -> Result<OAuthCredential, AgentSessionRuntimeError> {
    let status = response.status();
    let body = response.text().await.map_err(|error| {
        AgentSessionRuntimeError::FailedPrecondition(format!(
            "OAuth token response read failed: {error}"
        ))
    })?;
    if !status.is_success() {
        return Err(AgentSessionRuntimeError::FailedPrecondition(format!(
            "OAuth token request returned {status}: {body}"
        )));
    }
    let value: serde_json::Value = serde_json::from_str(&body).map_err(|error| {
        AgentSessionRuntimeError::FailedPrecondition(format!(
            "OAuth token response was not JSON: {error}"
        ))
    })?;
    let mut credential = OAuthCredential {
        access: value
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        refresh: value
            .get("refresh_token")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        expires_at_ms: value
            .get("expires_in")
            .and_then(serde_json::Value::as_i64)
            .map_or(0, |seconds| now_ms() + seconds * 1000),
        metadata: BTreeMap::new(),
    };
    for field in &definition.token_fields {
        let Some(field_value) = value.get(&field.source) else {
            continue;
        };
        match field.target {
            SessionOAuthTokenFieldTarget::Access => {
                credential.access = field_value.as_str().unwrap_or_default().to_owned();
            }
            SessionOAuthTokenFieldTarget::Refresh => {
                credential.refresh = field_value.as_str().unwrap_or_default().to_owned();
            }
            SessionOAuthTokenFieldTarget::ExpiresAtMs => {
                credential.expires_at_ms = field_value.as_i64().unwrap_or_default();
            }
            SessionOAuthTokenFieldTarget::ExpiresInSeconds => {
                credential.expires_at_ms =
                    now_ms() + field_value.as_i64().unwrap_or_default() * 1000;
            }
            SessionOAuthTokenFieldTarget::Metadata => {
                credential
                    .metadata
                    .insert(field.source.clone(), json_scalar_to_string(field_value));
            }
        }
    }
    if credential.access.is_empty() {
        return Err(AgentSessionRuntimeError::FailedPrecondition(
            "OAuth token response did not include an access token".to_owned(),
        ));
    }
    Ok(credential)
}

fn json_scalar_to_string(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), ToOwned::to_owned)
}

pub(super) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as i64)
}
