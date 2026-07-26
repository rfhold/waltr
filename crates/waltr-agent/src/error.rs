//! Error types for LLM streaming providers.

use std::{error::Error, fmt, io};

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
    /// A generic MCP client returned a structured bearer challenge.
    McpAuthorization {
        /// HTTP status code returned by the protected MCP endpoint.
        status: u16,
        /// Parsed, sanitized bearer challenge.
        challenge: Option<Box<mcp::McpBearerChallenge>>,
        /// Authentication failure message.
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
            Self::McpAuthorization { message, .. } => {
                write!(formatter, "mcp authorization required: {message}")
            }
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
            | Self::McpAuthorization { .. } => None,
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

impl From<mcp::Error> for LlmError {
    fn from(error: mcp::Error) -> Self {
        match error {
            mcp::Error::Authorization(error) => Self::McpAuthorization {
                status: error.status.as_u16(),
                challenge: error.challenge.clone().map(Box::new),
                message: error.to_string(),
            },
            mcp::Error::BearerProvider(mcp::client::McpBearerProviderError::Authorization) => {
                Self::Authorization {
                    provider: "mcp",
                    message: "MCP bearer authorization is required".to_owned(),
                    status: None,
                }
            }
            error => Self::Protocol {
                provider: "mcp",
                message: error.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LlmError;

    #[test]
    fn preserves_structured_mcp_authorization_challenge() {
        let challenge = mcp::McpBearerChallenge::parse(
            "Bearer resource_metadata=\"https://mcp.example/.well-known/oauth-protected-resource\", scope=\"tools.read\"",
        )
        .expect("challenge should parse");
        let error = LlmError::from(mcp::Error::Authorization(Box::new(
            mcp::McpAuthorizationError {
                status: reqwest::StatusCode::UNAUTHORIZED,
                challenge: Some(challenge.clone()),
            },
        )));

        assert!(matches!(
            error,
            LlmError::McpAuthorization {
                status: 401,
                challenge: Some(actual),
                ..
            } if *actual == challenge
        ));
    }

    #[test]
    fn maps_dynamic_bearer_provider_failures() {
        let authorization = LlmError::from(mcp::Error::BearerProvider(
            mcp::client::McpBearerProviderError::Authorization,
        ));
        let unavailable = LlmError::from(mcp::Error::BearerProvider(
            mcp::client::McpBearerProviderError::Unavailable,
        ));

        assert!(matches!(authorization, LlmError::Authorization { .. }));
        assert!(matches!(unavailable, LlmError::Protocol { .. }));
    }
}
