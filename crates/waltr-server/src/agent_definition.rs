//! Startup loading for server-owned agent definitions.

use std::{
    env,
    error::Error,
    ffi::OsString,
    fmt, fs, io,
    path::{Path, PathBuf},
};

#[cfg(any(test, debug_assertions, feature = "test"))]
use waltr_agent::AgentDefinitionBundle;
use waltr_agent::{
    AgentDefinitionCatalog, AgentDefinitionError, AgentDefinitionRegistry, ResolvedAgent,
};

const BUNDLED_DEFINITIONS: &str = include_str!("../agent-definitions.yaml");
const DEFAULT_AGENT: &str = "coding";
const ENV_DEFINITION_PATHS: &str = "WALTR_AGENT_DEFINITION_PATHS";
const ENV_DEFAULT_AGENT: &str = "WALTR_DEFAULT_AGENT";

/// Load the bundled and environment-configured definitions and resolve the default agent.
pub fn load_default_agent_from_env() -> Result<LoadedAgentDefinition, AgentDefinitionLoadError> {
    let paths = env::var_os(ENV_DEFINITION_PATHS)
        .map(|value| env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_default();
    let default_agent = env::var_os(ENV_DEFAULT_AGENT)
        .map(os_string_to_default_name)
        .transpose()?
        .unwrap_or_else(|| DEFAULT_AGENT.to_owned());
    load_default_agent(&paths, &default_agent)
}

fn os_string_to_default_name(value: OsString) -> Result<String, AgentDefinitionLoadError> {
    let value = value
        .into_string()
        .map_err(|_| AgentDefinitionLoadError::DefaultAgentNotUnicode)?;
    if value.trim().is_empty() {
        return Err(AgentDefinitionLoadError::EmptyDefaultAgent);
    }
    Ok(value)
}

pub fn load_default_agent(
    paths: &[PathBuf],
    default_agent: &str,
) -> Result<LoadedAgentDefinition, AgentDefinitionLoadError> {
    if default_agent.trim().is_empty() {
        return Err(AgentDefinitionLoadError::EmptyDefaultAgent);
    }

    let mut registry = AgentDefinitionRegistry::new();
    let mut sources = vec![BUNDLED_DEFINITIONS.to_owned()];
    registry
        .load_yaml(BUNDLED_DEFINITIONS)
        .map_err(AgentDefinitionLoadError::BundledDefinitions)?;
    for configured_path in paths {
        for source_path in source_paths(configured_path)? {
            let source = fs::read_to_string(&source_path).map_err(|source| {
                AgentDefinitionLoadError::Read {
                    path: source_path.clone(),
                    source,
                }
            })?;
            registry
                .load_yaml(&source)
                .map_err(|source| AgentDefinitionLoadError::Definition {
                    path: source_path,
                    source,
                })?;
            sources.push(source);
        }
    }
    let agents = registry
        .resolve_agents()
        .map_err(|source| AgentDefinitionLoadError::ResolveAll { source })?;
    let resolved = agents
        .iter()
        .find(|agent| agent.name == default_agent)
        .cloned()
        .ok_or_else(|| AgentDefinitionLoadError::Resolve {
            name: default_agent.to_owned(),
            source: AgentDefinitionError::MissingReference {
                kind: waltr_agent::AgentDefinitionResourceKind::Agent,
                name: default_agent.to_owned(),
                field: None,
            },
        })?;
    validate_compiled_features(&resolved)?;
    let selectable = agents
        .into_iter()
        .filter(|agent| validate_compiled_features(agent).is_ok());
    let catalog =
        AgentDefinitionCatalog::from_resolved_agents(sources.clone(), default_agent, selectable)
            .map_err(|source| AgentDefinitionLoadError::ResolveAll { source })?;
    Ok(LoadedAgentDefinition {
        #[cfg(any(test, debug_assertions, feature = "test"))]
        resolved,
        #[cfg(any(test, debug_assertions, feature = "test"))]
        bundle: AgentDefinitionBundle::new(default_agent, sources),
        catalog,
    })
}

/// Selected runtime policy and the exact ordered sources that produced it.
#[derive(Debug, Clone)]
pub struct LoadedAgentDefinition {
    /// Resolved policy installed in the runtime factory.
    #[cfg(any(test, debug_assertions, feature = "test"))]
    pub resolved: ResolvedAgent,
    /// Self-contained source bundle persisted with newly created sessions.
    #[cfg(any(test, debug_assertions, feature = "test"))]
    pub bundle: AgentDefinitionBundle,
    /// Every selectable, compiled-valid startup agent.
    pub catalog: AgentDefinitionCatalog,
}

#[cfg(not(feature = "shell"))]
fn validate_compiled_features(agent: &ResolvedAgent) -> Result<(), AgentDefinitionLoadError> {
    if agent.features.shell.is_some() {
        return Err(AgentDefinitionLoadError::UnsupportedFeature {
            name: agent.name.clone(),
            feature: "shell",
        });
    }
    Ok(())
}

#[cfg(feature = "shell")]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the cfg-disabled implementation validates the same fallible contract"
)]
const fn validate_compiled_features(_: &ResolvedAgent) -> Result<(), AgentDefinitionLoadError> {
    Ok(())
}

