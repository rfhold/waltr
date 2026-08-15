//! Server session storage configuration.

use std::{env, error::Error, fmt, path::PathBuf};

use aws_sdk_s3::{
    Client,
    config::{Credentials, Region},
};
use waltr_agent::{S3SessionStore, S3SessionWriteMode};
use url::Url;

const ENV_SESSION_STORE: &str = "WALTR_SESSION_STORE";
const ENV_FILESYSTEM_ROOT: &str = "WALTR_SESSION_FILESYSTEM_ROOT";
const ENV_S3_ENDPOINT: &str = "WALTR_SESSION_S3_ENDPOINT";
const ENV_S3_REGION: &str = "WALTR_SESSION_S3_REGION";
const ENV_S3_BUCKET: &str = "WALTR_SESSION_S3_BUCKET";
const ENV_S3_PREFIX: &str = "WALTR_SESSION_S3_PREFIX";
const ENV_S3_ACCESS_KEY_ID: &str = "WALTR_SESSION_S3_ACCESS_KEY_ID";
const ENV_S3_SECRET_ACCESS_KEY: &str = "WALTR_SESSION_S3_SECRET_ACCESS_KEY";
const ENV_S3_FORCE_PATH_STYLE: &str = "WALTR_SESSION_S3_FORCE_PATH_STYLE";
const ENV_S3_WRITE_MODE: &str = "WALTR_SESSION_S3_WRITE_MODE";

#[derive(Clone, PartialEq, Eq)]
pub(super) enum SessionStorageConfig {
    Memory,
    Filesystem { root: PathBuf },
    S3(Box<S3SessionStorageConfig>),
}

impl fmt::Debug for SessionStorageConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Memory => formatter.write_str("Memory"),
            Self::Filesystem { root } => formatter
                .debug_struct("Filesystem")
                .field("root", root)
                .finish(),
            Self::S3(config) => config.fmt(formatter),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct S3SessionStorageConfig {
    endpoint: Url,
    region: String,
    bucket: String,
    prefix: String,
    access_key_id: String,
    secret_access_key: String,
    force_path_style: bool,
    write_mode: S3SessionWriteMode,
}

impl fmt::Debug for S3SessionStorageConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("S3")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("access_key_id", &"<redacted>")
            .field("secret_access_key", &"<redacted>")
            .field("force_path_style", &self.force_path_style)
            .field("write_mode", &self.write_mode)
            .finish()
    }
}

impl S3SessionStorageConfig {
    pub(super) fn into_store(self) -> S3SessionStore {
        let credentials = Credentials::new(
            self.access_key_id,
            self.secret_access_key,
            None,
            None,
            "waltr-session-s3",
        );
        let sdk_config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .endpoint_url(self.endpoint.to_string())
            .region(Region::new(self.region))
            .credentials_provider(credentials)
            .force_path_style(self.force_path_style)
            .build();
        S3SessionStore::new_with_write_mode(
            Client::from_conf(sdk_config),
            self.bucket,
            self.prefix,
            self.write_mode,
        )
    }
}

impl SessionStorageConfig {
    pub(super) fn from_env() -> Result<Self, SessionStorageConfigError> {
        Self::from_lookup(|name| env::var(name).ok(), default_filesystem_root())
    }

