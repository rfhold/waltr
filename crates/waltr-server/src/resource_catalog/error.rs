//! Shared resource catalog failures and service-boundary classification.

use waltr_agent::AgentDefinitionError;
use std::{error::Error, fmt, io, path::PathBuf};

use super::state::ResourceKind;

/// Failure to load, validate, maintain, or persist server agent definitions.
#[derive(Debug)]
pub enum AgentDefinitionLoadError {
    /// A path could not be inspected.
    Inspect {
        /// Sanitized path that could not be inspected.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// A directory could not be enumerated.
    ReadDirectory {
        /// Sanitized directory path.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// An external path is neither a regular file nor directory.
    UnsupportedPath(PathBuf),
    /// A resource tree entry is a symlink or unsupported file type.
    UnsupportedResourcePath(PathBuf),
    /// A resource path is not valid Unicode.
    NonUnicodeResourcePath(PathBuf),
    /// A nested YAML resource was discovered.
    NestedResource(PathBuf),
    /// A source file could not be read as UTF-8.
    Read {
        /// Sanitized source path.
        path: PathBuf,
        /// Underlying read or UTF-8 conversion error.
        source: io::Error,
    },
    /// A resource file failed strict parsing.
    InvalidResource {
        /// Canonical relative resource path.
        path: String,
        /// Strict parser failure.
        source: AgentDefinitionError,
    },
    /// A non-agent resource failed strict YAML deserialization.
    ResourceYaml {
        /// Canonical resource path.
        path: String,
        /// YAML decoder error.
        source: serde_yaml_ng::Error,
    },
    /// A strictly parsed resource contained an invalid typed value.
    InvalidResourceValue {
        /// Canonical resource path.
        path: String,
        /// Typed validation failure.
        message: String,
    },
    /// A source contains other than exactly one resource document.
    ResourceCount {
        /// Canonical relative resource path.
        path: String,
        /// Number of parsed documents.
        count: usize,
    },
    /// A resource does not match its category and filename identity.
    ResourceIdentity {
        /// Canonical relative resource path.
        path: String,
        /// Kind required by the resource category.
        expected_kind: ResourceKind,
        /// Name required by the filename.
        expected_name: String,
        /// Kind declared by the source.
        actual_kind: ResourceKind,
        /// Name declared by the source.
        actual_name: String,
    },
    /// A canonical API path was invalid.
    InvalidResourcePath(String),
    /// Two effective files declared the same identity.
    DuplicateIdentity {
        /// Duplicated resource kind.
        kind: ResourceKind,
        /// Duplicated resource name.
        name: String,
    },
    /// An external or composed source failed registry loading.
    DefinitionSource(AgentDefinitionError),
    /// The default agent could not be resolved.
    Resolve {
        /// Requested default agent name.
        name: String,
        /// Definition resolution failure.
        source: AgentDefinitionError,
    },
    /// One or more loaded definitions could not be resolved.
    ResolveAll {
        /// Definition resolution failure.
        source: AgentDefinitionError,
    },
    /// An agent requires an omitted compiled feature.
    UnsupportedFeature {
        /// Agent requiring the feature.
        name: String,
        /// Feature omitted from this build.
        feature: &'static str,
    },
    /// The configured default agent was empty.
    EmptyDefaultAgent,
    /// The configured default agent was not Unicode.
    DefaultAgentNotUnicode,
    /// An expected persisted-slot `ETag` was missing.
    MissingExpectedEtag,
    /// An expected persisted-slot `ETag` was stale.
    StaleEtag {
        /// Canonical relative resource path.
        path: String,
        /// Caller-provided `ETag`.
        expected: String,
        /// Current persisted-slot `ETag`.
        actual: String,
    },
    /// The resource tree changed concurrently with an API mutation.
    ConcurrentFilesystemChange,
    /// A persisted file could not be atomically written or removed.
    Write {
        /// Sanitized resource path.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// The filesystem watcher could not start.
    Watch(notify::Error),
}

impl AgentDefinitionLoadError {
    /// Whether this failure is a stale-write precondition failure.
    #[must_use]
    pub const fn is_failed_precondition(&self) -> bool {
        matches!(
            self,
            Self::StaleEtag { .. } | Self::ConcurrentFilesystemChange
        )
    }

    /// Whether this failure is caused by caller-controlled definition input.
    #[must_use]
    pub const fn is_validation(&self) -> bool {
        matches!(
            self,
            Self::UnsupportedPath(_)
                | Self::UnsupportedResourcePath(_)
                | Self::NonUnicodeResourcePath(_)
                | Self::NestedResource(_)
                | Self::InvalidResource { .. }
                | Self::ResourceYaml { .. }
                | Self::InvalidResourceValue { .. }
                | Self::ResourceCount { .. }
                | Self::ResourceIdentity { .. }
                | Self::InvalidResourcePath(_)
                | Self::DuplicateIdentity { .. }
                | Self::DefinitionSource(_)
                | Self::Resolve { .. }
                | Self::ResolveAll { .. }
                | Self::UnsupportedFeature { .. }
                | Self::EmptyDefaultAgent
                | Self::DefaultAgentNotUnicode
                | Self::MissingExpectedEtag
        )
    }

    /// Return a bounded message that omits source text, filesystem paths, and OS details.
    #[must_use]
    pub const fn sanitized_validation_message(&self) -> &'static str {
        match self {
            Self::UnsupportedPath(_) => "unsupported agent-definition source path",
            Self::UnsupportedResourcePath(_) => "unsupported agent-definition resource entry",
            Self::NonUnicodeResourcePath(_) => "agent-definition resource paths must be Unicode",
            Self::NestedResource(_) => "nested agent-definition resources are not allowed",
            Self::InvalidResource { .. }
            | Self::ResourceYaml { .. }
            | Self::InvalidResourceValue { .. }
            | Self::DefinitionSource(_) => "agent-definition source is invalid",
            Self::ResourceCount { .. } => {
                "agent-definition resources must contain exactly one document"
            }
            Self::ResourceIdentity { .. } => {
                "agent-definition resource identity must match its path"
            }
            Self::InvalidResourcePath(_) => "invalid canonical agent-definition resource path",
            Self::DuplicateIdentity { .. } => "duplicate agent-definition resource identity",
            Self::Resolve { .. } | Self::ResolveAll { .. } => {
                "agent definitions could not be resolved"
            }
            Self::UnsupportedFeature { .. } => {
                "agent definition requires support not compiled into this server"
            }
            Self::EmptyDefaultAgent => "default agent must be nonempty",
            Self::DefaultAgentNotUnicode => "default agent must be valid Unicode",
            Self::MissingExpectedEtag => "expected_etag must not be empty",
            _ => "invalid agent-definition request",
        }
    }
}

impl fmt::Display for AgentDefinitionLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inspect { path, source } => write!(formatter, "failed to inspect `{}`: {source}", path.display()),
            Self::ReadDirectory { path, source } => write!(formatter, "failed to read directory `{}`: {source}", path.display()),
            Self::UnsupportedPath(path) => write!(formatter, "agent-definition path `{}` is not a regular file or directory", path.display()),
            Self::UnsupportedResourcePath(path) => write!(formatter, "resource path `{}` must be a nonsymlink directory or regular YAML file", path.display()),
            Self::NonUnicodeResourcePath(path) => write!(formatter, "resource path `{}` is not valid Unicode", path.display()),
            Self::NestedResource(path) => write!(formatter, "nested YAML resource `{}` is not allowed", path.display()),
            Self::Read { path, source } => write!(formatter, "failed to read agent-definition file `{}`: {source}", path.display()),
            Self::InvalidResource { path, source } => write!(formatter, "invalid resource `{path}`: {source}"),
            Self::ResourceYaml { path, source } => write!(formatter, "invalid resource `{path}`: {source}"),
            Self::InvalidResourceValue { path, message } => write!(formatter, "invalid resource `{path}`: {message}"),
            Self::ResourceCount { path, count } => write!(formatter, "resource `{path}` must contain exactly one document, found {count}"),
            Self::ResourceIdentity { path, expected_kind, expected_name, actual_kind, actual_name } => write!(formatter, "resource `{path}` must declare {expected_kind} `{expected_name}`, found {actual_kind} `{actual_name}`"),
            Self::InvalidResourcePath(path) => write!(formatter, "invalid canonical agent-definition resource path `{path}`"),
            Self::DuplicateIdentity { kind, name } => write!(formatter, "duplicate effective {kind} resource `{name}`"),
            Self::DefinitionSource(error) => write!(formatter, "invalid composed agent-definition source: {error}"),
            Self::Resolve { name, source } => write!(formatter, "failed to resolve default agent `{name}`: {source}"),
            Self::ResolveAll { source } => write!(formatter, "failed to resolve loaded agent definitions: {source}"),
            Self::UnsupportedFeature { name, feature } => write!(formatter, "agent `{name}` requires `{feature}` support not compiled into this server"),
            Self::EmptyDefaultAgent => formatter.write_str("WALTR_DEFAULT_AGENT must be nonempty"),
            Self::DefaultAgentNotUnicode => formatter.write_str("WALTR_DEFAULT_AGENT must be valid Unicode"),
            Self::MissingExpectedEtag => formatter.write_str("expected_etag must not be empty"),
            Self::StaleEtag { path, expected, actual } => write!(formatter, "failed precondition: stale ETag for `{path}`: expected `{expected}`, current `{actual}`"),
            Self::ConcurrentFilesystemChange => formatter.write_str("failed precondition: resource tree changed during the operation; inspect and retry"),
            Self::Write { path, source } => write!(formatter, "failed to update resource path `{}`: {source}", path.display()),
            Self::Watch(error) => write!(formatter, "failed to start agent-definition watcher: {error}"),
        }
    }
}

impl Error for AgentDefinitionLoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Inspect { source, .. }
            | Self::ReadDirectory { source, .. }
            | Self::Read { source, .. }
            | Self::Write { source, .. } => Some(source),
            Self::InvalidResource { source, .. }
            | Self::DefinitionSource(source)
            | Self::Resolve { source, .. }
            | Self::ResolveAll { source } => Some(source),
            Self::ResourceYaml { source, .. } => Some(source),
            Self::Watch(source) => Some(source),
            _ => None,
        }
    }
}
