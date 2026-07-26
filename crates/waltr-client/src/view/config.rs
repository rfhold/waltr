use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{
    CatalogMatchSource, Configuration, McpServer, McpServerType, Model, ModelCapabilities,
    ModelCatalogMatch, ModelCost, ModelLimits, ModelOptions, ModelType, OAuthProvider,
    OauthJwtMetadata, OauthTokenField, OauthTokenRequestStyle, OauthUrlParam, Provider,
    ProviderType, RuntimeDefaults,
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
    /// Default runtime selection for newly created sessions.
    pub runtime_defaults: Option<RuntimeDefaultsView>,
}

/// Stable runtime defaults view shared by Waltr shells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeDefaultsView {
    /// Default provider key.
    pub provider_key: String,
    /// Default model id within the selected provider.
    pub model_id: String,
    /// Whether all configured MCP servers are enabled by default.
    pub mcp_default_enabled: bool,
    /// Explicit MCP server keys enabled by default.
    pub enabled_mcp_server_keys: Vec<String>,
    /// Default reasoning effort for newly created sessions.
    pub reasoning_effort: Option<String>,
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
    /// Whether this provider can authenticate with OAuth.
    pub oauth_compatible: bool,
    /// Inline OAuth settings for custom OAuth-compatible providers.
    pub oauth_settings: Option<OAuthProviderView>,
    /// Models offered by this provider, keyed by stable id.
    pub models: BTreeMap<String, ModelView>,
}

/// Stable OAuth provider definition view shared by Waltr shells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct OAuthProviderView {
    /// OAuth provider display name.
    pub name: String,
    /// Authorization endpoint URL.
    pub authorize_url: String,
    /// Token endpoint URL.
    pub token_url: String,
    /// Public OAuth client id.
    pub client_id: String,
    /// Redirect URI shown to the provider.
    pub redirect_uri: String,
    /// OAuth scopes.
    pub scopes: Vec<String>,
    /// Token request encoding style.
    pub token_request_style: OAuthTokenRequestStyleView,
    /// Additional authorization URL parameters.
    pub auth_url_params: Vec<OAuthUrlParamView>,
    /// JWT metadata extraction rules.
    pub jwt_metadata: Vec<OAuthJwtMetadataView>,
    /// Whether authorization-code exchange sends the state value.
    pub send_state_in_token_request: bool,
}

/// Stable OAuth token request style view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum OAuthTokenRequestStyleView {
    /// Token request style was omitted or intentionally unspecified.
    Unspecified,
    /// JSON token request body.
    Json,
    /// Form-url-encoded token request body.
    FormUrlencoded,
    /// Token request style is unknown to this client version.
    Unknown,
}

/// Stable OAuth authorization URL parameter view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct OAuthUrlParamView {
    /// Query parameter key.
    pub key: String,
    /// Query parameter value.
    pub value: String,
}

/// Stable OAuth JWT metadata extraction view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct OAuthJwtMetadataView {
    /// Token to decode.
    pub token: OAuthTokenFieldView,
    /// Metadata key to store.
    pub metadata_key: String,
    /// JWT claim names to try in order.
    pub claim_names: Vec<String>,
    /// Whether this metadata is required for login success.
    pub required: bool,
}

/// Stable OAuth JWT token field view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum OAuthTokenFieldView {
    /// Token field was omitted or intentionally unspecified.
    Unspecified,
    /// Access token.
    Access,
    /// ID token.
    Id,
    /// Token field is unknown to this client version.
    Unknown,
}

/// Stable provider kind view shared by Waltr shells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ProviderKindView {
    /// Provider kind was omitted or intentionally unspecified.
    Unspecified,
    /// Anthropic Messages API provider.
    AnthropicMessages,
    /// `OpenAI` Chat Completions API provider.
    OpenAiChatCompletions,
    /// `OpenAI` Responses API provider over HTTP.
    OpenAiResponses,
    /// `OpenAI` Responses API provider over WebSocket.
    OpenAiResponsesWebsocket,
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
    /// Supported reasoning effort levels configured for this model.
    pub reasoning_efforts: Vec<String>,
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

