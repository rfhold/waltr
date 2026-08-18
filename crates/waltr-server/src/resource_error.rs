//! Resource catalog initialization errors.

use std::{fmt, io};

/// Failure to initialize or maintain the resource catalog service.
#[derive(Debug)]
pub struct ResourceError {
    kind: ResourceErrorKind,
}

#[derive(Debug)]
enum ResourceErrorKind {
    CurrentDir(io::Error),
    Invalid(String),
    Unavailable(String),
}

impl ResourceError {
    pub(crate) const fn current_dir(source: io::Error) -> Self {
        Self {
            kind: ResourceErrorKind::CurrentDir(source),
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: ResourceErrorKind::Invalid(message.into()),
        }
    }

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: ResourceErrorKind::Unavailable(message.into()),
        }
    }
}

impl fmt::Display for ResourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ResourceErrorKind::CurrentDir(source) => {
                write!(formatter, "failed to determine current directory: {source}")
            }
            ResourceErrorKind::Invalid(message) => {
                write!(formatter, "invalid resource catalog: {message}")
            }
            ResourceErrorKind::Unavailable(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ResourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            ResourceErrorKind::CurrentDir(source) => Some(source),
            ResourceErrorKind::Invalid(_) | ResourceErrorKind::Unavailable(_) => None,
        }
    }
}
