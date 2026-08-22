//! Strict typed resource materialization and public protobuf projection.

use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::{collections::BTreeMap, env};

use crate::catalog::CatalogMaterializer;
use waltr::grpc::waltr::{config::v1 as config_proto, resource::v1 as resource_proto};

use super::{
    state::{ActiveResource, AgentDefinitionLoadError, ResourceKind, ResourceOrigin},
    store::{encode_hex, slot_etag},
};

pub(super) fn proto_resource(
    path: &str,
    entry: &ActiveResource,
    persisted_source: Option<&String>,
) -> resource_proto::Resource {
    resource_proto::Resource {
        path: path.to_owned(),
        kind: proto_kind(entry.kind) as i32,
        name: entry.name.clone(),
        origin: match entry.origin {
            ResourceOrigin::Bundled => resource_proto::ResourceOrigin::Bundled as i32,
            ResourceOrigin::Filesystem => resource_proto::ResourceOrigin::Filesystem as i32,
        },
        source_etag: entry.etag.clone(),
        effective_value: json_struct(&entry.effective_value),
        persisted_etag: slot_etag(persisted_source.map(String::as_bytes)),
        active_source: entry.source.clone(),
    }
}

pub(super) const fn proto_kind(kind: ResourceKind) -> resource_proto::ResourceKind {
    match kind {
        ResourceKind::Agent => resource_proto::ResourceKind::Agent,
        ResourceKind::FeatureSet => resource_proto::ResourceKind::FeatureSet,
        ResourceKind::PermissionSet => resource_proto::ResourceKind::PermissionSet,
        ResourceKind::Provider => resource_proto::ResourceKind::Provider,
        ResourceKind::McpServer => resource_proto::ResourceKind::McpServer,
        ResourceKind::Defaults => resource_proto::ResourceKind::Defaults,
        ResourceKind::Routine => resource_proto::ResourceKind::Routine,
    }
}

fn json_struct(value: &serde_json::Value) -> Option<resource_proto::ResourceObject> {
    value
        .as_object()
        .map(|object| resource_proto::ResourceObject {
            fields: object
                .iter()
                .map(|(key, value)| (key.clone(), json_prost_value(value)))
                .collect(),
        })
}

