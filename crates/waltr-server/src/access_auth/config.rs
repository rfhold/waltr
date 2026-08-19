use std::{collections::HashMap, env, error::Error, fmt, net::IpAddr, time::Duration};

use url::Url;

const DEFAULT_SESSION_TTL_SECONDS: u64 = 8 * 60 * 60;

/// Server access authentication mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AccessAuthMode {
    /// Preserve unauthenticated local access.
    #[default]
    Disabled,
    /// Require OIDC-backed access authentication.
    Oidc,
}

/// Validated server access-authentication configuration.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessAuthConfig {
    mode: AccessAuthMode,
    oidc: Option<OidcConfig>,
}

impl fmt::Debug for AccessAuthConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccessAuthConfig")
            .field("mode", &self.mode)
            .field("oidc", &self.oidc)
            .finish()
    }
}

impl AccessAuthConfig {
    /// Read and strictly validate access authentication from the environment.
    ///
    /// # Errors
    ///
    /// Returns an error when an environment value is invalid or the OIDC configuration is incomplete.
    pub fn from_env() -> Result<Self, AccessAuthConfigError> {
        let names = [
            "WALTR_AUTH_MODE",
            "WALTR_OIDC_ISSUER",
            "WALTR_PUBLIC_URL",
            "WALTR_OIDC_CLIENT_ID",
            "WALTR_OIDC_CLIENT_SECRET",
            "WALTR_OIDC_REDIRECT_URI",
            "WALTR_OIDC_AUDIENCE",
            "WALTR_OIDC_SCOPES",
            "WALTR_API_OIDC_ISSUER",
            "WALTR_API_OIDC_AUDIENCE",
            "WALTR_API_OIDC_CLIENT_ID",
            "WALTR_API_REQUIRED_SCOPES",
            "WALTR_AUTH_SESSION_TTL",
            "WALTR_AUTH_ALLOW_INSECURE_COOKIE",
            "WALTR_OIDC_ALLOW_INSECURE_HTTP",
            "WALTR_TRUSTED_PROXIES",
        ];
        let mut values = HashMap::new();
        for name in names {
            match env::var(name) {
                Ok(value) => {
                    values.insert(name, value);
                }
                Err(env::VarError::NotPresent) => {}
                Err(env::VarError::NotUnicode(_)) => {
                    return Err(AccessAuthConfigError::Invalid(format!(
                        "{name} must contain valid UTF-8"
                    )));
                }
            }
        }
        Self::from_values(&values)
    }