/// Stable model pricing view shared by Waltr shells.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostView {
    /// Input token price per million tokens.
    pub input_per_million_tokens: Option<f64>,
    /// Output token price per million tokens.
    pub output_per_million_tokens: Option<f64>,
    /// Cache-read token price per million tokens.
    pub cache_read_per_million_tokens: Option<f64>,
    /// Cache-write token price per million tokens.
    pub cache_write_per_million_tokens: Option<f64>,
    /// Image input token price per million tokens.
    pub image_input_per_million_tokens: Option<f64>,
    /// Image input price per image.
    pub image_input_per_image: Option<f64>,
    /// Currency code for configured rates.
    pub currency: String,
}

/// Stable catalog match source view shared by Waltr shells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum CatalogMatchSourceView {
    /// Match source was omitted or intentionally unspecified.
    Unspecified,
    /// Match was inferred automatically.
    Auto,
    /// Match was chosen manually.
    Manual,
    /// Match was set while applying catalog defaults.
    Applied,
    /// Match source is unknown to this client version.
    Unknown,
}

/// Stable model catalog linkage view shared by Waltr shells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalogMatchView {
    /// Matched models.dev serving provider id.
    pub provider_id: String,
    /// Matched models.dev serving model id.
    pub model_id: String,
    /// Optional upstream provider identity for display and drift checks.
    pub upstream_provider_id: Option<String>,
    /// Optional upstream model identity for display and drift checks.
    pub upstream_model_id: Option<String>,
    /// How this linkage was selected.
    pub source: CatalogMatchSourceView,
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
    /// Optional models.dev catalog linkage.
    pub catalog_match: Option<ModelCatalogMatchView>,
    /// Optional model pricing used for session cost tracking.
    pub cost: Option<ModelCostView>,
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
            runtime_defaults: config.runtime_defaults.map(RuntimeDefaultsView::from),
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
            runtime_defaults: view.runtime_defaults.map(RuntimeDefaults::from),
        }
    }
}

impl From<RuntimeDefaults> for RuntimeDefaultsView {
    fn from(defaults: RuntimeDefaults) -> Self {
        Self {
            provider_key: defaults.provider_key,
            model_id: defaults.model_key,
            mcp_default_enabled: defaults.mcp_default_enabled.unwrap_or(false),
            enabled_mcp_server_keys: defaults.enabled_mcp_server_keys,
            reasoning_effort: defaults.reasoning_effort,
        }
    }
}

impl From<RuntimeDefaultsView> for RuntimeDefaults {
    fn from(view: RuntimeDefaultsView) -> Self {
        Self {
            provider_key: view.provider_key,
            model_key: view.model_id,
            mcp_default_enabled: Some(view.mcp_default_enabled),
            enabled_mcp_server_keys: view.enabled_mcp_server_keys,
            reasoning_effort: view.reasoning_effort,
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
            oauth_compatible: provider.oauth_compatible,
            oauth_settings: provider.oauth_settings.map(OAuthProviderView::from),
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
            oauth_compatible: view.oauth_compatible,
            oauth_settings: view.oauth_settings.map(OAuthProvider::from),
            models: view
                .models
                .into_iter()
                .map(|(id, model)| (id, Model::from(model)))
                .collect(),
        }
    }
}

impl From<OAuthProvider> for OAuthProviderView {
    fn from(provider: OAuthProvider) -> Self {
        Self {
            name: provider.name,
            authorize_url: provider.authorize_url,
            token_url: provider.token_url,
            client_id: provider.client_id,
            redirect_uri: provider.redirect_uri,
            scopes: provider.scopes,
            token_request_style: provider.token_request_style.into(),
            auth_url_params: provider
                .auth_url_params
                .into_iter()
                .map(OAuthUrlParamView::from)
                .collect(),
            jwt_metadata: provider
                .jwt_metadata
                .into_iter()
                .map(OAuthJwtMetadataView::from)
                .collect(),
            send_state_in_token_request: provider.send_state_in_token_request,
        }
    }
}

