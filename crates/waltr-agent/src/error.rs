//! Error types for LLM streaming providers.

use std::{error::Error, fmt, io};

use crate::AuthenticationFailure;

/// Result type used by LLM streaming providers.
pub type Result<T> = std::result::Result<T, LlmError>;

/// Error returned by LLM streaming providers.
#[derive(Debug)]
pub enum LlmError {
    /// Required configuration was not provided.
    MissingConfig {
        /// Configuration key name.
        name: &'static str,
    },
    /// An HTTP request failed.
    Http(reqwest::Error),
    /// A websocket operation failed.
    WebSocket(tokio_tungstenite::tungstenite::Error),
    /// An HTTP or websocket request could not be built.
    Request(http::Error),
    /// JSON serialization or parsing failed.
    Json(serde_json::Error),
    /// A filesystem operation failed.
    Io(io::Error),
    /// The provider returned an API error.
    Api {
        /// Provider that returned the error.
        provider: &'static str,
        /// Provider error message.
        message: String,
        /// HTTP status code, when the error came from an HTTP response.
        status: Option<u16>,
        /// Response headers captured from the provider.
        headers: Vec<(String, String)>,
        /// Provider-specific machine-readable error code or type.
        code: Option<String>,
        /// Raw response body or event payload, when available.
        body: Option<String>,
    },
    /// The provider returned an unsupported or unexpected event.
    Protocol {
        /// Provider that returned the protocol error.
        provider: &'static str,
        /// Protocol error message.
        message: String,
    },
    /// The provider or transport requires authentication.
    Authorization {
        /// Provider or transport that requires authentication.
        provider: &'static str,
        /// Authentication failure message.
        message: String,
        /// HTTP status code, when available.
        status: Option<u16>,
    },
    /// A configured provider returned a target-scoped authentication failure.
    AuthenticationFailure {
        /// Structured failure safe to persist and return to clients.
        failure: AuthenticationFailure,
        /// Sanitized human-readable failure message.
        message: String,
    },
}

impl fmt::Display for LlmError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingConfig { name } => write!(formatter, "missing required config: {name}"),
            Self::Http(error) => write!(formatter, "http error: {error}"),
            Self::WebSocket(error) => write!(formatter, "websocket error: {error}"),
            Self::Request(error) => write!(formatter, "request error: {error}"),
            Self::Json(error) => write!(formatter, "json error: {error}"),
            Self::Io(error) => write!(formatter, "io error: {error}"),
            Self::Api {
                provider, message, ..
            } => {
                write!(formatter, "{provider} api error: {message}")
            }
            Self::Protocol { provider, message } => {
                write!(formatter, "{provider} protocol error: {message}")
            }
            Self::Authorization {
                provider, message, ..
            } => {
                write!(formatter, "{provider} authorization required: {message}")
            }
            Self::AuthenticationFailure { message, .. } => formatter.write_str(message),
        }
    }
}

impl Error for LlmError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Http(error) => Some(error),
            Self::WebSocket(error) => Some(error),
            Self::Request(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::MissingConfig { .. }
            | Self::Api { .. }
            | Self::Protocol { .. }
            | Self::Authorization { .. }
            | Self::AuthenticationFailure { .. } => None,
        }
    }
}

impl LlmError {
    /// Construct a provider API error without transport metadata.
    #[must_use]
    pub fn api(provider: &'static str, message: impl Into<String>) -> Self {
        Self::Api {
            provider,
            message: message.into(),
            status: None,
            headers: Vec::new(),
            code: None,
            body: None,
        }
    }

    /// Return structured authentication data carried by this error.
    #[must_use]
    pub const fn authentication_failure(&self) -> Option<&AuthenticationFailure> {
        match self {
            Self::AuthenticationFailure { failure, .. } => Some(failure),
            _ => None,
        }
    }
}

impl From<reqwest::Error> for LlmError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error)
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for LlmError {
    fn from(error: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::WebSocket(error)
    }
}

impl From<http::Error> for LlmError {
    fn from(error: http::Error) -> Self {
        Self::Request(error)
    }
}

impl From<serde_json::Error> for LlmError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<io::Error> for LlmError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
