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
        let store = env::var(ENV_SESSION_STORE).unwrap_or_default();
        Self::from_values(
            &store,
            env::var(ENV_FILESYSTEM_ROOT).ok(),
            default_filesystem_root(),
        )
    }

    fn from_values(
        store: &str,
        filesystem_root: Option<String>,
        default_filesystem_root: PathBuf,
    ) -> Result<Self, SessionStorageConfigError> {
        match store {
            "" => Ok(Self::Filesystem {
                root: filesystem_root
                    .filter(|root| !root.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or(default_filesystem_root),
            }),
            "memory" => Ok(Self::Memory),
            "filesystem" => Ok(Self::Filesystem {
                root: filesystem_root
                    .filter(|root| !root.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or(default_filesystem_root),
            }),
            other => Err(SessionStorageConfigError::UnknownStore(other.to_owned())),
        }
    }
}

fn default_filesystem_root() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_else(|| ".".into());
    PathBuf::from(home).join(".local/share/waltr/sessions")
}

/// Session storage configuration failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStorageConfigError {
    /// `WALTR_SESSION_STORE` has an unsupported value.
    UnknownStore(String),
}

impl fmt::Display for SessionStorageConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownStore(store) => write!(
                formatter,
                "unknown session store `{store}`; expected `memory` or `filesystem`"
            ),
        }
    }
}

impl Error for SessionStorageConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_root() -> PathBuf {
        PathBuf::from("/home/test/.local/share/waltr/sessions")
    }

    #[test]
    fn unset_store_selects_filesystem_default() {
        assert_eq!(
            SessionStorageConfig::from_values("", None, default_root()).unwrap(),
            SessionStorageConfig::Filesystem {
                root: default_root()
            }
        );
    }

    #[test]
    fn memory_store_selects_default() {
        assert_eq!(
            SessionStorageConfig::from_values("memory", None, default_root()).unwrap(),
            SessionStorageConfig::Memory
        );
    }

    #[test]
    fn filesystem_store_uses_default_root() {
        assert_eq!(
            SessionStorageConfig::from_values("filesystem", None, default_root()).unwrap(),
            SessionStorageConfig::Filesystem {
                root: default_root()
            }
        );
    }

    #[test]
    fn filesystem_store_accepts_root() {
        assert_eq!(
            SessionStorageConfig::from_values(
                "filesystem",
                Some("/tmp/sessions".to_owned()),
                default_root()
            )
            .unwrap(),
            SessionStorageConfig::Filesystem {
                root: PathBuf::from("/tmp/sessions")
            }
        );
    }

    #[test]
    fn unknown_store_is_rejected() {
        assert_eq!(
            SessionStorageConfig::from_values("postgresql", None, default_root()).unwrap_err(),
            SessionStorageConfigError::UnknownStore("postgresql".to_owned())
        );
    }
}