    /// Construct disabled access authentication explicitly.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            mode: AccessAuthMode::Disabled,
            oidc: None,
        }
    }

    /// Construct OIDC access authentication from validated settings.
    #[must_use]
    pub const fn oidc(config: OidcConfig) -> Self {
        Self {
            mode: AccessAuthMode::Oidc,
            oidc: Some(config),
        }
    }

    /// Return the configured authentication mode.
    #[must_use]
    pub const fn mode(&self) -> AccessAuthMode {
        self.mode
    }

    /// Return OIDC settings when OIDC mode is active.
    #[must_use]
    pub const fn oidc_config(&self) -> Option<&OidcConfig> {
        self.oidc.as_ref()
    }

    fn from_values(values: &HashMap<&str, String>) -> Result<Self, AccessAuthConfigError> {
        let mode = value(values, "WALTR_AUTH_MODE").unwrap_or("disabled");
        match mode {
            "disabled" => {
                let partial = values.keys().any(|name| {
                    name.starts_with("WALTR_OIDC_")
                        || name.starts_with("WALTR_API_OIDC_")
                        || name.starts_with("WALTR_AUTH_") && *name != "WALTR_AUTH_MODE"
                        || *name == "WALTR_API_REQUIRED_SCOPES"
                        || *name == "WALTR_TRUSTED_PROXIES"
                });
                if partial {
                    return Err(AccessAuthConfigError::Invalid(
                        "OIDC settings are present while WALTR_AUTH_MODE is disabled".to_owned(),
                    ));
                }
                Ok(Self::disabled())
            }
            "oidc" => {
                let browser_issuer = required(values, "WALTR_OIDC_ISSUER")?;
                let api_issuer = value(values, "WALTR_API_OIDC_ISSUER").unwrap_or(browser_issuer);
                let api_audience = value(values, "WALTR_API_OIDC_AUDIENCE")
                    .or_else(|| value(values, "WALTR_OIDC_AUDIENCE"))
                    .ok_or_else(|| {
                        AccessAuthConfigError::Missing(
                            "WALTR_API_OIDC_AUDIENCE (or WALTR_OIDC_AUDIENCE)".to_owned(),
                        )
                    })?;
                let session_ttl = value(values, "WALTR_AUTH_SESSION_TTL").map_or(
                    Ok(DEFAULT_SESSION_TTL_SECONDS),
                    |raw| {
                        raw.parse::<u64>().map_err(|_| {
                            AccessAuthConfigError::Invalid(
                                "WALTR_AUTH_SESSION_TTL must be a positive integer in seconds"
                                    .to_owned(),
                            )
                        })
                    },
                )?;
                let mut config = OidcConfig::new(
                    browser_issuer,
                    required(values, "WALTR_OIDC_CLIENT_ID")?,
                    value(values, "WALTR_OIDC_CLIENT_SECRET").map(ToOwned::to_owned),
                    required(values, "WALTR_OIDC_REDIRECT_URI")?,
                    api_audience,
                    split_scopes(required(values, "WALTR_OIDC_SCOPES")?),
                    Duration::from_secs(session_ttl),
                    parse_bool(values, "WALTR_AUTH_ALLOW_INSECURE_COOKIE")?,
                    parse_bool(values, "WALTR_OIDC_ALLOW_INSECURE_HTTP")?,
                )?
                .with_access_domains(
                    required(values, "WALTR_PUBLIC_URL")?,
                    api_issuer,
                    api_audience,
                    split_scopes(value(values, "WALTR_API_REQUIRED_SCOPES").unwrap_or("")),
                )?;
                config.trusted_proxies =
                    parse_trusted_proxies(value(values, "WALTR_TRUSTED_PROXIES").unwrap_or(""))?;
                if let Some(api_client_id) = values.get("WALTR_API_OIDC_CLIENT_ID") {
                    config = config.with_api_client_id(api_client_id)?;
                }
                Ok(Self::oidc(config))
            }
            _ => Err(AccessAuthConfigError::Invalid(
                "WALTR_AUTH_MODE must be exactly `disabled` or `oidc`".to_owned(),
            )),
        }
    }
}

/// Validated OIDC deployment settings.
#[derive(Clone, PartialEq, Eq)]
pub struct OidcConfig {
    issuer: String,
    client_id: String,
    client_secret: Option<String>,
    redirect_uri: Url,
    public_url: String,
    api_issuer: String,
    api_audience: String,
    api_client_id: Option<String>,
    api_required_scopes: Vec<String>,
    requested_scopes: Vec<String>,
    session_ttl: Duration,
    transport_policy: OidcTransportPolicy,
    trusted_proxies: Vec<IpCidr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OidcTransportPolicy {
    HttpsOnly,
    LoopbackHttp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IpCidr {
    address: IpAddr,
    prefix: u8,
}

impl fmt::Debug for OidcConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OidcConfig")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[redacted]"),
            )
            .field("redirect_uri", &self.redirect_uri)
            .field("public_url", &self.public_url)
            .field("api_issuer", &self.api_issuer)
            .field("api_audience", &self.api_audience)
            .field("api_client_id", &self.api_client_id)
            .field("api_required_scopes", &self.api_required_scopes)
            .field("requested_scopes", &self.requested_scopes)
            .field("session_ttl", &self.session_ttl)
            .field("transport_policy", &self.transport_policy)
            .field("trusted_proxies", &self.trusted_proxies)
            .finish()
    }
}

