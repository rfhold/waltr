//! Configuration backend errors.

use std::{fmt, io, path::PathBuf};

/// Configuration backend error.
#[derive(Debug)]
pub struct ConfigError {
    kind: ConfigErrorKind,
}

#[derive(Debug)]
enum ConfigErrorKind {
    CurrentDir {
        source: io::Error,
    },
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Write {
        path: PathBuf,
        source: io::Error,
    },
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    Serialize {
        source: toml::ser::Error,
    },
    Invalid {
        message: String,
    },
    Watch {
        source: notify::Error,
    },
    EtagMismatch {
        expected: String,
        actual: String,
    },
}

impl ConfigError {
    pub(crate) const fn current_dir(source: io::Error) -> Self {
        Self {
            kind: ConfigErrorKind::CurrentDir { source },
        }
    }

    pub(crate) const fn read(path: PathBuf, source: io::Error) -> Self {
        Self {
            kind: ConfigErrorKind::Read { path, source },
        }
    }

    pub(crate) const fn write(path: PathBuf, source: io::Error) -> Self {
        Self {
            kind: ConfigErrorKind::Write { path, source },
        }
    }

    pub(crate) const fn parse(path: PathBuf, source: toml::de::Error) -> Self {
        Self {
            kind: ConfigErrorKind::Parse { path, source },
        }
    }

    pub(crate) const fn serialize(source: toml::ser::Error) -> Self {
        Self {
            kind: ConfigErrorKind::Serialize { source },
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: ConfigErrorKind::Invalid {
                message: message.into(),
            },
        }
    }

    pub(crate) const fn watch(source: notify::Error) -> Self {
        Self {
            kind: ConfigErrorKind::Watch { source },
        }
    }

    pub(crate) fn etag_mismatch(expected: &str, actual: &str) -> Self {
        Self {
            kind: ConfigErrorKind::EtagMismatch {
                expected: expected.to_owned(),
                actual: actual.to_owned(),
            },
        }
    }

    /// Whether this error is an etag conflict.
    #[must_use]
    pub const fn is_etag_mismatch(&self) -> bool {
        matches!(self.kind, ConfigErrorKind::EtagMismatch { .. })
    }

    /// Whether this error is a validation failure.
    #[must_use]
    pub const fn is_invalid(&self) -> bool {
        matches!(self.kind, ConfigErrorKind::Invalid { .. })
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ConfigErrorKind::CurrentDir { source } => {
                write!(f, "failed to determine current directory: {source}")
            }
            ConfigErrorKind::Read { path, source } => {
                write!(f, "failed to read {}: {source}", path.display())
            }
            ConfigErrorKind::Write { path, source } => {
                write!(f, "failed to write {}: {source}", path.display())
            }
            ConfigErrorKind::Parse { path, source } => {
                write!(f, "failed to parse {}: {source}", path.display())
            }
            ConfigErrorKind::Serialize { source } => {
                write!(f, "failed to serialize config: {source}")
            }
            ConfigErrorKind::Invalid { message } => write!(f, "invalid config: {message}"),
            ConfigErrorKind::Watch { source } => write!(f, "failed to watch config: {source}"),
            ConfigErrorKind::EtagMismatch { expected, actual } => write!(
                f,
                "config etag mismatch: expected {expected}, current {actual}"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}
