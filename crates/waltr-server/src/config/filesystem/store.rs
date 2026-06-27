//! Filesystem-backed TOML configuration store.

use crate::{
    auth::oauth,
    config::{ConfigError, ConfigPatch, ConfigStore, default_configuration},
};
use waltr::grpc::waltr::config::v1::{
    Configuration, McpServer, McpServerType, Model, ModelType, OAuthProvider, OauthTokenField,
    OauthTokenRequestStyle, Provider, RuntimeDefaults,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fmt::Write as _, io, path::PathBuf, sync::Arc};
use tokio::sync::Mutex;

const CONFIG_DIR: &str = ".waltr";
const CONFIG_FILE: &str = "waltr.toml";

/// Filesystem-backed TOML configuration store.
#[derive(Clone, Debug)]
pub struct FilesystemConfigStore {
    root: Arc<PathBuf>,
    write_lock: Arc<Mutex<()>>,
}

impl FilesystemConfigStore {
    /// Build a store rooted at `root`.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        tracing::debug!(root = %root.display(), "creating filesystem config store");
        Self {
            root: Arc::new(root),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn config_dir(&self) -> PathBuf {
        self.root.join(CONFIG_DIR)
    }

    pub(crate) fn config_path(&self) -> PathBuf {
        self.config_dir().join(CONFIG_FILE)
    }

    pub(crate) fn root_path(&self) -> &PathBuf {
        self.root.as_ref()
    }
}

impl ConfigStore for FilesystemConfigStore {
    async fn load(&self) -> Result<Configuration, ConfigError> {
        if let Some(config) = self.load_unlocked().await? {
            return Ok(config);
        }

        let _guard = self.write_lock.lock().await;
        if let Some(config) = self.load_unlocked().await? {
            return Ok(config);
        }
        let path = self.config_path();
        tracing::debug!(path = %path.display(), "config file missing; creating default config");
        self.save_unlocked(default_configuration()).await
    }

    async fn save(&self, config: Configuration) -> Result<Configuration, ConfigError> {
        tracing::debug!("saving config file");
        let _guard = self.write_lock.lock().await;
        validate_config(&config)?;
        self.save_unlocked(config).await
    }

    async fn patch(
        &self,
        patch: ConfigPatch,
        expected_etag: &str,
    ) -> Result<Configuration, ConfigError> {
        let _guard = self.write_lock.lock().await;
        let current = if let Some(config) = self.load_unlocked().await? {
            config
        } else {
            let path = self.config_path();
            tracing::debug!(path = %path.display(), "config file missing during patch; creating default config");
            self.save_unlocked(default_configuration()).await?
        };

        if current.etag != expected_etag {
            tracing::warn!(expected_etag, current_etag = %current.etag, "config patch rejected due to etag mismatch");
            return Err(ConfigError::etag_mismatch(expected_etag, &current.etag));
        }

        tracing::debug!(etag = %current.etag, "applying config patch");
        let mut next = current;
        if let Some(providers) = patch.providers {
            next.providers = providers;
        }
        if let Some(mcp_servers) = patch.mcp_servers {
            next.mcp_servers = mcp_servers;
        }
        if let Some(runtime_defaults) = patch.runtime_defaults {
            next.runtime_defaults = Some(runtime_defaults);
        }
        validate_config(&next)?;
        self.save_unlocked(next).await
    }
}

impl FilesystemConfigStore {
    async fn load_unlocked(&self) -> Result<Option<Configuration>, ConfigError> {
        let path = self.config_path();
        tracing::debug!(path = %path.display(), "loading config file");
        let data = match tokio::fs::read_to_string(&path).await {
            Ok(data) => data,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "failed to read config file");
                return Err(ConfigError::read(path, error));
            }
        };

        let mut config = toml::from_str::<Configuration>(&data).map_err(|error| {
            tracing::warn!(path = %path.display(), %error, "failed to parse config file");
            ConfigError::parse(path.clone(), error)
        })?;
        config.etag.clear();
        validate_config(&config)?;
        let config = with_etag(config)?;
        tracing::debug!(path = %path.display(), etag = %config.etag, "loaded config file");
        Ok(Some(config))
    }

    async fn save_unlocked(&self, mut config: Configuration) -> Result<Configuration, ConfigError> {
        config.etag.clear();
        let persisted = serialize_config(&config)?;
        let path = self.config_path();
        let temp_path = path.with_extension("toml.tmp");
        tracing::debug!(path = %path.display(), temp_path = %temp_path.display(), "writing config file");

        tokio::fs::create_dir_all(self.config_dir())
            .await
            .map_err(|error| ConfigError::write(self.config_dir(), error))?;
        tokio::fs::write(&temp_path, persisted)
            .await
            .map_err(|error| ConfigError::write(temp_path.clone(), error))?;
        tokio::fs::rename(&temp_path, &path)
            .await
            .map_err(|error| ConfigError::write(path.clone(), error))?;

        let config = with_etag(config)?;
        tracing::info!(path = %path.display(), etag = %config.etag, "saved config file");
        Ok(config)
    }
}