impl OidcConfig {
    /// Construct and validate OIDC settings.
    ///
    /// # Errors
    ///
    /// Returns an error when a required value, URL, scope, or transport policy is invalid.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        issuer: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: Option<String>,
        redirect_uri: impl AsRef<str>,
        api_audience: impl Into<String>,
        requested_scopes: Vec<String>,
        session_ttl: Duration,
        allow_insecure_cookie: bool,
        allow_insecure_http: bool,
    ) -> Result<Self, AccessAuthConfigError> {
        let issuer = clean_required("issuer", issuer.into())?;
        let client_id = clean_required("client ID", client_id.into())?;
        let api_audience = clean_required("API audience", api_audience.into())?;
        let issuer_url = validate_url("issuer", &issuer)?;
        let redirect_uri_raw = redirect_uri.as_ref();
        let redirect_uri = Url::parse(redirect_uri_raw).map_err(|_| {
            AccessAuthConfigError::Invalid("OIDC redirect URI must be an absolute URL".to_owned())
        })?;
        if redirect_uri.path() != "/oidc/callback"
            || redirect_uri.query().is_some()
            || redirect_uri.fragment().is_some()
            || !matches!(redirect_uri.scheme(), "http" | "https")
            || redirect_uri.host_str().is_none()
            || contains_userinfo(redirect_uri_raw)
            || !redirect_uri.username().is_empty()
            || redirect_uri.password().is_some()
        {
            return Err(AccessAuthConfigError::Invalid(
                "OIDC redirect URI must be an absolute HTTP(S) URL with exact path /oidc/callback and no query or fragment".to_owned(),
            ));
        }
        let loopback_redirect = redirect_uri.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        });
        let transport_policy = match (allow_insecure_cookie, allow_insecure_http) {
            (false, false) if redirect_uri.scheme() == "https" => OidcTransportPolicy::HttpsOnly,
            (true, true) if redirect_uri.scheme() == "http" && loopback_redirect => {
                OidcTransportPolicy::LoopbackHttp
            }
            _ => return Err(AccessAuthConfigError::Invalid(
                "WALTR_AUTH_ALLOW_INSECURE_COOKIE and WALTR_OIDC_ALLOW_INSECURE_HTTP must both be false with an HTTPS callback, or both be true with an HTTP loopback callback"
                    .to_owned(),
            )),
        };
        if !transport_policy.allows(&issuer_url) {
            return Err(AccessAuthConfigError::Invalid(
                "OIDC issuer must use HTTPS, or loopback HTTP in the explicitly enabled local mode"
                    .to_owned(),
            ));
        }
        let requested_scopes = clean_scopes(requested_scopes)?;
        if !requested_scopes.iter().any(|scope| scope == "openid") {
            return Err(AccessAuthConfigError::Invalid(
                "OIDC requested scopes must include openid".to_owned(),
            ));
        }
        if session_ttl.is_zero() || session_ttl > Duration::from_hours(720) {
            return Err(AccessAuthConfigError::Invalid(
                "authentication session TTL must be between one second and 30 days".to_owned(),
            ));
        }
        let client_secret = client_secret
            .map(|secret| clean_required("client secret", secret))
            .transpose()?;
        Ok(Self {
            api_issuer: issuer.clone(),
            issuer,
            client_id,
            client_secret,
            redirect_uri,
            // Existing programmatic fixtures predate WALTR_PUBLIC_URL. Environment-owned
            // construction always replaces this through with_access_domains.
            public_url: public_resource_from_redirect(redirect_uri_raw)?,
            api_audience,
            api_client_id: None,
            api_required_scopes: Vec::new(),
            requested_scopes,
            session_ttl,
            transport_policy,
            trusted_proxies: Vec::new(),
        })
    }

    /// Set and validate the independently configured API authorization domain.
    ///
    /// # Errors
    ///
    /// Returns an error when a resource, issuer, audience, or scope is invalid.
    #[allow(clippy::too_many_arguments)]
    pub fn with_access_domains(
        mut self,
        public_url: impl AsRef<str>,
        api_issuer: impl Into<String>,
        api_audience: impl Into<String>,
        api_required_scopes: Vec<String>,
    ) -> Result<Self, AccessAuthConfigError> {
        let public_url = public_url.as_ref();
        let parsed = validate_url("public URL", public_url)?;
        if !self.transport_policy.allows(&parsed) {
            return Err(AccessAuthConfigError::Invalid(
                "WALTR_PUBLIC_URL must use HTTPS, or loopback HTTP in the explicitly enabled local mode"
                    .to_owned(),
            ));
        }
        self.public_url = normalized_resource_url(parsed, "WALTR_PUBLIC_URL")?;
        self.api_issuer =
            validate_domain_issuer("API issuer", api_issuer.into(), self.transport_policy)?;
        self.api_audience = clean_required("API audience", api_audience.into())?;
        self.api_required_scopes = clean_scopes(api_required_scopes)?;
        Ok(self)
    }

    /// Set and validate the native API OAuth client identifier.
    ///
    /// # Errors
    ///
    /// Returns an error when the client identifier is empty or contains surrounding whitespace.
    pub fn with_api_client_id(
        mut self,
        api_client_id: impl Into<String>,
    ) -> Result<Self, AccessAuthConfigError> {
        self.api_client_id = Some(clean_required("API client ID", api_client_id.into())?);
        Ok(self)
    }

    /// Expected issuer, preserved exactly for claim comparison.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }
    /// OAuth client identifier.
    #[must_use]
    pub fn client_id(&self) -> &str {
        &self.client_id
    }
    /// Confidential client secret, when configured.
    #[must_use]
    pub fn client_secret(&self) -> Option<&str> {
        self.client_secret.as_deref()
    }
    /// Registered browser callback URI.
    #[must_use]
    pub const fn redirect_uri(&self) -> &Url {
        &self.redirect_uri
    }
    /// Public API protected-resource identifier.
    #[must_use]
    pub fn public_url(&self) -> &str {
        &self.public_url
    }
    /// Issuer trusted by the native API boundary.
    #[must_use]
    pub fn api_issuer(&self) -> &str {
        &self.api_issuer
    }
    /// Required access-token audience.
    #[must_use]
    pub fn api_audience(&self) -> &str {
        &self.api_audience
    }
    /// Native API OAuth client identifier used to bind provider-generic JWT access tokens.
    #[must_use]
    pub fn api_client_id(&self) -> Option<&str> {
        self.api_client_id.as_deref()
    }
    /// Scopes required by the native API boundary.
    #[must_use]
    pub fn api_required_scopes(&self) -> &[String] {
        &self.api_required_scopes
    }
    /// Scopes requested during browser authorization.
    #[must_use]
    pub fn requested_scopes(&self) -> &[String] {
        &self.requested_scopes
    }
    /// Maximum browser session lifetime.
    #[must_use]
    pub const fn session_ttl(&self) -> Duration {
        self.session_ttl
    }
    /// Whether non-Secure session cookies are explicitly permitted.
    #[must_use]
    pub const fn allow_insecure_cookie(&self) -> bool {
        matches!(self.transport_policy, OidcTransportPolicy::LoopbackHttp)
    }
    /// Whether local-development HTTP OIDC endpoints are explicitly permitted.
    #[must_use]
    pub const fn allow_insecure_http(&self) -> bool {
        matches!(self.transport_policy, OidcTransportPolicy::LoopbackHttp)
    }
    pub(crate) const fn transport_policy(&self) -> OidcTransportPolicy {
        self.transport_policy
    }
    pub(crate) fn is_trusted_proxy(&self, address: IpAddr) -> bool {
        self.trusted_proxies
            .iter()
            .any(|network| network.contains(address))
    }
}

