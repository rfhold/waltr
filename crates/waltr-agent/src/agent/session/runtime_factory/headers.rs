use std::collections::BTreeMap;

use super::{AgentSessionRuntimeError, ENV_PREFIX};

pub(super) fn resolve_provider_headers(
    headers: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, AgentSessionRuntimeError> {
    resolve_provider_headers_with(headers, |name| std::env::var(name).ok())
}

fn resolve_provider_headers_with(
    headers: &BTreeMap<String, String>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<BTreeMap<String, String>, AgentSessionRuntimeError> {
    let mut resolved = BTreeMap::new();
    for (name, value) in headers {
        let value = resolve_header_value(value, &env).map_err(|env_var| {
            AgentSessionRuntimeError::FailedPrecondition(format!(
                "provider header '{name}' references unset env var '{env_var}'"
            ))
        })?;
        if !value.trim().is_empty() {
            resolved.insert(name.clone(), value);
        }
    }
    Ok(resolved)
}

fn resolve_header_value(
    value: &str,
    env: impl Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let mut output = String::new();
    let mut remaining = value;

    while let Some(start) = remaining.find(ENV_PREFIX) {
        output.push_str(&remaining[..start]);
        let env_start = start + ENV_PREFIX.len();
        let Some(end) = remaining[env_start..].find('}') else {
            output.push_str(&remaining[start..]);
            return Ok(output);
        };
        let env_name = &remaining[env_start..env_start + end];
        let Some(env_value) = (!env_name.is_empty())
            .then(|| env(env_name))
            .flatten()
            .filter(|value| !value.trim().is_empty())
        else {
            return Err(env_name.to_owned());
        };
        output.push_str(&env_value);
        remaining = &remaining[env_start + end + 1..];
    }

    output.push_str(remaining);
    Ok(output)
}

pub(super) fn configured_credential(headers: &BTreeMap<String, String>) -> Option<&str> {
    headers.iter().find_map(|(name, value)| {
        (is_credential_header(name) && !value.trim().is_empty()).then_some(value.trim())
    })
}

pub(super) fn is_credential_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "api_key" | "x-api-key" | "authorization"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)]
    fn resolves_whole_and_embedded_env_header_templates() {
        let headers = BTreeMap::from([
            (
                "x-api-key".to_owned(),
                "${env:ANTHROPIC_API_KEY}".to_owned(),
            ),
            (
                "Authorization".to_owned(),
                "Bearer ${env:OPENAI_API_KEY}".to_owned(),
            ),
        ]);

        let resolved = resolve_provider_headers_with(&headers, |name| match name {
            "ANTHROPIC_API_KEY" => Some("sk-ant".to_owned()),
            "OPENAI_API_KEY" => Some("sk-openai".to_owned()),
            _ => None,
        })
        .expect("headers should resolve");

        assert_eq!(resolved["x-api-key"], "sk-ant");
        assert_eq!(resolved["Authorization"], "Bearer sk-openai");
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)]
    fn missing_env_header_template_fails_runtime_resolution() {
        let headers = BTreeMap::from([(
            "Authorization".to_owned(),
            "Bearer ${env:WALTR_AGENT_TEST_MISSING_HEADER_TOKEN_UNSET}".to_owned(),
        )]);

        let error =
            resolve_provider_headers_with(&headers, |_| None).expect_err("missing env should fail");

        assert!(matches!(
            error,
            AgentSessionRuntimeError::FailedPrecondition(_)
        ));
        assert!(
            error
                .to_string()
                .contains("provider header 'Authorization' references unset env var")
        );
    }
}
