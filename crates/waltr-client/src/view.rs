//! Stable shell-facing view models for Waltr configuration.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{
    Configuration, McpServer, McpServerType, Model, ModelCapabilities, ModelLimits, ModelOptions,
    ModelType, Provider, ProviderType,
};

/// Stable configuration view shared by Waltr shells.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ConfigView {
    /// Configuration version used for optimistic concurrency.
    pub etag: String,
    /// Configured model providers.
    pub providers: BTreeMap<String, ProviderView>,
    /// Configured MCP servers.
    pub mcp_servers: BTreeMap<String, McpServerView>,
}

/// Stable provider view shared by Waltr shells.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ProviderView {
    /// Provider type as a shell-friendly enum.
    pub kind: ProviderKindView,
    /// Provider display/configuration name.
    pub name: String,
    /// Provider endpoint URL.
    pub url: String,
    /// Provider-specific request headers.
    pub headers: BTreeMap<String, String>,
    /// Models offered by this provider, keyed by stable id.
    pub models: BTreeMap<String, ModelView>,
}

/// Stable provider kind view shared by Waltr shells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ProviderKindView {
    /// Provider kind was omitted or intentionally unspecified.
    Unspecified,
    /// Anthropic-compatible provider.
    Anthropic,
    /// OpenAI-compatible provider.
    OpenAi,
    /// Provider kind is unknown to this client version.
    Unknown,
}

/// Stable model type view shared by Waltr shells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ModelTypeView {
    /// Model type was omitted or intentionally unspecified.
    Unspecified,
    /// Large language model.
    Llm,
    /// Embedding model.
    Embedding,
    /// Speech-to-text model.
    Stt,
    /// Text-to-speech model.
    Tts,
    /// Model type is unknown to this client version.
    Unknown,
}

/// Stable model options view shared by Waltr shells.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ModelOptionsView {
    /// Sampling temperature, when set.
    pub temperature: Option<f64>,
    /// Nucleus sampling probability, when set.
    pub top_p: Option<f64>,
    /// Top-k sampling cutoff, when set.
    pub top_k: Option<u32>,
    /// Minimum probability cutoff, when set.
    pub min_p: Option<f64>,
    /// Presence penalty, when set.
    pub presence_penalty: Option<f64>,
    /// Repetition penalty, when set.
    pub repetition_penalty: Option<f64>,
}

/// Stable model limits view shared by Waltr shells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ModelLimitsView {
    /// Maximum context window size, when set.
    pub context: Option<u32>,
    /// Maximum output token count, when set.
    pub output: Option<u32>,
}

/// Stable model capabilities view shared by Waltr shells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ModelCapabilitiesView {
    /// Whether the model supports tool calling.
    pub tools: bool,
    /// Whether the model supports image input.
    pub image: bool,
}

/// Stable model view shared by Waltr shells.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ModelView {
    /// Model display name.
    pub name: String,
    /// Model type as a shell-friendly enum.
    pub kind: ModelTypeView,
    /// Model sampling options.
    pub options: ModelOptionsView,
    /// Model context and output limits.
    pub limits: ModelLimitsView,
    /// Model capabilities.
    pub capabilities: ModelCapabilitiesView,
}

/// Stable MCP server type view shared by Waltr shells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum McpServerTypeView {
    /// MCP server type was omitted or intentionally unspecified.
    Unspecified,
    /// Stdio-based MCP server.
    Stdio,
    /// SSE-based MCP server.
    Sse,
    /// MCP server type is unknown to this client version.
    Unknown,
}

/// Stable MCP server view shared by Waltr shells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct McpServerView {
    /// MCP server display name.
    pub name: String,
    /// MCP server type as a shell-friendly enum.
    pub kind: McpServerTypeView,
    /// Command to launch the server (stdio servers).
    pub command: String,
    /// Endpoint URL for the server (SSE servers).
    pub url: String,
}

impl From<Configuration> for ConfigView {
    fn from(config: Configuration) -> Self {
        Self {
            etag: config.etag,
            providers: config
                .providers
                .into_iter()
                .map(|(name, provider)| (name, ProviderView::from(provider)))
                .collect(),
            mcp_servers: config
                .mcp_servers
                .into_iter()
                .map(|(name, server)| (name, McpServerView::from(server)))
                .collect(),
        }
    }
}

impl From<&Configuration> for ConfigView {
    fn from(config: &Configuration) -> Self {
        config.clone().into()
    }
}

