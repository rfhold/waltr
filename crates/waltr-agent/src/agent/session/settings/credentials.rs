use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// Durable provider credential stored outside shared config.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StoredCredential {
    /// OAuth access and refresh tokens.
    OAuth(OAuthCredential),
}

/// OAuth token material and non-secret metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCredential {
    /// OAuth access token.
    pub access: String,
    /// OAuth refresh token.
    pub refresh: String,
    /// Expiration timestamp in Unix milliseconds.
    pub expires_at_ms: i64,
    /// Non-secret provider-specific metadata.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// File-backed provider credential store.
#[derive(Debug, Clone)]
pub struct FileCredentialStore {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl FileCredentialStore {
    /// Create a store at `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Arc::new(Mutex::new(())),
        }
    }

    /// Create a store from `WALTR_AUTH_PATH` or the user data default.
    #[must_use]
    pub fn from_env() -> Self {
        if let Some(path) = std::env::var_os("WALTR_AUTH_PATH") {
            return Self::new(path);
        }
        let home = std::env::var_os("HOME").unwrap_or_else(|| ".".into());
        Self::new(PathBuf::from(home).join(".local/share/waltr/auth.json"))
    }

    /// Return the configured path.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Read a provider credential.
    pub async fn read(
        &self,
        provider_key: &str,
    ) -> Result<Option<StoredCredential>, std::io::Error> {
        let _guard = self.lock.lock().await;
        let credentials = self.read_unlocked().await?;
        Ok(credentials.get(provider_key).cloned())
    }

    /// Serialize a read-modify-write for a provider credential.
    pub async fn modify<F>(
        &self,
        provider_key: &str,
        f: F,
    ) -> Result<Option<StoredCredential>, std::io::Error>
    where
        F: FnOnce(Option<StoredCredential>) -> Option<StoredCredential>,
    {
        let _guard = self.lock.lock().await;
        let mut credentials = self.read_unlocked().await?;
        let next = f(credentials.get(provider_key).cloned());
        match &next {
            Some(credential) => {
                credentials.insert(provider_key.to_owned(), credential.clone());
            }
            None => {
                credentials.remove(provider_key);
            }
        }
        self.write_unlocked(&credentials).await?;
        Ok(next)
    }

    /// Delete a provider credential.
    pub async fn delete(&self, provider_key: &str) -> Result<(), std::io::Error> {
        self.modify(provider_key, |_| None).await.map(|_| ())
    }

    async fn read_unlocked(&self) -> Result<BTreeMap<String, StoredCredential>, std::io::Error> {
        match tokio::fs::read(&self.path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(error),
        }
    }

    async fn write_unlocked(
        &self,
        credentials: &BTreeMap<String, StoredCredential>,
    ) -> Result<(), std::io::Error> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await?;
            }
        }
        let bytes = serde_json::to_vec_pretty(credentials).map_err(std::io::Error::other)?;
        let tmp = self.path.with_extension("json.tmp");
        tokio::fs::write(&tmp, bytes).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
        }
        tokio::fs::rename(tmp, &self.path).await
    }
}
