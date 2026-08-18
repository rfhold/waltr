use std::collections::BTreeMap;

use serde::Serialize;
use serde_yaml_ng::{Mapping, Value};

use crate::{
    ClientError, InferenceDefaults, McpServer, McpServerType, OAuthProvider, OauthTokenField,
    OauthTokenRequestStyle, Provider, ProviderType, ResourceWrite,
};

impl ResourceWrite {
    /// Prepare a deterministic strict provider document for a new resource slot.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider is invalid or cannot be serialized.
    pub fn new_provider(
        path: String,
        provider: &Provider,
        expected_etag: String,
    ) -> Result<Self, ClientError> {
        Ok(Self::Replace {
            path,
            source: provider_source(provider, None, None)?,
            expected_etag,
        })
    }

    /// Prepare a deterministic strict MCP server document for a new resource slot.
    ///
    /// # Errors
    ///
    /// Returns an error when the MCP server is invalid or cannot be serialized.
    pub fn new_mcp_server(
        path: String,
        server: &McpServer,
        expected_etag: String,
    ) -> Result<Self, ClientError> {
        Ok(Self::Replace {
            path,
            source: mcp_server_source(server, None, None)?,
            expected_etag,
        })
    }

    /// Prepare deterministic strict inference defaults for a resource slot.
    ///
    /// # Errors
    ///
    /// Returns an error when the defaults cannot be serialized.
    pub fn new_defaults(
        defaults: &InferenceDefaults,
        expected_etag: String,
    ) -> Result<Self, ClientError> {
        Ok(Self::Replace {
            path: "defaults/default.yaml".to_owned(),
            source: defaults_source(defaults, None, None)?,
            expected_etag,
        })
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResourceDocument<T> {
    api_version: &'static str,
    kind: &'static str,
    metadata: ResourceMetadata,
    spec: T,
}

#[derive(Serialize)]
struct ResourceMetadata {
    name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderSpec<'a> {
    transport: &'static str,
    url: &'a str,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    headers: BTreeMap<&'a str, &'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    oauth: Option<OAuthSpec<'a>>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    models: BTreeMap<&'a str, &'a crate::Model>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OAuthSpec<'a> {
    name: &'a str,
    authorize_url: &'a str,
    token_url: &'a str,
    client_id: &'a str,
    redirect_uri: &'a str,
    scopes: &'a [String],
    token_request_style: &'static str,
    auth_url_params: BTreeMap<&'a str, &'a str>,
    jwt_metadata: Vec<JwtMetadataSpec<'a>>,
    send_state_in_token_request: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JwtMetadataSpec<'a> {
    token: &'static str,
    metadata_key: &'a str,
    claim_names: &'a [String],
    required: bool,
}

#[derive(Serialize)]
struct McpServerSpec<'a> {
    transport: &'static str,
    #[serde(skip_serializing_if = "str::is_empty")]
    command: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    url: &'a str,
}

#[derive(Serialize)]
struct DefaultsSpec<'a> {
    inference: InferenceSpec<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InferenceSpec<'a> {
    provider: &'a str,
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'a str>,
}

pub fn provider_source(
    provider: &Provider,
    original: Option<&Provider>,
    active_source: Option<&str>,
) -> Result<String, ClientError> {
    let desired = strict_provider(provider)?;
    let (Some(original), Some(active_source)) = (original, active_source) else {
        return yaml(&desired);
    };
    let prior = strict_provider(original)?;
    let mut source = parse_source(active_source)?;

    patch_field(&mut source, &prior, &desired, &["metadata"], "name")?;
    for field in ["transport", "url"] {
        patch_field(&mut source, &prior, &desired, &["spec"], field)?;
    }
    patch_keyed_field(&mut source, &prior, &desired, &["spec"], "headers")?;
    patch_keyed_field(&mut source, &prior, &desired, &["spec"], "models")?;
    patch_oauth(&mut source, &prior, &desired)?;
    yaml(&source)
}

