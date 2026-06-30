//! Provider header resolution helpers.

use std::collections::HashMap;

const ENV_PREFIX: &str = "${env:";

#[derive(Debug, Default)]
pub(crate) struct ResolvedHeaders {
    pub(crate) headers: HashMap<String, String>,
    pub(crate) unresolved: Vec<UnresolvedHeader>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnresolvedHeader {
    pub(crate) name: String,
    pub(crate) env_var: String,
}

pub(crate) fn resolve_provider_headers(headers: &HashMap<String, String>) -> ResolvedHeaders {
    resolve_provider_headers_with(headers, |name| std::env::var(name).ok())
}

pub(crate) fn configured_credential(headers: &HashMap<String, String>) -> String {
    for (name, value) in headers {
        if is_credential_header(name) && !value.trim().is_empty() {
            let trimmed = value.trim();
            return trimmed
                .strip_prefix("Bearer ")
                .unwrap_or(trimmed)
                .to_owned();
        }
    }

    String::new()
}

pub(crate) fn has_unresolved_credential(headers: &ResolvedHeaders) -> bool {
    headers
        .unresolved
        .iter()
        .any(|header| is_credential_header(&header.name))
}

pub(crate) fn is_credential_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "api_key" | "x-api-key" | "authorization"
    )
}

fn resolve_provider_headers_with(
    headers: &HashMap<String, String>,
    env: impl Fn(&str) -> Option<String>,
) -> ResolvedHeaders {
    let mut resolved = ResolvedHeaders::default();
    for (name, value) in headers {
        match resolve_header_value(value, &env) {
            Ok(value) => {
                if !value.trim().is_empty() {
                    resolved.headers.insert(name.clone(), value);
                }
            }
            Err(env_var) => resolved.unresolved.push(UnresolvedHeader {
                name: name.clone(),
                env_var,
            }),
        }
    }
    resolved
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

#[cfg(test)]
mod tests {
    use super::{configured_credential, has_unresolved_credential, resolve_provider_headers_with};
    use std::collections::HashMap;

    #[test]
    fn resolves_whole_and_embedded_env_placeholders() {
        let headers = HashMap::from([
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
        });

        assert_eq!(resolved.headers["x-api-key"], "sk-ant");
        assert_eq!(resolved.headers["Authorization"], "Bearer sk-openai");
        assert!(resolved.unresolved.is_empty());
    }

    #[test]
    fn skips_headers_with_missing_env_placeholders() {
        let headers = HashMap::from([
            (
                "Authorization".to_owned(),
                "Bearer ${env:OPENAI_API_KEY}".to_owned(),
            ),
            ("x-app".to_owned(), "cli".to_owned()),
        ]);

        let resolved = resolve_provider_headers_with(&headers, |_| None);

        assert!(!resolved.headers.contains_key("Authorization"));
        assert_eq!(resolved.headers["x-app"], "cli");
        assert_eq!(resolved.unresolved.len(), 1);
        assert!(has_unresolved_credential(&resolved));
    }

    #[test]
    fn configured_credential_reads_resolved_headers() {
        let headers =
            HashMap::from([("Authorization".to_owned(), "Bearer oauth-token".to_owned())]);
        assert_eq!(configured_credential(&headers), "oauth-token");

        let headers = HashMap::from([("X-Api-Key".to_owned(), "sk-ant-key".to_owned())]);
        assert_eq!(configured_credential(&headers), "sk-ant-key");
    }
}