impl OidcTransportPolicy {
    pub(crate) fn allows(self, url: &Url) -> bool {
        url.scheme() == "https"
            || matches!(self, Self::LoopbackHttp)
                && url.scheme() == "http"
                && url.host_str().is_some_and(|host| {
                    host.eq_ignore_ascii_case("localhost")
                        || host
                            .parse::<IpAddr>()
                            .is_ok_and(|address| address.is_loopback())
                })
    }
}

impl IpCidr {
    fn contains(self, address: IpAddr) -> bool {
        match (self.address, address) {
            (IpAddr::V4(network), IpAddr::V4(address)) => {
                let bits = u32::from(network) ^ u32::from(address);
                self.prefix == 0 || bits.leading_zeros() >= u32::from(self.prefix)
            }
            (IpAddr::V6(network), IpAddr::V6(address)) => {
                let bits = u128::from(network) ^ u128::from(address);
                self.prefix == 0 || bits.leading_zeros() >= u32::from(self.prefix)
            }
            _ => false,
        }
    }
}

fn value<'a>(values: &'a HashMap<&str, String>, name: &str) -> Option<&'a str> {
    values
        .get(name)
        .map(String::as_str)
        .filter(|raw| !raw.is_empty())
}

fn required<'a>(
    values: &'a HashMap<&str, String>,
    name: &str,
) -> Result<&'a str, AccessAuthConfigError> {
    value(values, name).ok_or_else(|| AccessAuthConfigError::Missing(name.to_owned()))
}