impl From<ConfigView> for Configuration {
    fn from(view: ConfigView) -> Self {
        Self {
            providers: view
                .providers
                .into_iter()
                .map(|(name, provider)| (name, Provider::from(provider)))
                .collect(),
            etag: view.etag,
            mcp_servers: view
                .mcp_servers
                .into_iter()
                .map(|(name, server)| (name, McpServer::from(server)))
                .collect(),
        }
    }
}

impl From<Provider> for ProviderView {
    fn from(provider: Provider) -> Self {
        Self {
            kind: provider.r#type.into(),
            name: provider.name,
            url: provider.url,
            headers: provider.headers.into_iter().collect(),
            models: provider
                .models
                .into_iter()
                .map(|(id, model)| (id, ModelView::from(model)))
                .collect(),
        }
    }
}

impl From<&Provider> for ProviderView {
    fn from(provider: &Provider) -> Self {
        provider.clone().into()
    }
}

impl From<ProviderView> for Provider {
    fn from(view: ProviderView) -> Self {
        Self {
            r#type: ProviderType::from(view.kind) as i32,
            name: view.name,
            url: view.url,
            headers: HashMap::from_iter(view.headers),
            models: view
                .models
                .into_iter()
                .map(|(id, model)| (id, Model::from(model)))
                .collect(),
        }
    }
}

impl From<i32> for ProviderKindView {
    fn from(value: i32) -> Self {
        match ProviderType::try_from(value) {
            Ok(ProviderType::Unspecified) => Self::Unspecified,
            Ok(ProviderType::Anthropic) => Self::Anthropic,
            Ok(ProviderType::Openai) => Self::OpenAi,
            Err(_) => Self::Unknown,
        }
    }
}

impl From<ProviderKindView> for ProviderType {
    fn from(kind: ProviderKindView) -> Self {
        match kind {
            ProviderKindView::Unspecified | ProviderKindView::Unknown => Self::Unspecified,
            ProviderKindView::Anthropic => Self::Anthropic,
            ProviderKindView::OpenAi => Self::Openai,
        }
    }
}

impl From<i32> for ModelTypeView {
    fn from(value: i32) -> Self {
        match ModelType::try_from(value) {
            Ok(ModelType::Unspecified) => Self::Unspecified,
            Ok(ModelType::Llm) => Self::Llm,
            Ok(ModelType::Embedding) => Self::Embedding,
            Ok(ModelType::Stt) => Self::Stt,
            Ok(ModelType::Tts) => Self::Tts,
            Err(_) => Self::Unknown,
        }
    }
}

impl From<ModelTypeView> for ModelType {
    fn from(kind: ModelTypeView) -> Self {
        match kind {
            ModelTypeView::Unspecified | ModelTypeView::Unknown => Self::Unspecified,
            ModelTypeView::Llm => Self::Llm,
            ModelTypeView::Embedding => Self::Embedding,
            ModelTypeView::Stt => Self::Stt,
            ModelTypeView::Tts => Self::Tts,
        }
    }
}

impl From<i32> for McpServerTypeView {
    fn from(value: i32) -> Self {
        match McpServerType::try_from(value) {
            Ok(McpServerType::Unspecified) => Self::Unspecified,
            Ok(McpServerType::Stdio) => Self::Stdio,
            Ok(McpServerType::Sse) => Self::Sse,
            Err(_) => Self::Unknown,
        }
    }
}

impl From<McpServerTypeView> for McpServerType {
    fn from(kind: McpServerTypeView) -> Self {
        match kind {
            McpServerTypeView::Unspecified | McpServerTypeView::Unknown => Self::Unspecified,
            McpServerTypeView::Stdio => Self::Stdio,
            McpServerTypeView::Sse => Self::Sse,
        }
    }
}

impl From<ModelOptions> for ModelOptionsView {
    fn from(options: ModelOptions) -> Self {
        Self {
            temperature: options.temperature,
            top_p: options.top_p,
            top_k: options.top_k,
            min_p: options.min_p,
            presence_penalty: options.presence_penalty,
            repetition_penalty: options.repetition_penalty,
        }
    }
}

impl From<ModelOptionsView> for ModelOptions {
    fn from(view: ModelOptionsView) -> Self {
        Self {
            temperature: view.temperature,
            top_p: view.top_p,
            top_k: view.top_k,
            min_p: view.min_p,
            presence_penalty: view.presence_penalty,
            repetition_penalty: view.repetition_penalty,
        }
    }
}