    fn from_lookup(
        lookup: impl Fn(&str) -> Option<String>,
        default_filesystem_root: PathBuf,
    ) -> Result<Self, SessionStorageConfigError> {
        let store = lookup(ENV_SESSION_STORE).unwrap_or_default();
        match store.as_str() {
            "" => Ok(Self::Filesystem {
                root: lookup(ENV_FILESYSTEM_ROOT)
                    .filter(|root| !root.is_empty())
                    .map_or(default_filesystem_root, PathBuf::from),
            }),
            "memory" => Ok(Self::Memory),
            "filesystem" => Ok(Self::Filesystem {
                root: lookup(ENV_FILESYSTEM_ROOT)
                    .filter(|root| !root.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or(default_filesystem_root),
            }),
            "s3" => Ok(Self::S3(Box::new(S3SessionStorageConfig::from_lookup(
                lookup,
            )?))),
            other => Err(SessionStorageConfigError::UnknownStore(other.to_owned())),
        }
    }
}

impl S3SessionStorageConfig {
    fn from_lookup(
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, SessionStorageConfigError> {
        let endpoint_value = required(&lookup, ENV_S3_ENDPOINT)?;
        let endpoint = validate_endpoint(&endpoint_value)?;
        let region = required(&lookup, ENV_S3_REGION)?;
        let bucket = required(&lookup, ENV_S3_BUCKET)?;
        let prefix = required(&lookup, ENV_S3_PREFIX)?;
        let access_key_id = required(&lookup, ENV_S3_ACCESS_KEY_ID)?;
        let secret_access_key = required(&lookup, ENV_S3_SECRET_ACCESS_KEY)?;
        let force_path_style = parse_bool(
            ENV_S3_FORCE_PATH_STYLE,
            &required(&lookup, ENV_S3_FORCE_PATH_STYLE)?,
        )?;
        let write_mode = parse_s3_write_mode(lookup(ENV_S3_WRITE_MODE).as_deref())?;

        Ok(Self {
            endpoint,
            region,
            bucket,
            prefix,
            access_key_id,
            secret_access_key,
            force_path_style,
            write_mode,
        })
    }
}

fn required(
    lookup: &impl Fn(&str) -> Option<String>,
    variable: &'static str,
) -> Result<String, SessionStorageConfigError> {
    lookup(variable)
        .filter(|value| !value.trim().is_empty())
        .ok_or(SessionStorageConfigError::Missing(variable))
}

fn parse_bool(variable: &'static str, value: &str) -> Result<bool, SessionStorageConfigError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(SessionStorageConfigError::Invalid {
            variable,
            requirement: "must be exactly `true` or `false`",
        }),
    }
}

fn parse_s3_write_mode(
    value: Option<&str>,
) -> Result<S3SessionWriteMode, SessionStorageConfigError> {
    match value {
        None | Some("conditional-etag") => Ok(S3SessionWriteMode::ConditionalEtagCas),
        Some("single-writer") => Ok(S3SessionWriteMode::SingleWriter),
        Some(_) => Err(SessionStorageConfigError::Invalid {
            variable: ENV_S3_WRITE_MODE,
            requirement: "must be exactly `conditional-etag` or `single-writer`",
        }),
    }
}

fn validate_endpoint(value: &str) -> Result<Url, SessionStorageConfigError> {
    let endpoint = Url::parse(value).map_err(|_| invalid_endpoint())?;
    let http_is_loopback = endpoint.scheme() == "http"
        && endpoint.host().is_some_and(|host| match host {
            url::Host::Domain(host) => host == "localhost",
            url::Host::Ipv4(address) => address.is_loopback(),
            url::Host::Ipv6(address) => address.is_loopback(),
        });
    if !(endpoint.scheme() == "https" || http_is_loopback)
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || endpoint.path() != "/"
    {
        return Err(invalid_endpoint());
    }
    Ok(endpoint)
}

const fn invalid_endpoint() -> SessionStorageConfigError {
    SessionStorageConfigError::Invalid {
        variable: ENV_S3_ENDPOINT,
        requirement: "must be an HTTPS base URL without credentials, path, query, or fragment; HTTP is allowed only on loopback",
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
    /// A required environment variable is absent or empty.
    Missing(&'static str),
    /// An environment variable has an invalid value.
    Invalid {
        /// Environment variable name.
        variable: &'static str,
        /// Value-independent validation requirement.
        requirement: &'static str,
    },
}

impl fmt::Display for SessionStorageConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownStore(store) => write!(
                formatter,
                "unknown session store `{store}`; expected `memory`, `filesystem`, or `s3`"
            ),
            Self::Missing(variable) => write!(formatter, "{variable} must be set and nonempty"),
            Self::Invalid {
                variable,
                requirement,
            } => write!(formatter, "{variable} {requirement}"),
        }
    }
}