fn parse_bool(values: &HashMap<&str, String>, name: &str) -> Result<bool, AccessAuthConfigError> {
    match value(values, name).unwrap_or("false") {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(AccessAuthConfigError::Invalid(format!(
            "{name} must be exactly `true` or `false`"
        ))),
    }
}

fn split_scopes(value: &str) -> Vec<String> {
    value
        .split_ascii_whitespace()
        .map(ToOwned::to_owned)
        .collect()
}

fn clean_required(name: &str, value: String) -> Result<String, AccessAuthConfigError> {
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        return Err(AccessAuthConfigError::Invalid(format!(
            "OIDC {name} is empty or contains surrounding/control whitespace"
        )));
    }
    Ok(value)
}

fn clean_scopes(scopes: Vec<String>) -> Result<Vec<String>, AccessAuthConfigError> {
    let mut cleaned = Vec::with_capacity(scopes.len());
    for scope in scopes {
        if scope.is_empty()
            || scope.chars().any(char::is_whitespace)
            || scope.chars().any(char::is_control)
        {
            return Err(AccessAuthConfigError::Invalid(
                "OIDC scopes must be nonempty, whitespace-free values".to_owned(),
            ));
        }
        if !cleaned.contains(&scope) {
            cleaned.push(scope);
        }
    }
    Ok(cleaned)
}

fn validate_url(name: &str, value: &str) -> Result<Url, AccessAuthConfigError> {
    let url = Url::parse(value).map_err(|_| {
        AccessAuthConfigError::Invalid(format!("OIDC {name} must be an absolute URL"))
    })?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.query().is_some()
        || url.fragment().is_some()
        || contains_userinfo(value)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(AccessAuthConfigError::Invalid(format!(
            "OIDC {name} must be an absolute HTTP(S) URL without query or fragment"
        )));
    }
    Ok(url)
}

fn validate_domain_issuer(
    name: &str,
    value: String,
    policy: OidcTransportPolicy,
) -> Result<String, AccessAuthConfigError> {
    let value = clean_required(name, value)?;
    let url = validate_url(name, &value)?;
    if !policy.allows(&url) {
        return Err(AccessAuthConfigError::Invalid(format!(
            "OIDC {name} must use HTTPS, or loopback HTTP in the explicitly enabled local mode"
        )));
    }
    Ok(value)
}

fn normalized_resource_url(mut url: Url, name: &str) -> Result<String, AccessAuthConfigError> {
    if url.path().contains("//") {
        return Err(AccessAuthConfigError::Invalid(format!(
            "{name} contains an invalid path"
        )));
    }
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(if path.is_empty() { "/" } else { &path });
    let mut value = url.to_string();
    if url.path() == "/" {
        value.truncate(value.trim_end_matches('/').len());
    }
    Ok(value)
}

fn public_resource_from_redirect(redirect_uri: &str) -> Result<String, AccessAuthConfigError> {
    let mut url = Url::parse(redirect_uri).map_err(|_| {
        AccessAuthConfigError::Invalid("OIDC redirect URI must be an absolute URL".to_owned())
    })?;
    url.set_path("/");
    normalized_resource_url(url, "public URL")
}