fn with_etag(mut config: Configuration) -> Result<Configuration, ConfigError> {
    config.etag.clear();
    let serialized = serialize_config(&config)?;
    let hash = Sha256::digest(serialized.as_bytes());
    config.etag = String::from("sha256:");
    for byte in hash {
        write!(&mut config.etag, "{byte:02x}").expect("writing to a string cannot fail");
    }
    Ok(config)
}

fn serialize_config(config: &Configuration) -> Result<String, ConfigError> {
    toml::to_string_pretty(&CanonicalConfiguration::from(config)).map_err(ConfigError::serialize)
}

#[derive(Serialize)]
struct CanonicalConfiguration<'a> {
    providers: BTreeMap<&'a str, CanonicalProvider<'a>>,
    etag: &'a str,
    mcp_servers: BTreeMap<&'a str, &'a McpServer>,
    runtime_defaults: &'a Option<RuntimeDefaults>,
}

impl<'a> From<&'a Configuration> for CanonicalConfiguration<'a> {
    fn from(config: &'a Configuration) -> Self {
        Self {
            providers: config
                .providers
                .iter()
                .map(|(key, provider)| (key.as_str(), CanonicalProvider::from(provider)))
                .collect(),
            etag: config.etag.as_str(),
            mcp_servers: config
                .mcp_servers
                .iter()
                .map(|(key, server)| (key.as_str(), server))
                .collect(),
            runtime_defaults: &config.runtime_defaults,
        }
    }
}

#[derive(Serialize)]
struct CanonicalProvider<'a> {
    #[serde(rename = "type")]
    provider_type: i32,
    name: &'a str,
    url: &'a str,
    headers: BTreeMap<&'a str, &'a str>,
    models: BTreeMap<&'a str, &'a Model>,
    oauth_compatible: bool,
    oauth_settings: &'a Option<OAuthProvider>,
}

impl<'a> From<&'a Provider> for CanonicalProvider<'a> {
    fn from(provider: &'a Provider) -> Self {
        Self {
            provider_type: provider.r#type,
            name: provider.name.as_str(),
            url: provider.url.as_str(),
            headers: provider
                .headers
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect(),
            models: provider
                .models
                .iter()
                .map(|(key, model)| (key.as_str(), model))
                .collect(),
            oauth_compatible: provider.oauth_compatible,
            oauth_settings: &provider.oauth_settings,
        }
    }
}

fn validate_config(config: &Configuration) -> Result<(), ConfigError> {
    for (name, provider) in &config.providers {
        validate_provider_name(name)?;
        if provider.name != *name {
            return Err(ConfigError::invalid(format!(
                "provider key '{name}' must match provider name"
            )));
        }
        validate_provider_oauth(name, provider)?;
        validate_provider_models(name, provider)?;
    }

    validate_mcp_servers(config)?;
    validate_runtime_defaults(config)?;

    Ok(())
}

fn validate_provider_oauth(key: &str, provider: &Provider) -> Result<(), ConfigError> {
    if !provider.oauth_compatible {
        return Ok(());
    }

    let Some(settings) = &provider.oauth_settings else {
        return Err(ConfigError::invalid(format!(
            "provider '{key}' is OAuth compatible but requires OAuth settings"
        )));
    };
    if oauth::definition_is_empty(settings) {
        return Err(ConfigError::invalid(format!(
            "provider '{key}' is OAuth compatible but requires OAuth settings"
        )));
    }

    validate_oauth_provider(key, settings)
}