impl From<ModelLimits> for ModelLimitsView {
    fn from(limits: ModelLimits) -> Self {
        Self {
            context: limits.context,
            output: limits.output,
        }
    }
}

impl From<ModelLimitsView> for ModelLimits {
    fn from(view: ModelLimitsView) -> Self {
        Self {
            context: view.context,
            output: view.output,
        }
    }
}

impl From<ModelCapabilities> for ModelCapabilitiesView {
    fn from(capabilities: ModelCapabilities) -> Self {
        Self {
            tools: capabilities.tools,
            image: capabilities.image,
        }
    }
}

impl From<ModelCapabilitiesView> for ModelCapabilities {
    fn from(view: ModelCapabilitiesView) -> Self {
        Self {
            tools: view.tools,
            image: view.image,
        }
    }
}

impl From<Model> for ModelView {
    fn from(model: Model) -> Self {
        Self {
            name: model.name,
            kind: ModelTypeView::from(model.r#type),
            options: model.options.unwrap_or_default().into(),
            limits: model.limits.unwrap_or_default().into(),
            capabilities: model.capabilities.unwrap_or_default().into(),
        }
    }
}

impl From<ModelView> for Model {
    fn from(view: ModelView) -> Self {
        Self {
            name: view.name,
            r#type: ModelType::from(view.kind) as i32,
            options: Some(view.options.into()),
            limits: Some(view.limits.into()),
            capabilities: Some(view.capabilities.into()),
        }
    }
}

impl From<McpServer> for McpServerView {
    fn from(server: McpServer) -> Self {
        Self {
            name: server.name,
            kind: McpServerTypeView::from(server.r#type),
            command: server.command,
            url: server.url,
        }
    }
}

impl From<McpServerView> for McpServer {
    fn from(view: McpServerView) -> Self {
        Self {
            name: view.name,
            r#type: McpServerType::from(view.kind) as i32,
            command: view.command,
            url: view.url,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, fs, path::PathBuf};

    use ts_rs::TS as _;

    use crate::{ClientCommand, ClientSnapshot, ConfigSnapshot, OperationSnapshot, StatusSnapshot};

    use super::*;

    #[test]
    fn config_view_round_trips_known_provider_types() {
        let config = Configuration {
            providers: HashMap::from([(
                "anthropic".to_owned(),
                Provider {
                    r#type: ProviderType::Anthropic as i32,
                    name: "anthropic".to_owned(),
                    url: "https://api.anthropic.com".to_owned(),
                    headers: HashMap::from([("x-test".to_owned(), "true".to_owned())]),
                    models: HashMap::new(),
                },
            )]),
            etag: "v1".to_owned(),
            mcp_servers: HashMap::new(),
        };

        let view = ConfigView::from(config.clone());
        assert_eq!(
            view.providers["anthropic"].kind,
            ProviderKindView::Anthropic
        );

        let round_trip = Configuration::from(view);
        assert_eq!(round_trip, config);
    }

    #[test]
    fn config_view_maps_unknown_provider_type_to_unknown() {
        let provider = Provider {
            r#type: 99,
            ..Provider::default()
        };

        assert_eq!(ProviderView::from(provider).kind, ProviderKindView::Unknown);
    }

    #[test]
    fn exports_web_typescript_bindings() {
        let bindings = [
            ProviderKindView::decl(),
            ModelTypeView::decl(),
            ModelOptionsView::decl(),
            ModelLimitsView::decl(),
            ModelCapabilitiesView::decl(),
            ModelView::decl(),
            McpServerTypeView::decl(),
            McpServerView::decl(),
            ProviderView::decl(),
            ConfigView::decl(),
            OperationSnapshot::decl(),
            ConfigSnapshot::decl(),
            StatusSnapshot::decl(),
            ClientSnapshot::decl(),
            ClientCommand::decl(),
        ]
        .join("\n\n")
        .lines()
        .map(|line| {
            line.strip_prefix("type ").map_or_else(
                || line.to_owned(),
                |declaration| format!("export type {declaration}"),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
        let bindings =
            format!("// Generated by waltr-client. Do not edit directly.\n\n{bindings}\n");

        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../waltr-web/src/generated/waltr-client.ts");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("generated types directory should be created");
        }
        fs::write(path, bindings).expect("generated TypeScript bindings should be written");
    }
}