fn parse_trusted_proxies(value: &str) -> Result<Vec<IpCidr>, AccessAuthConfigError> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(|raw| {
            let (address, prefix) = raw.trim().split_once('/').ok_or_else(|| {
                AccessAuthConfigError::Invalid(
                    "WALTR_TRUSTED_PROXIES must contain comma-separated IP CIDRs".to_owned(),
                )
            })?;
            let address = address.parse::<IpAddr>().map_err(|_| {
                AccessAuthConfigError::Invalid(
                    "WALTR_TRUSTED_PROXIES must contain comma-separated IP CIDRs".to_owned(),
                )
            })?;
            let prefix = prefix.parse::<u8>().map_err(|_| {
                AccessAuthConfigError::Invalid(
                    "WALTR_TRUSTED_PROXIES must contain comma-separated IP CIDRs".to_owned(),
                )
            })?;
            let maximum = if address.is_ipv4() { 32 } else { 128 };
            if prefix > maximum {
                return Err(AccessAuthConfigError::Invalid(
                    "WALTR_TRUSTED_PROXIES must contain comma-separated IP CIDRs".to_owned(),
                ));
            }
            Ok(IpCidr { address, prefix })
        })
        .collect()
}

fn contains_userinfo(value: &str) -> bool {
    value
        .split_once("://")
        .and_then(|(_, remainder)| remainder.split(['/', '?', '#']).next())
        .is_some_and(|authority| authority.contains('@'))
}

/// Access authentication configuration failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessAuthConfigError {
    /// A required environment setting is absent.
    Missing(String),
    /// A setting is malformed or inconsistent.
    Invalid(String),
}

impl fmt::Display for AccessAuthConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(name) => write!(
                formatter,
                "required access authentication setting {name} is missing"
            ),
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl Error for AccessAuthConfigError {}

#[cfg(test)]
#[allow(
    clippy::duration_suboptimal_units,
    reason = "tests use boundary durations"
)]
mod tests {
    use super::*;

    fn valid_config() -> OidcConfig {
        OidcConfig::new(
            "https://issuer.example",
            "waltr",
            Some("secret-value".to_owned()),
            "https://waltr.example/oidc/callback",
            "waltr-api",
            vec!["openid".to_owned(), "profile".to_owned()],
            Duration::from_secs(3600),
            false,
            false,
        )
        .unwrap()
    }

