use std::{collections::BTreeMap, fmt, time::SystemTime};

use reqwest::StatusCode;

use crate::{Error, Result};

/// OAuth bearer challenge parsed from `WWW-Authenticate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpBearerChallenge {
    /// Protected resource metadata URL.
    pub resource_metadata: Option<String>,
    /// Required scope string.
    pub scope: Option<String>,
    /// Bearer error code.
    pub error: Option<String>,
    /// Human-readable error description.
    pub error_description: Option<String>,
    /// Error documentation URL.
    pub error_uri: Option<String>,
    /// Authentication realm.
    pub realm: Option<String>,
    /// Unknown challenge parameters.
    pub parameters: BTreeMap<String, String>,
}

impl McpBearerChallenge {
    /// Create an empty bearer challenge.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            resource_metadata: None,
            scope: None,
            error: None,
            error_description: None,
            error_uri: None,
            realm: None,
            parameters: BTreeMap::new(),
        }
    }

    /// Parse the first Bearer challenge from a `WWW-Authenticate` header value.
    #[must_use]
    pub fn parse(header_value: &str) -> Option<Self> {
        let rest = bearer_challenge_parameters(header_value.trim())?;
        let rest = &rest[..challenge_parameters_end(rest)];
        let mut challenge = Self::new();
        let mut seen = std::collections::BTreeSet::new();
        for (name, value) in parse_auth_params(rest)? {
            let name = name.to_ascii_lowercase();
            if !seen.insert(name.clone()) {
                return None;
            }
            match name.as_str() {
                "resource_metadata" => challenge.resource_metadata = Some(value),
                "scope" => challenge.scope = Some(value),
                "error" => challenge.error = Some(value),
                "error_description" => challenge.error_description = Some(value),
                "error_uri" => challenge.error_uri = Some(value),
                "realm" => challenge.realm = Some(value),
                _ => {
                    challenge.parameters.insert(name, value);
                }
            }
        }
        Some(challenge)
    }

    /// Parse the first Bearer challenge from multiple `WWW-Authenticate` header values.
    #[must_use]
    pub fn parse_any<'a>(header_values: impl IntoIterator<Item = &'a str>) -> Option<Self> {
        header_values.into_iter().find_map(Self::parse)
    }

    /// Format this challenge as a `WWW-Authenticate` header value.
    #[must_use]
    pub fn to_header_value(&self) -> String {
        let mut params = Vec::new();
        push_param(&mut params, "realm", self.realm.as_deref());
        push_param(
            &mut params,
            "resource_metadata",
            self.resource_metadata.as_deref(),
        );
        push_param(&mut params, "scope", self.scope.as_deref());
        push_param(&mut params, "error", self.error.as_deref());
        push_param(
            &mut params,
            "error_description",
            self.error_description.as_deref(),
        );
        push_param(&mut params, "error_uri", self.error_uri.as_deref());
        for (name, value) in &self.parameters {
            params.push(format!("{name}=\"{}\"", escape_quoted(value)));
        }
        if params.is_empty() {
            "Bearer".to_owned()
        } else {
            format!("Bearer {}", params.join(", "))
        }
    }
}

impl Default for McpBearerChallenge {
    fn default() -> Self {
        Self::new()
    }
}

/// Access token extracted from an HTTP Authorization header.
#[derive(Clone, PartialEq, Eq)]
pub struct McpAccessToken(pub String);

impl fmt::Debug for McpAccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("McpAccessToken([REDACTED])")
    }
}

/// Stable identity associated with an authorized MCP access token.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct McpPrincipalId(String);

impl McpPrincipalId {
    /// Create a nonempty principal identifier no larger than 256 UTF-8 bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when the identifier is empty or exceeds 256 UTF-8 bytes.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty() || value.len() > 256 {
            return Err(Error::protocol(
                "MCP principal identifier must contain 1 to 256 UTF-8 bytes",
            ));
        }
        Ok(Self(value))
    }

    /// Borrow the principal identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for McpPrincipalId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Result returned by a Streamable HTTP bearer token validator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTokenValidation {
    /// Token is valid for this resource and request.
    Authorized {
        /// Stable subject identity. This must not contain the bearer token.
        principal_id: McpPrincipalId,
        /// Access-token expiry, when known.
        expires_at: Option<SystemTime>,
    },
    /// Token validity could not be determined because authentication infrastructure is unavailable.
    Unavailable,
    /// Token is absent, invalid, expired, or not audience-bound to this resource.
    Unauthorized {
        /// Optional human-readable reason.
        error_description: Option<String>,
    },
    /// Token is valid but lacks required scopes.
    InsufficientScope {
        /// Scopes required to satisfy the request.
        required_scopes: Vec<String>,
        /// Optional human-readable reason.
        error_description: Option<String>,
    },
}