pub fn mcp_server_source(
    server: &McpServer,
    original: Option<&McpServer>,
    active_source: Option<&str>,
) -> Result<String, ClientError> {
    let desired = strict_mcp_server(server)?;
    let (Some(original), Some(active_source)) = (original, active_source) else {
        return yaml(&desired);
    };
    let prior = strict_mcp_server(original)?;
    let mut source = parse_source(active_source)?;

    patch_field(&mut source, &prior, &desired, &["metadata"], "name")?;
    for field in ["transport", "command"] {
        patch_field(&mut source, &prior, &desired, &["spec"], field)?;
    }
    if !mcp_url_matches_source_shape(&server.url, &original.url, &source) {
        patch_field(&mut source, &prior, &desired, &["spec"], "url")?;
    }
    yaml(&source)
}

fn mcp_url_matches_source_shape(desired: &str, original: &str, source: &Value) -> bool {
    if desired == original {
        return true;
    }
    let Some(path) = value_at(source, &["spec", "path"]).and_then(Value::as_str) else {
        return false;
    };
    format!(
        "{}/{}",
        original.trim_end_matches('/'),
        path.trim_start_matches('/')
    ) == desired
}

pub fn defaults_source(
    defaults: &InferenceDefaults,
    original: Option<&InferenceDefaults>,
    active_source: Option<&str>,
) -> Result<String, ClientError> {
    let desired = strict_defaults(defaults)?;
    let (Some(original), Some(active_source)) = (original, active_source) else {
        return yaml(&desired);
    };
    let prior = strict_defaults(original)?;
    let mut source = parse_source(active_source)?;
    for field in ["provider", "model", "reasoningEffort"] {
        patch_field(&mut source, &prior, &desired, &["spec", "inference"], field)?;
    }
    yaml(&source)
}

fn strict_provider(provider: &Provider) -> Result<Value, ClientError> {
    let transport = match ProviderType::try_from(provider.r#type) {
        Ok(ProviderType::AnthropicMessages) => "anthropicMessages",
        Ok(ProviderType::OpenaiChatCompletions) => "openaiChatCompletions",
        Ok(ProviderType::OpenaiResponses) => "openaiResponses",
        Ok(ProviderType::OpenaiResponsesWebsocket) => "openaiResponsesWebsocket",
        _ => return Err(ClientError::new("provider transport is unspecified")),
    };
    let oauth = provider
        .oauth_settings
        .as_ref()
        .map(oauth_spec)
        .transpose()?;
    to_value(&ResourceDocument {
        api_version: "waltr.dev/v1alpha1",
        kind: "Provider",
        metadata: ResourceMetadata {
            name: provider.name.clone(),
        },
        spec: ProviderSpec {
            transport,
            url: &provider.url,
            headers: provider
                .headers
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect(),
            oauth,
            models: provider
                .models
                .iter()
                .map(|(key, model)| (key.as_str(), model))
                .collect(),
        },
    })
}

fn strict_mcp_server(server: &McpServer) -> Result<Value, ClientError> {
    let transport = match McpServerType::try_from(server.r#type) {
        Ok(McpServerType::Stdio) => "stdio",
        Ok(McpServerType::StreamableHttp) => "streamableHttp",
        _ => return Err(ClientError::new("MCP server transport is unspecified")),
    };
    to_value(&ResourceDocument {
        api_version: "waltr.dev/v1alpha1",
        kind: "McpServer",
        metadata: ResourceMetadata {
            name: server.name.clone(),
        },
        spec: McpServerSpec {
            transport,
            command: &server.command,
            url: &server.url,
        },
    })
}

fn strict_defaults(defaults: &InferenceDefaults) -> Result<Value, ClientError> {
    to_value(&ResourceDocument {
        api_version: "waltr.dev/v1alpha1",
        kind: "Defaults",
        metadata: ResourceMetadata {
            name: "default".to_owned(),
        },
        spec: DefaultsSpec {
            inference: InferenceSpec {
                provider: &defaults.provider_key,
                model: &defaults.model_key,
                reasoning_effort: defaults.reasoning_effort.as_deref(),
            },
        },
    })
}