fn json_prost_value(value: &serde_json::Value) -> resource_proto::ResourceValue {
    use resource_proto::resource_value::Kind;
    let kind = match value {
        serde_json::Value::Null => Kind::NullValue(true),
        serde_json::Value::Bool(value) => Kind::BoolValue(*value),
        serde_json::Value::Number(value) => Kind::NumberValue(value.as_f64().unwrap_or_default()),
        serde_json::Value::String(value) => Kind::StringValue(value.clone()),
        serde_json::Value::Array(values) => Kind::ListValue(resource_proto::ResourceList {
            values: values.iter().map(json_prost_value).collect(),
        }),
        serde_json::Value::Object(object) => Kind::ObjectValue(resource_proto::ResourceObject {
            fields: object
                .iter()
                .map(|(key, value)| (key.clone(), json_prost_value(value)))
                .collect(),
        }),
    };
    resource_proto::ResourceValue { kind: Some(kind) }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourceMetadata {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ProviderResource {
    api_version: String,
    kind: String,
    metadata: ResourceMetadata,
    spec: ProviderSpec,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ProviderSpec {
    transport: String,
    url: String,
    #[serde(default)]
    headers: std::collections::HashMap<String, String>,
    #[serde(default)]
    oauth: Option<OAuthSpec>,
    #[serde(default)]
    catalog: Option<CatalogSelector>,
    #[serde(default)]
    models: std::collections::HashMap<String, config_proto::Model>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CatalogSelector {
    provider: String,
    include: Vec<String>,
    #[serde(default)]
    aliases: BTreeMap<String, String>,
    #[serde(default)]
    key_prefix: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct OAuthSpec {
    name: String,
    authorize_url: String,
    token_url: String,
    client_id: String,
    redirect_uri: String,
    #[serde(default)]
    scopes: Vec<String>,
    token_request_style: String,
    #[serde(default)]
    auth_url_params: BTreeMap<String, String>,
    #[serde(default)]
    jwt_metadata: Vec<JwtMetadataSpec>,
    #[serde(default)]
    send_state_in_token_request: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct JwtMetadataSpec {
    token: String,
    metadata_key: String,
    claim_names: Vec<String>,
    #[serde(default)]
    required: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct McpServerResource {
    api_version: String,
    kind: String,
    metadata: ResourceMetadata,
    spec: McpServerSpec,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct McpServerSpec {
    transport: String,
    #[serde(default)]
    command: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct DefaultsResource {
    api_version: String,
    kind: String,
    #[serde(rename = "metadata")]
    _metadata: ResourceMetadata,
    spec: DefaultsSpec,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DefaultsSpec {
    inference: InferenceSpec,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct InferenceSpec {
    provider: String,
    model: String,
    #[serde(default)]
    reasoning_effort: Option<String>,
}

fn parse_resource<T: for<'de> Deserialize<'de>>(
    source: &str,
    path: &str,
) -> Result<T, AgentDefinitionLoadError> {
    serde_yaml_ng::from_str(source).map_err(|source| AgentDefinitionLoadError::ResourceYaml {
        path: path.to_owned(),
        source,
    })
}

fn validate_envelope(
    api_version: &str,
    kind: &str,
    expected_kind: &str,
    path: &str,
) -> Result<(), AgentDefinitionLoadError> {
    if api_version != waltr_agent::AGENT_DEFINITION_API_VERSION || kind != expected_kind {
        return Err(AgentDefinitionLoadError::InvalidResourceValue {
            path: path.to_owned(),
            message: format!("expected apiVersion `waltr.dev/v1alpha1` and kind `{expected_kind}`"),
        });
    }
    Ok(())
}

pub(super) fn materialize_provider(
    source: &str,
    path: &str,
    materializer: Option<&CatalogMaterializer>,
) -> Result<config_proto::Provider, AgentDefinitionLoadError> {
    let resource: ProviderResource = parse_resource(source, path)?;
    validate_envelope(&resource.api_version, &resource.kind, "Provider", path)?;
    let provider_type = match resource.spec.transport.as_str() {
        "anthropicMessages" => config_proto::ProviderType::AnthropicMessages,
        "openaiChatCompletions" => config_proto::ProviderType::OpenaiChatCompletions,
        "openaiResponses" => config_proto::ProviderType::OpenaiResponses,
        "openaiResponsesWebsocket" => config_proto::ProviderType::OpenaiResponsesWebsocket,
        value => {
            return Err(invalid_value(
                path,
                format!("unsupported provider transport `{value}`"),
            ));
        }
    };
    let mut models = resource.spec.models;
    if let Some(selector) = resource.spec.catalog {
        let materializer = materializer.ok_or_else(|| {
            invalid_value(
                path,
                "catalog selectors require a models.dev catalog".to_owned(),
            )
        })?;
        models.extend(
            materializer
                .select_models(
                    &selector.provider,
                    &selector.include,
                    &selector.aliases,
                    &selector.key_prefix,
                )
                .map_err(|message| invalid_value(path, message))?,
        );
    }
    let oauth_settings = resource
        .spec
        .oauth
        .map(|oauth| materialize_oauth(oauth, path))
        .transpose()?;
    Ok(config_proto::Provider {
        r#type: provider_type as i32,
        name: resource.metadata.name,
        url: materialize_url(&resource.spec.url, "", false, path)?,
        headers: resource.spec.headers,
        models,
        oauth_compatible: oauth_settings.is_some(),
        oauth_settings,
    })
}

fn materialize_oauth(
    oauth: OAuthSpec,
    path: &str,
) -> Result<config_proto::OAuthProvider, AgentDefinitionLoadError> {
    let token_request_style = match oauth.token_request_style.as_str() {
        "json" => config_proto::OauthTokenRequestStyle::Json,
        "formUrlencoded" => config_proto::OauthTokenRequestStyle::FormUrlencoded,
        value => {
            return Err(invalid_value(
                path,
                format!("unsupported token request style `{value}`"),
            ));
        }
    };
    let jwt_metadata = oauth
        .jwt_metadata
        .into_iter()
        .map(|metadata| {
            let token = match metadata.token.as_str() {
                "access" => config_proto::OauthTokenField::Access,
                "id" => config_proto::OauthTokenField::Id,
                value => {
                    return Err(invalid_value(
                        path,
                        format!("unsupported OAuth token `{value}`"),
                    ));
                }
            };
            Ok(config_proto::OauthJwtMetadata {
                token: token as i32,
                metadata_key: metadata.metadata_key,
                claim_names: metadata.claim_names,
                required: metadata.required,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(config_proto::OAuthProvider {
        name: oauth.name,
        authorize_url: materialize_url(&oauth.authorize_url, "", false, path)?,
        token_url: materialize_url(&oauth.token_url, "", false, path)?,
        client_id: oauth.client_id,
        redirect_uri: materialize_url(&oauth.redirect_uri, "", false, path)?,
        scopes: oauth.scopes,
        token_request_style: token_request_style as i32,
        auth_url_params: oauth
            .auth_url_params
            .into_iter()
            .map(|(key, value)| config_proto::OauthUrlParam { key, value })
            .collect(),
        jwt_metadata,
        send_state_in_token_request: oauth.send_state_in_token_request,
    })
}

pub(super) fn materialize_mcp_server(
    source: &str,
    path: &str,
) -> Result<config_proto::McpServer, AgentDefinitionLoadError> {
    let resource: McpServerResource = parse_resource(source, path)?;
    validate_envelope(&resource.api_version, &resource.kind, "McpServer", path)?;
    let (transport, command, url) = match resource.spec.transport.as_str() {
        "stdio" if !resource.spec.command.trim().is_empty() => (
            config_proto::McpServerType::Stdio,
            resource.spec.command,
            String::new(),
        ),
        "streamableHttp" if !resource.spec.url.trim().is_empty() => (
            config_proto::McpServerType::StreamableHttp,
            String::new(),
            materialize_url(
                &resource.spec.url,
                &resource.spec.path,
                path == "mcp-servers/waltr.yaml",
                path,
            )?,
        ),
        value => {
            return Err(invalid_value(
                path,
                format!("invalid MCP transport `{value}` configuration"),
            ));
        }
    };
    Ok(config_proto::McpServer {
        name: resource.metadata.name,
        r#type: transport as i32,
        command,
        url,
    })
}

pub(super) fn materialize_defaults(
    source: &str,
    path: &str,
) -> Result<config_proto::InferenceDefaults, AgentDefinitionLoadError> {
    let resource: DefaultsResource = parse_resource(source, path)?;
    validate_envelope(&resource.api_version, &resource.kind, "Defaults", path)?;
    Ok(config_proto::InferenceDefaults {
        provider_key: resource.spec.inference.provider,
        model_key: resource.spec.inference.model,
        reasoning_effort: resource.spec.inference.reasoning_effort,
    })
}

pub(super) fn materialize_url(
    template: &str,
    joined_path: &str,
    public: bool,
    resource_path: &str,
) -> Result<String, AgentDefinitionLoadError> {
    let expanded = expand_env(template, resource_path)?;
    let mut url = url::Url::parse(&expanded)
        .map_err(|_| invalid_value(resource_path, "URL must be absolute".to_owned()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid_value(
            resource_path,
            "URL must use HTTP(S)".to_owned(),
        ));
    }
    if public {
        let loopback_http = url.scheme() == "http"
            && url.host().is_some_and(|host| match host {
                url::Host::Domain(host) => host.eq_ignore_ascii_case("localhost"),
                url::Host::Ipv4(address) => address.is_loopback(),
                url::Host::Ipv6(address) => address.is_loopback(),
            });
        if !(url.scheme() == "https" || loopback_http)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid_value(
                resource_path,
                "public URL must be secure and contain no userinfo, query, or fragment".to_owned(),
            ));
        }
    }
    if !joined_path.is_empty() {
        let path = format!(
            "{}/{}",
            url.path().trim_end_matches('/'),
            joined_path.trim_start_matches('/')
        );
        url.set_path(&path);
    }
    Ok(url.into())
}

fn expand_env(template: &str, path: &str) -> Result<String, AgentDefinitionLoadError> {
    let mut result = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("${env:") {
        result.push_str(&rest[..start]);
        let expression = &rest[start + 6..];
        let end = expression
            .find('}')
            .ok_or_else(|| invalid_value(path, "unterminated environment template".to_owned()))?;
        let name = &expression[..end];
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(invalid_value(
                path,
                "invalid environment variable name".to_owned(),
            ));
        }
        let value = env::var(name).map_err(|_| {
            invalid_value(
                path,
                format!("required environment variable `{name}` is missing"),
            )
        })?;
        result.push_str(&value);
        rest = &expression[end + 1..];
    }
    result.push_str(rest);
    Ok(result)
}

pub(super) fn yaml_json(
    source: &str,
    path: &str,
) -> Result<serde_json::Value, AgentDefinitionLoadError> {
    let value: serde_yaml_ng::Value = parse_resource(source, path)?;
    serde_json::to_value(value).map_err(|error| invalid_value(path, error.to_string()))
}

pub(super) fn invalid_value(path: &str, message: String) -> AgentDefinitionLoadError {
    AgentDefinitionLoadError::InvalidResourceValue {
        path: path.to_owned(),
        message,
    }
}

pub(super) fn materialized_snapshot_etag(
    entries: &BTreeMap<String, ActiveResource>,
    configuration: &config_proto::Configuration,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"waltr-resource-catalog-v1\0");
    for (path, entry) in entries {
        digest.update(path.as_bytes());
        digest.update(entry.etag.as_bytes());
        digest
            .update(serde_json::to_vec(&entry.effective_value).expect("resource value serializes"));
    }
    digest.update(serde_json::to_vec(configuration).expect("configuration serializes"));
    format!("sha256:{}", encode_hex(&digest.finalize()))
}
