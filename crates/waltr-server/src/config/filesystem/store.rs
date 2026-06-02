//! Filesystem-backed TOML configuration store.

use crate::config::{ConfigError, ConfigPatch, ConfigStore};
use waltr::grpc::waltr::config::v1::{Configuration, McpServer, McpServerType, ModelType, Provider};
use sha2::{Digest, Sha256};
use std::{fmt::Write as _, io, path::PathBuf, sync::Arc};
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
        let path = self.config_path();
        tracing::debug!(path = %path.display(), "loading config file");
        let data = match tokio::fs::read_to_string(&path).await {
            Ok(data) => data,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "config file missing; using default config");
                return with_etag(Configuration::default());
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
        Ok(config)
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
        let current = self.load().await?;

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
        validate_config(&next)?;
        self.save_unlocked(next).await
    }
}

impl FilesystemConfigStore {
    async fn save_unlocked(&self, mut config: Configuration) -> Result<Configuration, ConfigError> {
        config.etag.clear();
        let persisted = toml::to_string_pretty(&config).map_err(ConfigError::serialize)?;
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
    let serialized = toml::to_string_pretty(&config).map_err(ConfigError::serialize)?;
    let hash = Sha256::digest(serialized.as_bytes());
    config.etag = String::from("sha256:");
    for byte in hash {
        write!(&mut config.etag, "{byte:02x}").expect("writing to a string cannot fail");
    }
    Ok(config)
}

fn validate_config(config: &Configuration) -> Result<(), ConfigError> {
    for (name, provider) in &config.providers {
        validate_provider_name(name)?;
        if provider.name != *name {
            return Err(ConfigError::invalid(format!(
                "provider key '{name}' must match provider name"
            )));
        }
        validate_provider_models(name, provider)?;
    }

    validate_mcp_servers(config)?;

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
    async fn missing_file_returns_empty_config() {
        let store = FilesystemConfigStore::new(temp_root());

        let config = store.load().await.expect("missing file should load");

        assert!(config.providers.is_empty());
        assert!(!config.etag.is_empty());
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
    async fn patch_rejects_stale_etag() {
        let store = FilesystemConfigStore::new(temp_root());
        let current = store.load().await.expect("config should load");

        let error = store
            .patch(
                ConfigPatch {
                    providers: Some(config_with_provider("anthropic").providers),
                    mcp_servers: None,
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
                    r#type: ProviderType::Anthropic as i32,
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