    fn valid_environment_values() -> HashMap<&'static str, String> {
        HashMap::from([
            ("WALTR_AUTH_MODE", "oidc".to_owned()),
            ("WALTR_PUBLIC_URL", "https://waltr.example".to_owned()),
            ("WALTR_OIDC_ISSUER", "https://issuer.example".to_owned()),
            ("WALTR_OIDC_CLIENT_ID", "browser".to_owned()),
            (
                "WALTR_OIDC_REDIRECT_URI",
                "https://waltr.example/oidc/callback".to_owned(),
            ),
            ("WALTR_API_OIDC_AUDIENCE", "waltr-api".to_owned()),
            ("WALTR_OIDC_SCOPES", "openid".to_owned()),
        ])
    }

    #[test]
    fn defaults_to_disabled() {
        assert_eq!(
            AccessAuthConfig::from_values(&HashMap::new()).unwrap(),
            AccessAuthConfig::disabled()
        );
    }

    #[test]
    fn public_url_is_allowed_while_access_authentication_is_disabled() {
        let values = HashMap::from([("WALTR_PUBLIC_URL", "https://waltr.example/api".to_owned())]);

        assert_eq!(
            AccessAuthConfig::from_values(&values).unwrap(),
            AccessAuthConfig::disabled()
        );
    }

    #[test]
    fn environment_domains_fall_back_to_the_existing_single_issuer_settings() {
        let values = HashMap::from([
            ("WALTR_AUTH_MODE", "oidc".to_owned()),
            ("WALTR_PUBLIC_URL", "https://waltr.example/api/".to_owned()),
            ("WALTR_OIDC_ISSUER", "https://issuer.example".to_owned()),
            ("WALTR_OIDC_CLIENT_ID", "browser".to_owned()),
            (
                "WALTR_OIDC_REDIRECT_URI",
                "https://login.waltr.example/oidc/callback".to_owned(),
            ),
            ("WALTR_OIDC_AUDIENCE", "legacy-audience".to_owned()),
            ("WALTR_OIDC_SCOPES", "openid profile".to_owned()),
            ("WALTR_API_REQUIRED_SCOPES", "api:read".to_owned()),
        ]);

        let config = AccessAuthConfig::from_values(&values).unwrap();
        let oidc = config.oidc_config().unwrap();
        assert_eq!(oidc.public_url(), "https://waltr.example/api");
        assert_eq!(oidc.api_issuer(), "https://issuer.example");
        assert_eq!(oidc.api_audience(), "legacy-audience");
        assert_eq!(oidc.api_client_id(), None);
        assert_eq!(oidc.api_required_scopes(), ["api:read"]);
    }

    #[test]
    fn environment_retains_valid_api_client_id() {
        let mut values = valid_environment_values();
        values.insert("WALTR_API_OIDC_CLIENT_ID", "waltr-native".to_owned());

        let config = AccessAuthConfig::from_values(&values).unwrap();
        assert_eq!(
            config.oidc_config().unwrap().api_client_id(),
            Some("waltr-native")
        );
    }

    #[test]
    fn environment_rejects_blank_or_padded_api_client_id() {
        for client_id in ["", " ", " waltr-native", "waltr-native "] {
            let mut values = valid_environment_values();
            values.insert("WALTR_API_OIDC_CLIENT_ID", client_id.to_owned());
            assert!(
                AccessAuthConfig::from_values(&values).is_err(),
                "{client_id:?}"
            );
        }
    }

    #[test]
    fn explicit_api_domain_and_public_url_are_secure() {
        let config = valid_config()
            .with_access_domains(
                "https://waltr.example",
                "https://api-id.example",
                "api-audience",
                vec!["api:use".to_owned()],
            )
            .unwrap();
        assert_eq!(config.api_issuer(), "https://api-id.example");
        assert_eq!(config.api_audience(), "api-audience");
        assert_eq!(config.api_required_scopes(), ["api:use"]);
        assert!(
            valid_config()
                .with_access_domains(
                    "http://waltr.example",
                    "https://issuer.example",
                    "api",
                    vec![],
                )
                .is_err()
        );
    }

    #[test]
    fn partial_oidc_configuration_fails_while_disabled() {
        for (name, value) in [
            ("WALTR_OIDC_ISSUER", "https://issuer.example"),
            ("WALTR_API_OIDC_CLIENT_ID", "waltr-native"),
            ("WALTR_AUTH_SESSION_TTL", "3600"),
            ("WALTR_AUTH_ALLOW_INSECURE_COOKIE", "true"),
            ("WALTR_TRUSTED_PROXIES", "10.0.0.0/8"),
        ] {
            let values = HashMap::from([(name, value.to_owned())]);
            assert!(AccessAuthConfig::from_values(&values).is_err(), "{name}");
        }
    }

    #[test]
    fn rejects_redirect_path_and_missing_openid_scope() {
        let wrong_path = OidcConfig::new(
            "https://issuer.example",
            "waltr",
            None,
            "https://waltr.example/callback",
            "api",
            vec!["openid".to_owned()],
            Duration::from_secs(1),
            false,
            false,
        );
        assert!(wrong_path.is_err());
        let missing_openid = OidcConfig::new(
            "https://issuer.example",
            "waltr",
            None,
            "https://waltr.example/oidc/callback",
            "api",
            vec!["profile".to_owned()],
            Duration::from_secs(1),
            false,
            false,
        );
        assert!(missing_openid.is_err());
    }

    #[test]
    fn debug_redacts_client_secret() {
        let debug = format!("{:?}", valid_config());
        assert!(debug.contains("[redacted]"));
        assert!(!debug.contains("secret-value"));
    }

    #[test]
    fn insecure_cookies_require_exact_loopback_http_deployment() {
        let build = |redirect_uri: &str, allow_cookie, allow_http| {
            OidcConfig::new(
                "https://issuer.example",
                "waltr",
                None,
                redirect_uri,
                "api",
                vec!["openid".to_owned()],
                Duration::from_secs(60),
                allow_cookie,
                allow_http,
            )
        };

        for (callback, cookie, http, valid) in [
            ("https://waltr.example/oidc/callback", false, false, true),
            ("https://waltr.example/oidc/callback", false, true, false),
            ("https://waltr.example/oidc/callback", true, false, false),
            ("https://waltr.example/oidc/callback", true, true, false),
            ("http://127.0.0.1:14333/oidc/callback", false, false, false),
            ("http://127.0.0.1:14333/oidc/callback", false, true, false),
            ("http://127.0.0.1:14333/oidc/callback", true, false, false),
            ("http://127.0.0.1:14333/oidc/callback", true, true, true),
            ("http://localhost:14333/oidc/callback", true, true, true),
            ("http://waltr.example/oidc/callback", true, true, false),
        ] {
            assert_eq!(
                build(callback, cookie, http).is_ok(),
                valid,
                "{callback} {cookie} {http}"
            );
        }
    }

    #[test]
    fn insecure_mode_applies_the_same_policy_to_issuer_and_endpoints() {
        assert!(
            valid_config()
                .transport_policy()
                .allows(&Url::parse("https://issuer.example/jwks").unwrap())
        );
        assert!(
            !valid_config()
                .transport_policy()
                .allows(&Url::parse("http://127.0.0.1/jwks").unwrap())
        );
        let local = OidcConfig::new(
            "http://127.0.0.1:8080",
            "waltr",
            None,
            "http://127.0.0.1:14333/oidc/callback",
            "api",
            vec!["openid".to_owned()],
            Duration::from_secs(60),
            true,
            true,
        )
        .unwrap();
        assert!(
            local
                .transport_policy()
                .allows(&Url::parse("http://localhost:8080/jwks").unwrap())
        );
        assert!(
            !local
                .transport_policy()
                .allows(&Url::parse("http://provider.example/jwks").unwrap())
        );
    }

    #[test]
    fn trusted_proxy_cidrs_are_strict_and_support_both_address_families() {
        let proxies = parse_trusted_proxies("10.0.0.0/8, 2001:db8::/32").unwrap();
        assert!(
            proxies
                .iter()
                .any(|network| network.contains("10.2.3.4".parse().unwrap()))
        );
        assert!(
            proxies
                .iter()
                .any(|network| network.contains("2001:db8::1".parse().unwrap()))
        );
        assert!(
            !proxies
                .iter()
                .any(|network| network.contains("192.0.2.1".parse().unwrap()))
        );
        for malformed in ["10.0.0.1", "10.0.0.0/33", "invalid/24", "10.0.0.0/x", ","] {
            assert!(parse_trusted_proxies(malformed).is_err(), "{malformed}");
        }
    }

    #[test]
    fn issuer_and_redirect_reject_userinfo_without_exposing_it() {
        let build = |issuer: &str, redirect_uri: &str| {
            OidcConfig::new(
                issuer,
                "waltr",
                None,
                redirect_uri,
                "api",
                vec!["openid".to_owned()],
                Duration::from_secs(60),
                false,
                false,
            )
        };

        for issuer in [
            "https://alice@issuer.example",
            "https://alice:secret@issuer.example",
            "https://@issuer.example",
        ] {
            let error = build(issuer, "https://waltr.example/oidc/callback").unwrap_err();
            let diagnostic = error.to_string();
            assert!(!diagnostic.contains("alice"));
            assert!(!diagnostic.contains("secret"));
        }
        for redirect_uri in [
            "https://alice@waltr.example/oidc/callback",
            "https://alice:secret@waltr.example/oidc/callback",
            "https://@waltr.example/oidc/callback",
        ] {
            let error = build("https://issuer.example", redirect_uri).unwrap_err();
            let diagnostic = error.to_string();
            assert!(!diagnostic.contains("alice"));
            assert!(!diagnostic.contains("secret"));
        }
    }
}