fn validate_oauth_provider(key: &str, provider: &OAuthProvider) -> Result<(), ConfigError> {
    for (field, value) in [
        ("authorize_url", provider.authorize_url.as_str()),
        ("token_url", provider.token_url.as_str()),
        ("client_id", provider.client_id.as_str()),
        ("redirect_uri", provider.redirect_uri.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(ConfigError::invalid(format!(
                "OAuth settings for provider '{key}' must have {field}"
            )));
        }
    }

    match OauthTokenRequestStyle::try_from(provider.token_request_style) {
        Ok(OauthTokenRequestStyle::Json | OauthTokenRequestStyle::FormUrlencoded) => {}
        Ok(OauthTokenRequestStyle::Unspecified) | Err(_) => {
            return Err(ConfigError::invalid(format!(
                "OAuth settings for provider '{key}' must have a token request style"
            )));
        }
    }

    for param in &provider.auth_url_params {
        if param.key.trim().is_empty() {
            return Err(ConfigError::invalid(format!(
                "OAuth settings for provider '{key}' has an auth URL parameter without a key"
            )));
        }
    }

    for metadata in &provider.jwt_metadata {
        if metadata.metadata_key.trim().is_empty() {
            return Err(ConfigError::invalid(format!(
                "OAuth settings for provider '{key}' has JWT metadata without a metadata key"
            )));
        }
        if metadata.claim_names.is_empty() {
            return Err(ConfigError::invalid(format!(
                "OAuth settings for provider '{key}' has JWT metadata without claim names"
            )));
        }
        match OauthTokenField::try_from(metadata.token) {
            Ok(OauthTokenField::Access | OauthTokenField::Id) => {}
            Ok(OauthTokenField::Unspecified) | Err(_) => {
                return Err(ConfigError::invalid(format!(
                    "OAuth settings for provider '{key}' has JWT metadata without a token field"
                )));
            }
        }
    }

    Ok(())
}

fn validate_runtime_defaults(config: &Configuration) -> Result<(), ConfigError> {
    let Some(defaults) = &config.runtime_defaults else {
        return Ok(());
    };

    if defaults.provider_key.trim().is_empty() && defaults.model_key.trim().is_empty() {
        validate_default_mcp_keys(defaults, config)?;
        return Ok(());
    }

    let provider = config
        .providers
        .get(&defaults.provider_key)
        .ok_or_else(|| {
            ConfigError::invalid(format!(
                "runtime default provider '{}' does not exist",
                defaults.provider_key
            ))
        })?;

    if defaults.model_key.trim().is_empty() {
        return Err(ConfigError::invalid(
            "runtime default model key must not be empty when provider key is set",
        ));
    }

    let model = provider.models.get(&defaults.model_key).ok_or_else(|| {
        ConfigError::invalid(format!(
            "runtime default model '{}' does not exist in provider '{}'",
            defaults.model_key, defaults.provider_key
        ))
    })?;

    if let Some(effort) = defaults
        .reasoning_effort
        .as_ref()
        .map(|effort| effort.trim())
        .filter(|effort| !effort.is_empty())
    {
        let supported = model.options.as_ref().is_some_and(|options| {
            options
                .reasoning_efforts
                .iter()
                .any(|value| value == effort)
        });
        if !supported {
            return Err(ConfigError::invalid(format!(
                "runtime default reasoning effort '{effort}' does not exist in model '{}' for provider '{}'",
                defaults.model_key, defaults.provider_key
            )));
        }
    }

    validate_default_mcp_keys(defaults, config)
}

fn validate_default_mcp_keys(
    defaults: &RuntimeDefaults,
    config: &Configuration,
) -> Result<(), ConfigError> {
    for key in &defaults.enabled_mcp_server_keys {
        if !config.mcp_servers.contains_key(key) {
            return Err(ConfigError::invalid(format!(
                "runtime default mcp server '{key}' does not exist"
            )));
        }
    }

    Ok(())
}

fn validate_provider_models(provider_key: &str, provider: &Provider) -> Result<(), ConfigError> {
    for (model_key, model) in &provider.models {
        match ModelType::try_from(model.r#type) {
            Ok(ModelType::Unspecified) | Err(_) => {
                return Err(ConfigError::invalid(format!(
                    "model '{model_key}' in provider '{provider_key}' must have a defined type"
                )));
            }
            Ok(_) => {}
        }
    }

    Ok(())
}

fn validate_mcp_servers(config: &Configuration) -> Result<(), ConfigError> {
    for (key, server) in &config.mcp_servers {
        match McpServerType::try_from(server.r#type) {
            Ok(McpServerType::Stdio) => validate_stdio_server(key, server)?,
            Ok(McpServerType::Sse) => validate_sse_server(key, server)?,
            Ok(McpServerType::Unspecified) | Err(_) => {
                return Err(ConfigError::invalid(format!(
                    "mcp server '{key}' must have a defined transport type"
                )));
            }
        }
    }

    Ok(())
}