fn oauth_spec(provider: &OAuthProvider) -> Result<OAuthSpec<'_>, ClientError> {
    let token_request_style = match OauthTokenRequestStyle::try_from(provider.token_request_style) {
        Ok(OauthTokenRequestStyle::Json) => "json",
        Ok(OauthTokenRequestStyle::FormUrlencoded) => "formUrlencoded",
        _ => return Err(ClientError::new("OAuth token request style is unspecified")),
    };
    let jwt_metadata = provider
        .jwt_metadata
        .iter()
        .map(|metadata| {
            let token = match OauthTokenField::try_from(metadata.token) {
                Ok(OauthTokenField::Access) => "access",
                Ok(OauthTokenField::Id) => "id",
                _ => return Err(ClientError::new("OAuth JWT token field is unspecified")),
            };
            Ok(JwtMetadataSpec {
                token,
                metadata_key: &metadata.metadata_key,
                claim_names: &metadata.claim_names,
                required: metadata.required,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(OAuthSpec {
        name: &provider.name,
        authorize_url: &provider.authorize_url,
        token_url: &provider.token_url,
        client_id: &provider.client_id,
        redirect_uri: &provider.redirect_uri,
        scopes: &provider.scopes,
        token_request_style,
        auth_url_params: provider
            .auth_url_params
            .iter()
            .map(|param| (param.key.as_str(), param.value.as_str()))
            .collect(),
        jwt_metadata,
        send_state_in_token_request: provider.send_state_in_token_request,
    })
}

fn patch_oauth(source: &mut Value, prior: &Value, desired: &Value) -> Result<(), ClientError> {
    let old = value_at(prior, &["spec", "oauth"]);
    let new = value_at(desired, &["spec", "oauth"]);
    if old == new {
        return Ok(());
    }
    if old.is_none() || new.is_none() {
        return set_field_at(source, &["spec"], "oauth", new.cloned());
    }
    for field in [
        "name",
        "authorizeUrl",
        "tokenUrl",
        "clientId",
        "redirectUri",
        "scopes",
        "tokenRequestStyle",
        "jwtMetadata",
        "sendStateInTokenRequest",
    ] {
        patch_field(source, prior, desired, &["spec", "oauth"], field)?;
    }
    patch_keyed_field(source, prior, desired, &["spec", "oauth"], "authUrlParams")
}

fn patch_field(
    source: &mut Value,
    prior: &Value,
    desired: &Value,
    path: &[&str],
    field: &str,
) -> Result<(), ClientError> {
    let mut full_path = path.to_vec();
    full_path.push(field);
    let old = value_at(prior, &full_path);
    let new = value_at(desired, &full_path);
    if old != new {
        set_field_at(source, path, field, new.cloned())?;
    }
    Ok(())
}

fn patch_keyed_field(
    source: &mut Value,
    prior: &Value,
    desired: &Value,
    path: &[&str],
    field: &str,
) -> Result<(), ClientError> {
    let mut full_path = path.to_vec();
    full_path.push(field);
    let old = mapping_at(prior, &full_path);
    let new = mapping_at(desired, &full_path);
    if old == new {
        return Ok(());
    }
    let target = mapping_field_mut(source, path, field)?;
    let keys = old
        .into_iter()
        .flat_map(Mapping::keys)
        .chain(new.into_iter().flat_map(Mapping::keys))
        .cloned()
        .collect::<Vec<_>>();
    for key in keys {
        let old_value = old.and_then(|map| map.get(&key));
        let new_value = new.and_then(|map| map.get(&key));
        if old_value != new_value {
            if let Some(value) = new_value {
                target.insert(key, value.clone());
            } else {
                target.remove(&key);
            }
        }
    }
    Ok(())
}

fn mapping_field_mut<'a>(
    source: &'a mut Value,
    path: &[&str],
    field: &str,
) -> Result<&'a mut Mapping, ClientError> {
    let parent = mapping_mut_at(source, path)?;
    let key = Value::String(field.to_owned());
    if !parent.contains_key(&key) {
        parent.insert(key.clone(), Value::Mapping(Mapping::new()));
    }
    parent
        .get_mut(&key)
        .and_then(Value::as_mapping_mut)
        .ok_or_else(|| ClientError::new(format!("resource source field '{field}' is not a map")))
}

fn set_field_at(
    source: &mut Value,
    path: &[&str],
    field: &str,
    value: Option<Value>,
) -> Result<(), ClientError> {
    let parent = mapping_mut_at(source, path)?;
    let key = Value::String(field.to_owned());
    if let Some(value) = value {
        parent.insert(key, value);
    } else {
        parent.remove(&key);
    }
    Ok(())
}

fn mapping_mut_at<'a>(value: &'a mut Value, path: &[&str]) -> Result<&'a mut Mapping, ClientError> {
    let mut current = value;
    for segment in path {
        current = current
            .as_mapping_mut()
            .and_then(|map| map.get_mut(Value::String((*segment).to_owned())))
            .ok_or_else(|| {
                ClientError::new(format!("resource source is missing '{segment}' mapping"))
            })?;
    }
    current
        .as_mapping_mut()
        .ok_or_else(|| ClientError::new("resource source field is not a map"))
}