fn source_paths(path: &Path) -> Result<Vec<PathBuf>, AgentDefinitionLoadError> {
    let metadata = fs::metadata(path).map_err(|source| AgentDefinitionLoadError::Inspect {
        path: path.to_owned(),
        source,
    })?;
    if metadata.is_file() {
        return Ok(vec![path.to_owned()]);
    }
    if !metadata.is_dir() {
        return Err(AgentDefinitionLoadError::UnsupportedPath(path.to_owned()));
    }

    let entries = fs::read_dir(path).map_err(|source| AgentDefinitionLoadError::ReadDirectory {
        path: path.to_owned(),
        source,
    })?;
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| AgentDefinitionLoadError::ReadDirectory {
            path: path.to_owned(),
            source,
        })?;
        let file_type = entry
            .file_type()
            .map_err(|source| AgentDefinitionLoadError::Inspect {
                path: entry.path(),
                source,
            })?;
        let entry_path = entry.path();
        if file_type.is_file() && is_yaml(&entry_path) {
            files.push(entry_path);
        }
    }
    files.sort();
    Ok(files)
}

fn is_yaml(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|value| value.to_str()),
        Some("yaml" | "yml")
    )
}

/// Failure to load or resolve the server's startup agent definition.
#[derive(Debug)]
pub enum AgentDefinitionLoadError {
    /// The server's compile-time definition bundle is invalid.
    BundledDefinitions(AgentDefinitionError),
    /// A configured path could not be inspected.
    Inspect {
        /// Configured path.
        path: PathBuf,
        /// Filesystem failure.
        source: io::Error,
    },
    /// A configured directory could not be enumerated.
    ReadDirectory {
        /// Configured directory.
        path: PathBuf,
        /// Filesystem failure.
        source: io::Error,
    },
    /// A configured path is neither a regular file nor a directory.
    UnsupportedPath(PathBuf),
    /// An agent-definition file could not be read as UTF-8 text.
    Read {
        /// Source file path.
        path: PathBuf,
        /// Filesystem failure.
        source: io::Error,
    },
    /// One source file contains invalid definitions.
    Definition {
        /// Source file path.
        path: PathBuf,
        /// Definition parser failure.
        source: AgentDefinitionError,
    },
    /// The selected default agent could not be resolved from the loaded registry.
    Resolve {
        /// Selected agent name.
        name: String,
        /// Definition resolution failure.
        source: AgentDefinitionError,
    },
    /// One or more loaded agent definitions could not be resolved.
    ResolveAll {
        /// Definition resolution failure.
        source: AgentDefinitionError,
    },
    /// The selected agent requires a feature omitted from this server build.
    UnsupportedFeature {
        /// Selected agent name.
        name: String,
        /// Unsupported feature name.
        feature: &'static str,
    },
    /// `WALTR_DEFAULT_AGENT` was explicitly empty or whitespace-only.
    EmptyDefaultAgent,
    /// `WALTR_DEFAULT_AGENT` was not valid Unicode.
    DefaultAgentNotUnicode,
}

impl fmt::Display for AgentDefinitionLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BundledDefinitions(error) => {
                write!(formatter, "invalid bundled agent definitions: {error}")
            }
            Self::Inspect { path, source } => write!(
                formatter,
                "failed to inspect agent-definition path `{}`: {source}",
                path.display()
            ),
            Self::ReadDirectory { path, source } => write!(
                formatter,
                "failed to read agent-definition directory `{}`: {source}",
                path.display()
            ),
            Self::UnsupportedPath(path) => write!(
                formatter,
                "agent-definition path `{}` is not a regular file or directory",
                path.display()
            ),
            Self::Read { path, source } => write!(
                formatter,
                "failed to read agent-definition file `{}`: {source}",
                path.display()
            ),
            Self::Definition { path, source } => write!(
                formatter,
                "invalid agent definitions in `{}`: {source}",
                path.display()
            ),
            Self::Resolve { name, source } => write!(
                formatter,
                "failed to resolve default agent `{name}`: {source}"
            ),
            Self::ResolveAll { source } => {
                write!(
                    formatter,
                    "failed to resolve loaded agent definitions: {source}"
                )
            }
            Self::UnsupportedFeature { name, feature } => write!(
                formatter,
                "agent `{name}` requires `{feature}` support not compiled into this server"
            ),
            Self::EmptyDefaultAgent => formatter.write_str("WALTR_DEFAULT_AGENT must be nonempty"),
            Self::DefaultAgentNotUnicode => {
                formatter.write_str("WALTR_DEFAULT_AGENT must be valid Unicode")
            }
        }
    }
}