impl From<OAuthProviderView> for OAuthProvider {
    fn from(view: OAuthProviderView) -> Self {
        Self {
            name: view.name,
            authorize_url: view.authorize_url,
            token_url: view.token_url,
            client_id: view.client_id,
            redirect_uri: view.redirect_uri,
            scopes: view.scopes,
            token_request_style: OauthTokenRequestStyle::from(view.token_request_style) as i32,
            auth_url_params: view
                .auth_url_params
                .into_iter()
                .map(OauthUrlParam::from)
                .collect(),
            jwt_metadata: view
                .jwt_metadata
                .into_iter()
                .map(OauthJwtMetadata::from)
                .collect(),
            send_state_in_token_request: view.send_state_in_token_request,
        }
    }
}

impl From<OauthUrlParam> for OAuthUrlParamView {
    fn from(param: OauthUrlParam) -> Self {
        Self {
            key: param.key,
            value: param.value,
        }
    }
}

impl From<OAuthUrlParamView> for OauthUrlParam {
    fn from(view: OAuthUrlParamView) -> Self {
        Self {
            key: view.key,
            value: view.value,
        }
    }
}

impl From<OauthJwtMetadata> for OAuthJwtMetadataView {
    fn from(metadata: OauthJwtMetadata) -> Self {
        Self {
            token: metadata.token.into(),
            metadata_key: metadata.metadata_key,
            claim_names: metadata.claim_names,
            required: metadata.required,
        }
    }
}

impl From<OAuthJwtMetadataView> for OauthJwtMetadata {
    fn from(view: OAuthJwtMetadataView) -> Self {
        Self {
            token: OauthTokenField::from(view.token) as i32,
            metadata_key: view.metadata_key,
            claim_names: view.claim_names,
            required: view.required,
        }
    }
}

impl From<i32> for ProviderKindView {
    fn from(value: i32) -> Self {
        match ProviderType::try_from(value) {
            Ok(ProviderType::Unspecified) => Self::Unspecified,
            Ok(ProviderType::AnthropicMessages) => Self::AnthropicMessages,
            Ok(ProviderType::OpenaiChatCompletions) => Self::OpenAiChatCompletions,
            Ok(ProviderType::OpenaiResponses) => Self::OpenAiResponses,
            Ok(ProviderType::OpenaiResponsesWebsocket) => Self::OpenAiResponsesWebsocket,
            Err(_) => Self::Unknown,
        }
    }
}

impl From<ProviderKindView> for ProviderType {
    fn from(kind: ProviderKindView) -> Self {
        match kind {
            ProviderKindView::Unspecified | ProviderKindView::Unknown => Self::Unspecified,
            ProviderKindView::AnthropicMessages => Self::AnthropicMessages,
            ProviderKindView::OpenAiChatCompletions => Self::OpenaiChatCompletions,
            ProviderKindView::OpenAiResponses => Self::OpenaiResponses,
            ProviderKindView::OpenAiResponsesWebsocket => Self::OpenaiResponsesWebsocket,
        }
    }
}

impl From<i32> for OAuthTokenRequestStyleView {
    fn from(value: i32) -> Self {
        match OauthTokenRequestStyle::try_from(value) {
            Ok(OauthTokenRequestStyle::Unspecified) => Self::Unspecified,
            Ok(OauthTokenRequestStyle::Json) => Self::Json,
            Ok(OauthTokenRequestStyle::FormUrlencoded) => Self::FormUrlencoded,
            Err(_) => Self::Unknown,
        }
    }
}

impl From<OAuthTokenRequestStyleView> for OauthTokenRequestStyle {
    fn from(style: OAuthTokenRequestStyleView) -> Self {
        match style {
            OAuthTokenRequestStyleView::Unspecified | OAuthTokenRequestStyleView::Unknown => {
                Self::Unspecified
            }
            OAuthTokenRequestStyleView::Json => Self::Json,
            OAuthTokenRequestStyleView::FormUrlencoded => Self::FormUrlencoded,
        }
    }
}