fn validate_stdio_server(key: &str, server: &McpServer) -> Result<(), ConfigError> {
    if server.command.trim().is_empty() {
        return Err(ConfigError::invalid(format!(
            "stdio mcp server '{key}' must have a command"
        )));
    }

    Ok(())
}

fn validate_sse_server(key: &str, server: &McpServer) -> Result<(), ConfigError> {
    if server.url.trim().is_empty() {
        return Err(ConfigError::invalid(format!(
            "sse mcp server '{key}' must have a url"
        )));
    }

    Ok(())
}

fn validate_provider_name(name: &str) -> Result<(), ConfigError> {
    if name.is_empty() {
        return Err(ConfigError::invalid("provider name cannot be empty"));
    }

    if name.chars().any(char::is_whitespace) {
        return Err(ConfigError::invalid(format!(
            "provider name '{name}' cannot contain spaces"
        )));
    }

    if name.chars().any(char::is_uppercase) {
        return Err(ConfigError::invalid(format!(
            "provider name '{name}' must be lowercase"
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use waltr::grpc::waltr::config::v1::{Provider, ProviderType};

    #[tokio::test]
    async fn missing_file_creates_default_config_file() {
        let root = temp_root();
        let store = FilesystemConfigStore::new(root.clone());

        let config = store.load().await.expect("missing file should load");

        assert!(config.providers.contains_key("anthropic"));
        assert!(config.providers.contains_key("openai"));
        assert!(config.providers.contains_key("openai-codex"));
        assert!(!config.etag.is_empty());
        assert!(root.join(CONFIG_DIR).join(CONFIG_FILE).exists());
        assert!(
            config.providers["anthropic"]
                .oauth_settings
                .as_ref()
                .is_some_and(|settings| !settings.authorize_url.is_empty())
        );
    }

    #[tokio::test]
    async fn save_creates_config_file() {
        let root = temp_root();
        let store = FilesystemConfigStore::new(root.clone());

        let saved = store
            .save(config_with_provider("anthropic"))
            .await
            .expect("save should work");

        assert!(!saved.etag.is_empty());
        assert!(root.join(CONFIG_DIR).join(CONFIG_FILE).exists());
    }

    #[tokio::test]
    async fn load_defaults_missing_reasoning_efforts_to_empty_vec() {
        let root = temp_root();
        let config_dir = root.join(CONFIG_DIR);
        tokio::fs::create_dir_all(&config_dir)
            .await
            .expect("config dir should be created");
        tokio::fs::write(
            config_dir.join(CONFIG_FILE),
            r#"
etag = ""

[providers.anthropic]
type = 1
name = "anthropic"
url = "https://api.anthropic.com/v1/messages"
oauth_compatible = false

[providers.anthropic.headers]

[providers.anthropic.models.claude]
name = "Claude"
type = 1

[providers.anthropic.models.claude.options]

[providers.anthropic.models.claude.limits]

[providers.anthropic.models.claude.capabilities]
tools = true
image = false

[mcp_servers]
"#,
        )
        .await
        .expect("config file should be written");

        let config = FilesystemConfigStore::new(root)
            .load()
            .await
            .expect("old config should load");

        let reasoning_efforts = &config.providers["anthropic"].models["claude"]
            .options
            .as_ref()
            .expect("options should be present")
            .reasoning_efforts;
        assert!(reasoning_efforts.is_empty());
    }

    #[tokio::test]
    async fn patch_rejects_stale_etag() {
        let store = FilesystemConfigStore::new(temp_root());
        let current = store.load().await.expect("config should load");

        let error = store
            .patch(
                ConfigPatch {
                    providers: Some(config_with_provider("anthropic").providers),
                    mcp_servers: None,
                    runtime_defaults: None,
                },
                "stale",
            )
            .await
            .expect_err("stale patch should fail");

        assert!(error.is_etag_mismatch());
        assert_ne!(current.etag, "stale");
    }

    fn config_with_provider(name: &str) -> Configuration {
        Configuration {
            providers: std::collections::HashMap::from([(
                name.to_owned(),
                Provider {
                    r#type: ProviderType::AnthropicMessages as i32,
                    name: name.to_owned(),
                    ..Provider::default()
                },
            )]),
            ..Configuration::default()
        }
    }

    fn temp_root() -> PathBuf {
        crate::test_support::temp_root()
    }
}