impl Error for AgentDefinitionLoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::BundledDefinitions(error) => Some(error),
            Self::Inspect { source, .. }
            | Self::ReadDirectory { source, .. }
            | Self::Read { source, .. } => Some(source),
            Self::Definition { source, .. }
            | Self::Resolve { source, .. }
            | Self::ResolveAll { source } => Some(source),
            Self::UnsupportedPath(_)
            | Self::UnsupportedFeature { .. }
            | Self::EmptyDefaultAgent
            | Self::DefaultAgentNotUnicode => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        process,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(test: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = env::temp_dir().join(format!(
                "waltr-agent-definitions-{test}-{}-{nonce}",
                process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn write(&self, name: &str, source: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, source).unwrap();
            path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn agent(name: &str, description: &str) -> String {
        format!(
            "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: {name}\nspec:\n  description: {description}\n"
        )
    }

    #[cfg(feature = "shell")]
    #[test]
    fn bundled_default_resolves_to_coding() {
        let loaded = load_default_agent(&[], "coding").unwrap();
        assert_eq!(loaded.resolved.name, "coding");
        assert!(loaded.resolved.features.filesystem.is_some());
        assert_eq!(loaded.resolved.permissions.len(), 3);
        assert_eq!(loaded.bundle.agent_name, "coding");
        assert_eq!(loaded.bundle.sources, [BUNDLED_DEFINITIONS]);
        assert_eq!(loaded.bundle.resolve().unwrap(), loaded.resolved);
    }

    #[cfg(not(feature = "shell"))]
    #[test]
    fn selected_shell_agent_is_rejected_without_shell_support() {
        let error = load_default_agent(&[], "coding").unwrap_err();
        assert!(matches!(
            error,
            AgentDefinitionLoadError::UnsupportedFeature {
                ref name,
                feature: "shell"
            } if name == "coding"
        ));
        assert!(error.to_string().contains("not compiled into this server"));
    }

    #[test]
    fn explicit_files_and_later_sources_replace_resources() {
        let directory = TestDirectory::new("files");
        let first_source = agent("selected", "first");
        let second_source = agent("selected", "second");
        let first = directory.write("first.yaml", &first_source);
        let second = directory.write("second.yml", &second_source);
        let loaded = load_default_agent(&[first, second], "selected").unwrap();
        assert_eq!(loaded.resolved.description.as_deref(), Some("second"));
        assert_eq!(
            loaded.bundle.sources,
            [BUNDLED_DEFINITIONS, &first_source, &second_source]
        );
    }

    #[test]
    fn catalog_lists_sorted_agents_and_builds_explicit_bundles() {
        let directory = TestDirectory::new("catalog");
        let source = format!("{}---\n{}", agent("zeta", "last"), agent("alpha", "first"));
        let path = directory.write("agents.yaml", &source);
        let loaded = load_default_agent(&[path], "alpha").unwrap();

        let names = loaded
            .catalog
            .entries()
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>();
        assert!(names.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(loaded.catalog.default_agent_name(), "alpha");
        assert_eq!(loaded.catalog.bundle("zeta").unwrap().agent_name, "zeta");
        assert!(loaded.catalog.bundle("missing").is_none());
    }

    #[test]
    fn directory_sources_are_loaded_in_lexical_order() {
        let directory = TestDirectory::new("directory");
        directory.write("b.yml", &agent("selected", "second"));
        directory.write("a.yaml", &agent("selected", "first"));
        directory.write("ignored.txt", "not YAML");
        fs::create_dir(directory.0.join("nested")).unwrap();
        let loaded = load_default_agent(std::slice::from_ref(&directory.0), "selected").unwrap();
        assert_eq!(loaded.resolved.description.as_deref(), Some("second"));
    }

    #[test]
    fn invalid_path_yaml_and_default_are_contextual() {
        let directory = TestDirectory::new("errors");
        let missing = directory.0.join("missing.yaml");
        let error = load_default_agent(std::slice::from_ref(&missing), "coding").unwrap_err();
        assert!(error.to_string().contains(missing.to_str().unwrap()));
        assert!(error.source().is_some());

        let invalid = directory.write("invalid.yaml", "kind: [");
        let error = load_default_agent(std::slice::from_ref(&invalid), "coding").unwrap_err();
        assert!(error.to_string().contains(invalid.to_str().unwrap()));
        assert!(error.source().is_some());

        let error = load_default_agent(&[], "missing").unwrap_err();
        assert!(error.to_string().contains("default agent `missing`"));
        assert!(error.source().is_some());
        assert!(matches!(
            load_default_agent(&[], "  "),
            Err(AgentDefinitionLoadError::EmptyDefaultAgent)
        ));
    }

    #[test]
    fn unselected_invalid_agent_fails_startup_resolution() {
        let directory = TestDirectory::new("unselected-invalid");
        let path = directory.write(
            "invalid.yaml",
            "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: invalid\nspec:\n  features:\n    - featureSetRef: { name: missing }\n",
        );

        assert!(matches!(
            load_default_agent(&[path], "coding"),
            Err(AgentDefinitionLoadError::ResolveAll { .. })
        ));
    }
}