/// OAuth authorization failure returned by HTTP clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpAuthorizationError {
    /// HTTP status code returned by the server.
    pub status: StatusCode,
    /// Parsed bearer challenge, if present.
    pub challenge: Option<McpBearerChallenge>,
}

impl fmt::Display for McpAuthorizationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "MCP HTTP authorization failed with status {}",
            self.status
        )
    }
}

impl std::error::Error for McpAuthorizationError {}

fn bearer_challenge_parameters(value: &str) -> Option<&str> {
    let mut in_quote = false;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if in_quote => escaped = true,
            '"' => in_quote = !in_quote,
            _ => {}
        }
        if in_quote {
            continue;
        }
        let candidate = &value[index..];
        let boundary = index == 0
            || value[..index]
                .chars()
                .next_back()
                .is_some_and(|previous| previous == ',' || previous.is_ascii_whitespace());
        if boundary && candidate.len() >= "Bearer".len() {
            let (scheme, rest) = candidate.split_at("Bearer".len());
            if scheme.eq_ignore_ascii_case("Bearer")
                && (rest.is_empty()
                    || rest.starts_with(|character: char| character.is_ascii_whitespace()))
            {
                return Some(rest);
            }
        }
    }
    None
}

fn challenge_parameters_end(value: &str) -> usize {
    let mut in_quote = false;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if in_quote => escaped = true,
            '"' => in_quote = !in_quote,
            ',' if !in_quote && starts_auth_scheme(&value[index + 1..]) => return index,
            _ => {}
        }
    }
    value.len()
}

fn starts_auth_scheme(value: &str) -> bool {
    let value = value.trim_start();
    let token_end = value
        .find(|character: char| !character.is_ascii_alphanumeric() && character != '-')
        .unwrap_or(value.len());
    token_end > 0
        && value[token_end..]
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_whitespace())
}

fn push_param(params: &mut Vec<String>, name: &str, value: Option<&str>) {
    if let Some(value) = value {
        params.push(format!("{name}=\"{}\"", escape_quoted(value)));
    }
}

fn escape_quoted(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn parse_auth_params(value: &str) -> Option<Vec<(String, String)>> {
    let mut params = Vec::new();
    let mut chars = value.chars().peekable();
    loop {
        while chars
            .peek()
            .is_some_and(|character| character.is_ascii_whitespace() || *character == ',')
        {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut name = String::new();
        while chars
            .peek()
            .is_some_and(|character| *character != '=' && *character != ',')
        {
            name.push(chars.next().expect("peeked character exists"));
        }
        let name = name.trim().to_owned();
        if name.is_empty()
            || !name.chars().all(|character| {
                character.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(character)
            })
            || chars.next() != Some('=')
        {
            return None;
        }
        while chars.peek().is_some_and(char::is_ascii_whitespace) {
            chars.next();
        }
        let value = if chars.peek() == Some(&'"') {
            chars.next();
            let mut output = String::new();
            let mut terminated = false;
            while let Some(character) = chars.next() {
                match character {
                    '"' => {
                        terminated = true;
                        break;
                    }
                    '\\' => {
                        output.push(chars.next()?);
                    }
                    character if character.is_ascii_control() => return None,
                    _ => output.push(character),
                }
            }
            if !terminated {
                return None;
            }
            output
        } else {
            let mut output = String::new();
            while chars.peek().is_some_and(|character| *character != ',') {
                output.push(chars.next().expect("peeked character exists"));
            }
            let output = output.trim();
            if output.is_empty()
                || !output.chars().all(|character| {
                    character.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~:/".contains(character)
                })
            {
                return None;
            }
            output.to_owned()
        };
        params.push((name, value));
    }
    Some(params)
}