fn value_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(value, |current, segment| {
        current.get(Value::String((*segment).to_owned()))
    })
}

fn mapping_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Mapping> {
    value_at(value, path).and_then(Value::as_mapping)
}

fn parse_source(source: &str) -> Result<Value, ClientError> {
    serde_yaml_ng::from_str(source).map_err(|error| ClientError::new(error.to_string()))
}

fn to_value(value: &impl Serialize) -> Result<Value, ClientError> {
    serde_yaml_ng::to_value(value).map_err(|error| ClientError::new(error.to_string()))
}

fn yaml(value: &impl Serialize) -> Result<String, ClientError> {
    serde_yaml_ng::to_string(value).map_err(|error| ClientError::new(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_edit_preserves_catalog_aliases_and_template_headers() {
        const SOURCE: &str = "apiVersion: waltr.dev/v1alpha1\nkind: Provider\nmetadata:\n  name: codex\nspec:\n  transport: openaiResponses\n  url: https://old.example.test\n  headers:\n    authorization: ${env:CODEX_TOKEN}\n  catalog:\n    provider: openai\n    include: [gpt-5.*]\n    keyPrefix: codex/\n    aliases:\n      codex/gpt-5.6: gpt-5.6\n";
        let original = provider("https://old.example.test");
        let mut edited = original.clone();
        edited.url = "https://new.example.test".to_owned();

        let source = provider_source(&edited, Some(&original), Some(SOURCE)).unwrap();

        assert!(source.contains("include:\n    - gpt-5.*"));
        assert!(source.contains("keyPrefix: codex/"));
        assert!(source.contains("codex/gpt-5.6: gpt-5.6"));
        assert!(source.contains(concat!("authorization: ${env:", "CODEX_TOKEN}")));
        assert!(source.contains("url: https://new.example.test"));
    }

    #[test]
    fn mcp_edit_preserves_environment_url_and_path() {
        const SOURCE: &str = "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata:\n  name: waltr\nspec:\n  transport: streamableHttp\n  url: ${env:WALTR_PUBLIC_URL}\n  path: /mcp\n";
        let original = McpServer {
            name: "waltr".to_owned(),
            r#type: McpServerType::StreamableHttp as i32,
            url: "https://resolved.example.test".to_owned(),
            ..McpServer::default()
        };

        let source = mcp_server_source(&original, Some(&original), Some(SOURCE)).unwrap();

        assert!(source.contains(concat!("url: ${env:", "WALTR_PUBLIC_URL}")));
        assert!(source.contains("path: /mcp"));
        assert!(!source.contains("resolved.example.test"));
    }

    fn provider(url: &str) -> Provider {
        Provider {
            name: "codex".to_owned(),
            r#type: ProviderType::OpenaiResponses as i32,
            url: url.to_owned(),
            headers: std::collections::HashMap::from([(
                "authorization".to_owned(),
                "resolved-secret".to_owned(),
            )]),
            ..Provider::default()
        }
    }
}
