//! Server session storage configuration.

use std::{env, error::Error, fmt, path::PathBuf};

const ENV_SESSION_STORE: &str = "WALTR_SESSION_STORE";
const ENV_FILESYSTEM_ROOT: &str = "WALTR_SESSION_FILESYSTEM_ROOT";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SessionStorageConfig {
    Memory,
    Filesystem { root: PathBuf },
}

impl SessionStorageConfig {
    pub(super) fn from_env() -> Result<Self, SessionStorageConfigError> {
        let store = env::var(ENV_SESSION_STORE).unwrap_or_else(|_| "memory".to_owned());
        Self::from_values(&store, env::var(ENV_FILESYSTEM_ROOT).ok())
    }

    fn from_values(
        store: &str,
        filesystem_root: Option<String>,
    ) -> Result<Self, SessionStorageConfigError> {
        match store {
            "" | "memory" => Ok(Self::Memory),
            "filesystem" => filesystem_root
                .filter(|root| !root.is_empty())
                .map(PathBuf::from)
                .map(|root| Self::Filesystem { root })
                .ok_or(SessionStorageConfigError::MissingFilesystemRoot),
            other => Err(SessionStorageConfigError::UnknownStore(other.to_owned())),
        }
    }
}

/// Session storage configuration failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStorageConfigError {
    /// `WALTR_SESSION_STORE` has an unsupported value.
    UnknownStore(String),
    /// Filesystem storage was requested without `WALTR_SESSION_FILESYSTEM_ROOT`.
    MissingFilesystemRoot,
}

impl fmt::Display for SessionStorageConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownStore(store) => write!(
                formatter,
                "unknown session store `{store}`; expected `memory` or `filesystem`"
            ),
            Self::MissingFilesystemRoot => write!(
                formatter,
                "{ENV_FILESYSTEM_ROOT} must be set when {ENV_SESSION_STORE}=filesystem"
            ),
        }
    }
}

impl Error for SessionStorageConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_store_selects_default() {
        assert_eq!(
            SessionStorageConfig::from_values("", None).unwrap(),
            SessionStorageConfig::Memory
        );
    }

    #[test]
    fn memory_store_selects_default() {
        assert_eq!(
            SessionStorageConfig::from_values("memory", None).unwrap(),
            SessionStorageConfig::Memory
        );
    }

    #[test]
    fn filesystem_store_requires_root() {
        assert_eq!(
            SessionStorageConfig::from_values("filesystem", None).unwrap_err(),
            SessionStorageConfigError::MissingFilesystemRoot
        );
    }

    #[test]
    fn filesystem_store_accepts_root() {
        assert_eq!(
            SessionStorageConfig::from_values("filesystem", Some("/tmp/sessions".to_owned()))
                .unwrap(),
            SessionStorageConfig::Filesystem {
                root: PathBuf::from("/tmp/sessions")
            }
        );
    }

    #[test]
    fn unknown_store_is_rejected() {
        assert_eq!(
            SessionStorageConfig::from_values("postgresql", None).unwrap_err(),
            SessionStorageConfigError::UnknownStore("postgresql".to_owned())
        );
    }
}
