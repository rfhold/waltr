use std::{error, fmt};

/// MCP crate result type.
pub type Result<T> = std::result::Result<T, Error>;

/// Error returned by MCP protocol, transport, and server helpers.
#[derive(Debug)]
pub enum Error {
    /// Protocol-level failure.
    Protocol(String),
    /// JSON serialization or parsing failure.
    Json(serde_json::Error),
    /// I/O failure.
    Io(std::io::Error),
    /// HTTP client failure.
    Http(reqwest::Error),
    /// HTTP server failure.
    Hyper(axum::BoxError),
    /// OAuth authorization failure.
    Authorization(Box<crate::oauth::McpAuthorizationError>),
}

impl Error {
    /// Create a protocol error.
    #[must_use]
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(message) => formatter.write_str(message),
            Self::Json(error) => write!(formatter, "{error}"),
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Http(error) => write!(formatter, "{error}"),
            Self::Hyper(error) => write!(formatter, "{error}"),
            Self::Authorization(error) => write!(formatter, "{error}"),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Self::Protocol(_) => None,
            Self::Json(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Http(error) => Some(error),
            Self::Hyper(error) => Some(error.as_ref()),
            Self::Authorization(error) => Some(error),
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error)
    }
}