impl Error for SessionStorageConfigError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn default_root() -> PathBuf {
        PathBuf::from("/home/test/.local/share/waltr/sessions")
    }

    fn config(values: &[(&str, &str)]) -> Result<SessionStorageConfig, SessionStorageConfigError> {
        let values = values
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect::<HashMap<_, _>>();
        SessionStorageConfig::from_lookup(|name| values.get(name).cloned(), default_root())
    }

    fn s3_values() -> Vec<(&'static str, &'static str)> {
        vec![
            (ENV_SESSION_STORE, "s3"),
            (ENV_S3_ENDPOINT, "https://rgw.example.test"),
            (ENV_S3_REGION, "us-east-1"),
            (ENV_S3_BUCKET, "waltr-sessions"),
            (ENV_S3_PREFIX, "production/sessions"),
            (ENV_S3_ACCESS_KEY_ID, "access-key-value"),
            (ENV_S3_SECRET_ACCESS_KEY, "secret-key-value"),
            (ENV_S3_FORCE_PATH_STYLE, "true"),
        ]
    }

    #[test]
    fn unset_store_selects_filesystem_default() {
        assert_eq!(
            config(&[]).unwrap(),
            SessionStorageConfig::Filesystem {
                root: default_root()
            }
        );
    }

    #[test]
    fn memory_store_selects_default() {
        assert_eq!(
            config(&[(ENV_SESSION_STORE, "memory")]).unwrap(),
            SessionStorageConfig::Memory
        );
    }

    #[test]
    fn filesystem_store_uses_default_root() {
        assert_eq!(
            config(&[(ENV_SESSION_STORE, "filesystem")]).unwrap(),
            SessionStorageConfig::Filesystem {
                root: default_root()
            }
        );
    }

    #[test]
    fn filesystem_store_accepts_root() {
        assert_eq!(
            config(&[
                (ENV_SESSION_STORE, "filesystem"),
                (ENV_FILESYSTEM_ROOT, "/tmp/sessions"),
            ])
            .unwrap(),
            SessionStorageConfig::Filesystem {
                root: PathBuf::from("/tmp/sessions")
            }
        );
    }

    #[test]
    fn unknown_store_is_rejected() {
        assert_eq!(
            config(&[(ENV_SESSION_STORE, "postgresql")]).unwrap_err(),
            SessionStorageConfigError::UnknownStore("postgresql".to_owned())
        );
    }

    #[test]
    fn s3_store_accepts_explicit_ceph_configuration() {
        let SessionStorageConfig::S3(config) = config(&s3_values()).unwrap() else {
            panic!("expected S3 config");
        };
        assert_eq!(config.endpoint.as_str(), "https://rgw.example.test/");
        assert_eq!(config.region, "us-east-1");
        assert_eq!(config.bucket, "waltr-sessions");
        assert_eq!(config.prefix, "production/sessions");
        assert!(config.force_path_style);
        assert_eq!(config.write_mode, S3SessionWriteMode::ConditionalEtagCas);
        let _store = config.into_store();
    }

    #[test]
    fn s3_store_parses_write_mode_exactly() {
        for (value, expected) in [
            ("conditional-etag", S3SessionWriteMode::ConditionalEtagCas),
            ("single-writer", S3SessionWriteMode::SingleWriter),
        ] {
            let mut values = s3_values();
            values.push((ENV_S3_WRITE_MODE, value));
            let SessionStorageConfig::S3(config) = config(&values).unwrap() else {
                panic!("expected S3 config");
            };
            assert_eq!(config.write_mode, expected);
        }

        for invalid in ["", "Conditional-Etag", "SINGLE-WRITER", "unknown"] {
            let mut values = s3_values();
            values.push((ENV_S3_WRITE_MODE, invalid));
            assert!(matches!(
                config(&values),
                Err(SessionStorageConfigError::Invalid {
                    variable: ENV_S3_WRITE_MODE,
                    ..
                })
            ));
        }
    }

    #[test]
    fn s3_store_requires_every_explicit_value() {
        for variable in [
            ENV_S3_ENDPOINT,
            ENV_S3_REGION,
            ENV_S3_BUCKET,
            ENV_S3_PREFIX,
            ENV_S3_ACCESS_KEY_ID,
            ENV_S3_SECRET_ACCESS_KEY,
            ENV_S3_FORCE_PATH_STYLE,
        ] {
            let values = s3_values()
                .into_iter()
                .filter(|(name, _)| *name != variable)
                .collect::<Vec<_>>();
            assert_eq!(
                config(&values).unwrap_err(),
                SessionStorageConfigError::Missing(variable)
            );
        }
    }

    #[test]
    fn s3_store_rejects_empty_bucket_and_prefix() {
        for variable in [ENV_S3_BUCKET, ENV_S3_PREFIX] {
            let mut values = s3_values();
            values
                .iter_mut()
                .find(|(name, _)| *name == variable)
                .unwrap()
                .1 = "  ";
            assert_eq!(
                config(&values).unwrap_err(),
                SessionStorageConfigError::Missing(variable)
            );
        }
    }

    #[test]
    fn s3_store_parses_boolean_exactly() {
        let mut values = s3_values();
        values
            .iter_mut()
            .find(|(name, _)| *name == ENV_S3_FORCE_PATH_STYLE)
            .unwrap()
            .1 = "false";
        let SessionStorageConfig::S3(s3_config) = config(&values).unwrap() else {
            panic!("expected S3 config");
        };
        assert!(!s3_config.force_path_style);

        for invalid in ["TRUE", "1", "yes"] {
            values
                .iter_mut()
                .find(|(name, _)| *name == ENV_S3_FORCE_PATH_STYLE)
                .unwrap()
                .1 = invalid;
            assert!(matches!(
                config(&values),
                Err(SessionStorageConfigError::Invalid {
                    variable: ENV_S3_FORCE_PATH_STYLE,
                    ..
                })
            ));
        }
    }

    #[test]
    fn s3_endpoint_requires_secure_base_url_except_on_loopback() {
        for valid in [
            "https://rgw.example.test",
            "http://localhost:7480",
            "http://127.0.0.1:7480",
            "http://[::1]:7480",
        ] {
            let mut values = s3_values();
            values
                .iter_mut()
                .find(|(name, _)| *name == ENV_S3_ENDPOINT)
                .unwrap()
                .1 = valid;
            assert!(config(&values).is_ok(), "expected {valid} to be valid");
        }

        for invalid in [
            "http://rgw.example.test",
            "ftp://rgw.example.test",
            "https://user:password@rgw.example.test",
            "https://rgw.example.test/s3",
            "https://rgw.example.test?tenant=waltr",
            "https://rgw.example.test#fragment",
            "not-a-url",
        ] {
            let mut values = s3_values();
            values
                .iter_mut()
                .find(|(name, _)| *name == ENV_S3_ENDPOINT)
                .unwrap()
                .1 = invalid;
            assert!(
                matches!(
                    config(&values),
                    Err(SessionStorageConfigError::Invalid {
                        variable: ENV_S3_ENDPOINT,
                        ..
                    })
                ),
                "expected {invalid} to be invalid"
            );
        }
    }

    #[test]
    fn s3_credentials_are_redacted_from_debug_and_errors() {
        let configured = config(&s3_values()).unwrap();
        let debug = format!("{configured:?}");
        assert!(!debug.contains("access-key-value"));
        assert!(!debug.contains("secret-key-value"));
        assert_eq!(debug.matches("<redacted>").count(), 2);
        assert!(debug.contains("write_mode: ConditionalEtagCas"));

        let mut values = s3_values();
        values
            .iter_mut()
            .find(|(name, _)| *name == ENV_S3_FORCE_PATH_STYLE)
            .unwrap()
            .1 = "secret-key-value";
        let error = config(&values).unwrap_err().to_string();
        assert!(!error.contains("access-key-value"));
        assert!(!error.contains("secret-key-value"));
    }
}