impl From<i32> for OAuthTokenFieldView {
    fn from(value: i32) -> Self {
        match OauthTokenField::try_from(value) {
            Ok(OauthTokenField::Unspecified) => Self::Unspecified,
            Ok(OauthTokenField::Access) => Self::Access,
            Ok(OauthTokenField::Id) => Self::Id,
            Err(_) => Self::Unknown,
        }
    }
}

impl From<OAuthTokenFieldView> for OauthTokenField {
    fn from(field: OAuthTokenFieldView) -> Self {
        match field {
            OAuthTokenFieldView::Unspecified | OAuthTokenFieldView::Unknown => Self::Unspecified,
            OAuthTokenFieldView::Access => Self::Access,
            OAuthTokenFieldView::Id => Self::Id,
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

impl From<i32> for CatalogMatchSourceView {
    fn from(value: i32) -> Self {
        match CatalogMatchSource::try_from(value) {
            Ok(CatalogMatchSource::Unspecified) => Self::Unspecified,
            Ok(CatalogMatchSource::Auto) => Self::Auto,
            Ok(CatalogMatchSource::Manual) => Self::Manual,
            Ok(CatalogMatchSource::Applied) => Self::Applied,
            Err(_) => Self::Unknown,
        }
    }
}

impl From<CatalogMatchSourceView> for CatalogMatchSource {
    fn from(source: CatalogMatchSourceView) -> Self {
        match source {
            CatalogMatchSourceView::Unspecified | CatalogMatchSourceView::Unknown => {
                Self::Unspecified
            }
            CatalogMatchSourceView::Auto => Self::Auto,
            CatalogMatchSourceView::Manual => Self::Manual,
            CatalogMatchSourceView::Applied => Self::Applied,
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
            reasoning_efforts: options.reasoning_efforts,
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
            reasoning_efforts: view.reasoning_efforts,
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

impl From<ModelCost> for ModelCostView {
    fn from(cost: ModelCost) -> Self {
        Self {
            input_per_million_tokens: cost.input_per_million_tokens,
            output_per_million_tokens: cost.output_per_million_tokens,
            cache_read_per_million_tokens: cost.cache_read_per_million_tokens,
            cache_write_per_million_tokens: cost.cache_write_per_million_tokens,
            image_input_per_million_tokens: cost.image_input_per_million_tokens,
            image_input_per_image: cost.image_input_per_image,
            currency: cost.currency,
        }
    }
}

impl From<ModelCostView> for ModelCost {
    fn from(view: ModelCostView) -> Self {
        Self {
            input_per_million_tokens: view.input_per_million_tokens,
            output_per_million_tokens: view.output_per_million_tokens,
            cache_read_per_million_tokens: view.cache_read_per_million_tokens,
            cache_write_per_million_tokens: view.cache_write_per_million_tokens,
            image_input_per_million_tokens: view.image_input_per_million_tokens,
            image_input_per_image: view.image_input_per_image,
            currency: view.currency,
        }
    }
}

impl From<ModelCatalogMatch> for ModelCatalogMatchView {
    fn from(catalog_match: ModelCatalogMatch) -> Self {
        Self {
            provider_id: catalog_match.provider_id,
            model_id: catalog_match.model_id,
            upstream_provider_id: catalog_match.upstream_provider_id,
            upstream_model_id: catalog_match.upstream_model_id,
            source: CatalogMatchSourceView::from(catalog_match.source),
        }
    }
}

impl From<ModelCatalogMatchView> for ModelCatalogMatch {
    fn from(view: ModelCatalogMatchView) -> Self {
        Self {
            provider_id: view.provider_id,
            model_id: view.model_id,
            upstream_provider_id: view.upstream_provider_id,
            upstream_model_id: view.upstream_model_id,
            source: CatalogMatchSource::from(view.source) as i32,
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
            catalog_match: model.catalog_match.map(ModelCatalogMatchView::from),
            cost: model.cost.map(ModelCostView::from),
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
            catalog_match: view.catalog_match.map(ModelCatalogMatch::from),
            cost: view.cost.map(ModelCost::from),
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
